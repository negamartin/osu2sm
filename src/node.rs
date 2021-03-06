//! Create, modify and transform in-memory simfiles.

use crate::node::prelude::*;

mod prelude {
    pub use crate::{
        node::{
            align::Align, filter::Filter, osuload::OsuLoad, pipe::Pipe, rate::Rate, rekey::Rekey,
            remap::Remap, select::Select, simfilewrite::SimfileWrite, simultaneous::Simultaneous,
            space::Space, BucketId, BucketIter, BucketKind,
        },
        prelude::*,
    };
}

pub mod align;
pub mod filter;
pub mod osuload;
pub mod pipe;
pub mod rate;
pub mod rekey;
pub mod remap;
pub mod select;
pub mod simfilewrite;
pub mod simultaneous;
pub mod space;

#[derive(Clone, Default)]
struct Bucket {
    simfiles: Vec<Box<Simfile>>,
    lists: Vec<usize>,
}
impl Bucket {
    fn take_all(&mut self) -> Vec<Box<Simfile>> {
        self.lists.clear();
        mem::replace(&mut self.simfiles, default())
    }

    fn take_lists<'a>(
        &'a mut self,
        tmp_vec: &mut Vec<Box<Simfile>>,
        mut consume: impl FnMut(&mut Vec<Box<Simfile>>) -> Result<()>,
    ) -> Result<()> {
        let mut flat_simfiles = mem::replace(&mut self.simfiles, default());
        if self.lists.is_empty() {
            return Ok(());
        }
        for start_idx in self.lists.drain(..).rev().skip(1) {
            tmp_vec.clear();
            tmp_vec.extend(flat_simfiles.drain(start_idx..));
            consume(tmp_vec)?;
        }
        consume(&mut flat_simfiles)?;
        Ok(())
    }

    fn put_list(&mut self, list: impl IntoIterator<Item = Box<Simfile>>) -> usize {
        let old_len = self.simfiles.len();
        self.simfiles.extend(list);
        self.lists.push(self.simfiles.len());
        self.simfiles.len() - old_len
    }
}
impl fmt::Debug for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        struct List(usize);
        impl fmt::Debug for List {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "{} simfiles", self.0)
            }
        }
        let mut last_end = 0;
        write!(f, "Bucket(")?;
        f.debug_list()
            .entries(self.lists.iter().map(|&end_idx| {
                let count = end_idx - last_end;
                last_end = end_idx;
                List(count)
            }))
            .finish()?;
        write!(f, ")")?;
        Ok(())
    }
}

/// Stores simfiles while in transit.
#[derive(Debug, Default, Clone)]
pub struct SimfileStore {
    by_name: HashMap<String, Bucket>,
    globals: HashMap<String, String>,
    tmp_vec: Vec<Box<Simfile>>,
}
impl SimfileStore {
    pub fn reset(&mut self) {
        self.by_name.clear();
        self.globals.clear();
    }

    pub fn global_set(&mut self, name: &str, value: String) {
        match self.globals.get_mut(name) {
            Some(val) => {
                *val = value;
            }
            None => {
                self.globals.insert(name.to_string(), value);
            }
        }
    }

    pub fn global_get_expect(&self, name: &str) -> Result<&str> {
        self.global_get(name)
            .ok_or(anyhow!("global \"{}\" not set", name))
    }

    pub fn global_get(&self, name: &str) -> Option<&str> {
        self.globals.get(name).map(|s| &s[..])
    }

    pub fn get<F>(&mut self, bucket: &BucketId, mut visit: F) -> Result<()>
    where
        F: FnMut(&mut SimfileStore, &mut Vec<Box<Simfile>>) -> Result<()>,
    {
        let (name, take) = bucket.unwrap_resolved();
        if name.is_empty() {
            //Null bucket
            trace!("    get null bucket");
            return Ok(());
        }
        let b = if take {
            self.by_name.remove(name).map(|b| {
                trace!("    take bucket \"{}\" ({:?})", name, b);
                b
            })
        } else {
            self.by_name.get(name).map(|b| {
                trace!("    get bucket \"{}\" ({:?})", name, b);
                b.clone()
            })
        };
        if let Some(mut b) = b {
            let mut tmp_vec = mem::replace(&mut self.tmp_vec, Vec::new());
            b.take_lists(&mut tmp_vec, |list| visit(self, list))?;
            self.tmp_vec = tmp_vec;
        }
        Ok(())
    }

