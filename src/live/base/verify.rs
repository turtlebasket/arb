//! Differential wei-exact verification: offline sim vs on-chain ground truth,
//! all pinned to ONE block. A single-wei discrepancy is a FAIL — the system
//! makes profit decisions on the last wei.
//!
//! The offline sim is built by [`crate::live::base::loader::load_sim`] — the
//! exact same constructor the live ranker uses — so "verified" pools and
//! "traded" pools are byte-identical.

use std::collections::HashMap;

use alloy::eips::BlockId;
use alloy::providers::Provider;

use crate::book::{PoolBook, PoolInfo};
use crate::live::base::groundtruth::{self, GtOutcome};
use crate::live::base::loader::load_sim;
use crate::live::base::{v3state, v3sync};
use crate::live::source::SourceError;
use crate::types::{Address, U256};

fn rpc<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Rpc(e.to_string())
}

#[derive(Debug, Clone)]
pub struct Mismatch {
    pub pool: Address,
    pub exchange: String,
    pub token_in: Address,
    pub amount_in: U256,
    pub mine: String,
    pub chain: String,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub block: u64,
    pub pools_checked: usize,
    pub quotes_checked: usize,
    pub passed: usize,
    /// Windowed-state quotes that exceeded the tick window (correctly signalled
    /// `IncompleteState` rather than a wrong value) — not failures.
    pub window_skips: usize,
    pub mismatches: Vec<Mismatch>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Input ladder for a `dec`-decimal token: dust, 0.01, 1, 100, 10_000 units.
fn sweep(dec: u8) -> Vec<U256> {
    let unit = U256::from(10u64).pow(U256::from(dec as u64));
    vec![
        U256::from(1u64),
        unit / U256::from(100u64),
        unit,
        unit * U256::from(100u64),
        unit * U256::from(10_000u64),
    ]
    .into_iter()
    .filter(|x| !x.is_zero())
    .collect()
}

/// On-chain ground-truth quote for a pool, dispatched by family. Pinned to a
/// block, so threading a cycle through these gives a consistent on-chain check.
pub async fn ground_truth<P: Provider>(
    provider: &P,
    p: &PoolInfo,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    block: BlockId,
) -> Result<Option<GtOutcome>, SourceError> {
    if p.kind == "uniswap_v3" {
        if let Some(q) = groundtruth::v3_quoter(&p.exchange) {
            let fee = p.fee_bps.unwrap_or(0);
            return Ok(Some(
                groundtruth::quote_v3(provider, q, token_in, token_out, amount_in, fee, block)
                    .await?,
            ));
        }
    } else if p.exchange == "aerodrome" {
        return Ok(Some(
            groundtruth::quote_aero(provider, p.address, token_in, amount_in, block).await?,
        ));
    } else if let Some(router) = groundtruth::v2_router(&p.exchange) {
        return Ok(Some(
            groundtruth::quote_v2(provider, router, token_in, token_out, amount_in, block).await?,
        ));
    }
    Ok(None)
}

fn reconcile(
    mine: Result<U256, crate::pool::SimError>,
    chain: &GtOutcome,
    p: &PoolInfo,
    token_in: Address,
    amount_in: U256,
    report: &mut VerifyReport,
) {
    report.quotes_checked += 1;
    // Strict where it matters: any positive sim quote MUST equal the chain to
    // the wei. "No actionable quote" (sim returns 0 or errors) is acceptable
    // ONLY when the chain also refuses (reverts) — both mean "do not trade", so
    // no capital is ever sized into a position the chain would reject.
    let pass = match (&mine, chain) {
        (Ok(m), GtOutcome::Out(c)) => m == c,            // value must match exactly
        (Ok(m), GtOutcome::Revert) => m.is_zero(),       // 0 == revert (no trade); >0 is a FAIL
        (Err(_), GtOutcome::Revert) => true,             // both refuse
        (Err(_), GtOutcome::Out(_)) => false,            // chain trades, sim doesn't — model gap
    };
    if pass {
        report.passed += 1;
    } else {
        report.mismatches.push(Mismatch {
            pool: p.address,
            exchange: p.exchange.clone(),
            token_in,
            amount_in,
            mine: match &mine {
                Ok(m) => m.to_string(),
                Err(e) => format!("Err({e})"),
            },
            chain: match chain {
                GtOutcome::Out(c) => c.to_string(),
                GtOutcome::Revert => "Revert".to_string(),
            },
        });
    }
}

/// Verify the EVENT-DRIVEN V3 syncer (Approach A): initialize each pool's state
/// at block B, replay its Swap/Mint/Burn logs over (B, N], then assert the
/// event-maintained state equals (a) a fresh `slot0` read at N and (b) QuoterV2
/// at N to the wei. This proves incremental sync reconstructs on-chain state
/// exactly — no per-block refetch needed.
pub async fn verify_v3_sync(
    ws_url: &str,
    book: &PoolBook,
    max_pools: usize,
    lookback: u64,
    window: i32,
) -> Result<VerifyReport, SourceError> {
    let provider = crate::live::base::rpc::connect(ws_url).await?;
    let head = provider.get_block_number().await.map_err(rpc)?;
    let n = head.saturating_sub(5);
    let b = n.saturating_sub(lookback);
    let block_n = BlockId::from(n);
    let decimals: HashMap<Address, u8> =
        book.tokens.values().map(|t| (t.address, t.decimals)).collect();

    let mut report = VerifyReport { block: n, ..Default::default() };
    eprintln!("v3-sync verify: init@{b}, replay logs ({b},{n}], check @ {n}");

    let v3_pools = book
        .pools
        .iter()
        .filter(|p| p.kind == "uniswap_v3" && groundtruth::v3_quoter(&p.exchange).is_some())
        .take(max_pools);

    for p in v3_pools {
        let quoter = groundtruth::v3_quoter(&p.exchange).unwrap();
        let fee = p.fee_bps.unwrap_or(0);
        // 1) init at B (full map, or Approach C windowed if window > 0)
        let init = if window > 0 {
            v3sync::V3PoolState::from_chain_windowed(
                &provider, p.address, p.token0, p.token1, fee, b, window,
            )
            .await
        } else {
            v3sync::V3PoolState::from_chain(&provider, p.address, p.token0, p.token1, fee, b).await
        };
        let mut state = match init {
            Ok(s) => s,
            Err(_) => continue,
        };
        // 2) replay events (B, N]
        let mut logs = v3sync::fetch_v3_logs(&provider, p.address, b + 1, n, 10).await?;
        let n_events = logs.len();
        state.apply_logs(&mut logs);
        report.pools_checked += 1;
        eprintln!("  {} {} replayed {} events", p.exchange, p.address, n_events);

        // 3a) scalar check vs fresh slot0 @ N
        if let Ok(fresh) = v3state::fetch_slot0(&provider, p.address, block_n).await {
            report.quotes_checked += 1;
            if fresh.sqrt_price_x96 == state.sqrt_price_x96
                && fresh.liquidity == state.liquidity
                && fresh.tick == state.tick
            {
                report.passed += 1;
            } else {
                report.mismatches.push(Mismatch {
                    pool: p.address,
                    exchange: format!("{}/scalars", p.exchange),
                    token_in: Address::ZERO,
                    amount_in: U256::ZERO,
                    mine: format!("sqrt={} liq={} tick={}", state.sqrt_price_x96, state.liquidity, state.tick),
                    chain: format!("sqrt={} liq={} tick={}", fresh.sqrt_price_x96, fresh.liquidity, fresh.tick),
                });
            }
        }

        // 3b) quote check vs QuoterV2 @ N
        let d0 = decimals.get(&p.token0).copied().unwrap_or(18);
        let d1 = decimals.get(&p.token1).copied().unwrap_or(18);
        for (tin, tout, din) in [(p.token0, p.token1, d0), (p.token1, p.token0, d1)] {
            for amt in sweep(din) {
                let mine = state.quote(tin, tout, amt);
                // Out-of-window (Approach C): correctly signalled, not a failure.
                if matches!(mine, Err(crate::pool::SimError::IncompleteState)) {
                    report.window_skips += 1;
                    continue;
                }
                let chain =
                    groundtruth::quote_v3(&provider, quoter, tin, tout, amt, fee, block_n).await?;
                reconcile(mine, &chain, p, tin, amt, &mut report);
            }
        }
    }
    Ok(report)
}

/// Verify supported pools wei-exact at one pinned block (`max_per_family` each).
pub async fn verify_all(
    ws_url: &str,
    book: &PoolBook,
    max_per_family: usize,
) -> Result<VerifyReport, SourceError> {
    let provider = crate::live::base::rpc::connect(ws_url).await?;
    let head = provider.get_block_number().await.map_err(rpc)?;
    let b = head.saturating_sub(5);
    let block = BlockId::from(b);
    let decimals: HashMap<Address, u8> =
        book.tokens.values().map(|t| (t.address, t.decimals)).collect();

    let mut report = VerifyReport { block: b, ..Default::default() };
    let (mut n_v3, mut n_v2, mut n_aero) = (0usize, 0usize, 0usize);

    for p in &book.pools {
        let family_full = if p.kind == "uniswap_v3" {
            if groundtruth::v3_quoter(&p.exchange).is_none() {
                continue;
            }
            let f = n_v3 >= max_per_family;
            n_v3 += if f { 0 } else { 1 };
            f
        } else if p.exchange == "aerodrome" {
            let f = n_aero >= max_per_family;
            n_aero += if f { 0 } else { 1 };
            f
        } else if groundtruth::v2_router(&p.exchange).is_some() {
            let f = n_v2 >= max_per_family;
            n_v2 += if f { 0 } else { 1 };
            f
        } else {
            continue; // unsupported family
        };
        if family_full {
            continue;
        }

        let sim = match load_sim(&provider, p, &decimals, block).await? {
            Some(s) => s,
            None => continue,
        };
        report.pools_checked += 1;
        let d0 = decimals.get(&p.token0).copied().unwrap_or(18);
        let d1 = decimals.get(&p.token1).copied().unwrap_or(18);
        eprintln!("  {} {} {}", p.kind, p.exchange, p.address);

        for (tin, tout, din) in [(p.token0, p.token1, d0), (p.token1, p.token0, d1)] {
            for amt in sweep(din) {
                let mine = sim.quote(tin, tout, amt);
                if let Some(chain) = ground_truth(&provider, p, tin, tout, amt, block).await? { reconcile(mine, &chain, p, tin, amt, &mut report) }
            }
        }
    }
    Ok(report)
}
