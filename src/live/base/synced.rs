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
use crate::live::base::status::{with_spinner, StatusLine};
use crate::live::base::v3sync::{self, V3PoolState};
use crate::live::base::verify::ground_truth;
use crate::live::source::SourceError;
use crate::path::{best_size, Leg};
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
    V3(Box<V3PoolState>),
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
            kind: LiveKind::V3(Box::new(state)),
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

/// Screen cycles in-memory then confirm the top candidates on-chain at
/// `confirm_at` (a sealed block number, or `pending` for flashblock-preconfirmed
/// state). Prints confirmed arbs (or every candidate under `show_screened`) and
/// accumulates the best-per-tick into the running totals. `tag` labels output.
#[allow(clippy::too_many_arguments)]
async fn evaluate<P: Provider>(
    provider: &P,
    reg: &HashMap<Address, LivePool>,
    info: &HashMap<Address, &PoolInfo>,
    cycles: &[Vec<Edge>],
    sym: &HashMap<Address, String>,
    scale: U256,
    amount_in: U256,
    min_profit: U256,
    top: usize,
    max_confirm: usize,
    gas_price: U256,
    weth: Option<Address>,
    usdc: Address,
    confirm_at: alloy::eips::BlockId,
    tag: &str,
    show_screened: bool,
    status: &StatusLine,
    total_potential: &mut U256,
    profitable_blocks: &mut u64,
) -> Result<(), SourceError> {
    let label = |a: &Address| sym.get(a).cloned().unwrap_or_else(|| format!("{a:#}"));
    // gas_price is maintained by the caller on an interval — NOT fetched here.
    // A per-eval RPC under free-tier rate-limiting would serialize and block the
    // select loop (the deadline/Ctrl-C arms can't run while we await inside one).
    let ratio = weth.map(|w| eth_usdc_ratio(reg, usdc, w)).unwrap_or(U256::ZERO);

    // 1) cheap in-memory screen. For each cycle, search the trade SIZE that
    //    maximizes net profit (a fixed probe size sits in the dead zone — too big
    //    for thin pools, irrelevant on deep ones). `amount_in` is the size CAP
    //    (max capital); `scale` (1 base-token unit) is the marginal-probe floor.
    let dbg_screen = std::env::var("ARB_DEBUG_SCREEN").is_ok();
    let (mut d_missing, mut d_latent, mut d_best_net) = (0usize, 0usize, U256::ZERO);
    // (sim_net, gas_cost, amount, idx) — `amount` is this cycle's optimal size.
    let mut cand: Vec<(U256, U256, U256, usize)> = Vec::new();
    for (i, cyc) in cycles.iter().enumerate() {
        let legs_pools: Option<Vec<&LivePool>> = cyc.iter().map(|e| reg.get(&e.pool)).collect();
        let Some(pools) = legs_pools else {
            d_missing += 1;
            continue;
        };
        let legs: Vec<Leg> = cyc
            .iter()
            .zip(pools.iter())
            .map(|(e, lp)| Leg { pool: *lp as &dyn Pool, token_in: e.from, token_out: e.to })
            .collect();
        // `Some` => a latent arb (effective rate > 1 at the optimum); `net_profit`
        // tells us whether it also clears gas.
        if let Some(s) = best_size(&legs, scale, amount_in, gas_price, ratio) {
            d_latent += 1;
            if let Some(net) = s.result.net_profit {
                if net > d_best_net {
                    d_best_net = net;
                }
                if net >= min_profit {
                    cand.push((net, s.result.gas_cost_in_token, s.amount_in, i));
                }
            }
        }
    }
    if dbg_screen {
        status.note(&format!(
            "[{tag}] screen: {} cycles | latent_arbs(rate>1) {d_latent} | actionable(after gas) {} | best_net {}.{:06} | missing_pool {d_missing}",
            cycles.len(),
            cand.len(),
            d_best_net / scale,
            d_best_net % scale,
        ));
    }
    if cand.is_empty() {
        return Ok(());
    }
    cand.sort_by(|a, b| b.0.cmp(&a.0));
    cand.truncate(max_confirm.max(1)); // bound on-chain confirmations (RPC load)

    // 2) CONFIRM each candidate against the chain at `confirm_at` (consistent
    //    snapshot) — presence + exact values.
    let mut confirmed: Vec<(U256, U256, Vec<Address>, Vec<Address>)> = Vec::new();
    let mut screened_lines: Vec<String> = Vec::new();
    for (sim_net, gas_cost, amount, i) in &cand {
        let cyc = &cycles[*i];
        let toks: Vec<Address> =
            std::iter::once(cyc[0].from).chain(cyc.iter().map(|e| e.to)).collect();
        let pools: Vec<Address> = cyc.iter().map(|e| e.pool).collect();
        let cost = *amount + *gas_cost;
        // Confirm at the cycle's OWN optimal size (not a global fixed amount).
        // Bound each confirmation: a `pending`/sealed eth_call that never returns
        // (silent RPC stall — not an error, so retry/backoff won't fire) must not
        // block the watcher. On timeout we skip the cycle this tick.
        let confirm = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            confirm_cycle(provider, info, cyc, *amount, confirm_at),
        )
        .await;
        let chain_desc = match confirm {
            Ok(Ok(Some(gross))) if gross > cost => {
                let net = gross - cost;
                if net >= min_profit {
                    confirmed.push((net, *amount, pools.clone(), toks.clone()));
                }
                format!("chain +{}.{:06}", net / scale, net % scale)
            }
            Ok(Ok(Some(gross))) => {
                let d = cost - gross;
                format!("chain -{}.{:06} (not profitable)", d / scale, d % scale)
            }
            Ok(Ok(None)) => "chain: reverts / not a real cycle".to_string(),
            Ok(Err(e)) => format!("chain: error {e}"),
            Err(_) => "chain: RPC timeout (skipped)".to_string(),
        };
        if show_screened {
            let path: Vec<String> = toks.iter().map(&label).collect();
            screened_lines.push(format!(
                "  screened {} | size {}.{:06} | screen +{}.{:06} | {} | pools={:?}",
                path.join("->"),
                amount / scale,
                amount % scale,
                sim_net / scale,
                sim_net % scale,
                chain_desc,
                pools
            ));
        }
    }

    if show_screened {
        status.log(&format!("[{tag}] {} screened candidate(s):", cand.len()));
        for l in &screened_lines {
            status.log(l);
        }
    }

    if !confirmed.is_empty() {
        confirmed.sort_by(|a, b| b.0.cmp(&a.0));
        if !show_screened {
            status.log(&format!(
                "[{tag}] {} CONFIRMED arb(s) ({} screened):",
                confirmed.len(),
                cand.len()
            ));
            for (net, amount, pools, toks) in confirmed.iter().take(top) {
                let path: Vec<String> = toks.iter().map(&label).collect();
                status.log(&format!(
                    "  +{}.{:06} USDC @ size {}.{:06} | {} | pools={:?}",
                    net / scale,
                    net % scale,
                    amount / scale,
                    amount % scale,
                    path.join("->"),
                    pools
                ));
            }
        }
        *total_potential += confirmed[0].0;
        *profitable_blocks += 1;
    } else if !show_screened {
        status.note(&format!("[{tag}] screened {} candidate(s) -> 0 confirmed on-chain", cand.len()));
    }
    Ok(())
}