    pub fn get_each<F>(&mut self, bucket: &BucketId, mut visit: F) -> Result<()>
    where
        F: FnMut(&mut SimfileStore, Box<Simfile>) -> Result<()>,
    {
        let (name, take) = bucket.unwrap_resolved();
        if name.is_empty() {
            //Null bucket
            trace!("    get flat null bucket");
            return Ok(());
        }
        let all = if take {
            self.by_name.remove(name).map(|mut b| {
                trace!("    take flat bucket \"{}\" ({:?})", name, b);
                b.take_all()
            })
        } else {
            let tmp_vec_ref = &mut self.tmp_vec;
            self.by_name.get(name).map(|b| {
                trace!("    get flat bucket \"{}\" ({:?})", name, b);
                let mut tmp_vec = mem::replace(tmp_vec_ref, default());
                tmp_vec.extend(b.simfiles.iter().cloned());
                tmp_vec
            })
        };
        if let Some(mut all) = all {
            for sm in all.drain(..) {
                visit(self, sm)?;
            }
            if all.capacity() > self.tmp_vec.capacity() {
                self.tmp_vec = all;
            }
        }
        Ok(())
    }

    pub fn put<I>(&mut self, bucket: &BucketId, simfiles: I)
    where
        I: IntoIterator<Item = Box<Simfile>>,
        I::IntoIter: ExactSizeIterator,
    {
        let simfiles = simfiles.into_iter();
        let name = bucket.unwrap_name();
        if name.is_empty() {
            //Null bucket
            trace!("    put {} simfiles in null bucket", simfiles.len());
            return;
        }
        trace!("    put {} simfiles in bucket \"{}\"", simfiles.len(), name);
        self.by_name
            .entry(name.to_string())
            .or_default()
            .put_list(simfiles);
    }

