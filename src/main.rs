//! `arb` CLI.
//!
//! Subcommands:
//!   - `run`          — stream live pool state for a chain (the scanner's base).
//!   - `simulate`     — estimate earnings on one chain under a latency PDF.
//!   - `timing-bench` — verify streamed/replayed state == on-chain state.
//!
//! Chain + exchange + pool selection (`--chain`, `--exchange`, `--pool`) is
//! shared across subcommands via [`ChainArgs`]. The WS endpoint is read from a
//! per-chain env var (see `Chain::rpc_env_var`), which the devshell loads from
//! a local `.env` file (see `.env.example`).

use clap::{Args, Parser, Subcommand};

use arb::config::Selection;
use arb::sim::pdf::LatencyPdf;
use arb::types::Address;

#[derive(Parser)]
#[command(name = "arb", about = "Cross-AMM arbitrage scanner (Base / BSC / Tron)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stream live pool state for the selected chain (requires --features live-rpc).
    Run(LiveArgs),
    /// Simulate earnings on one chain under a latency PDF.
    Simulate(SimulateArgs),
    /// Stream for a fixed duration, then verify replayed state == on-chain state.
    TimingBench(LiveArgs),
    /// Discover pools (factory events) + forks (codehash), update the pool book.
    Scan(ScanArgs),
    /// Verify offline sims are wei-exact vs on-chain quoters (requires --features live-rpc).
    Verify(VerifyArgs),
    /// Verify the event-driven V3 syncer: replay logs, assert state == chain.
    VerifySync(VerifySyncArgs),
    /// Watch live: print net-positive USDC→…→USDC arb opportunities each block.
    #[command(name = "watch-arb")]
    WatchArb(WatchArbArgs),
}

#[derive(Args)]
struct WatchArbArgs {
    /// Chain to watch (currently base only).
    #[arg(long)]
    chain: String,
    /// Probe trade size in whole base-token (USDC) units.
    #[arg(long, default_value_t = 1000)]
    amount: u64,
    /// Only print cycles with net profit >= this many USDC base units (1e6 = 1 USDC).
    #[arg(long, default_value_t = 0)]
    min_profit: u64,
    /// Max hops in a USDC→…→USDC cycle (2 is lightest; higher = more RPC/block).
    #[arg(long, default_value_t = 2)]
    max_hops: usize,
    /// Max opportunities to print per block.
    #[arg(long, default_value_t = 20)]
    top: usize,
    /// Stop after this many seconds (0 = run until Ctrl-C).
    #[arg(long, default_value_t = 0)]
    secs: u64,
    /// V3 windowed init: ±this many ticks (0 = full map; smaller = faster start).
    #[arg(long, default_value_t = 20000)]
    window: i32,
    /// Full re-sync from chain every N seconds (drift safety net; 0 = never).
    #[arg(long, default_value_t = 180)]
    resync_secs: u64,
    /// Print every screened candidate with its on-chain confirmation result
    /// (shows what looked profitable + why it was/wasn't real).
    #[arg(long)]
    show_screened: bool,
}

#[derive(Args)]
struct VerifyArgs {
    /// Chain to verify (currently base only).
    #[arg(long)]
    chain: String,
    /// Max pools to check per family (bounds RPC usage).
    #[arg(long, default_value_t = 5)]
    max_pools: usize,
}

#[derive(Args)]
struct VerifySyncArgs {
    /// Chain to verify (currently base only).
    #[arg(long)]
    chain: String,
    /// Max V3 pools to check.
    #[arg(long, default_value_t = 3)]
    max_pools: usize,
    /// How many blocks of history to replay (init at head-5-lookback).
    #[arg(long, default_value_t = 100)]
    lookback: u64,
    /// Approach C: init only ±this many ticks around current (0 = full map).
    #[arg(long, default_value_t = 0)]
    window: i32,
}

#[derive(Args)]
struct ScanArgs {
    /// Chain to scan (currently base only).
    #[arg(long)]
    chain: String,
    /// Number of recent blocks to scan back from head.
    #[arg(long, default_value_t = 2000)]
    blocks: u64,
    /// Max blocks per eth_getLogs request (Alchemy free tier = 10).
    #[arg(long, default_value_t = 10)]
    window: u64,
}

