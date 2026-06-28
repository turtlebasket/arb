//! Earnings simulation for the `simulate` command.
//!
//! Replays a swap-event stream for a single chain and estimates realized arb
//! profit under a latency model: when our arbitrage transaction lands `K`
//! events late (drawn from a [`pdf::LatencyPdf`]), the opportunity is
//! recomputed against the pool state advanced by those `K` events and only
//! counted if still net-positive.

pub mod engine;
pub mod pdf;
pub mod rng;
