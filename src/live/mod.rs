//! Live streaming + block-height-safe replay.
//!
//! - [`event`]: chain event/state model.
//! - [`reconcile`]: the pure `<=B discard / >B apply` rule + ordered replay.
//! - [`registry`]: lock-striped, wait-free-read pool-state store.
//! - [`source`]: the `ChainSource` trait + deterministic `MockChain`.
//! - [`engine`]: subscribe-first init + live drain.
//! - [`bench`]: timing-bench correctness check vs on-chain state.
//! - `ws` (feature `live-rpc`): the real alloy WebSocket source for Base/BSC.

pub mod bench;
pub mod engine;
pub mod event;
pub mod reconcile;
pub mod registry;
pub mod source;

/// Base-chain-specific: Flashblocks preconfirmations + dual reconcile.
pub mod base;

#[cfg(feature = "live-rpc")]
pub mod ws;
