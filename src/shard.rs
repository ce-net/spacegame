//! Sharding & placement — the **distribution / concurrency / latency** core of spacegame.
//!
//! Spacegame's galaxy is partitioned into a grid of fixed-size **sectors**. Each sector is an
//! independent authoritative simulation cell ([`crate::sim::Sim`]). Unlike a single-room game,
//! distinct *regions of space* are simulated by *distinct nodes*, so the world scales horizontally:
//! more players in more places ⇒ more sectors ⇒ load spreads across the mesh, each sector a separate
//! concurrent cell.
//!
//! This module is the **pure decision layer** that decides *which node owns which sector*:
//!
//! * **Sharding (concurrency):** [`shard_for`] maps a sector id to a host with a **rendezvous hash**
//!   (highest-random-weight, HRW). Every node computes the same owner for a `(sector, candidate-set)`
//!   with no coordinator, so the galaxy's sectors spread evenly across the mesh and only `~1/N` of
//!   sectors move when a node joins or leaves — the substrate for running *many independent sector
//!   cells at once*.
//!
//! * **Latency-aware placement:** when several hosts are capable, [`best_host`] prefers the
//!   **lowest-latency** one, folding the node's measured latency graph ([`ce_rs::CeClient::netgraph`])
//!   and capacity atlas ([`ce_rs::CeClient::atlas`]) into one score. A client, symmetrically, uses
//!   [`nearest_host`] to pick the sector host closest to *it*.
//!
//! * **Interest management (latency / bandwidth):** [`SectorId::neighbors`] yields the player's
//!   sector and its eight neighbours — the only sectors a client subscribes to and renders. A client
//!   never receives state for the far side of the galaxy, so per-client bandwidth stays bounded no
//!   matter how big the world gets.
//!
//! Everything here is pure (no mesh I/O) so the host-selection math is unit-tested without a running
//! node. The thin I/O that gathers atlas/netgraph and calls `mesh_deploy` lives in
//! [`crate::director`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A sector's grid coordinate in the galaxy. `(0, 0)` is the origin sector; sectors tile infinitely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct SectorId {
    pub sx: i32,
    pub sy: i32,
}

impl SectorId {
    pub fn new(sx: i32, sy: i32) -> Self {
        SectorId { sx, sy }
    }

    /// The sector containing a galaxy-world point. Sectors are [`crate::sim::SECTOR_SIZE`] wide.
    pub fn containing(world_x: f32, world_y: f32) -> Self {
        SectorId {
            sx: (world_x / crate::sim::SECTOR_SIZE).floor() as i32,
            sy: (world_y / crate::sim::SECTOR_SIZE).floor() as i32,
        }
    }

    /// Stable string id used in topic names and the DHT service ("`sx_sy`", negatives as `n`).
    pub fn token(&self) -> String {
        fn part(v: i32) -> String {
            if v < 0 { format!("n{}", -v) } else { v.to_string() }
        }
        format!("{}_{}", part(self.sx), part(self.sy))
    }

    /// Parse a token produced by [`token`](Self::token). Returns `None` on malformed input.
    pub fn parse(token: &str) -> Option<SectorId> {
        let (a, b) = token.split_once('_')?;
        fn part(s: &str) -> Option<i32> {
            if let Some(rest) = s.strip_prefix('n') {
                rest.parse::<i32>().ok().map(|v| -v)
            } else {
                s.parse::<i32>().ok()
            }
        }
        Some(SectorId { sx: part(a)?, sy: part(b)? })
    }

    /// This sector plus its eight neighbours — the **interest set** a client at this sector
    /// subscribes to. Ordered deterministically (row-major) so tests and clients agree.
    pub fn neighbors(&self) -> Vec<SectorId> {
        let mut v = Vec::with_capacity(9);
        for dy in -1..=1 {
            for dx in -1..=1 {
                v.push(SectorId { sx: self.sx + dx, sy: self.sy + dy });
            }
        }
        v
    }
}

/// FNV-1a/64 over bytes — wide rendezvous-weight space, consistent with the sim's 32-bit field hash.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The rendezvous weight of placing `sector` on `node_id`. The node with the highest weight owns the
/// sector. Hashes are mixed (not string-concatenated) to stay cheap and avoid per-call allocation.
pub fn rendezvous_weight(sector: &str, node_id: &str) -> u64 {
    let a = fnv1a64(sector.as_bytes());
    let b = fnv1a64(node_id.as_bytes());
    let mut h = a ^ b.wrapping_mul(0x9e3779b97f4a7c15);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h
}

