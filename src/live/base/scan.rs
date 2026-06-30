//! Base-specific: pool discovery + fork detection (`arb scan`).
//!
//! Two passes:
//!   1. **Factory enumeration** — for each official exchange, read its
//!      pool-creation events (`PairCreated`/`PoolCreated`) over a block range,
//!      decode tokens/fee, and upsert into the **official** book. Record each
//!      exchange's pool runtime codehash.
//!   2. **Fork detection by codehash** — scan all addresses that emitted a
//!      known AMM event (UniV2 `Sync`, UniV3 `Swap`) in the range; for any not
//!      already known, fetch its bytecode codehash. A codehash that matches a
//!      known exchange ⇒ a fork/clone of it ⇒ upsert into the **secondary**
//!      book as `<name>-fork`. Unknown codehashes that emit a known event are
//!      clustered and reported as candidate new DEXes.
//!
//! Network-bound; only built under `live-rpc`. Decoding covers uniswap_v2 /
//! uniswap_v3 / solidly creation events (balancer_v2 uses the Vault and is
//! skipped here).

use std::collections::{HashMap, HashSet};

use alloy::providers::Provider;
use alloy::rpc::types::{Filter, TransactionInput, TransactionRequest};
use alloy_primitives::{keccak256, Bytes, TxKind};

use crate::book::{PoolBook, PoolInfo, Sources};
use crate::live::source::SourceError;
use crate::types::{Address, B256};

fn topic(sig: &str) -> B256 {
    keccak256(sig.as_bytes())
}

fn selector(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

/// ABI-encode a 32-byte word for an address (left-padded).
fn enc_addr(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}
fn enc_u24(v: u32) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[29] = (v >> 16) as u8;
    w[30] = (v >> 8) as u8;
    w[31] = v as u8;
    w
}
fn enc_bool(b: bool) -> [u8; 32] {
    let mut w = [0u8; 32];
    if b {
        w[31] = 1;
    }
    w
}

/// `eth_call` returning a single address (or None if zero/empty).
async fn call_addr<P: Provider>(provider: &P, to: Address, data: Vec<u8>) -> Option<Address> {
    let tx = TransactionRequest {
        to: Some(TxKind::Call(to)),
        input: TransactionInput::new(Bytes::from(data)),
        ..Default::default()
    };
    let out = provider.call(tx).await.ok()?;
    let raw = out.as_ref();
    if raw.len() < 32 {
        return None;
    }
    let a = Address::from_slice(&raw[12..32]);
    (a != Address::ZERO).then_some(a)
}

/// 20-byte address from a 32-byte indexed topic word.
fn addr_from_word(word: &B256) -> Address {
    Address::from_slice(&word.as_slice()[12..32])
}

/// `eth_call` returning the first 32-byte word (or None on empty/revert).
async fn call_word<P: Provider>(provider: &P, to: Address, selector: [u8; 4]) -> Option<[u8; 32]> {
    let tx = TransactionRequest {
        to: Some(TxKind::Call(to)),
        input: TransactionInput::new(Bytes::from(selector.to_vec())),
        ..Default::default()
    };
    let out = provider.call(tx).await.ok()?;
    let raw = out.as_ref();
    if raw.len() < 32 {
        return None;
    }
    let mut w = [0u8; 32];
    w.copy_from_slice(&raw[..32]);
    Some(w)
}

/// `eth_call` returning raw return bytes (or None on revert).
async fn call_bytes<P: Provider>(provider: &P, to: Address, selector: [u8; 4]) -> Option<Vec<u8>> {
    let tx = TransactionRequest {
        to: Some(TxKind::Call(to)),
        input: TransactionInput::new(Bytes::from(selector.to_vec())),
        ..Default::default()
    };
    provider.call(tx).await.ok().map(|b| b.as_ref().to_vec())
}

/// `decimals()` -> u8 (last byte of the returned word).
async fn call_decimals<P: Provider>(provider: &P, token: Address) -> Option<u8> {
    call_word(provider, token, selector("decimals()")).await.map(|w| w[31])
}

