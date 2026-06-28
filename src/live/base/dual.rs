//! Base-specific: dual-tier reconcile engine.
//!
//! Maintains two views of pool state:
//!   - **optimistic** — confirmed state + Flashblock preconfirmations for the
//!     in-progress block(s). The arb scanner quotes against this for ~200ms-
//!     earlier visibility.
//!   - **confirmed** — sealed-block (canonical) state.
//!
//! When a block seals, we reconcile: advance `confirmed`, drop now-sealed
//! preconfirmations, rebase `optimistic` onto the new `confirmed`, and report
//! any pool where the Flashblock prediction differed from the sealed truth
//! (a divergence — preconf was wrong, or we mis-decoded).
//!
//! The reconcile *algorithm* is generic, but it is scoped here under `base`
//! because its only driver — Flashblocks — is Base/OP-stack-specific. The pure
//! state methods are exhaustively unit-tested; the async runner is a thin merge
//! of the two (mocked) streams.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use futures::StreamExt;

use crate::live::base::flashblocks::{Flashblock, PreconfSource};
use crate::live::event::{ChainItem, PoolEvent, PoolState};
use crate::live::registry::Entry;
use crate::live::source::{ChainSource, SourceError};
use crate::types::Address;

/// Outcome of applying one Flashblock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashOutcome {
    /// Applied to the optimistic view.
    Applied,
    /// For a block at/below the confirmed tip — ignored.
    Stale,
    /// Index was non-contiguous (a Flashblock was missed) — applied, but the
    /// caller should consider a resnapshot. The gap is also counted.
    Gap,
}

/// A pool whose Flashblock prediction did not match the sealed block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub pool: Address,
    pub predicted: PoolState,
    pub sealed: PoolState,
}

pub struct DualState {
    confirmed: HashMap<Address, Entry>,
    optimistic: HashMap<Address, Entry>,
    confirmed_block: u64,
    /// Preconfirmation events for blocks `> confirmed_block`, in arrival order.
    pending: Vec<PoolEvent>,
    /// Last `(block, index)` Flashblock seen, for gap detection.
    last_flash: Option<(u64, u64)>,
    pub gaps: u64,
    pub divergences: u64,
}

fn apply_event(map: &mut HashMap<Address, Entry>, ev: &PoolEvent) -> bool {
    match map.get(&ev.pool) {
        Some(cur) if ev.key() <= (cur.block, cur.log_index) => false, // stale/dupe
        Some(cur) => {
            let next = ev.apply(&cur.state);
            map.insert(
                ev.pool,
                Entry {
                    state: next,
                    block: ev.block,
                    log_index: ev.log_index,
                },
            );
            true
        }
        None => false, // untracked pool
    }
}

impl DualState {
    /// Initialize from a sealed snapshot pinned at `block`. The snapshot entries
    /// use `log_index = u64::MAX` to mark "end of block" (so any same-block
    /// event is treated as already included).
    pub fn new(snapshot: HashMap<Address, Entry>, block: u64) -> Self {
        Self {
            optimistic: snapshot.clone(),
            confirmed: snapshot,
            confirmed_block: block,
            pending: Vec::new(),
            last_flash: None,
            gaps: 0,
            divergences: 0,
        }
    }

    pub fn confirmed_block(&self) -> u64 {
        self.confirmed_block
    }

    /// State the scanner should quote against (preconfirmed).
    pub fn optimistic(&self, pool: Address) -> Option<PoolState> {
        self.optimistic.get(&pool).map(|e| e.state.clone())
    }

    /// Canonical sealed state.
    pub fn confirmed(&self, pool: Address) -> Option<PoolState> {
        self.confirmed.get(&pool).map(|e| e.state.clone())
    }

    /// Apply a Flashblock preconfirmation to the optimistic view.
    pub fn apply_flashblock(&mut self, fb: &Flashblock) -> FlashOutcome {
        if fb.block <= self.confirmed_block {
            return FlashOutcome::Stale;
        }
        let outcome = match self.last_flash {
            None => FlashOutcome::Applied,
            Some((lb, li)) => {
                if fb.block == lb {
                    if fb.index == li + 1 {
                        FlashOutcome::Applied
                    } else {
                        FlashOutcome::Gap
                    }
                } else if fb.block > lb {
                    // a new in-progress block should start at index 0
                    if fb.index == 0 {
                        FlashOutcome::Applied
                    } else {
                        FlashOutcome::Gap
                    }
                } else {
                    FlashOutcome::Stale
                }
            }
        };
        if outcome == FlashOutcome::Stale {
            return outcome;
        }
        if outcome == FlashOutcome::Gap {
            self.gaps += 1;
        }
        self.last_flash = Some((fb.block, fb.index));
        for ev in &fb.events {
            self.pending.push(ev.clone());
            apply_event(&mut self.optimistic, ev);
        }
        outcome
    }

