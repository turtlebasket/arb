//! Approach A: event-driven incremental Uniswap V3 state.
//!
//! Initialize a pool's full state once, then keep it exact from its logs with
//! ~zero per-block RPC:
//!   - `Swap` carries post-swap `sqrtPriceX96`, `liquidity`, `tick` → set them
//!     directly (free + self-correcting).
//!   - `Mint` / `Burn` mutate the initialized-tick map (`gross`/`net`) and the
//!     active `liquidity` if the position straddles the current tick — exactly
//!     as the pool's `_modifyPosition` does.
//!   - `Collect` / `Flash` don't change liquidity/ticks → ignored.
//!
//! Quotes reuse the verified [`UniV3Pool`] math, so the synced state is
//! wei-exact (proved by [`verify_v3_sync`] replaying mainnet logs).

use std::collections::BTreeMap;

use alloy::eips::BlockId;
use alloy::primitives::B256;
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol;
use alloy::sol_types::SolEvent;

use crate::amm::univ3::{TickData, UniV3Pool};
use crate::live::base::v3state;
use crate::live::source::SourceError;
use crate::pool::{Pool, SimError};
use crate::types::{Address, U256};

sol! {
    #[derive(Debug)]
    event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
    #[derive(Debug)]
    event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
    #[derive(Debug)]
    event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
}

fn rpc<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Rpc(e.to_string())
}

/// topic0s for the V3 events that change quotable state.
pub fn v3_event_topics() -> Vec<B256> {
    vec![
        Swap::SIGNATURE_HASH,
        Mint::SIGNATURE_HASH,
        Burn::SIGNATURE_HASH,
    ]
}

/// Live, mutable V3 pool state maintained from events.
#[derive(Debug, Clone)]
pub struct V3PoolState {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee_pips: u32,
    pub tick_spacing: i32,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    /// tick -> (liquidityGross, liquidityNet). Present == initialized.
    pub ticks: BTreeMap<i32, (u128, i128)>,
    /// Last applied `(block, log_index)` — dedup + ordering guard.
    pub last_key: (u64, u64),
    pub snapshot_block: u64,
    /// `Some((lo,hi))` if the tick map is only complete within that range
    /// (Approach C windowed init); `None` = full-range data.
    pub window: Option<(i32, i32)>,
}

impl V3PoolState {
    /// Full initial sync from chain at `block`.
    pub async fn from_chain<P: Provider>(
        provider: &P,
        address: Address,
        token0: Address,
        token1: Address,
        fee_pips: u32,
        block: u64,
    ) -> Result<Self, SourceError> {
        let bid = BlockId::from(block);
        let snap = v3state::fetch_slot0(provider, address, bid).await?;
        let full = v3state::fetch_ticks_full(provider, address, snap.tick_spacing, bid).await?;
        let ticks = full.into_iter().map(|(t, g, n)| (t, (g, n))).collect();
        Ok(Self {
            address,
            token0,
            token1,
            fee_pips,
            tick_spacing: snap.tick_spacing,
            sqrt_price_x96: snap.sqrt_price_x96,
            liquidity: snap.liquidity,
            tick: snap.tick,
            ticks,
            last_key: (block, u64::MAX), // block B fully reflected in snapshot
            snapshot_block: block,
            window: None,
        })
    }

    /// Approach C: initial sync fetching only `±window_ticks` around the current
    /// tick. Quotes that would move price beyond the window return
    /// [`SimError::IncompleteState`] (caller widens + refetches).
    pub async fn from_chain_windowed<P: Provider>(
        provider: &P,
        address: Address,
        token0: Address,
        token1: Address,
        fee_pips: u32,
        block: u64,
        window_ticks: i32,
    ) -> Result<Self, SourceError> {
        let bid = BlockId::from(block);
        let snap = v3state::fetch_slot0(provider, address, bid).await?;
        let lo = snap.tick - window_ticks;
        let hi = snap.tick + window_ticks;
        let full =
            v3state::fetch_ticks_window(provider, address, snap.tick_spacing, bid, lo, hi).await?;
        let ticks = full.into_iter().map(|(t, g, n)| (t, (g, n))).collect();
        Ok(Self {
            address,
            token0,
            token1,
            fee_pips,
            tick_spacing: snap.tick_spacing,
            sqrt_price_x96: snap.sqrt_price_x96,
            liquidity: snap.liquidity,
            tick: snap.tick,
            ticks,
            last_key: (block, u64::MAX),
            snapshot_block: block,
            window: Some((lo, hi)),
        })
    }

