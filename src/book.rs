//! Two on-disk registries per chain, both TOML and committed to git:
//!
//! - [`PoolBook`] — the **graph**: tokens (nodes) + pools (edges). This is what
//!   the runtime loads and traverses to find USDC→…→USDC cycles. Tiered into
//!   `official` (trusted) and `secondary` (discovered fork candidates).
//! - [`Sources`] — **discovery scaffolding** only: exchange factories +
//!   per-exchange pool codehashes. Read/written by `arb scan`; the traversal
//!   engine never looks at it. One file per chain (`<chain>.sources.toml`).
//!
//! Factories are deliberately NOT in the pool book: they aren't graph data, just
//! a means of finding edges.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::types::{Address, B256};

#[derive(Debug, thiserror::Error)]
pub enum BookError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("toml encode: {0}")]
    Encode(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenInfo {
    pub address: Address,
    pub decimals: u8,
}

/// Per-chain incremental-scan watermark: the last block `scan --active` covered.
/// Lets indexing run as a "catch up from watermark → head" job (no gaps, no
/// `--blocks` guessing). Git-tracked alongside the books.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ScanState {
    #[serde(default)]
    pub last_active_block: u64,
}

impl ScanState {
    /// `config/<chain>.scanstate.toml`.
    pub fn path_for_chain(chain: &str) -> std::path::PathBuf {
        Path::new("config").join(format!("{chain}.scanstate.toml"))
    }

    pub fn load_or_default(chain: &str) -> Self {
        let path = Self::path_for_chain(chain);
        std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, chain: &str) -> Result<(), BookError> {
        std::fs::write(Self::path_for_chain(chain), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// A pool = one graph edge (token0 <-> token1). The runtime traverses these.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolInfo {
    pub address: Address,
    /// Label of the owning exchange (or "<family>-fork" for discoveries).
    pub exchange: String,
    /// Math family: "uniswap_v2" | "uniswap_v3" | "solidly" | "balancer_v2".
    pub kind: String,
    pub token0: Address,
    pub token1: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_bps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered_block: Option<u64>,
}

/// The pool graph for one chain/tier: nodes (`tokens`) + edges (`pools`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolBook {
    pub chain: String,
    #[serde(default)]
    pub tokens: BTreeMap<String, TokenInfo>,
    #[serde(default, rename = "pool")]
    pub pools: Vec<PoolInfo>,
}

/// Which pool book: curated/trusted vs discovered/candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Curated graph: trusted tokens + pools. Traded by default.
    Official,
    /// Discovered forks/clones (codehash clustering). Review before trusting.
    Secondary,
}

impl Tier {
    pub fn suffix(&self) -> &'static str {
        match self {
            Tier::Official => "official",
            Tier::Secondary => "secondary",
        }
    }
}

impl PoolBook {
    /// `config/<chain>.<tier>.toml` — committed to git.
    pub fn path_for_chain(chain: &str, tier: Tier) -> std::path::PathBuf {
        Path::new("config").join(format!("{chain}.{}.toml", tier.suffix()))
    }

    pub fn empty(chain: &str) -> Self {
        PoolBook {
            chain: chain.to_string(),
            tokens: BTreeMap::new(),
            pools: Vec::new(),
        }
    }

    pub fn load_or_new(chain: &str, tier: Tier) -> Result<Self, BookError> {
        let path = Self::path_for_chain(chain, tier);
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::empty(chain))
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, BookError> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), BookError> {
        let mut sorted = self.clone();
        sorted.pools.sort_by_key(|p| p.address); // stable diffs
        std::fs::write(path, toml::to_string_pretty(&sorted)?)?;
        Ok(())
    }

    /// Insert or replace a pool by address. Returns true if newly added.
    pub fn upsert_pool(&mut self, pool: PoolInfo) -> bool {
        match self.pools.iter_mut().find(|p| p.address == pool.address) {
            Some(existing) => {
                *existing = pool;
                false
            }
            None => {
                self.pools.push(pool);
                true
            }
        }
    }

    pub fn has_pool(&self, address: Address) -> bool {
        self.pools.iter().any(|p| p.address == address)
    }

    /// Fold `other`'s tokens and pools into this book. `self` (the trusted tier)
    /// wins every conflict: an existing token symbol or pool address is kept, only
    /// previously-unseen entries are added. Returns `(tokens_added, pools_added)`.
    pub fn merge(&mut self, other: &PoolBook) -> (usize, usize) {
        let mut tokens_added = 0;
        for (sym, tok) in &other.tokens {
            if !self.tokens.contains_key(sym) {
                self.tokens.insert(sym.clone(), tok.clone());
                tokens_added += 1;
            }
        }
        let mut pools_added = 0;
        for p in &other.pools {
            if !self.has_pool(p.address) {
                self.pools.push(p.clone());
                pools_added += 1;
            }
        }
        (tokens_added, pools_added)
    }
}

// ===========================================================================
// Discovery sources (scan-only; not graph data)
// ===========================================================================

/// A known exchange deployment: a factory + the codehashes of its pools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExchangeInfo {
    pub name: String,
    /// Math family the factory mints: uniswap_v2 | uniswap_v3 | solidly | balancer_v2.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<Address>,
    /// Pool runtime-bytecode codehashes; used to classify forks (same code).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pool_codehashes: Vec<B256>,
}

/// Discovery scaffolding for one chain: the factories `arb scan` enumerates and
/// the codehashes it learns. Never consulted by the traversal engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sources {
    pub chain: String,
    #[serde(default, rename = "exchange")]
    pub exchanges: Vec<ExchangeInfo>,
}

