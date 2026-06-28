//! Lock-striped pool-state registry for the hot path.
//!
//! `DashMap` shards internally so concurrent writers to *different* pools don't
//! contend (the log fan-out pattern), and each entry is published behind an
//! `ArcSwap` so readers (the arb scanner) load a consistent immutable snapshot
//! wait-free. Each pool is written by exactly one logical writer, so there are
//! no per-pool data races.

use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::live::event::PoolState;
use crate::types::Address;

/// A pool's published state plus the block it was last updated at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub state: PoolState,
    pub block: u64,
    pub log_index: u64,
}

#[derive(Default)]
pub struct PoolRegistry {
    pools: DashMap<Address, Arc<ArcSwap<Entry>>>,
}

impl PoolRegistry {
    pub fn new() -> Self {
        Self {
            pools: DashMap::new(),
        }
    }

    /// Install / overwrite a pool's state (writer side).
    pub fn store(&self, pool: Address, entry: Entry) {
        match self.pools.get(&pool) {
            Some(cell) => cell.store(Arc::new(entry)),
            None => {
                self.pools
                    .insert(pool, Arc::new(ArcSwap::from_pointee(entry)));
            }
        }
    }

    /// Load a pool's current state (wait-free read).
    pub fn load(&self, pool: Address) -> Option<Entry> {
        self.pools.get(&pool).map(|cell| (**cell.load()).clone())
    }

    pub fn contains(&self, pool: Address) -> bool {
        self.pools.contains_key(&pool)
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }
}
