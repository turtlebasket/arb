//! AMM simulators. Each submodule implements [`crate::pool::Pool`] for one
//! math family. Concrete DEX forks map onto these:
//!
//! | Fork                         | Module        |
//! |------------------------------|---------------|
//! | Uniswap V2, Pancake V2, SunSwap, Solidly-volatile | [`univ2`]   |
//! | Uniswap V3, Pancake V3, Aerodrome Slipstream      | [`univ3`]   |
//! | Aerodrome/Velodrome stable                        | [`solidly`] |
//! | Curve StableSwap                                  | [`curve`]   |
//! | Balancer V2 weighted                              | [`balancer`]|

pub mod aerodrome;
pub mod balancer;
pub mod balancer_math;
pub mod curve;
pub mod solidly;
pub mod univ2;
pub mod univ3;
