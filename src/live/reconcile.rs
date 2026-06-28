//! The pure, network-free heart of block-height-safe replay.
//!
//! Correctness rule (derived in the design): a state read pinned to block `B`
//! returns the post-state of the *entire* block `B`, so **every** event in
//! block `B` (all log indices) is already reflected. Therefore:
//!
//! ```text
//! event.block <= B  -> Discard (already in snapshot)
//! event.block >  B  -> Apply
//! ```
//!
//! Do NOT split block `B` by log index — that is the classic double-apply bug.
//! Ordering by `(block, log_index)` still matters for blocks `> B` (relative
//! events) and is enforced by [`replay`], together with per-pool dedup.

use std::collections::HashMap;

use crate::live::event::{PoolEvent, PoolState};
use crate::types::Address;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Discard,
    Apply,
}

/// The reconcile rule for a single event against a pinned snapshot block.
#[inline]
pub fn drain_decision(event_block: u64, snapshot_block: u64) -> Decision {
    if event_block <= snapshot_block {
        Decision::Discard
    } else {
        Decision::Apply
    }
}

/// Apply an ordered tail of events to a snapshot of pool states, enforcing:
///  - the `> snapshot_block` reconcile rule,
///  - strict per-pool `(block, log_index)` monotonicity (out-of-order or
///    re-delivered events `<= last_applied[pool]` are dropped — idempotent
///    dedup, e.g. after a WS reconnect).
///
/// `states` is mutated in place. Events must already be sorted ascending by
/// `(block, log_index)` (a single node subscription delivers them so).
pub fn replay(
    states: &mut HashMap<Address, PoolState>,
    snapshot_block: u64,
    ordered_tail: &[PoolEvent],
) {
    let mut last_applied: HashMap<Address, (u64, u64)> = HashMap::new();
    for ev in ordered_tail {
        if drain_decision(ev.block, snapshot_block) == Decision::Discard {
            continue;
        }
        let key = ev.key();
        if let Some(prev) = last_applied.get(&ev.pool) {
            if key <= *prev {
                continue; // stale / duplicate
            }
        }
        if let Some(state) = states.get(&ev.pool) {
            let next = ev.apply(state);
            states.insert(ev.pool, next);
            last_applied.insert(ev.pool, key);
        }
        // Events for pools not in the snapshot set are ignored (not tracked).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::event::EventKind;
    use crate::types::U256;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }
    fn sync(pool: u8, block: u64, log_index: u64, r0: u64, r1: u64) -> PoolEvent {
        PoolEvent {
            pool: addr(pool),
            block,
            log_index,
            kind: EventKind::SyncV2 {
                reserve0: U256::from(r0),
                reserve1: U256::from(r1),
            },
        }
    }
    fn v2(r0: u64, r1: u64) -> PoolState {
        PoolState::UniV2 {
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
        }
    }

    #[test]
    fn decision_boundary_includes_block_b() {
        assert_eq!(drain_decision(5, 10), Decision::Discard);
        assert_eq!(drain_decision(10, 10), Decision::Discard); // block B fully in snapshot
        assert_eq!(drain_decision(11, 10), Decision::Apply);
    }

    #[test]
    fn no_double_apply_at_snapshot_block() {
        // Snapshot at B=10 already reflects block-10 events.
        let mut states = HashMap::new();
        states.insert(addr(1), v2(1000, 1000)); // snapshot value (post-block-10)
        let tail = vec![
            sync(1, 10, 7, 999, 999), // block 10 -> must be discarded
            sync(1, 11, 0, 800, 1200), // block 11 -> applied
        ];
        replay(&mut states, 10, &tail);
        assert_eq!(states[&addr(1)], v2(800, 1200));
    }

    #[test]
    fn applies_event_that_arrived_before_snapshot_completed() {
        // The subscribe-first guarantee: a block-(B+1) event buffered before the
        // snapshot finished is still applied -> no gap.
        let mut states = HashMap::new();
        states.insert(addr(1), v2(1000, 1000));
        let tail = vec![sync(1, 11, 0, 500, 1500)];
        replay(&mut states, 10, &tail);
        assert_eq!(states[&addr(1)], v2(500, 1500));
    }

    #[test]
    fn dedup_on_redelivery_is_idempotent() {
        // Replaying the same tail twice (e.g. reconnect) yields the same state.
        let mut states = HashMap::new();
        states.insert(addr(1), v2(1000, 1000));
        let tail = vec![
            sync(1, 11, 0, 900, 1100),
            sync(1, 12, 0, 800, 1200),
            sync(1, 11, 0, 900, 1100), // duplicate of an earlier key -> dropped
        ];
        replay(&mut states, 10, &tail);
        assert_eq!(states[&addr(1)], v2(800, 1200));
    }

    #[test]
    fn ordering_within_block_last_write_wins_for_absolute() {
        let mut states = HashMap::new();
        states.insert(addr(1), v2(1, 1));
        let tail = vec![
            sync(1, 11, 0, 10, 10),
            sync(1, 11, 1, 20, 20),
            sync(1, 11, 2, 30, 30), // highest log_index wins
        ];
        replay(&mut states, 10, &tail);
        assert_eq!(states[&addr(1)], v2(30, 30));
    }
}
