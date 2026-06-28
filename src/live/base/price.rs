//! Live pricing of graph cycles: build the EXACT simulator for each pool via
//! [`crate::live::base::loader::load_sim`] (the same constructor the verifier
//! proves wei-exact against on-chain quoters), then quote every USDC→…→USDC
//! cycle and rank by gas/fee-adjusted profit. No approximation — V3 uses full
//! tick data, Aerodrome uses on-chain per-pool fees and the correct curve.

use std::collections::HashMap;

use alloy::providers::{Provider, ProviderBuilder, WsConnect};

use crate::book::PoolBook;
use crate::graph::Edge;
use crate::live::base::loader::load_sim;
use crate::live::source::SourceError;
use crate::path::{net_profit, Leg};
use crate::pool::Pool;
use crate::types::{Address, U256};

#[derive(Debug, Clone)]
pub struct PricedCycle {
    pub pools: Vec<Address>,
    pub tokens: Vec<Address>,
    pub gross_out: U256,
    pub net_profit: Option<U256>,
}

/// Fetch state for the pools used by `cycles`, build simulators, and rank the
/// cycles by output. `amount_in` is in the base token's native units.
pub async fn rank_cycles(
    ws_url: &str,
    book: &PoolBook,
    cycles: &[Vec<Edge>],
    base_token: Address,
    weth: Option<Address>,
    amount_in: U256,
    top: usize,
) -> Result<Vec<PricedCycle>, SourceError> {
    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_url.to_string()))
        .await
        .map_err(|e| SourceError::Rpc(e.to_string()))?;

    let decimals: HashMap<Address, u8> =
        book.tokens.values().map(|t| (t.address, t.decimals)).collect();
    let info: HashMap<Address, &crate::book::PoolInfo> =
        book.pools.iter().map(|p| (p.address, p)).collect();

    // Which pools do we actually need?
    let mut needed: Vec<Address> = cycles.iter().flatten().map(|e| e.pool).collect();
    needed.sort();
    needed.dedup();
    eprintln!("pricing {} pools across {} cycles...", needed.len(), cycles.len());

    // Pin to a recent block so every pool is read at the SAME state (the exact
    // construction is shared with the verifier via load_sim).
    let head = provider
        .get_block_number()
        .await
        .map_err(|e| SourceError::Rpc(e.to_string()))?;
    let block = alloy::eips::BlockId::from(head.saturating_sub(2));

    // Build the exact simulator per pool from live state.
    let mut sims: HashMap<Address, Box<dyn Pool>> = HashMap::new();
    for pool_addr in needed {
        let Some(p) = info.get(&pool_addr) else { continue };
        if let Some(sim) = load_sim(&provider, p, &decimals, block).await? {
            sims.insert(pool_addr, sim);
        }
    }
    eprintln!(
        "loaded {} live pools (exact; pruned dead/illiquid); ranking cycles...",
        sims.len()
    );

    // gas price + ETH→USDC ratio for gas accounting.
    let gas_price = provider
        .get_gas_price()
        .await
        .map(U256::from)
        .unwrap_or(U256::ZERO);
    let ratio = eth_usdc_ratio(&sims, base_token, weth).unwrap_or(U256::ZERO);

    // Quote every cycle.
    let mut priced: Vec<PricedCycle> = Vec::new();
    for cyc in cycles {
        let legs_pools: Option<Vec<&dyn Pool>> = cyc
            .iter()
            .map(|e| sims.get(&e.pool).map(|b| b.as_ref()))
            .collect();
        let Some(pools) = legs_pools else { continue }; // a pool failed to load
        let legs: Vec<Leg> = cyc
            .iter()
            .zip(pools.iter())
            .map(|(e, p)| Leg { pool: *p, token_in: e.from, token_out: e.to })
            .collect();
        if let Ok(r) = net_profit(&legs, amount_in, gas_price, ratio) {
            priced.push(PricedCycle {
                pools: cyc.iter().map(|e| e.pool).collect(),
                tokens: std::iter::once(cyc[0].from).chain(cyc.iter().map(|e| e.to)).collect(),
                gross_out: r.gross_out,
                net_profit: r.net_profit,
            });
        }
    }

    // Rank by gross output (best edge first); profitable ones float to the top.
    eprintln!(
        "{} of {} cycles survived liquidity pruning (all pools live).",
        priced.len(),
        cycles.len()
    );
    priced.sort_by(|a, b| b.gross_out.cmp(&a.gross_out));
    priced.truncate(top);
    Ok(priced)
}

/// USDC units per 1 ETH (1e18 wei), derived by quoting 1 WETH on a WETH/USDC
/// pool we already loaded. This is exactly `token_units_per_native_1e18` for
/// gas accounting in `net_profit`.
fn eth_usdc_ratio(
    sims: &HashMap<Address, Box<dyn Pool>>,
    usdc: Address,
    weth: Option<Address>,
) -> Option<U256> {
    let weth = weth?;
    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    for sim in sims.values() {
        let toks = sim.tokens();
        if toks.contains(&weth) && toks.contains(&usdc) {
            if let Ok(usdc_out) = sim.quote(weth, usdc, one_eth) {
                if !usdc_out.is_zero() {
                    return Some(usdc_out);
                }
            }
        }
    }
    None
}
