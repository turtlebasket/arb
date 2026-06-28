//! The `ChainSource` abstraction: a stream of ordered chain items plus
//! point-in-time pinned snapshots. The real implementation wraps an alloy WS
//! subscription (see `ws.rs`, behind the `live-rpc` feature); [`MockChain`] is a
//! deterministic in-memory source for tests.

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::{self, Stream};

use crate::live::event::{ChainItem, PoolState};
use crate::types::Address;

pub type ItemStream = Pin<Box<dyn Stream<Item = ChainItem> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("pool {0} has no snapshot at block {1}")]
    NoSnapshot(Address, u64),
}

/// A source of ordered chain events + pinned state snapshots for one chain.
#[async_trait]
pub trait ChainSource: Send + Sync + 'static {
    /// Live (or scripted) stream of decoded items, already in arrival order.
    /// MUST be called BEFORE snapshotting so no event between snapshot read and
    /// subscription registration can be lost (the subscribe-first invariant).
    async fn subscribe(&self) -> Result<ItemStream, SourceError>;

    /// Pinned snapshot of one pool's state at an explicit block.
    async fn snapshot_pool(&self, pool: Address, at: u64) -> Result<PoolState, SourceError>;

    /// The head block the source has reached (used to choose the snapshot block).
    async fn head(&self) -> Result<u64, SourceError>;
}

/// Deterministic in-memory source. The scripted `items` are delivered in order;
/// `snapshots[(pool, block)]` defines what a pinned read returns.
pub struct MockChain {
    items: Vec<ChainItem>,
    snapshots: HashMap<(Address, u64), PoolState>,
    head: u64,
}

impl MockChain {
    pub fn new(items: Vec<ChainItem>, head: u64) -> Self {
        Self {
            items,
            snapshots: HashMap::new(),
            head,
        }
    }

    pub fn with_snapshot(mut self, pool: Address, block: u64, state: PoolState) -> Self {
        self.snapshots.insert((pool, block), state);
        self
    }
}

#[async_trait]
impl ChainSource for MockChain {
    async fn subscribe(&self) -> Result<ItemStream, SourceError> {
        Ok(Box::pin(stream::iter(self.items.clone())))
    }

    async fn snapshot_pool(&self, pool: Address, at: u64) -> Result<PoolState, SourceError> {
        self.snapshots
            .get(&(pool, at))
            .cloned()
            .ok_or(SourceError::NoSnapshot(pool, at))
    }

    async fn head(&self) -> Result<u64, SourceError> {
        Ok(self.head)
    }
}