/// `symbol()` -> String. Handles both ABI `string` (offset/len/data) and legacy
/// `bytes32` symbols; falls back to the short address on garbage.
async fn call_symbol<P: Provider>(provider: &P, token: Address) -> String {
    let fallback = || format!("{:#x}", token)[..10].to_string();
    let Some(raw) = call_bytes(provider, token, selector("symbol()")).await else {
        return fallback();
    };
    let clean = |b: &[u8]| {
        let s: String = b.iter().filter(|c| c.is_ascii_graphic()).map(|c| *c as char).collect();
        s
    };
    if raw.len() == 32 {
        // legacy bytes32
        let s = clean(&raw);
        return if s.is_empty() { fallback() } else { s };
    }
    if raw.len() >= 64 {
        let len = u32::from_be_bytes([raw[60], raw[61], raw[62], raw[63]]) as usize;
        if len > 0 && 64 + len <= raw.len() {
            let s = clean(&raw[64..64 + len]);
            if !s.is_empty() {
                return s;
            }
        }
    }
    fallback()
}

/// The three AMM `Swap` event signatures we treat as "this address is a pool".
fn swap_topics() -> Vec<B256> {
    vec![
        topic("Swap(address,uint256,uint256,uint256,uint256,address)"), // UniV2
        topic("Swap(address,address,int256,int256,uint160,uint128,int24)"), // UniV3 / Slipstream
        topic("Swap(address,address,uint256,uint256,uint256,uint256)"), // Aerodrome
    ]
}

