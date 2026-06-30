//! Exact Uniswap V3 (and fork) state fetching: the FULL set of initialized
//! ticks with `liquidityNet`, so the offline swap simulator does exact
//! tick-crossing — no within-range approximation.
//!
//! Walks the pool's `tickBitmap` across the whole tick range (batched via
//! Multicall3) to find initialized ticks, then batch-reads `ticks(tick)` for
//! each `liquidityNet`. All reads are pinned to a block for reproducibility.

use alloy::eips::BlockId;
use alloy::primitives::aliases::I24;
use alloy::primitives::{address, Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::amm::univ3::{TickData, MAX_TICK, MIN_TICK};
use crate::live::source::SourceError;

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function tickSpacing() external view returns (int24);
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        function slot0() external view returns (
            uint160 sqrtPriceX96, int24 tick, uint16 obsIndex, uint16 obsCard,
            uint16 obsCardNext, uint8 feeProtocol, bool unlocked
        );
        function liquidity() external view returns (uint128);
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross, int128 liquidityNet,
            uint256 f0, uint256 f1, int56 tc, uint160 spl, uint32 so, bool initialized
        );
    }

    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result { bool success; bytes returnData; }
        function aggregate3(Call3[] calls) external payable returns (Result[] returnData);
    }
}

pub const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

fn rpc<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Rpc(e.to_string())
}

/// Floor division toward -inf (Solidity `int24 >> ` / `%` semantics).
fn compress(tick: i32, spacing: i32) -> i32 {
    let mut c = tick / spacing;
    if tick % spacing != 0 && (tick ^ spacing) < 0 {
        c -= 1;
    }
    c
}
fn word_pos(compressed: i32) -> i16 {
    (compressed >> 8) as i16
}

/// Current price/liquidity/tick of a V3 pool, pinned to `block`.
pub struct V3Snapshot {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub tick_spacing: i32,
}

pub async fn fetch_slot0<P: Provider>(
    provider: &P,
    pool: Address,
    block: BlockId,
) -> Result<V3Snapshot, SourceError> {
    let c = IUniswapV3Pool::new(pool, provider);
    let liq = c.liquidity().block(block).call().await.map_err(rpc)?;
    let spacing = c.tickSpacing().block(block).call().await.map_err(rpc)?;
    // slot0()'s tail fields differ across V3 forks (e.g. Aerodrome Slipstream
    // drops `feeProtocol`), so the typed decode fails on them. The first two
    // return words — sqrtPriceX96 then tick — are identical across all forks, so
    // fall back to parsing those raw when the strict decode fails.
    let (sqrt_price_x96, tick) = match c.slot0().block(block).call().await {
        Ok(s) => (U256::from(s.sqrtPriceX96), s.tick.as_i32()),
        Err(_) => {
            use alloy::primitives::{Bytes, TxKind};
            use alloy::rpc::types::{TransactionInput, TransactionRequest};
            let tx = TransactionRequest {
                to: Some(TxKind::Call(pool)),
                input: TransactionInput::new(Bytes::from(vec![0x38, 0x50, 0xc7, 0xbd])), // slot0()
                ..Default::default()
            };
            let raw = provider.call(tx).block(block).await.map_err(rpc)?;
            let b = raw.as_ref();
            if b.len() < 64 {
                return Err(SourceError::Rpc("short slot0 return".into()));
            }
            let sqrt = U256::from_be_slice(&b[0..32]);
            // tick: int24 sign-extended to 32 bytes -> low 4 bytes as i32.
            let mut t = [0u8; 4];
            t.copy_from_slice(&b[60..64]);
            (sqrt, i32::from_be_bytes(t))
        }
    };
    Ok(V3Snapshot {
        sqrt_price_x96,
        liquidity: liq,
        tick,
        tick_spacing: spacing.as_i32(),
    })
}

/// Enumerate every initialized tick and its `liquidityNet`, pinned to `block`.
pub async fn fetch_ticks<P: Provider>(
    provider: &P,
    pool: Address,
    tick_spacing: i32,
    block: BlockId,
) -> Result<Vec<TickData>, SourceError> {
    Ok(fetch_ticks_full(provider, pool, tick_spacing, block)
        .await?
        .into_iter()
        .map(|(tick, _gross, net)| TickData { tick, liquidity_net: net })
        .collect())
}

