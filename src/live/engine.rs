//! The init + live-replay engine.
//!
//! Startup order (the block-height-safe sequence):
//!   1. **subscribe first** — open the event stream so the node queues every
//!      matching log; nothing can slip between snapshot and subscription.
//!   2. choose a pinned snapshot block `B` (the current head).
//!   3. snapshot every pool's state at `B`.
//!   4. drain the stream applying only events with `block > B` (the reconcile
//!      rule from [`crate::live::reconcile`]), in `(block, log_index)` order
//!      with per-pool dedup.
//!
//! This module runs a single writer task (race-free by construction). The
//! production sharded-writer topology (one `WriterTask` per `hash(addr) % N`,
//! readers wait-free via `ArcSwap`) is described in the design notes; the
//! reconcile/dedup logic — the part that determines correctness — is identical
//! and lives in pure functions so it is exhaustively unit-tested.

use std::sync::Arc;

use futures::StreamExt;

use crate::live::event::ChainItem;
use crate::live::reconcile::{drain_decision, Decision};
use crate::live::registry::{Entry, PoolRegistry};
use crate::live::source::{ChainSource, ItemStream, SourceError};
use crate::types::Address;

pub struct EngineOutcome {
    pub registry: Arc<PoolRegistry>,
    /// The pinned block the initial snapshot was taken at.
    pub snapshot_block: u64,
    /// Highest block header observed during the run.
    pub last_head: u64,
}

/// Subscribe-first, then snapshot every pool at the pinned head block `B`.
/// Returns the seeded registry, the live stream, and `B`.
async fn initialize<S: ChainSource>(
    src: &S,
    pools: &[Address],
) -> Result<(Arc<PoolRegistry>, ItemStream, u64), SourceError> {
    // 1. SUBSCRIBE FIRST — before any snapshot read.
    let stream = src.subscribe().await?;
    // 2. pin the snapshot block to the current head.
    let b = src.head().await?;
    // 3. snapshot all pools at B (log_index = MAX marks "end of block B").
    let registry = Arc::new(PoolRegistry::new());
    for &p in pools {
        let state = src.snapshot_pool(p, b).await?;
        registry.store(
            p,
            Entry {
                state,
                block: b,
                log_index: u64::MAX,
            },
        );
    }
    Ok((registry, stream, b))
}

/// Apply one streamed item to the registry (the single-writer apply path).
fn apply_item(registry: &PoolRegistry, snapshot_block: u64, item: &ChainItem, last_head: &mut u64) {
    match item {
        ChainItem::NewHead { number, .. } => {
            if *number > *last_head {
                *last_head = *number;
            }
        }
        ChainItem::Event(ev) => {
            // reconcile rule: discard everything already in the snapshot.
            if drain_decision(ev.block, snapshot_block) == Decision::Discard {
                return;
            }
            if let Some(cur) = registry.load(ev.pool) {
                // per-pool dedup / ordering: only strictly-newer keys apply.
                if ev.key() > (cur.block, cur.log_index) {
                    let next = ev.apply(&cur.state);
                    registry.store(
                        ev.pool,
                        Entry {
                            state: next,
                            block: ev.block,
                            log_index: ev.log_index,
                        },
                    );
                }
            }
            // events for untracked pools are ignored.
        }
    }
}

/// Initialize then drain the stream until it ends (used with finite/mock
/// sources and tests).
pub async fn run_to_completion<S: ChainSource>(
    src: &S,
    pools: &[Address],
) -> Result<EngineOutcome, SourceError> {
    let (registry, mut stream, b) = initialize(src, pools).await?;
    let mut last_head = b;
    while let Some(item) = stream.next().await {
        apply_item(&registry, b, &item, &mut last_head);
    }
    Ok(EngineOutcome {
        registry,
        snapshot_block: b,
        last_head,
    })
}

/// Initialize then stream live for `run_for`, then stop (used for the real WS
/// source, which never ends, and for the timing bench).
pub async fn run_with_deadline<S: ChainSource>(
    src: &S,
    pools: &[Address],
    run_for: std::time::Duration,
) -> Result<EngineOutcome, SourceError> {
    let (registry, mut stream, b) = initialize(src, pools).await?;
    let mut last_head = b;
    let deadline = tokio::time::sleep(run_for);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            item = stream.next() => match item {
                Some(it) => apply_item(&registry, b, &it, &mut last_head),
                None => break,
            }
        }
    }
    Ok(EngineOutcome {
        registry,
        snapshot_block: b,
        last_head,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::event::{EventKind, PoolEvent, PoolState};
    use crate::live::source::MockChain;
    use crate::types::{B256, U256};

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }
    fn v2(r0: u64, r1: u64) -> PoolState {
        PoolState::UniV2 {
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
        }
    }
    fn sync(pool: u8, block: u64, log_index: u64, r0: u64, r1: u64) -> ChainItem {
        ChainItem::Event(PoolEvent {
            pool: addr(pool),
            block,
            log_index,
            kind: EventKind::SyncV2 {
                reserve0: U256::from(r0),
                reserve1: U256::from(r1),
            },
        })
    }
    fn head(n: u64) -> ChainItem {
        ChainItem::NewHead {
            number: n,
            hash: B256::repeat_byte(n as u8),
            parent_hash: B256::repeat_byte((n - 1) as u8),
        }
    }

    #[tokio::test]
    async fn snapshot_then_apply_newer_events() {
        // Snapshot at B=10 (reserves 1000/1000); a block-10 event must be
        // discarded, block-11 and 12 events applied in order.
        let items = vec![
            head(11),
            sync(1, 10, 3, 1, 1), // <= B -> discard
            sync(1, 11, 0, 800, 1200),
            sync(1, 12, 0, 700, 1300),
        ];
        let src = MockChain::new(items, 10).with_snapshot(addr(1), 10, v2(1000, 1000));
        let out = run_to_completion(&src, &[addr(1)]).await.unwrap();
        assert_eq!(out.snapshot_block, 10);
        assert_eq!(out.registry.load(addr(1)).unwrap().state, v2(700, 1300));
        assert_eq!(out.last_head, 11);
    }

    #[tokio::test]
    async fn no_missed_block_event_before_snapshot() {
        // The subscribe-first guarantee: even though the block-11 event is first
        // in the stream (arrived while we were snapshotting), it is applied.
        let items = vec![sync(1, 11, 0, 500, 1500), head(11)];
        let src = MockChain::new(items, 10).with_snapshot(addr(1), 10, v2(1000, 1000));
        let out = run_to_completion(&src, &[addr(1)]).await.unwrap();
        assert_eq!(out.registry.load(addr(1)).unwrap().state, v2(500, 1500));
    }

    #[tokio::test]
    async fn dedup_redelivery() {
        let items = vec![
            sync(1, 11, 0, 900, 1100),
            sync(1, 12, 0, 800, 1200),
            sync(1, 11, 0, 900, 1100), // stale redelivery -> ignored
        ];
        let src = MockChain::new(items, 10).with_snapshot(addr(1), 10, v2(1000, 1000));
        let out = run_to_completion(&src, &[addr(1)]).await.unwrap();
        assert_eq!(out.registry.load(addr(1)).unwrap().state, v2(800, 1200));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_stops_the_run() {
        // Finite stream; deadline path returns and preserves applied state.
        let items = vec![head(11), sync(1, 11, 0, 800, 1200)];
        let src = MockChain::new(items, 10).with_snapshot(addr(1), 10, v2(1000, 1000));
        let out = run_with_deadline(&src, &[addr(1)], std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(out.registry.load(addr(1)).unwrap().state, v2(800, 1200));
    }
}