/// Discover pools by **recent swap activity** — the pools that actually trade are
/// where arbs live (factory enumeration / fixed pair-lookup misses long-tail
/// pools and fee tiers). For every address that emitted a `Swap` in `[from, to]`
/// and isn't already `known`, classify it by its on-chain `factory()` against the
/// configured exchanges, resolve `token0`/`token1` (+ V3 `fee()` / Aerodrome
/// `stable()`), and return a [`PoolInfo`] for every pool on a **confirmable** DEX
/// (one with a ground-truth quoter: uniswap-v3, pancakeswap-v3, aerodrome,
/// uniswap-v2). Pools on DEXes we can't yet confirm wei-exact
/// (aerodrome-slipstream, balancer-v2) or from unknown factories are tallied but
/// **skipped** — indexing a pool we can't verify would break the wei-exact rule.
///
/// Returns the new pools plus a per-bucket histogram (exchange name, or
/// `skip:<reason>`), so coverage gaps (e.g. how much volume is on slipstream) are
/// visible rather than silently dropped.
#[allow(clippy::type_complexity)]
pub async fn discover_active_pools<P: Provider>(
    provider: &P,
    sources: &Sources,
    known: &HashSet<Address>,
    known_tokens: &HashSet<Address>,
    from: u64,
    to: u64,
    window: u64,
) -> Result<(Vec<PoolInfo>, Vec<(Address, String, u8)>, std::collections::BTreeMap<String, usize>), SourceError>
{
    use std::collections::BTreeMap;

    let filter = Filter::new().event_signature(swap_topics());
    let logs = logs_chunked(provider, &filter, from, to, window).await?;

    // Unique pool addresses that traded, minus ones we already track.
    let mut pools: Vec<Address> = logs.iter().map(|l| l.address()).collect();
    pools.sort();
    pools.dedup();
    pools.retain(|p| !known.contains(p));

    // factory address -> exchange.
    let by_factory: HashMap<Address, &crate::book::ExchangeInfo> =
        sources.exchanges.iter().filter_map(|e| e.factory.map(|f| (f, e))).collect();

    let fee_sel = selector("fee()");
    let stable_sel = selector("stable()");
    let t0_sel = selector("token0()");
    let t1_sel = selector("token1()");
    let factory_sel = selector("factory()");

    let mut out = Vec::new();
    let mut hist: BTreeMap<String, usize> = BTreeMap::new();
    for pool in pools {
        let Some(factory) = call_addr(provider, pool, factory_sel.to_vec()).await else {
            *hist.entry("skip:no_factory()".into()).or_default() += 1;
            continue;
        };
        let Some(exch) = by_factory.get(&factory) else {
            *hist.entry("skip:unknown_factory".into()).or_default() += 1;
            continue;
        };
        // Only index pools we can confirm wei-exact on-chain.
        let (kind, fee_bps) = match exch.name.as_str() {
            "uniswap-v3" | "pancakeswap-v3" => {
                let fee = call_word(provider, pool, fee_sel)
                    .await
                    .map(|w| u32::from_be_bytes([w[28], w[29], w[30], w[31]]));
                ("uniswap_v3".to_string(), fee) // V3: fee stored in pips
            }
            "aerodrome" => {
                // stable() true -> solidly stable math; false -> volatile (V2). Fee
                // is read from the factory at runtime, so leave it None here.
                let stable = call_word(provider, pool, stable_sel).await.map(|w| w[31] != 0);
                match stable {
                    Some(true) => ("solidly".to_string(), None),
                    Some(false) => ("uniswap_v2".to_string(), None),
                    None => {
                        *hist.entry("skip:aero_no_stable()".into()).or_default() += 1;
                        continue;
                    }
                }
            }
            "uniswap-v2" => ("uniswap_v2".to_string(), Some(30)),
            other => {
                *hist.entry(format!("skip:{other}")).or_default() += 1;
                continue;
            }
        };
        let (Some(t0), Some(t1)) = (
            call_addr(provider, pool, t0_sel.to_vec()).await,
            call_addr(provider, pool, t1_sel.to_vec()).await,
        ) else {
            *hist.entry("skip:no_tokens".into()).or_default() += 1;
            continue;
        };
        out.push(PoolInfo {
            address: pool,
            exchange: exch.name.clone(),
            kind,
            token0: t0,
            token1: t1,
            fee_bps,
            discovered_block: None,
        });
        *hist.entry(exch.name.clone()).or_default() += 1;
    }

    // Resolve metadata for any NEW tokens these pools reference (decimals are
    // required for wei-exact solidly-stable math; symbol keys the book).
    let mut new_tok_addrs: Vec<Address> =
        out.iter().flat_map(|p| [p.token0, p.token1]).filter(|a| !known_tokens.contains(a)).collect();
    new_tok_addrs.sort();
    new_tok_addrs.dedup();
    let mut tokens = Vec::new();
    for a in new_tok_addrs {
        if let Some(d) = call_decimals(provider, a).await {
            tokens.push((a, call_symbol(provider, a).await, d));
        }
    }
    Ok((out, tokens, hist))
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub from_block: u64,
    pub to_block: u64,
    pub official_added: usize,
    pub fork_added: usize,
    /// (codehash, number of emitting addresses) for unknown clusters worth review.
    pub unknown_clusters: Vec<(B256, usize)>,
}

/// Creation-event signature + decoder for a given exchange `kind`.
/// (Retained for event-based discovery; pair lookup is the primary path now.)
#[allow(dead_code)]
fn creation_topic(kind: &str) -> Option<B256> {
    match kind {
        "uniswap_v2" => Some(topic("PairCreated(address,address,address,uint256)")),
        "uniswap_v3" => Some(topic("PoolCreated(address,address,uint24,int24,address)")),
        "solidly" => Some(topic("PoolCreated(address,address,bool,address,uint256)")),
        _ => None, // balancer_v2 etc. not enumerated this way
    }
}