    /// Reconcile a sealed block: advance confirmed, rebase optimistic, and
    /// return any pools where the preconfirmation diverged from the sealed truth.
    pub fn apply_sealed_block(&mut self, block: u64, events: &[PoolEvent]) -> Vec<Divergence> {
        let mut divergences = Vec::new();
        if block <= self.confirmed_block {
            return divergences; // already sealed
        }

        // Capture the optimistic prediction for the pools this block touches,
        // BEFORE we overwrite with sealed truth.
        let mut predicted: HashMap<Address, PoolState> = HashMap::new();
        for ev in events {
            if let Some(state) = self.optimistic(ev.pool) {
                predicted.entry(ev.pool).or_insert(state);
            }
        }

        // Advance confirmed with the sealed events (ordered, deduped).
        let mut ordered: Vec<&PoolEvent> = events.iter().collect();
        ordered.sort_by_key(|e| (e.block, e.log_index));
        for ev in ordered {
            apply_event(&mut self.confirmed, ev);
        }
        self.confirmed_block = block;

        // Drop now-sealed preconfirmations; reset flash tracking if it pointed
        // at a sealed block.
        self.pending.retain(|e| e.block > block);
        if let Some((lb, _)) = self.last_flash {
            if lb <= block {
                self.last_flash = None;
            }
        }

        // Rebase optimistic = confirmed + remaining preconfirmations.
        self.optimistic = self.confirmed.clone();
        let mut pend = self.pending.clone();
        pend.sort_by_key(|e| (e.block, e.log_index));
        for ev in &pend {
            apply_event(&mut self.optimistic, ev);
        }

        // Report divergences (preconf prediction vs sealed truth).
        for (pool, pred) in predicted {
            if let Some(truth) = self.confirmed(pool) {
                if truth != pred {
                    divergences.push(Divergence {
                        pool,
                        predicted: pred,
                        sealed: truth,
                    });
                }
            }
        }
        self.divergences += divergences.len() as u64;
        divergences
    }
}