/// Shared chain + exchange + pool selection.
#[derive(Args, Clone)]
struct ChainArgs {
    /// Chain to operate on: base | bsc | tron.
    #[arg(long)]
    chain: String,
    /// Exchanges to include (repeatable), e.g. --exchange univ3 --exchange aerodrome.
    #[arg(long = "exchange", required = true)]
    exchanges: Vec<String>,
    /// Pool addresses to track (repeatable), e.g. --pool 0xabc...
    #[arg(long = "pool")]
    pools: Vec<String>,
}

impl ChainArgs {
    fn selection(&self) -> Selection {
        match Selection::build(&self.chain, &self.exchanges) {
            Ok(s) => s,
            Err(e) => fail(&e),
        }
    }

    fn parsed_pools(&self) -> Vec<Address> {
        self.pools
            .iter()
            .map(|p| {
                p.parse::<Address>()
                    .unwrap_or_else(|_| fail(&format!("invalid pool address '{p}'")))
            })
            .collect()
    }
}

#[derive(Args)]
struct LiveArgs {
    #[command(flatten)]
    chain: ChainArgs,
    /// How long to stream before stopping/checking, in seconds.
    #[arg(long, default_value_t = 10)]
    secs: u64,
}

#[derive(Args)]
struct SimulateArgs {
    #[command(flatten)]
    chain: ChainArgs,
    /// Latency PDF, comma-separated weights summing to 1, e.g. "0.5,0.25,0.25".
    #[arg(long, default_value = "1")]
    pdf: String,
    /// RNG seed for reproducible runs.
    #[arg(long, default_value_t = 0)]
    seed: u64,
    /// Max hops (edges) in a USDC→…→USDC cycle.
    #[arg(long, default_value_t = 3)]
    max_hops: usize,
    /// How many example cycles to print.
    #[arg(long, default_value_t = 10)]
    show: usize,
    /// Fetch live pool state and RANK cycles by gas/fee-adjusted profit
    /// (requires --features live-rpc).
    #[arg(long)]
    live: bool,
    /// Probe trade size in whole base-token units (USDC) for live ranking.
    #[arg(long, default_value_t = 1000)]
    amount: u64,
}

/// Look up a token's symbol for display, falling back to a short address.
fn token_label(book: &arb::book::PoolBook, addr: Address) -> String {
    book.tokens
        .iter()
        .find(|(_, t)| t.address == addr)
        .map(|(sym, _)| sym.clone())
        .unwrap_or_else(|| format!("{}…", &addr.to_string()[..10]))
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => {
            let sel = args.chain.selection();
            live_command(&sel, args.chain.parsed_pools(), args.secs, false);
        }
        Command::TimingBench(args) => {
            let sel = args.chain.selection();
            if !sel.chain.is_evm() {
                fail(&format!(
                    "timing-bench supports EVM chains only (not {})",
                    sel.chain
                ));
            }
            live_command(&sel, args.chain.parsed_pools(), args.secs, true);
        }
        Command::Scan(args) => {
            let chain = arb::config::Chain::parse(&args.chain).unwrap_or_else(|e| fail(&e));
            if chain != arb::config::Chain::Base {
                fail("scan is currently Base-only");
            }
            scan_command(chain, args.blocks, args.window);
        }
        Command::Verify(args) => {
            let chain = arb::config::Chain::parse(&args.chain).unwrap_or_else(|e| fail(&e));
            if chain != arb::config::Chain::Base {
                fail("verify is currently Base-only");
            }
            verify_command(chain, args.max_pools);
        }
        Command::VerifySync(args) => {
            let chain = arb::config::Chain::parse(&args.chain).unwrap_or_else(|e| fail(&e));
            if chain != arb::config::Chain::Base {
                fail("verify-sync is currently Base-only");
            }
            verify_sync_command(chain, args.max_pools, args.lookback, args.window);
        }
        Command::WatchArb(args) => {
            let chain = arb::config::Chain::parse(&args.chain).unwrap_or_else(|e| fail(&e));
            if chain != arb::config::Chain::Base {
                fail("watch-arb is currently Base-only");
            }
            watch_arb_command(chain, args.amount, args.min_profit, args.max_hops, args.top, args.secs, args.window, args.resync_secs, args.show_screened);
        }
        Command::Simulate(args) => {
            let sel = args.chain.selection();
            let pdf = LatencyPdf::parse(&args.pdf)
                .unwrap_or_else(|e| fail(&format!("invalid --pdf: {e}")));

            // Load the real graph for this chain and enumerate USDC cycles.
            use arb::book::{PoolBook, Tier};
            use arb::graph::Graph;
            let chain_name = sel.chain.to_string();
            let book = PoolBook::load(PoolBook::path_for_chain(&chain_name, Tier::Official))
                .unwrap_or_else(|e| fail(&format!("load {chain_name} official book: {e} (run `arb scan` first)")));
            let usdc = book
                .tokens
                .get("USDC")
                .unwrap_or_else(|| fail("no USDC token in book"))
                .address;
            let graph = Graph::from_book(&book);
            let cycles = graph.cycles(usdc, args.max_hops);

            println!(
                "simulate: chain={} pools={} tokens={} pdf_max_delay={} seed={}",
                sel.chain,
                book.pools.len(),
                graph.token_count(),
                pdf.max_delay(),
                args.seed,
            );
            println!(
                "USDC→…→USDC cycles up to {} hops: {}",
                args.max_hops,
                cycles.len()
            );

            if args.live {
                rank_live(&sel, &book, &cycles, usdc, args.amount, args.show);
            } else {
                for c in cycles.iter().take(args.show) {
                    let toks: Vec<String> = std::iter::once(token_label(&book, c[0].from))
                        .chain(c.iter().map(|e| token_label(&book, e.to)))
                        .collect();
                    println!("  [{}] {}", c.len(), toks.join(" -> "));
                }
                println!("(pass --live to fetch on-chain state and rank these by net profit.)");
            }
        }
    }
}