    fn apply_swap(&mut self, sqrt_price_x96: U256, liquidity: u128, tick: i32) {
        self.sqrt_price_x96 = sqrt_price_x96;
        self.liquidity = liquidity;
        self.tick = tick;
    }

    /// Mint/Burn tick-map mutation. `sign = +1` for mint, `-1` for burn.
    fn modify(&mut self, lower: i32, upper: i32, amount: u128, mint: bool) {
        let amt_i = amount as i128;
        // lower tick: gross += amount; net += amount (mint) / -= (burn)
        self.bump_tick(lower, amount, if mint { amt_i } else { -amt_i }, mint);
        // upper tick: gross += amount; net -= amount (mint) / += (burn)
        self.bump_tick(upper, amount, if mint { -amt_i } else { amt_i }, mint);
        // active liquidity changes iff the range straddles the current tick.
        if lower <= self.tick && self.tick < upper {
            self.liquidity = if mint {
                self.liquidity.saturating_add(amount)
            } else {
                self.liquidity.saturating_sub(amount)
            };
        }
    }

    fn bump_tick(&mut self, tick: i32, gross_delta: u128, net_delta: i128, add: bool) {
        let e = self.ticks.entry(tick).or_insert((0, 0));
        e.0 = if add {
            e.0.saturating_add(gross_delta)
        } else {
            e.0.saturating_sub(gross_delta)
        };
        e.1 += net_delta;
        if e.0 == 0 {
            self.ticks.remove(&tick); // tick no longer initialized
        }
    }

    /// Apply one log (Swap/Mint/Burn) if newer than `last_key`. Returns true if
    /// it was a recognized, applied event.
    pub fn apply_log(&mut self, log: &Log) -> bool {
        let (Some(block), Some(li)) = (log.block_number, log.log_index) else {
            return false;
        };
        let key = (block, li);
        if key <= self.last_key {
            return false; // stale / already applied / in snapshot
        }
        let Some(t0) = log.topic0() else { return false };
        let applied = if *t0 == Swap::SIGNATURE_HASH {
            if let Ok(e) = Swap::decode_log(&log.inner) {
                self.apply_swap(U256::from(e.sqrtPriceX96), e.liquidity, e.tick.as_i32());
                true
            } else {
                false
            }
        } else if *t0 == Mint::SIGNATURE_HASH {
            if let Ok(e) = Mint::decode_log(&log.inner) {
                self.modify(e.tickLower.as_i32(), e.tickUpper.as_i32(), e.amount, true);
                true
            } else {
                false
            }
        } else if *t0 == Burn::SIGNATURE_HASH {
            if let Ok(e) = Burn::decode_log(&log.inner) {
                self.modify(e.tickLower.as_i32(), e.tickUpper.as_i32(), e.amount, false);
                true
            } else {
                false
            }
        } else {
            false
        };
        if applied {
            self.last_key = key;
        }
        applied
    }

    /// Apply a batch of logs in `(block, log_index)` order.
    pub fn apply_logs(&mut self, logs: &mut [Log]) {
        logs.sort_by_key(|l| (l.block_number.unwrap_or(0), l.log_index.unwrap_or(0)));
        for l in logs.iter() {
            self.apply_log(l);
        }
    }

    /// Build a quotable [`UniV3Pool`] snapshot from the current state.
    pub fn as_pool(&self) -> UniV3Pool {
        let ticks: Vec<TickData> = self
            .ticks
            .iter()
            .filter(|(_, (g, _))| *g > 0)
            .map(|(t, (_, n))| TickData { tick: *t, liquidity_net: *n })
            .collect();
        let pool = UniV3Pool::new(
            self.address,
            self.token0,
            self.token1,
            self.fee_pips,
            self.sqrt_price_x96,
            self.liquidity,
            self.tick,
            self.tick_spacing,
            ticks,
        );
        match self.window {
            Some((lo, hi)) => pool.with_known_range(lo, hi),
            None => pool,
        }
    }

    pub fn quote(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, SimError> {
        self.as_pool().quote(token_in, token_out, amount_in)
    }
}

/// Fetch this pool's Swap/Mint/Burn logs over `[from, to]` in `window`-block
/// chunks (free-tier `eth_getLogs` cap).
pub async fn fetch_v3_logs<P: Provider>(
    provider: &P,
    pool: Address,
    from: u64,
    to: u64,
    window: u64,
) -> Result<Vec<Log>, SourceError> {
    let window = window.max(1);
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + window - 1).min(to);
        let filter = Filter::new()
            .address(pool)
            .event_signature(v3_event_topics())
            .from_block(start)
            .to_block(end);
        out.extend(provider.get_logs(&filter).await.map_err(rpc)?);
        start = end + 1;
    }
    Ok(out)
}