/// Decode a creation log into (token0, token1, pool, fee_bps).
#[allow(dead_code)]
fn decode_creation(kind: &str, log: &alloy::rpc::types::Log) -> Option<(Address, Address, Address, Option<u32>)> {
    let topics = log.topics();
    let data = log.data().data.as_ref();
    match kind {
        "uniswap_v2" => {
            // topics: [sig, token0, token1]; data: [pair(32), allPairsLength(32)]
            let t0 = addr_from_word(topics.get(1)?);
            let t1 = addr_from_word(topics.get(2)?);
            if data.len() < 32 {
                return None;
            }
            let pool = Address::from_slice(&data[12..32]);
            Some((t0, t1, pool, Some(30)))
        }
        "solidly" => {
            // topics: [sig, token0, token1, stable]; data: [pool(32), len(32)]
            let t0 = addr_from_word(topics.get(1)?);
            let t1 = addr_from_word(topics.get(2)?);
            if data.len() < 32 {
                return None;
            }
            let pool = Address::from_slice(&data[12..32]);
            Some((t0, t1, pool, None))
        }
        "uniswap_v3" => {
            // topics: [sig, token0, token1, fee]; data: [tickSpacing(32), pool(32)]
            let t0 = addr_from_word(topics.get(1)?);
            let t1 = addr_from_word(topics.get(2)?);
            let fee_word = topics.get(3)?;
            let fee = u32::from_be_bytes([0, fee_word.as_slice()[29], fee_word.as_slice()[30], fee_word.as_slice()[31]]);
            if data.len() < 64 {
                return None;
            }
            let pool = Address::from_slice(&data[44..64]);
            Some((t0, t1, pool, Some(fee))) // V3: fee is in pips (1e-6), stored as-is
        }
        _ => None,
    }
}

/// Fetch logs for `filter` over `[from, to]` in `<= window`-block chunks
/// (Alchemy's free tier caps `eth_getLogs` at 10 blocks).
async fn logs_chunked<P: Provider>(
    provider: &P,
    filter: &Filter,
    from: u64,
    to: u64,
    window: u64,
) -> Result<Vec<alloy::rpc::types::Log>, SourceError> {
    let window = window.max(1);
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + window - 1).min(to);
        let chunk = filter.clone().from_block(start).to_block(end);
        let logs = provider
            .get_logs(&chunk)
            .await
            .map_err(|e| SourceError::Rpc(e.to_string()))?;
        out.extend(logs);
        start = end + 1;
    }
    Ok(out)
}

/// **Populate the top pools** by direct factory lookup for every pair of the
/// book's tokens — deterministic and free-tier friendly (a handful of
/// `eth_call`s, no `getLogs`). For each exchange/factory we call the right
/// getter (`getPair` / `getPool(...,fee)` / `getPool(...,stable)`); a non-zero
/// result is a real pool, upserted into the official book. Also seeds one pool
/// codehash per exchange (into `sources`) so later fork detection can classify.
pub async fn discover_pairs<P: Provider>(
    provider: &P,
    sources: &mut Sources,
    official: &mut PoolBook,
) -> usize {
    let tokens: Vec<Address> = official.tokens.values().map(|t| t.address).collect();
    let exchanges = sources.exchanges.clone();
    let mut added = 0usize;

    for exch in &exchanges {
        let Some(factory) = exch.factory else { continue };
        let mut got_codehash = false;
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                let (a, b) = (tokens[i], tokens[j]);
                let (t0, t1) = if a < b { (a, b) } else { (b, a) }; // UniV2/V3/solidly order

                // (calldata, kind, fee) candidates for this exchange
                let mut candidates: Vec<(Vec<u8>, &str, Option<u32>)> = Vec::new();
                match exch.kind.as_str() {
                    "uniswap_v2" => {
                        let mut d = selector("getPair(address,address)").to_vec();
                        d.extend(enc_addr(a));
                        d.extend(enc_addr(b));
                        candidates.push((d, "uniswap_v2", Some(30)));
                    }
                    "uniswap_v3" => {
                        for fee in [100u32, 500, 3000, 10000] {
                            let mut d = selector("getPool(address,address,uint24)").to_vec();
                            d.extend(enc_addr(a));
                            d.extend(enc_addr(b));
                            d.extend(enc_u24(fee));
                            candidates.push((d, "uniswap_v3", Some(fee)));
                        }
                    }
                    "solidly" => {
                        for stable in [false, true] {
                            let mut d = selector("getPool(address,address,bool)").to_vec();
                            d.extend(enc_addr(a));
                            d.extend(enc_addr(b));
                            d.extend(enc_bool(stable));
                            let kind = if stable { "solidly" } else { "uniswap_v2" };
                            candidates.push((d, kind, None));
                        }
                    }
                    _ => {} // balancer_v2 (Vault) not a simple pair getter
                }

                for (data, kind, fee) in candidates {
                    if let Some(pool) = call_addr(provider, factory, data).await {
                        if official.upsert_pool(PoolInfo {
                            address: pool,
                            exchange: exch.name.clone(),
                            kind: kind.to_string(),
                            token0: t0,
                            token1: t1,
                            fee_bps: fee,
                            discovered_block: None,
                        }) {
                            added += 1;
                        }
                        if !got_codehash {
                            if let Ok(code) = provider.get_code_at(pool).await {
                                sources.add_codehash(&exch.name, keccak256(code.as_ref()));
                                got_codehash = true;
                            }
                        }
                    }
                }
            }
        }
    }
    added
}

