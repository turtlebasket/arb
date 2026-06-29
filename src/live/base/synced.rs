//! Synced live registry + watcher: maintain every tracked pool's state in
//! memory from ONE log subscription, then quote cycles per block with ~zero RPC.
//!
//! - V3 pools: [`V3PoolState`] maintained from `Swap`/`Mint`/`Burn` (Approach A),
//!   windowed init (Approach C).
//! - V2 / Aerodrome pools: reserves maintained from `Sync` (absolute, every
//!   change), fee/decimals fixed at init.
//!
//! Subscribe-first then snapshot (each pool's `last_key = (B, MAX)` discards
//! events already in the snapshot), so no event is missed and none double-applied.

use std::collections::HashMap;

use alloy::primitives::{keccak256, B256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol;
use futures::StreamExt;

use crate::amm::aerodrome::AerodromeVolatilePool;
use crate::amm::solidly::SolidlyStablePool;
use crate::amm::univ2::UniV2Pool;
use crate::book::{PoolBook, PoolInfo};
use crate::graph::Edge;
use crate::live::base::groundtruth::{v2_router, GtOutcome};
use crate::live::base::loader::AERO_FACTORY;
use crate::live::base::v3sync::{self, V3PoolState};
use crate::live::base::verify::ground_truth;
use crate::live::source::SourceError;
use crate::path::{net_profit, Leg};
use crate::pool::{Pool, Protocol, SimError};
use crate::types::{Address, U256};

sol! {
    #[sol(rpc)]
    interface IV2Pair { function getReserves() external view returns (uint112 r0, uint112 r1, uint32 ts); }
    #[sol(rpc)]
    interface IAero {
        function getReserves() external view returns (uint256 r0, uint256 r1, uint256 ts);
        function stable() external view returns (bool);
    }
    #[sol(rpc)]
    interface IAeroFactory { function getFee(address pool, bool stable) external view returns (uint256); }
}

fn rpc<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Rpc(e.to_string())
}
fn sync_uint112() -> B256 {
    keccak256("Sync(uint112,uint112)")
}
fn sync_uint256() -> B256 {
    keccak256("Sync(uint256,uint256)")
}

/// All topic0s the registry consumes (V3 events + both Sync variants).
fn registry_topics() -> Vec<B256> {
    let mut t = v3sync::v3_event_topics();
    t.push(sync_uint112());
    t.push(sync_uint256());
    t
}

enum LiveKind {
    V3(V3PoolState),
    V2 { sim: UniV2Pool, last: (u64, u64) },
    AeroVol { sim: AerodromeVolatilePool, last: (u64, u64) },
    AeroStable { sim: SolidlyStablePool, last: (u64, u64) },
}

/// A pool whose state is kept live from events; quotable as a [`Pool`].
pub struct LivePool {
    pub address: Address,
    tokens: [Address; 2],
    kind: LiveKind,
}

fn apply_sync(r0: &mut U256, r1: &mut U256, last: &mut (u64, u64), log: &Log) -> bool {
    let (Some(b), Some(li)) = (log.block_number, log.log_index) else {
        return false;
    };
    let key = (b, li);
    if key <= *last {
        return false;
    }
    let Some(t0) = log.topic0() else { return false };
    if *t0 != sync_uint112() && *t0 != sync_uint256() {
        return false;
    }
    let data = log.data().data.as_ref();
    if data.len() < 64 {
        return false;
    }
    *r0 = U256::from_be_slice(&data[0..32]);
    *r1 = U256::from_be_slice(&data[32..64]);
    *last = key;
    true
}

impl LivePool {
    /// Apply one routed log to this pool's state.
    pub fn apply_log(&mut self, log: &Log) -> bool {
        match &mut self.kind {
            LiveKind::V3(s) => s.apply_log(log),
            LiveKind::V2 { sim, last } => apply_sync(&mut sim.reserve0, &mut sim.reserve1, last, log),
            LiveKind::AeroVol { sim, last } => {
                apply_sync(&mut sim.reserve0, &mut sim.reserve1, last, log)
            }
            LiveKind::AeroStable { sim, last } => {
                apply_sync(&mut sim.reserve0, &mut sim.reserve1, last, log)
            }
        }
    }
}