/// Rendezvous-hash shard assignment: of `candidates`, the node id that should host `sector`.
/// Deterministic and coordinator-free — every node computes the same answer, so sectors shard evenly
/// and only `~1/N` move when the candidate set changes by one node. `None` only if `candidates` is
/// empty.
pub fn shard_for<'a>(sector: &str, candidates: &'a [String]) -> Option<&'a str> {
    candidates
        .iter()
        .max_by_key(|c| rendezvous_weight(sector, c))
        .map(String::as_str)
}

/// A candidate host with the inputs that decide placement.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCandidate {
    pub node_id: String,
    pub cpu_cores: u32,
    pub mem_mb: u32,
    pub running_jobs: u32,
    /// Round-trip latency in ms, if the local node has a measurement. `None` = unknown (a moderate
    /// penalty, not exclusion).
    pub rtt_ms: Option<f64>,
    /// True if the node advertises it can host a sector cell.
    pub can_host: bool,
}

impl HostCandidate {
    pub fn is_capable(&self, min_cores: u32, min_mem_mb: u32) -> bool {
        self.can_host && self.cpu_cores >= min_cores && self.mem_mb >= min_mem_mb
    }

    /// A single placement score — **higher is better**. Lower latency dominates (it is what players
    /// feel), then spare capacity, then lighter load.
    pub fn score(&self) -> f64 {
        let rtt = self.rtt_ms.unwrap_or(120.0).max(0.0);
        let latency = 1000.0 / (40.0 + rtt);
        let capacity = 1.0 + (self.cpu_cores as f64).min(32.0) / 32.0;
        let idle = 1.0 / (1.0 + self.running_jobs as f64 * 0.25);
        latency * capacity * idle
    }
}

/// Pick the best host to **place** a sector cell on: among capable candidates, the highest
/// [`HostCandidate::score`] (latency-first). Ties broken by the lexicographically smallest node id,
/// so the choice is deterministic and every node agrees. `None` if no candidate is capable.
pub fn best_host(candidates: &[HostCandidate], min_cores: u32, min_mem_mb: u32) -> Option<&HostCandidate> {
    candidates
        .iter()
        .filter(|c| c.is_capable(min_cores, min_mem_mb))
        .max_by(|a, b| {
            a.score()
                .total_cmp(&b.score())
                .then_with(|| b.node_id.cmp(&a.node_id))
        })
}

