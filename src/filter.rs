//! Filters to apply to parsed beatmaps.

use crate::prelude::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Filter {
    Convert(Convert),
    Simultaneous(i32),
    Whitelist(Vec<Gamemode>),
    Blacklist(Vec<Gamemode>),
}
impl Filter {
    pub fn apply(&self, sm: &mut Simfile) -> Result<(bool, SimfileList)> {
        Ok(match self {
            Filter::Convert(conf) => (true, batch_convert(sm, conf)?),
            Filter::Simultaneous(max) => {
                limit_simultaneous_keys(sm, *max as usize)?;
                (true, Vec::new())
            }
            Filter::Whitelist(gms) => (should_keep(sm, gms, true), Vec::new()),
            Filter::Blacklist(gms) => (should_keep(sm, gms, false), Vec::new()),
        })
    }
}

fn simfile_seed(sm: &Simfile, salt: &str) -> u64 {
    fxhash::hash64(&(&sm.music, &sm.title_trans, &sm.desc, salt))
}

fn should_keep(sm: &Simfile, gms: &[Gamemode], white: bool) -> bool {
    gms.iter().any(|gm| *gm == sm.gamemode) == white
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Convert {
    /// Into what gamemodes to convert.
    /// Note that a single map could be converted to multiple gamemodes simultaneously.
    pub into: Vec<Gamemode>,
    /// If the input keycount is the same as the output keycount, avoid key changes.
    pub avoid_shuffle: bool,
    /// Weighting options to prevent too many jacks (quick notes on the same key).
    pub weight_curve: Vec<(f32, f32)>,
}

fn batch_convert(sm: &mut Simfile, conf: &Convert) -> Result<SimfileList> {
    let mut out = Vec::with_capacity(conf.into.len());
    for (idx, &gm) in conf.into.iter().enumerate() {
        if idx + 1 == conf.into.len() {
            convert(sm, conf, gm)?;
        } else {
            let mut tmp = Box::new(sm.clone());
            convert(&mut tmp, conf, gm)?;
            out.push(tmp);
        }
    }
    Ok(out)
}

fn convert(sm: &mut Simfile, conf: &Convert, new_gm: Gamemode) -> Result<()> {
    // Map the amount of time since the key was last active to a choose weight.
    // Try to map higher times to more weight, but not too much
    let inactive_time_to_weight = {
        let mut points = Vec::with_capacity(conf.weight_curve.len().saturating_sub(1));
        for i in 0..conf.weight_curve.len().saturating_sub(1) {
            let (this_x, this_y) = conf.weight_curve[i];
            let (next_x, next_y) = conf.weight_curve[i + 1];
            let m = (next_y - this_y) / (next_x - this_x);
            points.push((next_x, m, -this_x * m + this_y));
        }
        let default_val = conf.weight_curve.last().map(|(_x, y)| *y).unwrap_or(1.);
        move |time: f32| {
            for &(up_to, m, c) in points.iter() {
                if time <= up_to {
                    return m * time + c;
                }
            }
            default_val
        }
    };

    //Keycounts
    let in_keycount = sm.gamemode.key_count() as usize;
    let out_keycount = new_gm.key_count() as usize;
    trace!("    converting {}K to {}K", in_keycount, out_keycount);
    ensure!(in_keycount > 0, "cannot convert 0-key map");
    ensure!(out_keycount > 0, "cannot convert to 0-key map");

    //Do nothing if there is nothing to do
    if conf.avoid_shuffle && in_keycount == out_keycount {
        sm.gamemode = new_gm;
        return Ok(());
    }

    //Detach note buffer for lifetiming purposes
    let mut notes = mem::replace(&mut sm.notes, Vec::new());
    //To randomize key mappings
    let mut rng = FastRng::seed_from_u64(simfile_seed(sm, "convert"));
    //Beat -> time
    let mut to_time = ToTime::new(sm);

    //Holds the last active time (ie. when was the last time a key was down) for each outkey.
    let mut last_active_times = vec![to_time.beat_to_time(BeatPos::from(0.)); out_keycount];
    //Holds which outkeys are locked.
    //If the inner option is `Some`, that outkey should be unlocked after that beat passes.
    let mut locked_outkeys = vec![None; out_keycount];
    //If a tail occurs at the given inkey, unlock the stored outkey.
    let mut unlock_by_tails = vec![0; in_keycount];
    //Auxiliary buffer to choose weighted outkeys
    let mut choose_tmp_buf = Vec::with_capacity(out_keycount);

    for note in notes.iter_mut() {
        let note_time = to_time.beat_to_time(note.beat);
        //Unlock any auto-unlocking keys
        for locked in locked_outkeys.iter_mut() {
            if let Some(Some(unlock_after)) = *locked {
                if note.beat > unlock_after {
                    *locked = None;
                }
            }
        }
        //Map key
        let mapped_key = if note.is_tail() {
            let out_key = unlock_by_tails[note.key as usize];
            locked_outkeys[out_key] = None;
            last_active_times[out_key] = note_time;
            out_key as i32
        } else {
            //Choose an outkey using randomness and weights
            choose_tmp_buf.clear();
            choose_tmp_buf.extend(
                locked_outkeys
                    .iter()
                    .enumerate()
                    .filter(|(_i, locked)| locked.is_none())
                    .map(|(i, _locked)| i),
            );
            let mapped = choose_tmp_buf
                .choose_weighted(&mut rng, |&out_key| {
                    let time = (note_time - last_active_times[out_key]) as f32;
                    let weight = inactive_time_to_weight(time);
                    weight
                })
                .ok();
            match mapped {
                Some(&out_key) => {
                    if note.is_head() {
                        locked_outkeys[out_key] = Some(None);
                        unlock_by_tails[note.key as usize] = out_key;
                    } else {
                        locked_outkeys[out_key] = Some(Some(note.beat));
                    }
                    last_active_times[out_key] = note_time;
                    out_key as i32
                }
                None => {
                    //All output keys are locked
                    -1
                }
            }
        };
        note.key = mapped_key;
    }
    notes.retain(|note| note.key >= 0);
    //Finally, modify simfile
    sm.notes = notes;
    sm.gamemode = new_gm;
    Ok(())
}

fn limit_simultaneous_keys(sm: &mut Simfile, max_simultaneous: usize) -> Result<()> {
    let key_count = sm.gamemode.key_count() as usize;
    trace!(
        "    limiting max simultaneous keys to {}/{}K",
        max_simultaneous,
        key_count,
    );
    let mut rng = FastRng::seed_from_u64(simfile_seed(sm, "simultaneous"));
    let mut active_notes = vec![false; key_count];
    let mut beat_notes = Vec::with_capacity(key_count);
    let mut note_idx = 0;
    while note_idx < sm.notes.len() {
        //Go through the notes in this beat
        let cur_beat = sm.notes[note_idx].beat;
        let mut tmp_active_notes = 0;
        beat_notes.clear();
        while note_idx < sm.notes.len() && sm.notes[note_idx].beat == cur_beat {
            //Check out this note
            let note = &sm.notes[note_idx];
            if note.is_tail() {
                active_notes[note.key as usize] = false;
            } else {
                beat_notes.push(note_idx);
                if note.is_head() {
                    active_notes[note.key as usize] = true;
                } else {
                    tmp_active_notes += 1;
                }
            }
            //Advance to the next note
            note_idx += 1;
        }
        //Determine how many notes to remove
        let total_active_notes =
            active_notes.iter().map(|&b| b as usize).sum::<usize>() + tmp_active_notes;
        let notes_to_remove = total_active_notes.saturating_sub(max_simultaneous);
        //Actually remove notes
        for &rem_note in beat_notes.choose_multiple(&mut rng, notes_to_remove) {
            let note = &mut sm.notes[rem_note];
            if note.is_head() {
                active_notes[note.key as usize] = false;
            }
            note.key = -1;
        }
    }
    //Actually remove notes
    sm.notes.retain(|note| note.key >= 0);
    Ok(())
}