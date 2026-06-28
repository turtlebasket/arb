//! Timing-bench: prove the streamed/replayed state equals on-chain state.
//!
//! Runs the engine live for `run_for`, then compares each pool's replayed state
//! against a FRESH pinned snapshot at a coherent cut block. Both sides mean
//! "state as of the close of block `cut_block`", so a correct replay matches
//! exactly; any field mismatch is a replay bug (wrong reconcile rule, a missed
//! event, or relative/absolute confusion).
//!
//! In production `cut_block` is chosen as `last_head - reorg_margin` and the
//! comparison snapshot is pinned **by hash** so a reorg can't make the two sides
//! reference different blocks. Here `cut_block` is a parameter so the check is
//! deterministically testable against a [`crate::live::source::MockChain`].

use std::time::Duration;

use crate::live::engine::run_with_deadline;
use crate::live::event::PoolState;
use crate::live::source::{ChainSource, SourceError};
use crate::types::Address;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub pool: Address,
    pub replayed: PoolState,
    pub onchain: PoolState,
}

#[derive(Debug, Clone, Default)]
pub struct BenchReport {
    pub cut_block: u64,
    pub pools_checked: usize,
    pub mismatches: Vec<Mismatch>,
}

impl BenchReport {
    pub fn passed(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Run the engine for `run_for`, then verify replayed state == on-chain state
/// at `cut_block` for every pool.
pub async fn timing_bench<S: ChainSource>(
    src: &S,
    pools: &[Address],
    run_for: Duration,
    cut_block: u64,
) -> Result<BenchReport, SourceError> {
    let outcome = run_with_deadline(src, pools, run_for).await?;

    let mut report = BenchReport {
        cut_block,
        pools_checked: pools.len(),
        mismatches: Vec::new(),
    };

    for &p in pools {
        let replayed = outcome
            .registry
            .load(p)
            .ok_or(SourceError::NoSnapshot(p, cut_block))?
            .state;
        let onchain = src.snapshot_pool(p, cut_block).await?;
        if replayed != onchain {
            report.mismatches.push(Mismatch {
                pool: p,
                replayed,
                onchain,
            });
        }
    }
    Ok(report)
}

/// Verify each pool's replayed state against a fresh pinned snapshot at the
/// block we last applied for that pool (a per-pool coherent cut). Unlike
/// [`timing_bench`] this needs no single global cut and is what the live CLI
/// uses after streaming for a while.
///
/// Caveat: comparing at the most-recently-applied block carries a small reorg
/// risk near the tip; in production pin by block *hash* and stay
/// `reorg_margin` blocks behind head.
pub async fn verify_against_chain<S: ChainSource>(
    src: &S,
    registry: &crate::live::registry::PoolRegistry,
    pools: &[Address],
) -> Result<BenchReport, SourceError> {
    let mut report = BenchReport {
        cut_block: 0,
        pools_checked: pools.len(),
        mismatches: Vec::new(),
    };
    for &p in pools {
        let entry = registry.load(p).ok_or(SourceError::NoSnapshot(p, 0))?;
        report.cut_block = report.cut_block.max(entry.block);
        let onchain = src.snapshot_pool(p, entry.block).await?;
        if entry.state != onchain {
            report.mismatches.push(Mismatch {
                pool: p,
                replayed: entry.state,
                onchain,
            });
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::event::{ChainItem, EventKind, PoolEvent};
    use crate::live::source::MockChain;
    use crate::types::U256;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }
    fn v2(r0: u64, r1: u64) -> PoolState {
        PoolState::UniV2 {
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
        }
    }
    fn sync(pool: u8, block: u64, li: u64, r0: u64, r1: u64) -> ChainItem {
        ChainItem::Event(PoolEvent {
            pool: addr(pool),
            block,
            log_index: li,
            kind: EventKind::SyncV2 {
                reserve0: U256::from(r0),
                reserve1: U256::from(r1),
            },
        })
    }

    #[tokio::test(start_paused = true)]
    async fn bench_passes_when_replay_matches_chain() {
        // Replay applies block-11 event -> 800/1200. The on-chain snapshot at the
        // cut block (11) reports the same -> no mismatch.
        let items = vec![sync(1, 11, 0, 800, 1200)];
        let src = MockChain::new(items, 10)
            .with_snapshot(addr(1), 10, v2(1000, 1000)) // init snapshot at B=10
            .with_snapshot(addr(1), 11, v2(800, 1200)); // verification at cut=11
        let report = timing_bench(&src, &[addr(1)], Duration::from_secs(1), 11)
            .await
            .unwrap();
        assert!(report.passed(), "mismatches: {:?}", report.mismatches);
        assert_eq!(report.pools_checked, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn bench_detects_a_missed_event() {
        // The stream OMITS the block-11 event, but the chain's state at cut=11
        // reflects it -> the bench must catch the divergence.
        let items: Vec<ChainItem> = vec![]; // missed the update entirely
        let src = MockChain::new(items, 10)
            .with_snapshot(addr(1), 10, v2(1000, 1000))
            .with_snapshot(addr(1), 11, v2(800, 1200)); // chain moved; we didn't
        let report = timing_bench(&src, &[addr(1)], Duration::from_secs(1), 11)
            .await
            .unwrap();
        assert!(!report.passed());
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].replayed, v2(1000, 1000));
        assert_eq!(report.mismatches[0].onchain, v2(800, 1200));
    }
}
