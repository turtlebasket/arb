//! Central numeric/address types.
//!
//! Aliased in one place so the whole crate shares a single definition of the
//! arithmetic/address surface. Backed by `alloy-primitives` (exact fixed-width
//! integers; `I256` for signed math such as Balancer's LogExpMath), which is
//! also what the RPC/contract layer uses.

pub use alloy_primitives::{Address, B256, I256, U256, U512};