#[cfg(feature = "live-rpc")]
fn live_command(sel: &Selection, pools: Vec<Address>, secs: u64, verify: bool) {
    use arb::config::Chain;

    // The live path is Base-only for now (Flashblocks + sealed reconciliation).
    if sel.chain != Chain::Base {
        fail(&format!(
            "live streaming is currently Base-only (got {}); other chains are parked.",
            sel.chain
        ));
    }
    if pools.is_empty() {
        fail("at least one --pool address is required for live streaming");
    }

    let env_var = sel.chain.rpc_env_var(); // BASE_WSS_URL (Alchemy) — from .env
    let sealed_url = std::env::var(env_var)
        .unwrap_or_else(|_| fail(&format!("set ${env_var} to your Alchemy Base WS endpoint")));
    let flash_url = std::env::var("BASE_FLASHBLOCKS_WSS_URL").unwrap_or_else(|_| {
        arb::live::base::flashblocks::BaseFlashblocksSource::DEFAULT_URL.to_string()
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&format!("tokio runtime: {e}")));

    let result = rt.block_on(async move {
        use arb::live::base::dual::run_dual;
        use arb::live::base::flashblocks::BaseFlashblocksSource;
        use arb::live::source::ChainSource; // for snapshot_pool
        use arb::live::ws::WsChainSource;

        let sealed = WsChainSource::new(sealed_url, pools.clone()); // Alchemy sealed
        let preconf = BaseFlashblocksSource::new(flash_url); // Flashblocks (stub)

        println!(
            "streaming base for {}s: sealed=${} flashblocks={} ({} pools)...",
            secs,
            env_var,
            preconf.url,
            pools.len()
        );

        let state =
            run_dual(&sealed, &preconf, &pools, std::time::Duration::from_secs(secs)).await?;

        println!(
            "confirmed block {} | flashblock gaps {} | preconf divergences {}",
            state.confirmed_block(),
            state.gaps,
            state.divergences
        );
        for p in &pools {
            println!(
                "  {p}  optimistic={:?}  confirmed={:?}",
                state.optimistic(*p),
                state.confirmed(*p)
            );
        }

        if verify {
            // Compare confirmed (sealed) state to a fresh pinned read at the
            // confirmed block — the per-pool exactness check.
            let cut = state.confirmed_block();
            let mut mismatches = 0usize;
            for p in &pools {
                if let Some(local) = state.confirmed(*p) {
                    let onchain = sealed.snapshot_pool(*p, cut).await?;
                    if local != onchain {
                        mismatches += 1;
                        println!("  MISMATCH {p}: confirmed={local:?} onchain={onchain:?}");
                    }
                }
            }
            if mismatches == 0 {
                println!("TIMING-BENCH PASS: {} pools exact at block {cut}", pools.len());
            } else {
                println!("TIMING-BENCH FAIL: {mismatches} mismatch(es) at block {cut}");
            }
        }
        Ok::<(), arb::live::source::SourceError>(())
    });

    if let Err(e) = result {
        fail(&format!("live error: {e}"));
    }
}