impl Sources {
    /// `config/<chain>.sources.toml`.
    pub fn path_for_chain(chain: &str) -> std::path::PathBuf {
        Path::new("config").join(format!("{chain}.sources.toml"))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, BookError> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), BookError> {
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Record a pool codehash for an exchange (idempotent). Returns true if it
    /// was newly added.
    pub fn add_codehash(&mut self, exchange: &str, codehash: B256) -> bool {
        if let Some(e) = self.exchanges.iter_mut().find(|e| e.name == exchange) {
            if !e.pool_codehashes.contains(&codehash) {
                e.pool_codehashes.push(codehash);
                return true;
            }
        }
        false
    }

    pub fn exchange_for_codehash(&self, codehash: B256) -> Option<&ExchangeInfo> {
        self.exchanges
            .iter()
            .find(|e| e.pool_codehashes.contains(&codehash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn tmp_path() -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("arb_book_test_{}_{id}.toml", std::process::id()))
    }

    fn sample() -> PoolBook {
        let mut tokens = BTreeMap::new();
        tokens.insert("WETH".into(), TokenInfo { address: addr(1), decimals: 18 });
        tokens.insert("USDC".into(), TokenInfo { address: addr(2), decimals: 6 });
        PoolBook {
            chain: "base".into(),
            tokens,
            pools: vec![PoolInfo {
                address: addr(10),
                exchange: "aerodrome".into(),
                kind: "uniswap_v2".into(),
                token0: addr(1),
                token1: addr(2),
                fee_bps: Some(30),
                discovered_block: Some(123),
            }],
        }
    }

    #[test]
    fn pool_book_round_trips() {
        let book = sample();
        let path = tmp_path();
        book.save(&path).unwrap();
        assert_eq!(book, PoolBook::load(&path).unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn upsert_merges_by_address() {
        let mut book = sample();
        assert!(book.upsert_pool(PoolInfo {
            address: addr(11),
            exchange: "uniswap-v3".into(),
            kind: "uniswap_v3".into(),
            token0: addr(1),
            token1: addr(2),
            fee_bps: Some(5),
            discovered_block: None,
        }));
        assert_eq!(book.pools.len(), 2);
        assert!(!book.upsert_pool(PoolInfo {
            address: addr(10),
            exchange: "aerodrome".into(),
            kind: "uniswap_v2".into(),
            token0: addr(1),
            token1: addr(2),
            fee_bps: Some(5),
            discovered_block: Some(456),
        }));
        assert_eq!(book.pools.len(), 2);
    }

    #[test]
    fn merge_dedups_and_official_wins() {
        let mut official = sample(); // tokens WETH,USDC; pool addr(10)
        let mut secondary = PoolBook::empty("base");
        // A NEW token + a NEW pool, plus a colliding token (different decimals) and
        // a colliding pool address (different exchange) that must NOT overwrite.
        secondary.tokens.insert("DAI".into(), TokenInfo { address: addr(3), decimals: 18 });
        secondary.tokens.insert("USDC".into(), TokenInfo { address: addr(2), decimals: 99 });
        secondary.pools.push(PoolInfo {
            address: addr(20),
            exchange: "pancakeswap-v3".into(),
            kind: "uniswap_v3".into(),
            token0: addr(1),
            token1: addr(3),
            fee_bps: Some(25),
            discovered_block: None,
        });
        secondary.pools.push(PoolInfo {
            address: addr(10), // collides with official's pool
            exchange: "SHOULD_NOT_WIN".into(),
            kind: "uniswap_v2".into(),
            token0: addr(1),
            token1: addr(2),
            fee_bps: Some(1),
            discovered_block: None,
        });

        let (toks, pools) = official.merge(&secondary);
        assert_eq!((toks, pools), (1, 1), "only DAI + pool addr(20) are new");
        assert_eq!(official.pools.len(), 2);
        // Official's USDC (6 decimals) and pool addr(10) (aerodrome) are preserved.
        assert_eq!(official.tokens["USDC"].decimals, 6);
        assert_eq!(official.pools.iter().find(|p| p.address == addr(10)).unwrap().exchange, "aerodrome");
        assert!(official.tokens.contains_key("DAI"));
        assert!(official.has_pool(addr(20)));
    }

    #[test]
    fn sources_codehash_classification() {
        let mut s = Sources {
            chain: "base".into(),
            exchanges: vec![ExchangeInfo {
                name: "aerodrome".into(),
                kind: "solidly".into(),
                factory: Some(addr(9)),
                router: None,
                pool_codehashes: vec![],
            }],
        };
        let ch = B256::repeat_byte(7);
        s.add_codehash("aerodrome", ch);
        s.add_codehash("aerodrome", ch); // idempotent
        assert_eq!(s.exchanges[0].pool_codehashes.len(), 1);
        assert_eq!(s.exchange_for_codehash(ch).unwrap().name, "aerodrome");
        assert!(s.exchange_for_codehash(B256::repeat_byte(8)).is_none());
    }

    #[test]
    fn committed_base_books_are_valid() {
        let book = PoolBook::load(PoolBook::path_for_chain("base", Tier::Official))
            .expect("config/base.official.toml must load");
        assert_eq!(book.chain, "base");
        assert!(book.tokens.contains_key("USDC"));
        let sources = Sources::load(Sources::path_for_chain("base"))
            .expect("config/base.sources.toml must load");
        assert!(sources.exchanges.iter().any(|e| e.name == "aerodrome"));
    }
}