impl Pool for LivePool {
    fn address(&self) -> Address {
        self.address
    }
    fn protocol(&self) -> Protocol {
        match &self.kind {
            LiveKind::V3(_) => Protocol::UniswapV3,
            LiveKind::AeroStable { .. } => Protocol::SolidlyStable,
            _ => Protocol::UniswapV2,
        }
    }
    fn tokens(&self) -> &[Address] {
        &self.tokens
    }
    fn quote(&self, ti: Address, to: Address, a: U256) -> Result<U256, SimError> {
        match &self.kind {
            LiveKind::V3(s) => s.quote(ti, to, a),
            LiveKind::V2 { sim, .. } => sim.quote(ti, to, a),
            LiveKind::AeroVol { sim, .. } => sim.quote(ti, to, a),
            LiveKind::AeroStable { sim, .. } => sim.quote(ti, to, a),
        }
    }
    fn gas_estimate(&self) -> u64 {
        match &self.kind {
            LiveKind::V3(s) => s.as_pool().gas_estimate(),
            LiveKind::V2 { sim, .. } => sim.gas_estimate(),
            LiveKind::AeroVol { sim, .. } => sim.gas_estimate(),
            LiveKind::AeroStable { sim, .. } => sim.gas_estimate(),
        }
    }
}

impl std::fmt::Debug for LivePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LivePool({})", self.address)
    }
}