#[cfg(not(feature = "live-rpc"))]
fn live_command(_sel: &Selection, _pools: Vec<Address>, _secs: u64, _verify: bool) {
    eprintln!(
        "live streaming requires the RPC backend. Rebuild with:\n    cargo run --features live-rpc -- <run|timing-bench> ..."
    );
    std::process::exit(1);
}

#[cfg(feature = "live-rpc")]
fn scan_command(chain: arb::config::Chain, blocks: u64, window: u64) {
    use arb::book::{PoolBook, Sources, Tier};
    use arb::live::base::scan::scan;

    let chain_name = chain.to_string();
    let url = std::env::var(chain.rpc_env_var())
        .unwrap_or_else(|_| fail(&format!("set ${} to your Alchemy Base WS endpoint", chain.rpc_env_var())));

    let mut sources = Sources::load(Sources::path_for_chain(&chain_name))
        .unwrap_or_else(|e| fail(&format!("load sources: {e}")));
    let mut official = PoolBook::load(PoolBook::path_for_chain(&chain_name, Tier::Official))
        .unwrap_or_else(|e| fail(&format!("load official book: {e}")));
    let mut secondary = PoolBook::load_or_new(&chain_name, Tier::Secondary)
        .unwrap_or_else(|e| fail(&format!("load secondary book: {e}")));

    let codehashes_before: usize = sources.exchanges.iter().map(|e| e.pool_codehashes.len()).sum();
    let do_forks = blocks > 0;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&format!("tokio runtime: {e}")));

    let report = rt.block_on(async {
        use arb::live::source::ChainSource;
        let head = arb::live::ws::WsChainSource::new(url.clone(), vec![])
            .head()
            .await
            .map_err(|e| format!("head: {e}"))?;
        let from = head.saturating_sub(blocks);
        if do_forks {
            println!("scanning {chain_name}: pair lookup + fork scan blocks {from}..={head} ...");
        } else {
            println!("scanning {chain_name}: pair lookup (top pools) ...");
        }
        scan(&url, &mut sources, &mut official, &mut secondary, from, head, window, do_forks)
            .await
            .map_err(|e| e.to_string())
    });

    match report {
        Ok(r) => {
            // Only rewrite a file when it actually changed (saving drops comments).
            if r.official_added > 0 {
                official
                    .save(PoolBook::path_for_chain(&chain_name, Tier::Official))
                    .unwrap_or_else(|e| fail(&format!("save official: {e}")));
            }
            if r.fork_added > 0 {
                secondary
                    .save(PoolBook::path_for_chain(&chain_name, Tier::Secondary))
                    .unwrap_or_else(|e| fail(&format!("save secondary: {e}")));
            }
            let codehashes_after: usize = sources.exchanges.iter().map(|e| e.pool_codehashes.len()).sum();
            if codehashes_after > codehashes_before {
                sources
                    .save(Sources::path_for_chain(&chain_name))
                    .unwrap_or_else(|e| fail(&format!("save sources: {e}")));
            }
            println!(
                "scan: +{} official pools, +{} fork candidates",
                r.official_added, r.fork_added
            );
            if !r.unknown_clusters.is_empty() {
                println!("unknown codehash clusters (candidate new DEXes):");
                for (ch, n) in r.unknown_clusters.iter().take(10) {
                    println!("  {ch}  {n} pools");
                }
            }
            println!("books written (commit them to git).");
        }
        Err(e) => fail(&format!("scan error: {e}")),
    }
}

