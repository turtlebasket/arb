//! Cross-AMM arbitrage scanner — pool simulation core.
//!
//! This crate currently implements the **simulation** layer: exact (or, for
//! Balancer, near-exact) off-chain reproductions of each AMM's swap math, plus
//! multi-hop path evaluation that nets out swap fees and gas. Live data
//! ingestion, opportunity search, and execution are built on top of this.
//!
//! Supported chains (later layers): Base, BSC, Tron. Supported AMM families:
//! see [`amm`].

pub mod amm;
pub mod book;
pub mod config;
pub mod graph;
pub mod live;
pub mod math;
pub mod path;
pub mod pool;
pub mod sim;
pub mod types;

pub use pool::{Pool, Protocol, SimError};