/// Build a pool's initial live state at `block` (`window > 0` => windowed V3).
pub async fn build_live_pool<P: Provider>(
    provider: &P,
    p: &PoolInfo,
    decimals: &HashMap<Address, u8>,
    block: u64,
    window: i32,
) -> Result<Option<LivePool>, SourceError> {
    let bid = alloy::eips::BlockId::from(block);
    let d0 = decimals.get(&p.token0).copied().unwrap_or(18);
    let d1 = decimals.get(&p.token1).copied().unwrap_or(18);
    let fee = p.fee_bps.unwrap_or(0);

    if p.kind == "uniswap_v3" {
        let state = if window > 0 {
            V3PoolState::from_chain_windowed(provider, p.address, p.token0, p.token1, fee, block, window).await?
        } else {
            V3PoolState::from_chain(provider, p.address, p.token0, p.token1, fee, block).await?
        };
        return Ok(Some(LivePool {
            address: p.address,
            tokens: [p.token0, p.token1],
            kind: LiveKind::V3(state),
        }));
    }
    if p.exchange == "aerodrome" {
        let aero = IAero::new(p.address, provider);
        let stable = match aero.stable().block(bid).call().await {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let r = aero.getReserves().block(bid).call().await.map_err(rpc)?;
        if r.r0.is_zero() || r.r1.is_zero() {
            return Ok(None);
        }
        let fee_bps = IAeroFactory::new(AERO_FACTORY, provider)
            .getFee(p.address, stable)
            .block(bid)
            .call()
            .await
            .map_err(rpc)?
            .to::<u64>() as u32;
        let kind = if stable {
            LiveKind::AeroStable {
                sim: SolidlyStablePool::new(p.address, p.token0, p.token1, r.r0, r.r1, d0, d1, fee_bps),
                last: (block, u64::MAX),
            }
        } else {
            LiveKind::AeroVol {
                sim: AerodromeVolatilePool::new(p.address, p.token0, p.token1, r.r0, r.r1, fee_bps),
                last: (block, u64::MAX),
            }
        };
        return Ok(Some(LivePool { address: p.address, tokens: [p.token0, p.token1], kind }));
    }
    if v2_router(&p.exchange).is_some() {
        let fee_bps = if p.exchange == "pancakeswap-v2" { 25 } else { 30 };
        let pair = IV2Pair::new(p.address, provider);
        let r = match pair.getReserves().block(bid).call().await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        if r.r0 == 0 || r.r1 == 0 {
            return Ok(None);
        }
        return Ok(Some(LivePool {
            address: p.address,
            tokens: [p.token0, p.token1],
            kind: LiveKind::V2 {
                sim: UniV2Pool::new(p.address, p.token0, p.token1, U256::from(r.r0), U256::from(r.r1), fee_bps),
                last: (block, u64::MAX),
            },
        }));
    }
    Ok(None)
}

/// Re-quote a cycle entirely against on-chain ground truth at a pinned block
/// (consistent snapshot), threading each hop's output into the next. Returns the
/// final output, or `None` if any hop reverts / returns zero on chain. This
/// confirms both PRESENCE and exact VALUES, killing cross-pool state-lag
/// artifacts from the in-memory registry.
async fn confirm_cycle<P: Provider>(
    provider: &P,
    info: &HashMap<Address, &PoolInfo>,
    cyc: &[Edge],
    amount_in: U256,
    block: alloy::eips::BlockId,
) -> Result<Option<U256>, SourceError> {
    let mut amt = amount_in;
    for e in cyc {
        let Some(p) = info.get(&e.pool) else { return Ok(None) };
        match ground_truth(provider, p, e.from, e.to, amt, block).await? {
            Some(GtOutcome::Out(o)) if !o.is_zero() => amt = o,
            _ => return Ok(None), // chain reverts / zero -> not a real cycle
        }
    }
    Ok(Some(amt))
}

/// ETH→USDC ratio (USDC units per 1e18 wei) from the in-memory registry.
fn eth_usdc_ratio(reg: &HashMap<Address, LivePool>, usdc: Address, weth: Address) -> U256 {
    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    for lp in reg.values() {
        if lp.tokens.contains(&weth) && lp.tokens.contains(&usdc) {
            if let Ok(out) = lp.quote(weth, usdc, one_eth) {
                if !out.is_zero() {
                    return out;
                }
            }
        }
    }
    U256::ZERO
}

/// Live watcher driven by the synced registry. ~0 RPC per block (only a gas-price
/// read); pool state is maintained from the log subscription.
#[allow(clippy::too_many_arguments)]
pub async fn watch(
    ws_url: &str,
    book: &PoolBook,
    cycles: &[Vec<Edge>],
    base_token: Address,
    weth: Option<Address>,
    amount_in: U256,
    min_profit: U256,
    top: usize,
    window: i32,
    run_for: Option<std::time::Duration>,
    resync_secs: u64,
    show_screened: bool,
) -> Result<(), SourceError> {
    let provider = crate::live::base::rpc::connect(ws_url).await?;

    let decimals: HashMap<Address, u8> =
        book.tokens.values().map(|t| (t.address, t.decimals)).collect();
    let info: HashMap<Address, &PoolInfo> = book.pools.iter().map(|p| (p.address, p)).collect();
    let sym: HashMap<Address, String> =
        book.tokens.iter().map(|(s, t)| (t.address, s.clone())).collect();
    let label = |a: &Address| sym.get(a).cloned().unwrap_or_else(|| format!("{a:#}"));
    let scale = {
        let dec = book.tokens.values().find(|t| t.address == base_token).map(|t| t.decimals).unwrap_or(6);
        U256::from(10u64).pow(U256::from(dec as u64))
    };

    let mut needed: Vec<Address> = cycles.iter().flatten().map(|e| e.pool).collect();
    needed.sort();
    needed.dedup();

    // SUBSCRIBE FIRST (logs + heads) so nothing is missed during the snapshot.
    let mut logs = provider
        .subscribe_logs(&Filter::new().address(needed.clone()).event_signature(registry_topics()))
        .await
        .map_err(rpc)?
        .into_stream();
    let mut heads = provider.subscribe_blocks().await.map_err(rpc)?.into_stream();

    // Snapshot each pool at the current head B.
    let b = provider.get_block_number().await.map_err(rpc)?;
    eprintln!("synced watch: initializing {} pools @ block {b} (window={window})...", needed.len());
    let mut reg: HashMap<Address, LivePool> = HashMap::new();
    for addr in &needed {
        let Some(p) = info.get(addr) else { continue };
        match build_live_pool(&provider, p, &decimals, b, window).await {
            Ok(Some(lp)) => {
                reg.insert(*addr, lp);
            }
            Ok(None) => {}
            Err(e) => eprintln!("  init {addr} failed: {e}"),
        }
    }
    eprintln!("synced watch: {} pools live; streaming (Ctrl-C to stop)...", reg.len());

    let usdc = base_token;
    let mut total_potential = U256::ZERO;
    let (mut blocks, mut profitable_blocks) = (0u64, 0u64);

    let deadline = async {
        match run_for {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);
    // Create the Ctrl-C future ONCE (recreating it per-iteration can miss an
    // already-delivered SIGINT — and tokio's handler suppresses the default kill).
    let ctrlc = tokio::signal::ctrl_c();
    tokio::pin!(ctrlc);
    // Periodic full re-sync from chain (drift safety net; event-sync handles the
    // in-between). 0 = never.
    let mut resync = tokio::time::interval(std::time::Duration::from_secs(resync_secs.max(1)));
    resync.tick().await; // consume the immediate first tick

    'watch: loop {
        tokio::select! {
            _ = &mut ctrlc => { eprintln!("\n^C — stopping."); break 'watch; }
            _ = &mut deadline => { eprintln!("\nrun time elapsed — stopping."); break 'watch; }
            // Apply pool events as they stream in (no per-block refetch).
            Some(log) = logs.next() => {
                if let Some(lp) = reg.get_mut(&log.address()) {
                    lp.apply_log(&log);
                }
            }
            // Periodic full re-sync from chain (corrects any drift/missed logs).
            _ = resync.tick(), if resync_secs > 0 => {
                let rb = provider.get_block_number().await.unwrap_or(b);
                for addr in &needed {
                    if let Some(p) = info.get(addr) {
                        if let Ok(Some(lp)) = build_live_pool(&provider, p, &decimals, rb, window).await {
                            reg.insert(*addr, lp);
                        }
                    }
                }
                eprintln!("re-synced {} pools @ block {rb}", reg.len());
            }
            // On each new block: screen cycles in-memory (fast), then CONFIRM the
            // candidates against on-chain quoters at this block before reporting.
            Some(h) = heads.next() => {
                blocks += 1;
                let gas_price = provider.get_gas_price().await.map(U256::from).unwrap_or(U256::ZERO);
                let ratio = weth.map(|w| eth_usdc_ratio(&reg, usdc, w)).unwrap_or(U256::ZERO);

                // 1) cheap in-memory screen -> candidate cycle indices (+gas cost).
                let mut cand: Vec<(U256, U256, usize)> = Vec::new(); // (sim_net, gas_cost, idx)
                for (i, cyc) in cycles.iter().enumerate() {
                    let legs_pools: Option<Vec<&LivePool>> =
                        cyc.iter().map(|e| reg.get(&e.pool)).collect();
                    let Some(pools) = legs_pools else { continue };
                    let legs: Vec<Leg> = cyc
                        .iter()
                        .zip(pools.iter())
                        .map(|(e, lp)| Leg { pool: *lp as &dyn Pool, token_in: e.from, token_out: e.to })
                        .collect();
                    if let Ok(r) = net_profit(&legs, amount_in, gas_price, ratio) {
                        if let Some(net) = r.net_profit {
                            if net >= min_profit {
                                cand.push((net, r.gas_cost_in_token, i));
                            }
                        }
                    }
                }
                if cand.is_empty() {
                    continue;
                }
                cand.sort_by(|a, b| b.0.cmp(&a.0));
                cand.truncate(top.max(1) * 2); // bound confirmations

                // 2) CONFIRM each candidate against the chain at THIS block
                //    (consistent snapshot) — presence + exact values.
                let bn = alloy::eips::BlockId::from(h.number);
                let mut confirmed: Vec<(U256, Vec<Address>, Vec<Address>)> = Vec::new();
                let mut screened_lines: Vec<String> = Vec::new();
                for (sim_net, gas_cost, i) in &cand {
                    let cyc = &cycles[*i];
                    let toks: Vec<Address> =
                        std::iter::once(cyc[0].from).chain(cyc.iter().map(|e| e.to)).collect();
                    let pools: Vec<Address> = cyc.iter().map(|e| e.pool).collect();
                    let cost = amount_in + *gas_cost;
                    let chain_desc = match confirm_cycle(&provider, &info, cyc, amount_in, bn).await {
                        Ok(Some(gross)) if gross > cost => {
                            let net = gross - cost;
                            if net >= min_profit {
                                confirmed.push((net, pools.clone(), toks.clone()));
                            }
                            format!("chain +{}.{:06}", net / scale, net % scale)
                        }
                        Ok(Some(gross)) => {
                            let d = cost - gross;
                            format!("chain -{}.{:06} (not profitable)", d / scale, d % scale)
                        }
                        Ok(None) => "chain: reverts / not a real cycle".to_string(),
                        Err(e) => format!("chain: error {e}"),
                    };
                    if show_screened {
                        let path: Vec<String> = toks.iter().map(label).collect();
                        screened_lines.push(format!(
                            "  screened {} | screen +{}.{:06} | {} | pools={:?}",
                            path.join("->"), sim_net / scale, sim_net % scale, chain_desc, pools
                        ));
                    }
                }

                // --show-screened: print every candidate + its on-chain result so
                // you can SEE what looked profitable and why it was/wasn't real.
                if show_screened {
                    println!("[block {}] {} screened candidate(s):", h.number, cand.len());
                    for l in &screened_lines {
                        println!("{l}");
                    }
                }

                if !confirmed.is_empty() {
                    confirmed.sort_by(|a, b| b.0.cmp(&a.0));
                    if !show_screened {
                        println!(
                            "[block {}] {} CONFIRMED arb(s) ({} screened):",
                            h.number, confirmed.len(), cand.len()
                        );
                        for (net, pools, toks) in confirmed.iter().take(top) {
                            let path: Vec<String> = toks.iter().map(label).collect();
                            println!("  +{}.{:06} USDC | {} | pools={:?}", net / scale, net % scale, path.join("->"), pools);
                        }
                    }
                    total_potential += confirmed[0].0;
                    profitable_blocks += 1;
                } else if !show_screened {
                    // stderr diagnostic so default stdout stays clean.
                    eprintln!(
                        "[block {}] screened {} candidate(s) -> 0 confirmed on-chain",
                        h.number, cand.len()
                    );
                }
            }
        }
    }

    println!(
        "\n=== summary ===\nblocks watched: {blocks}\nprofitable blocks: {profitable_blocks}\nnet potential profit (best per block): {}.{:06} USDC",
        total_potential / scale,
        total_potential % scale
    );
    Ok(())
}