#[cfg(feature = "live-rpc")]
fn rank_live(
    sel: &Selection,
    book: &arb::book::PoolBook,
    cycles: &[Vec<arb::graph::Edge>],
    usdc: Address,
    amount: u64,
    top: usize,
) {
    use arb::live::base::price::rank_cycles;
    use arb::types::U256;

    let env_var = sel.chain.rpc_env_var();
    let url = std::env::var(env_var)
        .unwrap_or_else(|_| fail(&format!("set ${env_var} to your Alchemy Base WS endpoint")));
    let dec = book.tokens.get("USDC").map(|t| t.decimals).unwrap_or(6);
    let amount_in = U256::from(amount) * U256::from(10u64).pow(U256::from(dec as u64));
    let weth = book.tokens.get("WETH").map(|t| t.address);
    let scale = U256::from(10u64).pow(U256::from(dec as u64));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&format!("tokio runtime: {e}")));

    let ranked = rt.block_on(rank_cycles(&url, book, cycles, usdc, weth, amount_in, top));
    match ranked {
        Ok(rows) => {
            println!("\ntop {} cycles by output for {amount} USDC in:", rows.len());
            for r in &rows {
                let toks: Vec<String> = r.tokens.iter().map(|t| token_label(book, *t)).collect();
                let out_h = r.gross_out / scale;
                let net = match r.net_profit {
                    Some(p) => format!("+{} USDC ✅", p / scale),
                    None => "unprofitable".to_string(),
                };
                println!(
                    "  {} | out={}.{:06} USDC | net={}",
                    toks.join("->"),
                    out_h,
                    (r.gross_out % scale),
                    net
                );
            }
            println!(
                "(V3 priced within current tick range; large sizes need the tick map. \
                 Gas valued via the live WETH/USDC price.)"
            );
        }
        Err(e) => fail(&format!("live ranking error: {e}")),
    }
}

#[cfg(not(feature = "live-rpc"))]
fn rank_live(
    _sel: &Selection,
    _book: &arb::book::PoolBook,
    _cycles: &[Vec<arb::graph::Edge>],
    _usdc: Address,
    _amount: u64,
    _top: usize,
) {
    eprintln!("--live requires the RPC backend: cargo run --features live-rpc -- simulate --live ...");
    std::process::exit(1);
}

#[cfg(feature = "live-rpc")]
fn verify_command(chain: arb::config::Chain, max_pools: usize) {
    use arb::book::{PoolBook, Tier};
    use arb::live::base::verify::verify_all;

    let chain_name = chain.to_string();
    let url = std::env::var(chain.rpc_env_var())
        .unwrap_or_else(|_| fail(&format!("set ${} to your Alchemy Base WS endpoint", chain.rpc_env_var())));
    let book = PoolBook::load(PoolBook::path_for_chain(&chain_name, Tier::Official))
        .unwrap_or_else(|e| fail(&format!("load official book: {e}")));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&format!("tokio runtime: {e}")));

    println!("verifying pools wei-exact vs on-chain quoters (max {max_pools}/family)...");
    let report = rt.block_on(verify_all(&url, &book, max_pools));
    match report {
        Ok(r) => {
            println!(
                "\nblock {} | pools {} | quotes {} | passed {} | mismatches {}",
                r.block, r.pools_checked, r.quotes_checked, r.passed, r.mismatches.len()
            );
            for m in r.mismatches.iter().take(40) {
                println!(
                    "  MISMATCH {} ({}) in={} amt={} mine={} chain={}",
                    m.pool, m.exchange, m.token_in, m.amount_in, m.mine, m.chain
                );
            }
            if r.ok() {
                println!(
                    "\n✅ WEI-EXACT: all {} quotes across {} pools match on-chain quoters.",
                    r.quotes_checked, r.pools_checked
                );
            } else {
                println!("\n❌ NOT EXACT: {} mismatches — do not trust these pools.", r.mismatches.len());
                std::process::exit(1);
            }
        }
        Err(e) => fail(&format!("verify error: {e}")),
    }
}

