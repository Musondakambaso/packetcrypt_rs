#![allow(dead_code)]
use crate::ann_buf::Hash;
use crate::ann_class::{AnnBufSz, AnnClass, ANNBUF_SZ};
use crate::blkmine::{AnnChunk, HeightWork};
use crate::blkminer::BlkMiner;
use crate::prooftree::ProofTree;
use packetcrypt_sys::difficulty::pc_degrade_announcement_target;
use rayon::prelude::*;
use std::cmp::max;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

#[derive(Debug)]
pub struct ClassInfo {
    pub hw: HeightWork,
    pub ann_count: usize,
    pub ann_effective_work: u32,
}

struct AnnStoreMut {
    classes: BTreeMap<HeightWork, Box<AnnClass>>,
    recent_blocks: HashMap<i32, Hash>,
}

pub struct AnnStore {
    m: RwLock<AnnStoreMut>,
}

impl AnnStore {
    pub fn new(bm: Arc<BlkMiner>) -> Self {
        // initial buf store, capable of filling the received miner entirely.
        let buf_store = (0..bm.max_anns)
            .step_by(ANNBUF_SZ)
            .map(|i| Box::new(AnnBufSz::new(Arc::clone(&bm), i as usize)));

        let hw_store = HeightWork {
            block_height: 0,
            work: 0xffffffff,
        };
        // bufs will always be stolen from this class until it is used up.
        let class_store = Box::new(AnnClass::with_bufs(buf_store, &hw_store));

        let mut classes = BTreeMap::new();
        classes.insert(hw_store, class_store);
        Self {
            m: RwLock::new(AnnStoreMut {
                classes,
                recent_blocks: HashMap::new(),
            }),
        }
    }

    pub fn block(&self, height: i32, hash: [u8; 32]) {
        let mut m = self.m.write().unwrap();
        m.recent_blocks.insert(height, hash.into());
    }

    pub fn push_anns(&self, hw: HeightWork, ac: AnnChunk) {
        let mut m = self.m.write().unwrap();

        // attempt to push the whole chunk, stealing bufs as necessary.
        let (mut indexes, mut next_block_height) = (ac.indexes, None);
        loop {
            // lookup the class matching this HeightWork, if any.
            if let Some(class) = m.classes.get(&hw) {
                let n = class.push_anns(ac.anns, indexes);
                if n == indexes.len() {
                    return;
                }
                indexes = &indexes[n..];
            }

            if let None = next_block_height {
                next_block_height = Some(1 + *m.recent_blocks.keys().max().unwrap() as u32);
            }

            // it didn't fit or there wasn't any suitable class.
            let buf = steal_non_mining_buf(&mut m, next_block_height.unwrap());
            if let Some(class) = m.classes.get(&hw) {
                class.add_buf(buf);
            } else {
                let new_class = Box::new(AnnClass::with_topbuf(buf, &hw));
                assert!(m.classes.insert(hw, new_class).is_none());
            }
        }
    }

    /// Return the classes that does have announcements at the moment, already ranked according to
    /// their effective ann work.
    /// Also it is sure to exclude the 0xffffffff effective work announcements.
    pub fn ready_classes(&self, next_height: i32) -> Vec<ClassInfo> {
        let m = self.m.read().unwrap();
        let mut ready = m
            .classes
            .par_iter()
            .map(|(&hw, ac)| {
                let age = max(0, next_height - hw.block_height) as u32;
                let aew = pc_degrade_announcement_target(hw.work, age);
                (hw, ac, aew)
            })
            .filter(|(_hw, _ac, aew)| *aew != 0xffffffff)
            .map(|(hw, ac, aew)| ClassInfo {
                hw,
                ann_count: ac.ready_anns(),
                ann_effective_work: aew,
            })
            .collect::<Vec<_>>();

        ready.sort_unstable_by_key(|ci| ci.ann_effective_work);
        ready
    }

    pub fn compute_tree(
        &self,
        set: &[HeightWork],
        pt: &mut ProofTree,
    ) -> Result<Vec<u32>, &'static str> {
        let m = self.m.read().unwrap(); // keep a read lock, so no push is made.
        let mut set = set
            .into_par_iter() // parallel, since locks must be acquired for all classes.
            .map(|hw| {
                let c = &m.classes[hw]; // will panic if a wrong hw is passed.
                (c, c.ready_anns(), None) // count again, since they may have changed.
            })
            .collect::<Vec<_>>();
        let total_anns = set.iter().map(|(_, r, _)| r).sum();
        let mut buffer = Vec::with_capacity(total_anns);

        // split the out buffer into sub-buffers for each class.
        let mut out = &mut buffer[..];
        for (_, this, dst) in &mut set {
            let (data, excess) = out.split_at_mut(*this);
            *dst = Some(data);
            out = excess;
        }
        // now that they're split, copy the hashes over in parallel.
        set.into_par_iter().for_each(|(c, _, dst)| {
            c.read_ready_anns(dst.unwrap());
        });

        // compute the tree.
        pt.compute(&mut buffer)
    }
}

fn steal_non_mining_buf(m: &mut AnnStoreMut, next_block_height: u32) -> Box<AnnBufSz> {
    let mut mining = Vec::new();
    loop {
        // find the worst AnnClass to steal a buf from.
        let (&key, worst) = m
            .classes
            .iter()
            .filter(|&(hw, _c)| !mining.contains(hw))
            .max_by_key(|&(_hw, c)| c.ann_effective_work(next_block_height))
            .unwrap();

        match worst.steal_buf() {
            Err(_) => mining.push(key),
            Ok(None) => return m.classes.remove(&key).unwrap().destroy(),
            Ok(Some(buf)) => return buf,
        }
    }
}