//! The pool graph: tokens are nodes, pools are (bidirectional) edges. Arb paths
//! are cycles `base → … → base` (USDC → … → USDC). This module builds the graph
//! from a [`PoolBook`] and enumerates candidate cycles to feed the simulator /
//! scanner.

use std::collections::{HashMap, HashSet};

use crate::book::PoolBook;
use crate::types::Address;

/// One directed traversal of a pool: swap `from` → `to` through `pool`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub pool: Address,
    pub kind: String,
    pub from: Address,
    pub to: Address,
    pub fee_bps: Option<u32>,
}

pub struct Graph {
    adj: HashMap<Address, Vec<Edge>>,
}

impl Graph {
    /// Build adjacency from a pool book (each pool yields both directions).
    pub fn from_book(book: &PoolBook) -> Self {
        let mut adj: HashMap<Address, Vec<Edge>> = HashMap::new();
        for p in &book.pools {
            adj.entry(p.token0).or_default().push(Edge {
                pool: p.address,
                kind: p.kind.clone(),
                from: p.token0,
                to: p.token1,
                fee_bps: p.fee_bps,
            });
            adj.entry(p.token1).or_default().push(Edge {
                pool: p.address,
                kind: p.kind.clone(),
                from: p.token1,
                to: p.token0,
                fee_bps: p.fee_bps,
            });
        }
        Self { adj }
    }

    pub fn token_count(&self) -> usize {
        self.adj.len()
    }

    pub fn neighbors(&self, token: Address) -> &[Edge] {
        self.adj.get(&token).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Enumerate closed cycles `base → … → base` with 2..=`max_hops` edges,
    /// distinct intermediate tokens, and no pool reused within a cycle.
    pub fn cycles(&self, base: Address, max_hops: usize) -> Vec<Vec<Edge>> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        visited.insert(base);
        self.dfs(base, base, max_hops, &mut path, &mut visited, &mut out);
        out
    }

    fn dfs(
        &self,
        base: Address,
        cur: Address,
        max_hops: usize,
        path: &mut Vec<Edge>,
        visited: &mut HashSet<Address>,
        out: &mut Vec<Vec<Edge>>,
    ) {
        let len = path.len(); // edges so far
        for e in self.neighbors(cur) {
            if path.iter().any(|x| x.pool == e.pool) {
                continue; // don't reuse a pool in one cycle
            }
            if e.to == base {
                // closing edge -> cycle of length len+1
                if len + 1 >= 2 && len + 1 <= max_hops {
                    let mut c = path.clone();
                    c.push(e.clone());
                    out.push(c);
                }
                continue;
            }
            // extend only if there's room to still close afterwards
            if len + 2 > max_hops || visited.contains(&e.to) {
                continue;
            }
            visited.insert(e.to);
            path.push(e.clone());
            self.dfs(base, e.to, max_hops, path, visited, out);
            path.pop();
            visited.remove(&e.to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::{PoolInfo, TokenInfo};
    use std::collections::BTreeMap;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn book(pools: Vec<PoolInfo>) -> PoolBook {
        let mut tokens = BTreeMap::new();
        tokens.insert("USDC".into(), TokenInfo { address: addr(1), decimals: 6 });
        tokens.insert("WETH".into(), TokenInfo { address: addr(2), decimals: 18 });
        tokens.insert("DAI".into(), TokenInfo { address: addr(3), decimals: 18 });
        PoolBook { chain: "base".into(), tokens, pools }
    }

    fn pool(a: u8, t0: u8, t1: u8) -> PoolInfo {
        PoolInfo {
            address: addr(a),
            exchange: "x".into(),
            kind: "uniswap_v2".into(),
            token0: addr(t0),
            token1: addr(t1),
            fee_bps: Some(30),
            discovered_block: None,
        }
    }

    #[test]
    fn finds_two_hop_cycle_between_parallel_pools() {
        // Two USDC/WETH pools => USDC -> WETH (A) -> USDC (B), and the reverse.
        let g = Graph::from_book(&book(vec![pool(10, 1, 2), pool(11, 1, 2)]));
        let cycles = g.cycles(addr(1), 2);
        assert_eq!(cycles.len(), 2, "expected 2 directed 2-hop cycles");
        for c in &cycles {
            assert_eq!(c.len(), 2);
            assert_eq!(c[0].from, addr(1));
            assert_eq!(c.last().unwrap().to, addr(1));
            assert_ne!(c[0].pool, c[1].pool); // distinct pools
        }
    }

    #[test]
    fn finds_triangular_cycle() {
        // USDC-WETH, WETH-DAI, DAI-USDC => a 3-hop triangle (both directions).
        let g = Graph::from_book(&book(vec![pool(10, 1, 2), pool(11, 2, 3), pool(12, 3, 1)]));
        let tri: Vec<_> = g.cycles(addr(1), 3).into_iter().filter(|c| c.len() == 3).collect();
        assert_eq!(tri.len(), 2);
    }

    #[test]
    fn no_cycle_without_return_path() {
        // Single USDC/WETH pool can't form a cycle (would reuse the pool).
        let g = Graph::from_book(&book(vec![pool(10, 1, 2)]));
        assert!(g.cycles(addr(1), 3).is_empty());
    }

    #[test]
    fn respects_max_hops() {
        let g = Graph::from_book(&book(vec![pool(10, 1, 2), pool(11, 2, 3), pool(12, 3, 1)]));
        // max_hops 2 can't fit the triangle (needs 3 edges).
        assert!(g.cycles(addr(1), 2).is_empty());
    }
}