/// Async runner: subscribe to BOTH tiers first, snapshot the sealed state at a
/// pinned block, then merge Flashblocks (applied immediately) with sealed blocks
/// (flushed per-block on the next head). Returns the final [`DualState`].
///
/// Sealed events are buffered by block and flushed when a later head proves the
/// block complete — the same "block is done once we see its successor" rule the
/// single-tier engine uses.
pub async fn run_dual<S: ChainSource, P: PreconfSource>(
    sealed: &S,
    preconf: &P,
    pools: &[Address],
    run_for: Duration,
) -> Result<DualState, SourceError> {
    // subscribe to both BEFORE snapshotting (no-gap invariant).
    let mut sealed_stream = sealed.subscribe().await?;
    let mut flash_stream = preconf.subscribe().await?;

    let b = sealed.head().await?;
    let mut snapshot: HashMap<Address, Entry> = HashMap::new();
    for &p in pools {
        let state = sealed.snapshot_pool(p, b).await?;
        snapshot.insert(
            p,
            Entry {
                state,
                block: b,
                log_index: u64::MAX,
            },
        );
    }
    let mut state = DualState::new(snapshot, b);

    let mut buf: BTreeMap<u64, Vec<PoolEvent>> = BTreeMap::new();
    let deadline = tokio::time::sleep(run_for);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => break,
            item = sealed_stream.next() => match item {
                Some(ChainItem::Event(ev)) => {
                    if ev.block > state.confirmed_block() {
                        buf.entry(ev.block).or_default().push(ev);
                    }
                }
                Some(ChainItem::NewHead { number, .. }) => {
                    let complete: Vec<u64> =
                        buf.range(..number).map(|(k, _)| *k).collect();
                    for blk in complete {
                        if let Some(evs) = buf.remove(&blk) {
                            state.apply_sealed_block(blk, &evs);
                        }
                    }
                }
                None => {}
            },
            fb = flash_stream.next() => match fb {
                Some(fb) => { state.apply_flashblock(&fb); }
                None => {}
            },
            else => break,
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::event::EventKind;
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
    fn ev(pool: u8, block: u64, log_index: u64, r0: u64, r1: u64) -> PoolEvent {
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
    fn snapshot(block: u64, pools: &[(u8, u64, u64)]) -> DualState {
        let mut m = HashMap::new();
        for &(p, r0, r1) in pools {
            m.insert(
                addr(p),
                Entry {
                    state: v2(r0, r1),
                    block,
                    log_index: u64::MAX,
                },
            );
        }
        DualState::new(m, block)
    }

    #[test]
    fn flashblock_updates_optimistic_not_confirmed() {
        let mut s = snapshot(10, &[(1, 1000, 1000)]);
        let fb = Flashblock {
            block: 11,
            index: 0,
            events: vec![ev(1, 11, 0, 800, 1200)],
        };
        assert_eq!(s.apply_flashblock(&fb), FlashOutcome::Applied);
        // optimistic sees the preconf immediately; confirmed still at snapshot.
        assert_eq!(s.optimistic(addr(1)), Some(v2(800, 1200)));
        assert_eq!(s.confirmed(addr(1)), Some(v2(1000, 1000)));
    }

    #[test]
    fn sealing_promotes_and_matches_when_preconf_correct() {
        let mut s = snapshot(10, &[(1, 1000, 1000)]);
        s.apply_flashblock(&Flashblock {
            block: 11,
            index: 0,
            events: vec![ev(1, 11, 0, 800, 1200)],
        });
        // Sealed block 11 confirms exactly what the flashblock predicted.
        let div = s.apply_sealed_block(11, &[ev(1, 11, 0, 800, 1200)]);
        assert!(div.is_empty(), "no divergence expected");
        assert_eq!(s.confirmed(addr(1)), Some(v2(800, 1200)));
        assert_eq!(s.optimistic(addr(1)), Some(v2(800, 1200)));
        assert_eq!(s.confirmed_block(), 11);
    }

    #[test]
    fn divergence_detected_when_preconf_wrong() {
        let mut s = snapshot(10, &[(1, 1000, 1000)]);
        // Flashblock predicts 800/1200...
        s.apply_flashblock(&Flashblock {
            block: 11,
            index: 0,
            events: vec![ev(1, 11, 0, 800, 1200)],
        });
        // ...but the sealed block actually had 850/1150.
        let div = s.apply_sealed_block(11, &[ev(1, 11, 0, 850, 1150)]);
        assert_eq!(div.len(), 1);
        assert_eq!(div[0].predicted, v2(800, 1200));
        assert_eq!(div[0].sealed, v2(850, 1150));
        assert_eq!(s.confirmed(addr(1)), Some(v2(850, 1150)));
        // optimistic rebased onto sealed truth (no later preconfs pending).
        assert_eq!(s.optimistic(addr(1)), Some(v2(850, 1150)));
        assert_eq!(s.divergences, 1);
    }

    #[test]
    fn stale_flashblock_for_sealed_block_ignored() {
        let mut s = snapshot(10, &[(1, 1000, 1000)]);
        s.apply_sealed_block(11, &[ev(1, 11, 0, 800, 1200)]);
        let out = s.apply_flashblock(&Flashblock {
            block: 11,
            index: 5,
            events: vec![ev(1, 11, 5, 1, 1)],
        });
        assert_eq!(out, FlashOutcome::Stale);
        assert_eq!(s.optimistic(addr(1)), Some(v2(800, 1200)));
    }

    #[test]
    fn gap_in_flashblock_index_is_flagged() {
        let mut s = snapshot(10, &[(1, 1000, 1000)]);
        s.apply_flashblock(&Flashblock { block: 11, index: 0, events: vec![] });
        // index jumps 0 -> 2 (missed index 1)
        let out = s.apply_flashblock(&Flashblock {
            block: 11,
            index: 2,
            events: vec![ev(1, 11, 3, 700, 1300)],
        });
        assert_eq!(out, FlashOutcome::Gap);
        assert_eq!(s.gaps, 1);
        // still applied despite the gap
        assert_eq!(s.optimistic(addr(1)), Some(v2(700, 1300)));
    }

    #[test]
    fn later_preconf_survives_earlier_block_sealing() {
        // Preconf for block 12 should remain on the optimistic view after block
        // 11 seals (rebase keeps pending > confirmed_block).
        let mut s = snapshot(10, &[(1, 1000, 1000), (2, 5000, 5000)]);
        s.apply_flashblock(&Flashblock {
            block: 11,
            index: 0,
            events: vec![ev(1, 11, 0, 900, 1100)],
        });
        s.apply_flashblock(&Flashblock {
            block: 12,
            index: 0,
            events: vec![ev(2, 12, 0, 4000, 6000)],
        });
        s.apply_sealed_block(11, &[ev(1, 11, 0, 900, 1100)]);
        // pool 1 sealed; pool 2's block-12 preconf still optimistically applied.
        assert_eq!(s.confirmed(addr(1)), Some(v2(900, 1100)));
        assert_eq!(s.confirmed(addr(2)), Some(v2(5000, 5000)));
        assert_eq!(s.optimistic(addr(2)), Some(v2(4000, 6000)));
    }
}
