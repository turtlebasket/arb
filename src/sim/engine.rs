//! Earnings simulation engine.
//!
//! Replays a single chain's swap-event stream and estimates realized arbitrage
//! profit under the latency model: when a candidate USDC→…→USDC cycle is
//! detected at stream index `t`, we draw `K` from the [`LatencyPdf`] and
//! recompute the *same* trade against the pool state advanced by `K` events
//! (`state_at(pool, t+K)`), counting it only if still net-positive.
//!
//! Everything here is deterministic: `(seed, stream, cycles, pdf)` fully
//! determine the [`SimReport`]. State lookups are O(log) array indexing — no AMM
//! math is replayed to move through the stream; only each candidate cycle's own
//! `Pool::quote` calls run, against the chosen snapshot.

use crate::amm::univ2::UniV2Pool;
use crate::path::{net_profit, Leg};
use crate::pool::Pool;
use crate::sim::pdf::LatencyPdf;
use crate::sim::rng::SplitMix64;
use crate::types::{Address, U256};

pub type PoolId = usize;

/// Absolute post-event state of a pool (what on-chain logs carry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolStateSnapshot {
    UniV2 { reserve0: U256, reserve1: U256 },
    // Extend with V3 { sqrt_price_x96, liquidity, tick, ticks }, Curve { balances }, etc.
}

/// Static (immutable) pool configuration, paired with snapshots to build a
/// concrete [`Pool`] for quoting.
#[derive(Clone, Debug)]
pub enum PoolMeta {
    UniV2 {
        address: Address,
        token0: Address,
        token1: Address,
        fee_bps: u32,
    },
}

impl PoolMeta {
    fn build(&self, state: &PoolStateSnapshot) -> Box<dyn Pool> {
        match (self, state) {
            (
                PoolMeta::UniV2 {
                    address,
                    token0,
                    token1,
                    fee_bps,
                },
                PoolStateSnapshot::UniV2 { reserve0, reserve1 },
            ) => Box::new(UniV2Pool::new(
                *address, *token0, *token1, *reserve0, *reserve1, *fee_bps,
            )),
            #[allow(unreachable_patterns)]
            _ => panic!("PoolMeta/PoolStateSnapshot variant mismatch"),
        }
    }
}

/// The set of pools being simulated, with their initial states.
pub struct PoolUniverse {
    pub metas: Vec<PoolMeta>,
    pub initial: Vec<PoolStateSnapshot>,
}

/// One state-changing event in global stream order; carries the touched pool's
/// absolute post-event state.
#[derive(Clone, Debug)]
pub struct PoolEvent {
    pub pool: PoolId,
    pub state: PoolStateSnapshot,
}

pub struct ChainEventStream {
    pub events: Vec<PoolEvent>,
}

/// One hop of a closed cycle. `hops[0].token_in == hops[last].token_out` (USDC).
#[derive(Clone, Debug)]
pub struct Hop {
    pub pool: PoolId,
    pub token_in: Address,
    pub token_out: Address,
}

#[derive(Clone, Debug)]
pub struct Cycle {
    pub hops: Vec<Hop>,
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub pdf: LatencyPdf,
    pub seed: u64,
    /// Fixed input size committed at detection time (same `amount_in` reused for
    /// the delayed recompute — calldata is fixed once submitted).
    pub amount_in: U256,
    pub gas_price_wei: U256,
    pub token_units_per_native_1e18: U256,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimReport {
    pub realized_profit: U256,
    pub detected: u64,
    pub landed_profitable: u64,
    pub missed_latency: u64,
    /// `delay_histogram[k]` = number of fired opportunities that drew `K = k`.
    pub delay_histogram: Vec<u64>,
}

/// Precomputed per-pool state history for O(log) `state_at` lookups.
pub struct StateHistory {
    /// `snapshots[p][s]` = pool `p`'s state after its `s`-th event (`s==0` initial).
    snapshots: Vec<Vec<PoolStateSnapshot>>,
    /// `positions[p][k]` = global index of pool `p`'s `(k+1)`-th event.
    positions: Vec<Vec<usize>>,
}

impl StateHistory {
    pub fn build(initial: &[PoolStateSnapshot], stream: &ChainEventStream) -> Self {
        let n = initial.len();
        let mut snapshots: Vec<Vec<PoolStateSnapshot>> =
            initial.iter().map(|st| vec![st.clone()]).collect();
        let mut positions: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (g, ev) in stream.events.iter().enumerate() {
            snapshots[ev.pool].push(ev.state.clone());
            positions[ev.pool].push(g);
        }
        Self {
            snapshots,
            positions,
        }
    }

    /// Number of pool-`p` events with global index `<= t`.
    #[inline]
    fn seq_at(&self, p: PoolId, t: usize) -> usize {
        self.positions[p].partition_point(|&g| g <= t)
    }