/// A client's view: pick the **nearest** sector host to render from, given the node ids currently
/// advertising the sector and the client's measured latency to each. Closest measured wins; hosts
/// with no measurement are considered only if nothing is measured (then the smallest id, for
/// determinism). `None` if `hosts` is empty.
pub fn nearest_host<'a>(hosts: &'a [String], rtt: &HashMap<String, f64>) -> Option<&'a str> {
    if hosts.is_empty() {
        return None;
    }
    let measured = hosts
        .iter()
        .filter_map(|h| rtt.get(h).map(|r| (h, *r)))
        .min_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    if let Some((h, _)) = measured {
        return Some(h.as_str());
    }
    hosts.iter().min().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, cores: u32, mem: u32, jobs: u32, rtt: Option<f64>, host: bool) -> HostCandidate {
        HostCandidate { node_id: id.into(), cpu_cores: cores, mem_mb: mem, running_jobs: jobs, rtt_ms: rtt, can_host: host }
    }

    #[test]
    fn sector_token_roundtrips_including_negatives() {
        for &(sx, sy) in &[(0, 0), (3, 7), (-2, 5), (4, -9), (-11, -1)] {
            let id = SectorId::new(sx, sy);
            let tok = id.token();
            assert_eq!(SectorId::parse(&tok), Some(id), "token {tok}");
        }
        assert!(SectorId::parse("garbage").is_none());
    }

    #[test]
    fn containing_maps_world_point_to_sector() {
        let s = crate::sim::SECTOR_SIZE;
        assert_eq!(SectorId::containing(10.0, 10.0), SectorId::new(0, 0));
        assert_eq!(SectorId::containing(s + 5.0, 2.0 * s + 5.0), SectorId::new(1, 2));
        assert_eq!(SectorId::containing(-5.0, -5.0), SectorId::new(-1, -1));
    }

    #[test]
    fn neighbors_is_the_nine_cell_interest_set() {
        let n = SectorId::new(2, 3).neighbors();
        assert_eq!(n.len(), 9);
        assert!(n.contains(&SectorId::new(2, 3)));
        assert!(n.contains(&SectorId::new(1, 2)));
        assert!(n.contains(&SectorId::new(3, 4)));
        // Far sectors are NOT in the interest set (interest management bounds client bandwidth).
        assert!(!n.contains(&SectorId::new(5, 5)));
    }

    #[test]
    fn rendezvous_is_deterministic_and_id_sensitive() {
        assert_eq!(rendezvous_weight("0_0", "nodeA"), rendezvous_weight("0_0", "nodeA"));
        assert_ne!(rendezvous_weight("0_0", "nodeA"), rendezvous_weight("0_0", "nodeB"));
        assert_ne!(rendezvous_weight("0_0", "nodeA"), rendezvous_weight("1_0", "nodeA"));
    }

    #[test]
    fn shard_picks_a_stable_winner() {
        let nodes: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        let w1 = shard_for("3_4", &nodes).unwrap().to_string();
        assert_eq!(shard_for("3_4", &nodes), Some(w1.as_str()));
        assert_eq!(shard_for("3_4", &[]), None);
    }

    #[test]
    fn sectors_spread_across_the_mesh() {
        // Many sectors over a handful of nodes should use most nodes — the concurrency story:
        // independent sector cells distributed across the mesh, not one hot host.
        let nodes: Vec<String> = (0..6).map(|i| format!("node{i}")).collect();
        let mut used = std::collections::HashSet::new();
        for sx in 0..15 {
            for sy in 0..15 {
                let tok = SectorId::new(sx, sy).token();
                used.insert(shard_for(&tok, &nodes).unwrap().to_string());
            }
        }
        assert!(used.len() >= 4, "sectors should spread across most nodes, used {}", used.len());
    }

    #[test]
    fn shard_is_minimally_disruptive_on_node_removal() {
        // Removing one node only reassigns the sectors that lived on it — HRW's whole point, vs a
        // modulo hash that reshuffles almost everything.
        let full: Vec<String> = (0..8).map(|i| format!("n{i}")).collect();
        let removed = "n3";
        let reduced: Vec<String> = full.iter().filter(|n| *n != removed).cloned().collect();
        let mut moved = 0;
        let total = 400;
        for i in 0..total {
            let tok = SectorId::new(i % 20, i / 20).token();
            let before = shard_for(&tok, &full).unwrap().to_string();
            let after = shard_for(&tok, &reduced).unwrap().to_string();
            if before != after {
                moved += 1;
                assert_eq!(before, removed, "only sectors on the removed node should move");
            }
        }
        assert!(moved < total / 3, "HRW should move ~1/N sectors, moved {moved}/{total}");
    }

    #[test]
    fn best_host_prefers_low_latency_capable_node() {
        let cands = vec![
            cand("fardc", 32, 64000, 0, Some(220.0), true),
            cand("nearby", 4, 8000, 1, Some(15.0), true),
            cand("tiny", 1, 256, 0, Some(10.0), true),
        ];
        let best = best_host(&cands, 2, 1000).unwrap();
        assert_eq!(best.node_id, "nearby");
    }

    #[test]
    fn best_host_excludes_incapable_and_non_hosts() {
        let cands = vec![
            cand("nothost", 32, 64000, 0, Some(5.0), false),
            cand("ok", 2, 2000, 0, Some(80.0), true),
        ];
        assert_eq!(best_host(&cands, 2, 1000).unwrap().node_id, "ok");
        let none = vec![cand("x", 1, 100, 0, Some(1.0), true)];
        assert!(best_host(&none, 4, 8000).is_none());
    }

    #[test]
    fn best_host_tie_breaks_deterministically() {
        let cands = vec![
            cand("zzz", 4, 4000, 0, Some(50.0), true),
            cand("aaa", 4, 4000, 0, Some(50.0), true),
        ];
        assert_eq!(best_host(&cands, 1, 1000).unwrap().node_id, "aaa");
    }

    #[test]
    fn nearest_host_picks_closest_measured_else_smallest() {
        let hosts: Vec<String> = ["h1", "h2", "h3"].iter().map(|s| s.to_string()).collect();
        let mut rtt = HashMap::new();
        rtt.insert("h1".to_string(), 90.0);
        rtt.insert("h2".to_string(), 12.0);
        rtt.insert("h3".to_string(), 45.0);
        assert_eq!(nearest_host(&hosts, &rtt), Some("h2"));

        let unmeasured: Vec<String> = ["zeta", "alpha"].iter().map(|s| s.to_string()).collect();
        assert_eq!(nearest_host(&unmeasured, &HashMap::new()), Some("alpha"));
        assert_eq!(nearest_host(&[], &HashMap::new()), None);
    }
}