/// Like [`fetch_ticks`] but also returns `liquidityGross` per tick — needed by
/// the event-driven syncer to know when a tick becomes (un)initialized.
/// Returns `(tick, liquidityGross, liquidityNet)`.
pub async fn fetch_ticks_full<P: Provider>(
    provider: &P,
    pool: Address,
    tick_spacing: i32,
    block: BlockId,
) -> Result<Vec<(i32, u128, i128)>, SourceError> {
    let min_word = word_pos(compress(MIN_TICK, tick_spacing));
    let max_word = word_pos(compress(MAX_TICK, tick_spacing));
    read_ticks_in_words(provider, pool, tick_spacing, block, min_word, max_word).await
}

/// Approach C: fetch only the ticks within `[lo_tick, hi_tick]` (the bounded
/// window the largest probe trade can reach), instead of the whole map.
pub async fn fetch_ticks_window<P: Provider>(
    provider: &P,
    pool: Address,
    tick_spacing: i32,
    block: BlockId,
    lo_tick: i32,
    hi_tick: i32,
) -> Result<Vec<(i32, u128, i128)>, SourceError> {
    let lo = lo_tick.clamp(MIN_TICK, MAX_TICK);
    let hi = hi_tick.clamp(MIN_TICK, MAX_TICK);
    let min_word = word_pos(compress(lo, tick_spacing));
    let max_word = word_pos(compress(hi, tick_spacing));
    read_ticks_in_words(provider, pool, tick_spacing, block, min_word, max_word).await
}

/// Read all initialized ticks (+gross/net) within an inclusive bitmap-word range.
async fn read_ticks_in_words<P: Provider>(
    provider: &P,
    pool: Address,
    tick_spacing: i32,
    block: BlockId,
    min_word: i16,
    max_word: i16,
) -> Result<Vec<(i32, u128, i128)>, SourceError> {
    let mc = IMulticall3::new(MULTICALL3, provider);

    // 1. Read all bitmap words across the range, batched.
    let mut bitmap_calls = Vec::new();
    let mut words = Vec::new();
    for w in min_word..=max_word {
        bitmap_calls.push(IMulticall3::Call3 {
            target: pool,
            allowFailure: false,
            callData: Bytes::from(
                IUniswapV3Pool::tickBitmapCall { wordPosition: w }.abi_encode(),
            ),
        });
        words.push(w);
    }

    let mut initialized: Vec<i32> = Vec::new();
    const CHUNK: usize = 400;
    let mut idx = 0;
    for chunk in bitmap_calls.chunks(CHUNK) {
        let ret = mc
            .aggregate3(chunk.to_vec())
            .block(block)
            .call()
            .await
            .map_err(rpc)?;
        for res in ret {
            let w = words[idx];
            idx += 1;
            if !res.success {
                continue;
            }
            let word = IUniswapV3Pool::tickBitmapCall::abi_decode_returns(&res.returnData)
                .map_err(rpc)?;
            if word.is_zero() {
                continue;
            }
            for bit in 0u32..256 {
                if word.bit(bit as usize) {
                    let compressed = (w as i32) * 256 + bit as i32;
                    initialized.push(compressed * tick_spacing);
                }
            }
        }
    }
    initialized.sort_unstable();

    // 2. Read liquidityGross + liquidityNet for each initialized tick, batched.
    let mut out: Vec<(i32, u128, i128)> = Vec::with_capacity(initialized.len());
    for chunk in initialized.chunks(CHUNK) {
        let calls: Vec<IMulticall3::Call3> = chunk
            .iter()
            .map(|&t| IMulticall3::Call3 {
                target: pool,
                allowFailure: false,
                callData: Bytes::from(
                    IUniswapV3Pool::ticksCall { tick: I24::try_from(t).unwrap() }.abi_encode(),
                ),
            })
            .collect();
        let ret = mc.aggregate3(calls).block(block).call().await.map_err(rpc)?;
        for (res, &t) in ret.iter().zip(chunk.iter()) {
            if !res.success {
                continue;
            }
            // ticks()'s tail fields differ across V3 forks (Slipstream adds
            // staked-liquidity / reward-growth fields), but liquidityGross (word
            // 0, uint128) and liquidityNet (word 1, int128) are always first and
            // at the same offset. Parse them raw so every fork decodes correctly.
            let rd = res.returnData.as_ref();
            if rd.len() < 64 {
                continue;
            }
            let gross = u128::from_be_bytes(rd[16..32].try_into().unwrap());
            let net = i128::from_be_bytes(rd[48..64].try_into().unwrap());
            out.push((t, gross, net));
        }
    }
    Ok(out)
}
