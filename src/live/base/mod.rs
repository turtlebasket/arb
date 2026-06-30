//! Base-chain live streaming: **Flashblocks preconfirmations + sealed-block
//! reconciliation**.
//!
//! Scope marker: everything in this module is Base/OP-stack-specific. The
//! chain-agnostic building blocks it composes live in the parent [`crate::live`]
//! module:
//!   - sealed blocks + pinned snapshots → [`crate::live::source::ChainSource`]
//!     (implemented for real EVM by [`crate::live::ws::WsChainSource`], which we
//!     point at Alchemy for Base);
//!   - pool event/state model → [`crate::live::event`];
//!   - single-tier reconcile rule + registry → [`crate::live::reconcile`],
//!     [`crate::live::registry`].
//!
//! Base-specific here: [`flashblocks`] (the ~200ms preconfirmation stream) and
//! [`dual`] (the optimistic + sealed reconcile engine driven by it).

pub mod dual;
pub mod flashblocks;

/// Pool discovery + fork detection (network-bound).
#[cfg(feature = "live-rpc")]
pub mod scan;

/// Shared WS provider with retry/backoff (network-bound).
#[cfg(feature = "live-rpc")]
pub mod rpc;

/// Exact offline-sim construction from live state (shared by price + verify).
#[cfg(feature = "live-rpc")]
pub mod loader;

/// Live pricing + ranking of graph cycles (network-bound).
#[cfg(feature = "live-rpc")]
pub mod price;

/// Exact V3 state (full tick data) fetching (network-bound).
#[cfg(feature = "live-rpc")]
pub mod v3state;

/// Event-driven incremental V3 state sync (network-bound).
#[cfg(feature = "live-rpc")]
pub mod v3sync;

/// Sticky bottom status bar (block counter + activity spinner) for the watcher.
#[cfg(feature = "live-rpc")]
pub mod status;

/// Synced live registry + watcher (~0 RPC/block) (network-bound).
#[cfg(feature = "live-rpc")]
pub mod synced;

/// On-chain ground-truth quoters (network-bound).
#[cfg(feature = "live-rpc")]
pub mod groundtruth;

/// Differential wei-exact verification harness (network-bound).
#[cfg(feature = "live-rpc")]
pub mod verify;

/// The sealed-block source for Base is the generic EVM WS source pointed at an
/// Alchemy Base endpoint. Aliased to make intent explicit at call sites.
#[cfg(feature = "live-rpc")]
pub use crate::live::ws::WsChainSource as BaseSealedSource;