    pub fn check(&self) -> Result<()> {
        for (bucket_name, bucket) in self.by_name.iter() {
            for (idx, sm) in bucket.simfiles.iter().enumerate() {
                sm.check().with_context(|| {
                    anyhow!(
                        "simfile {} at bucket \"{}\" failed the sanity check",
                        idx,
                        bucket_name
                    )
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BucketId {
    Resolved(String, bool),
    Auto,
    Null,
    Name(String),
    Nest(Vec<ConcreteNode>),
    Chain(Vec<ConcreteNode>),
}
impl Default for BucketId {
    fn default() -> Self {
        Self::Auto
    }
}
impl BucketId {
    #[track_caller]
    fn unwrap_name(&self) -> &str {
        self.unwrap_resolved().0
    }

    #[track_caller]
    fn unwrap_resolved(&self) -> (&str, bool) {
        match self {
            BucketId::Resolved(name, take) => (&name[..], *take),
            _ => panic!("node i/o bucket not resolved: {:?}", self),
        }
    }
}

pub trait Node: fmt::Debug {
    /// Must yield all `BucketIter::Input` values before all `BucketIter::Output` values.
    fn buckets_mut(&mut self) -> BucketIter;
    /// Run on all filters once before starting.
    fn prepare(&mut self) -> Result<()> {
        Ok(())
    }
    /// Run on every filters once, so that entry point filters can load simfiles.
    fn entry(
        &self,
        _sm_store: &mut SimfileStore,
        _on_bmset: &mut dyn FnMut(&mut SimfileStore) -> Result<()>,
    ) -> Result<()> {
        Ok(())
    }
    /// Run on every filter once for each simfile set.
    fn apply(&self, sm_store: &mut SimfileStore) -> Result<()>;
}

pub type BucketIter<'a> = Box<dyn 'a + Iterator<Item = (BucketKind, &'a mut BucketId)>>;

pub enum BucketKind {
    Generic,
    Input,
    Output,
}
impl BucketKind {
    pub fn is_input(&self) -> bool {
        match self {
            Self::Input => true,
            _ => false,
        }
    }
    pub fn is_output(&self) -> bool {
        match self {
            Self::Output => true,
            _ => false,
        }
    }
}

pub fn resolve_buckets(nodes: &[ConcreteNode]) -> Result<Vec<Box<dyn Node>>> {
    struct State {
        out: Vec<Box<dyn Node>>,
        next_id: u32,
    }
    impl State {
        fn gen_unique_name(&mut self) -> String {
            self.next_id += 1;
            format!("~{}", self.next_id)
        }
    }
    //Keep track of the last auto-output, to bind it to any auto-input
    fn resolve_layer(
        ctx: &mut State,
        input: Option<&str>,
        output: Option<&str>,
        nodes: &[ConcreteNode],
        chained: bool,
    ) -> Result<()> {
        let mut last_magnetic_out = input.map(str::to_string);
        let in_node_count = nodes.len();
        for (i, orig_node) in nodes.iter().enumerate() {
            let mut node = orig_node.clone().into_dyn();
            //The last node has its output automatically bound to the output
            //However, in non-chained mode the output is always bound to the parent output
            let mut magnetic_out = if !chained || i + 1 == in_node_count {
                output.map(str::to_string)
            } else {
                None
            };
            //In non-chained mode the input is always the parent input
            if !chained {
                last_magnetic_out = input.map(str::to_string);
            }
            let mut insert_idx = ctx.out.len();
            //Resolve each bucket
            for (kind, bucket) in node.buckets_mut() {
                let is_chained = match bucket {
                    BucketId::Chain(..) => true,
                    _ => false,
                };
                let name = match bucket {
                    BucketId::Auto => match kind {
                        BucketKind::Input => last_magnetic_out
                            .take()
                            .ok_or_else(|| anyhow!("attempt to use input, but previous node does not output (in node {:?})", orig_node))?,
                        BucketKind::Output => magnetic_out
                            .get_or_insert_with(|| ctx.gen_unique_name())
                            .clone(),
                        BucketKind::Generic => bail!(
                            "attempt to auto-bind generic bucket (in node {})",
                            i + 1
                        ),
                    },
                    BucketId::Name(name) => {
                        ensure!(
                            !name.starts_with("~"),
                            "bucket names starting with '~' are reserved and cannot be used"
                        );
                        mem::replace(name, String::new())
                    }
                    BucketId::Nest(inner_list) | BucketId::Chain(inner_list) => {
                        match kind {
                            BucketKind::Input => {
                                let into_nested = last_magnetic_out
                                    .take()
                                    .ok_or_else(|| anyhow!("attempt to use input, but previous node does not output (in node {:?})", orig_node))?;
                                let from_nested = ctx.gen_unique_name();
                                resolve_layer(ctx, Some(&into_nested), Some(&from_nested), inner_list, is_chained)?;
                                //Evaluate the current node _after_ the nested node is evaluated
                                insert_idx = ctx.out.len();
                                from_nested
                            }
                            BucketKind::Output => {
                                let into_nested = ctx.gen_unique_name();
                                let from_nested =
                                    magnetic_out.get_or_insert_with(|| ctx.gen_unique_name());
                                resolve_layer(ctx, Some(&into_nested), Some(from_nested), inner_list, is_chained)?;
                                into_nested
                            }
                            BucketKind::Generic => bail!("cannot use generic buckets with `Nest`"),
                        }
                    },
                    BucketId::Null => "".to_string(),
                    BucketId::Resolved(..) => bail!("resolved buckets cannot be used directly"),
                };
                *bucket = BucketId::Resolved(name, false);
            }
            //Bookkeeping
            ensure!(
                last_magnetic_out.is_none() || i == 0,
                "output from previous node is not used as input (in node {:?})",
                node
            );
            last_magnetic_out = magnetic_out;
            ctx.out.insert(insert_idx, node);
        }
        Ok(())
    }
    //Process nodes and output them here
    let mut ctx = State {
        out: Vec::with_capacity(nodes.len()),
        next_id: 0,
    };
    resolve_layer(&mut ctx, None, None, nodes, true)?;
    //Optimize the last reads from each bucket, by taking the value instead of cloning it
    let mut last_reads: HashMap<String, &mut BucketId> = default();
    for node in ctx.out.iter_mut() {
        for (kind, bucket) in node.buckets_mut() {
            if kind.is_input() {
                last_reads.insert(bucket.unwrap_name().to_string(), bucket);
            }
        }
    }
    for (_name, bucket) in last_reads {
        match bucket {
            BucketId::Resolved(_name, take) => {
                *take = true;
            }
            _ => panic!("unresolved bucket"),
        }
    }
    //Prepare nodes, allowing them to modify themselves
    for node in ctx.out.iter_mut() {
        node.prepare()?;
    }
    //Finally, unwrap the output
    Ok(ctx.out)
}

macro_rules! make_concrete {
    ($($node:ident,)*) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum ConcreteNode {
            $($node($node),)*
        }
        impl ConcreteNode {
            pub fn into_dyn(self) -> Box<dyn Node> {
                match self {
                    $(
                        ConcreteNode::$node(node) => Box::new(node),
                    )*
                }
            }

            pub fn as_dyn(&self) -> &dyn Node {
                match self {
                    $(
                        ConcreteNode::$node(node) => node,
                    )*
                }
            }
        }
        $(
            impl From<$node> for ConcreteNode {
                fn from(node: $node) -> Self {
                    Self::$node(node)
                }
            }
        )*
    };
}

make_concrete!(
    Pipe,
    Filter,
    Remap,
    Rekey,
    Simultaneous,
    Align,
    Select,
    Rate,
    Space,
    OsuLoad,
    SimfileWrite,
);
