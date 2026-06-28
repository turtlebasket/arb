//! Base-specific: **Flashblocks** preconfirmations.
//!
//! Flashblocks are an OP-stack/Base feature (rollup-boost): the sequencer emits
//! partial-block "preconfirmations" roughly every ~200ms, appended in `index`
//! order to the in-progress block, well before the sealed block (~2s). This
//! module is therefore **NOT chain-agnostic** — it only makes sense on Base (or
//! other Flashblocks-enabled OP chains). Other chains would model low-latency
//! visibility differently (e.g. pending-tx/mempool) and must not depend on this.
//!
//! Chain-agnostic types (`PoolEvent`, `PoolState`) are reused from
//! [`crate::live::event`]; only the Flashblocks framing lives here.

use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;

use crate::live::event::PoolEvent;
use crate::live::source::SourceError;

/// Where a piece of state came from in Base's confirmation pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// Preconfirmation: the `index`-th Flashblock of `block` (0,1,2,…).
    Flashblock { index: u64 },
    /// Canonical sealed block.
    Sealed,
}

/// One Flashblock preconfirmation: a batch of pool-state updates appended to the
/// in-progress `block`. `events` are in intra-block order with block-cumulative
/// `log_index`, so ordering is consistent with the eventual sealed block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flashblock {
    pub block: u64,
    pub index: u64,
    pub events: Vec<PoolEvent>,
}

pub type FlashblockStream = Pin<Box<dyn Stream<Item = Flashblock> + Send>>;

/// A source of Base Flashblock preconfirmations. Kept separate from the
/// sealed-block [`crate::live::source::ChainSource`] so the two tiers can be
/// mocked and reconciled independently.
#[async_trait]
pub trait PreconfSource: Send + Sync + 'static {
    async fn subscribe(&self) -> Result<FlashblockStream, SourceError>;
}

/// Deterministic in-memory preconfirmation source for tests.
pub struct MockPreconf {
    batches: Vec<Flashblock>,
}

impl MockPreconf {
    pub fn new(batches: Vec<Flashblock>) -> Self {
        Self { batches }
    }
}

#[async_trait]
impl PreconfSource for MockPreconf {
    async fn subscribe(&self) -> Result<FlashblockStream, SourceError> {
        Ok(Box::pin(futures::stream::iter(self.batches.clone())))
    }
}

/// Real Base Flashblocks source — **STUB**.
///
/// The production endpoint is the Base Flashblocks WebSocket
/// (`wss://mainnet.flashblocks.base.org/ws`, or Alchemy's Flashblocks API),
/// which streams `flashblocks_subscribe`-style payloads: per-flashblock diffs
/// with `index`, `base` metadata on index 0, and appended txs/receipts/logs.
/// Wiring = connect, parse each payload, decode pool logs into [`PoolEvent`]s,
/// and emit a [`Flashblock`]. Decoding reuses the same log-decode path as the
/// sealed source. Until then this yields an empty stream so the dual engine
/// runs sealed-only.
#[cfg(feature = "live-rpc")]
pub struct BaseFlashblocksSource {
    pub url: String,
}

#[cfg(feature = "live-rpc")]
impl BaseFlashblocksSource {
    /// Default public Base Flashblocks WS endpoint.
    pub const DEFAULT_URL: &'static str = "wss://mainnet.flashblocks.base.org/ws";

    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

#[cfg(feature = "live-rpc")]
#[async_trait]
impl PreconfSource for BaseFlashblocksSource {
    async fn subscribe(&self) -> Result<FlashblockStream, SourceError> {
        // TODO(base): connect to self.url, parse Flashblocks payloads, decode
        // pool logs -> PoolEvent, emit Flashblock per (block, index).
        eprintln!(
            "[stub] BaseFlashblocksSource: flashblocks streaming not implemented yet \
             (endpoint {}); running sealed-only.",
            self.url
        );
        Ok(Box::pin(futures::stream::empty()))
    }
}
