//! On-chain ground-truth quoters — the source of truth the offline sim is
//! diffed against, to the wei. All calls are pinned to a block.
//!
//! - V3 (Uniswap, Pancake): `QuoterV2.quoteExactInputSingle` — executes the real
//!   `pool.swap` (full exact tick-crossing) and returns `amountOut`. Must be an
//!   `eth_call` (the function reverts-with-result internally).
//! - V2 (Uniswap, Pancake): `Router.getAmountsOut`.
//! - Aerodrome (stable + volatile): `Pool.getAmountOut(amountIn, tokenIn)`.

use alloy::eips::BlockId;
use alloy::primitives::aliases::{U160, U24};
use alloy::primitives::{address, Address, U256};
use alloy::providers::Provider;
use alloy::sol;

use crate::live::source::SourceError;

sol! {
    #[sol(rpc)]
    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24  fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams params)
            external returns (uint256 amountOut, uint160 a, uint32 b, uint256 c);
    }

    #[sol(rpc)]
    interface IUniV2Router {
        function getAmountsOut(uint256 amountIn, address[] path)
            external view returns (uint256[] amounts);
    }

    #[sol(rpc)]
    interface IAeroPool {
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    }
}

// --- Base mainnet addresses (Basescan-verified) ---
pub const UNISWAP_QUOTER_V2: Address = address!("3d4e44Eb1374240CE5F1B871ab261CD16335B76a");
pub const PANCAKE_QUOTER_V2: Address = address!("B048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997");
pub const UNI_V2_ROUTER: Address = address!("4752ba5DBc23f44D87826276BF6Fd6b1C372aD24");
pub const PANCAKE_V2_ROUTER: Address = address!("8cFe327CEc66d1C090Dd72bd0FF11d690C33a2Eb");

fn rpc<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Rpc(e.to_string())
}

/// The V3 quoter for a given exchange name, if we have one.
pub fn v3_quoter(exchange: &str) -> Option<Address> {
    match exchange {
        "uniswap-v3" => Some(UNISWAP_QUOTER_V2),
        "pancakeswap-v3" => Some(PANCAKE_QUOTER_V2),
        // aerodrome-slipstream uses a different (int24 tickSpacing) ABI — handled separately.
        _ => None,
    }
}

pub fn v2_router(exchange: &str) -> Option<Address> {
    match exchange {
        "uniswap-v2" => Some(UNI_V2_ROUTER),
        "pancakeswap-v2" => Some(PANCAKE_V2_ROUTER),
        _ => None,
    }
}

/// Outcome of a ground-truth quote: a value, or "the chain refuses" (revert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GtOutcome {
    Out(U256),
    Revert,
}

/// V3 ground truth via QuoterV2 (exact tick-crossing). `fee` is the pool's pip
/// fee (500/3000/...). Reverts (e.g. insufficient liquidity) map to `Revert`.
pub async fn quote_v3<P: Provider>(
    provider: &P,
    quoter: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    fee: u32,
    block: BlockId,
) -> Result<GtOutcome, SourceError> {
    let q = IQuoterV2::new(quoter, provider);
    let params = IQuoterV2::QuoteExactInputSingleParams {
        tokenIn: token_in,
        tokenOut: token_out,
        amountIn: amount_in,
        fee: U24::from(fee),
        sqrtPriceLimitX96: U160::ZERO,
    };
    match q.quoteExactInputSingle(params).block(block).call().await {
        Ok(r) => Ok(GtOutcome::Out(r.amountOut)),
        Err(e) => {
            // A contract revert is an expected "no quote"; surface other errors.
            if is_revert(&e) {
                Ok(GtOutcome::Revert)
            } else {
                Err(rpc(e))
            }
        }
    }
}

/// Aerodrome ground truth straight from the pool (exact for stable & volatile).
pub async fn quote_aero<P: Provider>(
    provider: &P,
    pool: Address,
    token_in: Address,
    amount_in: U256,
    block: BlockId,
) -> Result<GtOutcome, SourceError> {
    let p = IAeroPool::new(pool, provider);
    match p.getAmountOut(amount_in, token_in).block(block).call().await {
        Ok(out) => Ok(GtOutcome::Out(out)),
        Err(e) => {
            if is_revert(&e) {
                Ok(GtOutcome::Revert)
            } else {
                Err(rpc(e))
            }
        }
    }
}

/// V2 ground truth via the router's `getAmountsOut`.
pub async fn quote_v2<P: Provider>(
    provider: &P,
    router: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    block: BlockId,
) -> Result<GtOutcome, SourceError> {
    let r = IUniV2Router::new(router, provider);
    match r
        .getAmountsOut(amount_in, vec![token_in, token_out])
        .block(block)
        .call()
        .await
    {
        Ok(amounts) => Ok(amounts
            .last()
            .map(|x| GtOutcome::Out(*x))
            .unwrap_or(GtOutcome::Revert)),
        Err(e) => {
            if is_revert(&e) {
                Ok(GtOutcome::Revert)
            } else {
                Err(rpc(e))
            }
        }
    }
}

/// Heuristic: did this alloy error come from an on-chain revert (expected) vs a
/// transport/RPC failure (must surface)?
fn is_revert(e: &alloy::contract::Error) -> bool {
    matches!(e, alloy::contract::Error::TransportError(te) if te.as_error_resp().is_some())
}