#[cfg(feature = "live-rpc")]
#[allow(clippy::too_many_arguments)]
fn watch_arb_command(chain: arb::config::Chain, amount: u64, min_profit: u64, max_hops: usize, top: usize, secs: u64, window: i32, resync_secs: u64, show_screened: bool) {
    use arb::book::{PoolBook, Tier};
    use arb::graph::Graph;
    use arb::live::base::synced::watch;
    use arb::types::U256;

    let chain_name = chain.to_string();
    let url = std::env::var(chain.rpc_env_var())
        .unwrap_or_else(|_| fail(&format!("set ${} to your Alchemy Base WS endpoint", chain.rpc_env_var())));
    let book = PoolBook::load(PoolBook::path_for_chain(&chain_name, Tier::Official))
        .unwrap_or_else(|e| fail(&format!("load official book: {e} (run `arb scan` first)")));
    let usdc = book.tokens.get("USDC").unwrap_or_else(|| fail("no USDC in book")).address;
    let weth = book.tokens.get("WETH").map(|t| t.address);
    let dec = book.tokens.get("USDC").map(|t| t.decimals).unwrap_or(6);

    let graph = Graph::from_book(&book);
    let cycles = graph.cycles(usdc, max_hops);
    let scale = U256::from(10u64).pow(U256::from(dec as u64));
    let amount_in = U256::from(amount) * scale;
    let min_profit = U256::from(min_profit);

    println!(
        "watch-arb: chain={chain_name} cycles={} amount={amount} USDC min_profit={min_profit} base-units (Ctrl-C to stop)",
        cycles.len()
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&format!("tokio runtime: {e}")));
    let run_for = (secs > 0).then(|| std::time::Duration::from_secs(secs));
    if let Err(e) = rt.block_on(watch(&url, &book, &cycles, usdc, weth, amount_in, min_profit, top, window, run_for, resync_secs, show_screened)) {
        fail(&format!("watch error: {e}"));
    }
}

#[cfg(feature = "live-rpc")]
fn verify_sync_command(chain: arb::config::Chain, max_pools: usize, lookback: u64, window: i32) {
    use arb::book::{PoolBook, Tier};
    use arb::live::base::verify::verify_v3_sync;

    let chain_name = chain.to_string();
    let url = std::env::var(chain.rpc_env_var())
        .unwrap_or_else(|_| fail(&format!("set ${}", chain.rpc_env_var())));
    let book = PoolBook::load(PoolBook::path_for_chain(&chain_name, Tier::Official))
        .unwrap_or_else(|e| fail(&format!("load official book: {e}")));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&format!("tokio runtime: {e}")));
    let mode = if window > 0 { format!("windowed ±{window} ticks") } else { "full map".into() };
    println!("verifying event-driven V3 sync (replay {lookback} blocks, max {max_pools} pools, {mode})...");
    match rt.block_on(verify_v3_sync(&url, &book, max_pools, lookback, window)) {
        Ok(r) => {
            println!(
                "\nblock {} | pools {} | checks {} | passed {} | window-skips {} | mismatches {}",
                r.block, r.pools_checked, r.quotes_checked, r.passed, r.window_skips, r.mismatches.len()
            );
            for m in r.mismatches.iter().take(30) {
                println!("  MISMATCH {} ({}) in={} amt={} mine={} chain={}", m.pool, m.exchange, m.token_in, m.amount_in, m.mine, m.chain);
            }
            if r.ok() {
                println!("\n✅ EVENT-SYNC EXACT: replayed state matches chain + QuoterV2 to the wei.");
            } else {
                println!("\n❌ event-sync NOT exact: {} mismatches.", r.mismatches.len());
                std::process::exit(1);
            }
        }
        Err(e) => fail(&format!("verify-sync error: {e}")),
    }
}

#[cfg(not(feature = "live-rpc"))]
fn verify_sync_command(_chain: arb::config::Chain, _max_pools: usize, _lookback: u64, _window: i32) {
    eprintln!("verify-sync requires --features live-rpc");
    std::process::exit(1);
}

#[cfg(not(feature = "live-rpc"))]
#[allow(clippy::too_many_arguments)]
fn watch_arb_command(_chain: arb::config::Chain, _amount: u64, _min_profit: u64, _max_hops: usize, _top: usize, _secs: u64, _window: i32, _resync_secs: u64, _show_screened: bool) {
    eprintln!("watch-arb requires the RPC backend: cargo run --features live-rpc -- watch-arb --chain base");
    std::process::exit(1);
}

#[cfg(not(feature = "live-rpc"))]
fn verify_command(_chain: arb::config::Chain, _max_pools: usize) {
    eprintln!("verify requires the RPC backend: cargo run --features live-rpc -- verify --chain base");
    std::process::exit(1);
}

#[cfg(not(feature = "live-rpc"))]
fn scan_command(_chain: arb::config::Chain, _blocks: u64, _window: u64) {
    eprintln!(
        "scan requires the RPC backend. Rebuild with:\n    cargo run --features live-rpc -- scan --chain base"
    );
    std::process::exit(1);
}
