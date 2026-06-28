//! Single source of truth for building an EXACT offline pool simulator from
//! live on-chain state. Both the verifier and the live ranker call this, so the
//! pools that pass wei-exact verification are byte-identical to the pools the
//! ranker quotes against.
//!
//! Dispatch (Base):
//!   - `uniswap_v3` (Uniswap/Pancake V3) → slot0 + liquidity + FULL tick data
//!   - exchange `aerodrome` → stable: solidly invariant; volatile: two-step fee
//!     constant-product; per-pool fee read from the factory
//!   - Uniswap/Pancake V2 → reserves + constant fee (30 / 25 bps)

use std::collections::HashMap;

use alloy::eips::BlockId;
use alloy::primitives::address;
use alloy::providers::Provider;
use alloy::sol;

use crate::amm::aerodrome::AerodromeVolatilePool;
use crate::amm::solidly::SolidlyStablePool;
use crate::amm::univ2::UniV2Pool;
use crate::amm::univ3::UniV3Pool;
use crate::book::PoolInfo;
use crate::live::base::groundtruth::v2_router;
use crate::live::base::v3state;
use crate::live::source::SourceError;
use crate::pool::Pool;
use crate::types::{Address, U256};

sol! {
    #[sol(rpc)]
    interface IV2Pair {
        function getReserves() external view returns (uint112 r0, uint112 r1, uint32 ts);
    }
    #[sol(rpc)]
    interface IAero {
        function getReserves() external view returns (uint256 r0, uint256 r1, uint256 ts);
        function stable() external view returns (bool);
    }
    #[sol(rpc)]
    interface IAeroFactory {
        function getFee(address pool, bool stable) external view returns (uint256);
    }
}

pub const AERO_FACTORY: Address = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");

fn rpc<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Rpc(e.to_string())
}

/// Build the exact simulator for a pool from its on-chain state at `block`.
/// Returns `Ok(None)` for unsupported pools or dead/illiquid state.
pub async fn load_sim<P: Provider>(
    provider: &P,
    p: &PoolInfo,
    decimals: &HashMap<Address, u8>,
    block: BlockId,
) -> Result<Option<Box<dyn Pool>>, SourceError> {
    let d0 = decimals.get(&p.token0).copied().unwrap_or(18);
    let d1 = decimals.get(&p.token1).copied().unwrap_or(18);

    if p.kind == "uniswap_v3" {
        let snap = match v3state::fetch_slot0(provider, p.address, block).await {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        if snap.liquidity == 0 || snap.sqrt_price_x96.is_zero() {
            return Ok(None);
        }
        let ticks = v3state::fetch_ticks(provider, p.address, snap.tick_spacing, block).await?;
        return Ok(Some(Box::new(UniV3Pool::new(
            p.address,
            p.token0,
            p.token1,
            p.fee_bps.unwrap_or(0),
            snap.sqrt_price_x96,
            snap.liquidity,
            snap.tick,
            snap.tick_spacing,
            ticks,
        ))));
    }

    if p.exchange == "aerodrome" {
        let aero = IAero::new(p.address, provider);
        let stable = match aero.stable().block(block).call().await {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let r = aero.getReserves().block(block).call().await.map_err(rpc)?;
        if r.r0.is_zero() || r.r1.is_zero() {
            return Ok(None);
        }
        let fee = IAeroFactory::new(AERO_FACTORY, provider)
            .getFee(p.address, stable)
            .block(block)
            .call()
            .await
            .map_err(rpc)?;
        let fee_bps = fee.to::<u64>() as u32;
        return Ok(Some(if stable {
            Box::new(SolidlyStablePool::new(
                p.address, p.token0, p.token1, r.r0, r.r1, d0, d1, fee_bps,
            ))
        } else {
            Box::new(AerodromeVolatilePool::new(
                p.address, p.token0, p.token1, r.r0, r.r1, fee_bps,
            ))
        }));
    }

    if v2_router(&p.exchange).is_some() {
        let fee_bps = if p.exchange == "pancakeswap-v2" { 25 } else { 30 };
        let pair = IV2Pair::new(p.address, provider);
        let r = match pair.getReserves().block(block).call().await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        if r.r0 == 0 || r.r1 == 0 {
            return Ok(None);
        }
        return Ok(Some(Box::new(UniV2Pool::new(
            p.address,
            p.token0,
            p.token1,
            U256::from(r.r0),
            U256::from(r.r1),
            fee_bps,
        ))));
    }

    Ok(None)
}