    /// Pool `p`'s state as of just after global index `t` (clamped to the end).
    #[inline]
    pub fn state_at(&self, p: PoolId, t: usize) -> &PoolStateSnapshot {
        let s = self.seq_at(p, t).min(self.snapshots[p].len() - 1);
        &self.snapshots[p][s]
    }
}

/// Evaluate a cycle's net profit against the state as of global index `t`.
/// Returns `Some(profit)` only if strictly net-positive (after fees + gas).
fn eval_cycle(
    universe: &PoolUniverse,
    hist: &StateHistory,
    cycle: &Cycle,
    t: usize,
    cfg: &SimConfig,
) -> Option<U256> {
    // Build concrete pools from the chosen snapshots (kept alive for borrowing).
    let pools: Vec<Box<dyn Pool>> = cycle
        .hops
        .iter()
        .map(|h| universe.metas[h.pool].build(hist.state_at(h.pool, t)))
        .collect();
    let legs: Vec<Leg> = cycle
        .hops
        .iter()
        .zip(pools.iter())
        .map(|(h, p)| Leg {
            pool: p.as_ref(),
            token_in: h.token_in,
            token_out: h.token_out,
        })
        .collect();
    match net_profit(
        &legs,
        cfg.amount_in,
        cfg.gas_price_wei,
        cfg.token_units_per_native_1e18,
    ) {
        Ok(r) => r.net_profit,
        Err(_) => None,
    }
}

/// Run the simulation. Iteration order (stream order, then ascending cycle id)
/// is load-bearing for RNG reproducibility.
pub fn simulate(
    universe: &PoolUniverse,
    cycles: &[Cycle],
    stream: &ChainEventStream,
    cfg: &SimConfig,
) -> SimReport {
    let hist = StateHistory::build(&universe.initial, stream);
    let mut rng = SplitMix64::new(cfg.seed);

    // pool -> cycle ids that touch it (so each event only re-evals relevant cycles).
    let mut inverted: Vec<Vec<usize>> = vec![Vec::new(); universe.metas.len()];
    for (ci, cyc) in cycles.iter().enumerate() {
        for h in &cyc.hops {
            if !inverted[h.pool].contains(&ci) {
                inverted[h.pool].push(ci);
            }
        }
    }

    let mut active = vec![false; cycles.len()];
    let mut report = SimReport {
        delay_histogram: vec![0; cfg.pdf.max_delay() + 1],
        ..Default::default()
    };

    for t in 0..stream.events.len() {
        let p = stream.events[t].pool;
        for &c in &inverted[p] {
            match eval_cycle(universe, &hist, &cycles[c], t, cfg) {
                Some(_profit) => {
                    if active[c] {
                        continue; // already mid-episode: fire once
                    }
                    active[c] = true;
                    report.detected += 1;

                    let k = cfg.pdf.sample(&mut rng);
                    report.delay_histogram[k] += 1;

                    // Recompute the SAME trade against the K-late world state.
                    match eval_cycle(universe, &hist, &cycles[c], t + k, cfg) {
                        Some(pr) => {
                            report.realized_profit += pr;
                            report.landed_profitable += 1;
                        }
                        None => report.missed_latency += 1,
                    }
                }
                None => active[c] = false, // edge closed -> re-arm
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usdc() -> Address {
        Address::repeat_byte(1)
    }
    fn weth() -> Address {
        Address::repeat_byte(2)
    }

    /// Build a 2-pool USDC↔WETH↔USDC universe + cycle. Pool B (id 1) is fixed
    /// and favorable; pool A (id 0) state is driven by the event stream.
    fn universe() -> (PoolUniverse, Vec<Cycle>) {
        let metas = vec![
            PoolMeta::UniV2 {
                address: Address::repeat_byte(10),
                token0: usdc(),
                token1: weth(),
                fee_bps: 30,
            },
            PoolMeta::UniV2 {
                address: Address::repeat_byte(11),
                token0: usdc(),
                token1: weth(),
                fee_bps: 30,
            },
        ];
        // initial pool A at parity (no opportunity); pool B sells WETH high.
        let initial = vec![
            PoolStateSnapshot::UniV2 {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(1_000_000u64),
            },
            PoolStateSnapshot::UniV2 {
                reserve0: U256::from(1_100_000u64),
                reserve1: U256::from(1_000_000u64),
            },
        ];
        let cycles = vec![Cycle {
            hops: vec![
                Hop { pool: 0, token_in: usdc(), token_out: weth() },
                Hop { pool: 1, token_in: weth(), token_out: usdc() },
            ],
        }];
        (PoolUniverse { metas, initial }, cycles)
    }

    /// Stream of 3 events on pool A with monotonically DECREASING profit:
    /// more WETH reserve == cheaper WETH == bigger arb edge.
    fn decaying_stream() -> ChainEventStream {
        ChainEventStream {
            events: vec![
                PoolEvent {
                    pool: 0,
                    state: PoolStateSnapshot::UniV2 {
                        reserve0: U256::from(1_000_000u64),
                        reserve1: U256::from(1_300_000u64),
                    },
                },
                PoolEvent {
                    pool: 0,
                    state: PoolStateSnapshot::UniV2 {
                        reserve0: U256::from(1_000_000u64),
                        reserve1: U256::from(1_150_000u64),
                    },
                },
                PoolEvent {
                    pool: 0,
                    state: PoolStateSnapshot::UniV2 {
                        reserve0: U256::from(1_000_000u64),
                        reserve1: U256::from(1_010_000u64),
                    },
                },
            ],
        }
    }

    fn cfg(pdf: &str) -> SimConfig {
        SimConfig {
            pdf: LatencyPdf::parse(pdf).unwrap(),
            seed: 0,
            amount_in: U256::from(1000u64),
            gas_price_wei: U256::ZERO, // pure edge; gas isolated out for the math tests
            token_units_per_native_1e18: U256::from(1u64),
        }
    }

    #[test]
    fn degenerate_pdfs_are_monotone() {
        let (uni, cycles) = universe();
        let stream = decaying_stream();

        let r0 = simulate(&uni, &cycles, &stream, &cfg("1")); // always on time
        let r1 = simulate(&uni, &cycles, &stream, &cfg("0,1")); // always 1 late
        let r2 = simulate(&uni, &cycles, &stream, &cfg("0,0,1")); // always 2 late

        // Each fires exactly once (episode dedup over a standing opportunity).
        assert_eq!(r0.detected, 1);
        assert_eq!(r1.detected, 1);
        assert_eq!(r2.detected, 1);

        // Realized profit is non-increasing as latency mass moves to higher K.
        assert!(r0.realized_profit >= r1.realized_profit, "R0 {} R1 {}", r0.realized_profit, r1.realized_profit);
        assert!(r1.realized_profit >= r2.realized_profit, "R1 {} R2 {}", r1.realized_profit, r2.realized_profit);
        // And the opportunity is real at the top of the episode.
        assert!(r0.realized_profit > U256::ZERO);
    }

    #[test]
    fn always_on_time_lands_everything() {
        let (uni, cycles) = universe();
        let stream = decaying_stream();
        let r = simulate(&uni, &cycles, &stream, &cfg("1"));
        assert_eq!(r.missed_latency, 0);
        assert_eq!(r.landed_profitable, 1);
        assert_eq!(r.delay_histogram, vec![1]);
    }

    #[test]
    fn reproducible_across_runs() {
        let (uni, cycles) = universe();
        let stream = decaying_stream();
        let a = simulate(&uni, &cycles, &stream, &cfg("0.5,0.25,0.25"));
        let b = simulate(&uni, &cycles, &stream, &cfg("0.5,0.25,0.25"));
        assert_eq!(a, b);
    }

    #[test]
    fn mixed_pdf_bounded_by_degenerates() {
        let (uni, cycles) = universe();
        let stream = decaying_stream();
        let r0 = simulate(&uni, &cycles, &stream, &cfg("1")).realized_profit;
        let r2 = simulate(&uni, &cycles, &stream, &cfg("0,0,1")).realized_profit;
        let mixed = simulate(&uni, &cycles, &stream, &cfg("0.5,0.25,0.25")).realized_profit;
        assert!(mixed <= r0 && mixed >= r2, "mixed={mixed} R0={r0} R2={r2}");
    }

    #[test]
    fn episode_dedup_fires_once() {
        // A standing opportunity across many events must be detected once.
        let (uni, cycles) = universe();
        let stream = decaying_stream();
        let r = simulate(&uni, &cycles, &stream, &cfg("1"));
        assert_eq!(r.detected, 1);
    }

    #[test]
    fn k_beyond_end_clamps_without_panic() {
        // PDF that can draw K=5 although only 3 events exist; must clamp.
        let (uni, cycles) = universe();
        let stream = decaying_stream();
        let r = simulate(&uni, &cycles, &stream, &cfg("0,0,0,0,0,1"));
        assert_eq!(r.detected, 1);
        // Landed against the clamped (final) state, which is still profitable here.
        assert_eq!(r.landed_profitable + r.missed_latency, 1);
    }

    #[test]
    fn skewed_pdf_first_order_dominance() {
        // Shifting probability mass to higher K cannot raise expected profit.
        let (uni, cycles) = universe();
        let stream = decaying_stream();
        let good = simulate(&uni, &cycles, &stream, &cfg("0.8,0.2")).realized_profit;
        let bad = simulate(&uni, &cycles, &stream, &cfg("0.2,0.8")).realized_profit;
        assert!(good >= bad, "good={good} bad={bad}");
    }
}