/// Run a scan: always populate top pools by pair lookup; if `do_forks`, also
/// scan `[from_block, to_block]` for fork pools via codehash clustering.
/// `window` bounds the per-request block span (use 10 for Alchemy free tier).
#[allow(clippy::too_many_arguments)]
pub async fn scan(
    ws_url: &str,
    sources: &mut Sources,
    official: &mut PoolBook,
    secondary: &mut PoolBook,
    from_block: u64,
    to_block: u64,
    window: u64,
    do_forks: bool,
) -> Result<ScanReport, SourceError> {
    let provider = crate::live::base::rpc::connect(ws_url).await?;

    let mut report = ScanReport {
        from_block,
        to_block,
        ..Default::default()
    };

    // --- Pass 0: populate top pools by direct factory pair lookup ---
    report.official_added += discover_pairs(&provider, sources, official).await;

    if !do_forks {
        return Ok(report);
    }

    // --- Pass: fork detection by codehash ---
    let known: HashSet<Address> = official
        .pools
        .iter()
        .chain(secondary.pools.iter())
        .map(|p| p.address)
        .collect();

    let amm_topics = vec![
        topic("Sync(uint112,uint112)"),
        topic("Swap(address,address,int256,int256,uint160,uint128,int24)"),
    ];
    let filter = Filter::new().event_signature(amm_topics);
    let logs = logs_chunked(&provider, &filter, from_block, to_block, window).await?;

    let mut seen: HashSet<Address> = HashSet::new();
    let mut unknown: HashMap<B256, usize> = HashMap::new();
    for log in &logs {
        let addr = log.address();
        if known.contains(&addr) || !seen.insert(addr) {
            continue;
        }
        let code = match provider.get_code_at(addr).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let ch = keccak256(code.as_ref());
        match sources.exchange_for_codehash(ch) {
            Some(exch) => {
                // a fork/clone of a known exchange -> secondary book
                let (name, kind) = (exch.name.clone(), exch.kind.clone());
                if secondary.upsert_pool(PoolInfo {
                    address: addr,
                    exchange: format!("{name}-fork"),
                    kind,
                    token0: Address::ZERO, // resolved later via token0()/token1()
                    token1: Address::ZERO,
                    fee_bps: None,
                    discovered_block: log.block_number,
                }) {
                    report.fork_added += 1;
                }
            }
            None => {
                *unknown.entry(ch).or_default() += 1;
            }
        }
    }

    // clusters of >1 unknown address sharing a codehash are likely a new DEX
    report.unknown_clusters = unknown.into_iter().filter(|(_, n)| *n > 1).collect();
    report.unknown_clusters.sort_by(|a, b| b.1.cmp(&a.1));

    Ok(report)
}