/// Live watcher driven by the synced registry. ~0 RPC per block (only a gas-price
/// read); pool state is maintained from the log subscription. When `flash_url` is
/// `Some`, evaluation is driven at Flashblock cadence (~200ms) and candidates are
/// confirmed against the `pending` block tag (flashblock-preconfirmed state), so
/// arbs are surfaced as they form rather than only at seal.
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
    flash_url: Option<String>,
) -> Result<(), SourceError> {
    let provider = crate::live::base::rpc::connect(ws_url).await?;

    let decimals: HashMap<Address, u8> =
        book.tokens.values().map(|t| (t.address, t.decimals)).collect();
    let info: HashMap<Address, &PoolInfo> = book.pools.iter().map(|p| (p.address, p)).collect();
    let sym: HashMap<Address, String> =
        book.tokens.iter().map(|(s, t)| (t.address, s.clone())).collect();
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

    // Snapshot each pool at the current head B. Build pools CONCURRENTLY
    // (bounded) — sequential init of dozens of windowed V3 pools is RPC-bound and
    // slow on rate-limited tiers; bounded concurrency cuts it ~N× while staying
    // under provider limits (RetryBackoffLayer self-throttles any 429s).
    let status = StatusLine::new();
    let b = provider.get_block_number().await.map_err(rpc)?;
    status.note(&format!(
        "synced watch: initializing {} pools @ block {b} (window={window})...",
        needed.len()
    ));
    let init_t0 = std::time::Instant::now();
    let mut reg: HashMap<Address, LivePool> = HashMap::new();
    let init_job = futures::stream::iter(
        needed.iter().filter_map(|addr| info.get(addr).map(|p| (*addr, *p))),
    )
    .map(|(addr, p)| {
        let provider = &provider;
        let decimals = &decimals;
        async move { (addr, build_live_pool(provider, p, decimals, b, window).await) }
    })
    .buffer_unordered(8)
    .collect::<Vec<(Address, Result<Option<LivePool>, SourceError>)>>();
    let built = with_spinner(&status, "initializing pools", init_job).await;
    for (addr, res) in built {
        match res {
            Ok(Some(lp)) => {
                reg.insert(addr, lp);
            }
            Ok(None) => {}
            Err(e) => status.note(&format!("  init {addr} failed: {e}")),
        }
    }
    status.set_blocks(0);
    status.note(&format!(
        "synced watch: {} pools live in {:.1}s; streaming (Ctrl-C to stop)...",
        reg.len(),
        init_t0.elapsed().as_secs_f64()
    ));

    // Flashblock cadence (pending mode). When set, evaluation is driven by
    // preconfirmations and confirmed against the `pending` tag; otherwise this is
    // an empty stream and evaluation is driven by sealed blocks.
    // IMPORTANT: a `select!` arm `Some(x) = stream.next()` over a TERMINATING
    // stream (one that returns `Ready(None)`) is immediately-ready every poll and
    // starves the other arms (the deadline/Ctrl-C never get to fire). So the
    // flashes stream must never terminate: `pending()` when off, and chained with
    // `pending()` so a dropped WS connection becomes "quiet" rather than "ended".
    let pending_mode = flash_url.is_some();
    let mut flashes: crate::live::base::flashblocks::FlashblockStream = match &flash_url {
        Some(u) => {
            use crate::live::base::flashblocks::{BaseFlashblocksSource, Flashblock, PreconfSource};
            status.note(&format!("pending mode: flashblock cadence via {u}; confirming at `pending` tag"));
            let s = BaseFlashblocksSource::new(u.clone()).subscribe().await?;
            Box::pin(s.chain(futures::stream::pending::<Flashblock>()))
        }
        None => Box::pin(futures::stream::pending()),
    };

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
    // Last pending-mode evaluation time (throttle); start in the past so the
    // first flashblock evaluates immediately.
    let mut last_eval = std::time::Instant::now() - std::time::Duration::from_secs(3600);

    // Gas price is refreshed on an interval, not per-eval (see `evaluate`).
    let mut gas_price = provider.get_gas_price().await.map(U256::from).unwrap_or(U256::ZERO);
    let mut gas_refresh = tokio::time::interval(std::time::Duration::from_secs(5));
    gas_refresh.tick().await;

    'watch: loop {
        tokio::select! {
            _ = &mut ctrlc => { status.finish(); eprintln!("\n^C — stopping."); break 'watch; }
            _ = &mut deadline => { status.finish(); eprintln!("\nrun time elapsed — stopping."); break 'watch; }
            // Refresh the cached gas price off the hot path. Bounded so a stalled
            // RPC (free-tier rate-limit) can't freeze the loop / block shutdown.
            _ = gas_refresh.tick() => {
                let g = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    provider.get_gas_price(),
                ).await;
                if let Ok(Ok(g)) = g {
                    gas_price = U256::from(g);
                }
            }
            // Apply pool events as they stream in (no per-block refetch).
            Some(log) = logs.next() => {
                if let Some(lp) = reg.get_mut(&log.address()) {
                    lp.apply_log(&log);
                }
            }
            // Periodic full re-sync from chain (corrects any drift/missed logs).
            // Whole block bounded so a rate-limited rebuild can't freeze the loop.
            _ = resync.tick(), if resync_secs > 0 => {
                let resync_job = async {
                    let rb = provider.get_block_number().await.unwrap_or(b);
                    for addr in &needed {
                        if let Some(p) = info.get(addr) {
                            if let Ok(Some(lp)) = build_live_pool(&provider, p, &decimals, rb, window).await {
                                reg.insert(*addr, lp);
                            }
                        }
                    }
                    rb
                };
                let resync_bounded = tokio::time::timeout(std::time::Duration::from_secs(60), resync_job);
                match with_spinner(&status, "re-syncing pools", resync_bounded).await {
                    Ok(rb) => status.note(&format!("re-synced {} pools @ block {rb}", reg.len())),
                    Err(_) => status.note("re-sync timed out (RPC stall) — keeping current state"),
                }
            }
            // Flashblock cadence (pending mode): evaluate against preconfirmed
            // state (`pending` tag) ~every 200ms — arbs as they form.
            Some(fb) = flashes.next() => {
                use futures::FutureExt;
                // Drain to the LATEST buffered flashblock — flashblocks arrive
                // ~5/s; processing each would queue unboundedly and flood the
                // `pending` RPC. We only care about the freshest preconf state.
                let mut latest = fb;
                while let Some(Some(f)) = flashes.next().now_or_never() {
                    latest = f;
                }
                // Throttle confirmations to bound RPC load (each eval can issue
                // many `pending` eth_calls); skip ticks inside the window.
                if last_eval.elapsed() < std::time::Duration::from_millis(800) {
                    continue;
                }
                last_eval = std::time::Instant::now();
                let tag = format!("block {} fb#{}", latest.block, latest.index);
                // Pending streaming runs ~every 800ms; cap confirmations hard so
                // we don't flood the `pending` RPC into rate-limit/retry storms.
                // Bound the whole eval so a silently-stalled RPC await (gas price,
                // quoter call) can never freeze the watcher.
                let ev = evaluate(
                    &provider, &reg, &info, cycles, &sym, scale, amount_in, min_profit, top,
                    top.clamp(1, 4), gas_price, weth, usdc, alloy::eips::BlockId::pending(), &tag,
                    show_screened, &status, &mut total_potential, &mut profitable_blocks,
                );
                let ev = tokio::time::timeout(std::time::Duration::from_secs(10), ev);
                if with_spinner(&status, "scanning (pending)", ev).await.is_err() {
                    status.note(&format!("[{tag}] eval timed out (RPC stall) — skipped"));
                }
                status.set_confirmed(profitable_blocks);
            }
            // On each new sealed block: count it. In sealed mode, evaluate +
            // confirm at this block. In pending mode, flashblocks drive evaluation.
            Some(h) = heads.next() => {
                blocks += 1;
                status.set_blocks(blocks);
                if !pending_mode {
                    let tag = format!("block {}", h.number);
                    let ev = evaluate(
                        &provider, &reg, &info, cycles, &sym, scale, amount_in, min_profit, top,
                        top.max(1) * 2, gas_price, weth, usdc, alloy::eips::BlockId::from(h.number), &tag,
                        show_screened, &status, &mut total_potential, &mut profitable_blocks,
                    );
                    let ev = tokio::time::timeout(std::time::Duration::from_secs(15), ev);
                    if with_spinner(&status, "scanning block", ev).await.is_err() {
                        status.note(&format!("[{tag}] eval timed out (RPC stall) — skipped"));
                    }
                    status.set_confirmed(profitable_blocks);
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
