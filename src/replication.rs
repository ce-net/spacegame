//! Proximity-replica fault tolerance — the system spacegame exists to harden: **critical real-time
//! state survives a device vanishing at any instant**, because the devices of the players standing
//! next to you already hold a high-precision replica of your shared corner of the world.
//!
//! In a compute mesh, any node can disappear without warning — a phone locks, a laptop sleeps, a home
//! connection drops. For a region of space that several players are fighting over, losing the
//! authoritative host must *not* lose the fight. So we keep a **replication constraint**: at least `k`
//! high-precision replicas of a region exist at all times, and they are placed on the **nodes of the
//! players who are physically close in-game** — they are already simulating that region at
//! [`Lod::High`](crate::physics::Lod), so a replica is nearly free for them and instantly warm.
//!
//! When a replica drops:
//! 1. failure is detected from missing heartbeats ([`ReplicaSet::expire`]);
//! 2. if it was the **primary** (authoritative host), the best healthy backup is **promoted**
//!    deterministically — every node computes the same winner, so there is no split brain;
//! 3. the **high-precision map is copied to the next-best node** ([`ReplicationPlan::copy_to`]) to
//!    restore the `k` constraint — typically the nearest remaining player's device, else a capable
//!    mesh node.
//!
//! All of that is the *pure decision layer* here ([`ReplicaSet::plan`]): deterministic, unit-tested, no
//! mesh. The actual heartbeat publish, snapshot copy and promotion handshake are thin `ce-rs` calls in
//! [`crate::director`]. The replica payload itself is the content-addressed [`SectorSnapshot`] plus a
//! live delta stream, so "copy the high-precision map to the next best node" is a CID fetch from the
//! nearest holder — every node that has the snapshot is a CDN edge for it.
//!
//! ### Why this is also the GPU / cross-compatibility forcing function
//!
//! For a backup to *instantly* take over, its replica must evolve **bit-identically** to the primary.
//! That forces the simulation to be deterministic across heterogeneous hardware — the same result
//! whether a node runs the physics kernels on a CPU or a GPU. The physics state is laid out for that
//! (see [`crate::physics`]): plain `Copy` arrays and fixed-iteration kernels that produce the same
//! numbers on either backend. Fault tolerance and GPU cross-compatibility are the same requirement
//! viewed from two sides: *every replica must agree, on any device.*

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The replication constraint for a region: the total number of high-precision replicas (including the
/// primary) that must exist. `k = 3` means one authoritative host + two hot standbys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationConstraint {
    pub k: usize,
}

impl Default for ReplicationConstraint {
    fn default() -> Self {
        ReplicationConstraint { k: 3 }
    }
}

/// A node that *could* hold a replica of the region — usually a nearby player's device, possibly a
/// general mesh node. The fields are exactly what the placement decision needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplicaCandidate {
    pub node_id: String,
    /// Measured round-trip latency from the primary, if known.
    pub rtt_ms: Option<f64>,
    /// How close this node's player is to the region **in game**. Smaller = closer; closer players
    /// already simulate the region at high precision, so they are the cheapest, warmest replicas.
    pub in_game_dist: f32,
    /// Spare cores the node advertises (capacity headroom).
    pub free_cores: u32,
    /// Is the node currently reachable?
    pub alive: bool,
}

impl ReplicaCandidate {
    /// Suitability score — **higher is better**. In-game proximity dominates (a close player's device
    /// is the ideal replica), then low latency, then spare capacity. A dead node scores nothing.
    pub fn score(&self) -> f64 {
        if !self.alive {
            return f64::NEG_INFINITY;
        }
        let proximity = 1000.0 / (1.0 + self.in_game_dist as f64); // closer => much higher
        let rtt = self.rtt_ms.unwrap_or(120.0).max(0.0);
        let latency = 200.0 / (20.0 + rtt);
        let capacity = 1.0 + (self.free_cores as f64).min(16.0) / 16.0;
        proximity * 4.0 + latency + capacity
    }
}

/// A member's role in the replica set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaRole {
    /// The authoritative host that owns the live simulation.
    Primary,
    /// A hot standby holding a high-precision replica, ready to take over.
    Backup,
}

/// One member of a region's replica set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaMember {
    pub node_id: String,
    pub role: ReplicaRole,
    /// Last tick we heard a heartbeat from this member.
    pub last_seen_tick: u64,
    /// Health flag, updated by [`ReplicaSet::expire`].
    pub healthy: bool,
}

/// The live replica set for one region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaSet {
    pub region: String,
    pub constraint: ReplicationConstraint,
    pub members: Vec<ReplicaMember>,
}

/// What to do this round to satisfy the constraint — computed deterministically by [`ReplicaSet::plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationPlan {
    /// A backup to promote to primary (the old primary is gone). `None` if the primary is healthy.
    pub promote: Option<String>,
    /// Nodes to copy the high-precision map to, restoring `k` healthy replicas (best candidates first).
    pub copy_to: Vec<String>,
    /// Dead members to retire from the set.
    pub drop: Vec<String>,
}

impl ReplicaSet {
    /// A new set with `primary` as the only (healthy) member.
    pub fn new(region: &str, primary: &str, constraint: ReplicationConstraint, now: u64) -> Self {
        ReplicaSet {
            region: region.to_string(),
            constraint,
            members: vec![ReplicaMember {
                node_id: primary.to_string(),
                role: ReplicaRole::Primary,
                last_seen_tick: now,
                healthy: true,
            }],
        }
    }

    pub fn primary(&self) -> Option<&ReplicaMember> {
        self.members.iter().find(|m| m.role == ReplicaRole::Primary)
    }

    pub fn healthy_count(&self) -> usize {
        self.members.iter().filter(|m| m.healthy).count()
    }

    pub fn contains(&self, node: &str) -> bool {
        self.members.iter().any(|m| m.node_id == node)
    }

    /// Record a heartbeat from a member (marks it seen + healthy). A heartbeat from a node not in the
    /// set is ignored (it joins only via a [`ReplicationPlan::copy_to`] handshake).
    pub fn observe(&mut self, node: &str, now: u64) {
        if let Some(m) = self.members.iter_mut().find(|m| m.node_id == node) {
            m.last_seen_tick = now;
            m.healthy = true;
        }
    }

    /// Add a member that has finished receiving the high-precision map (the other side of `copy_to`).
    pub fn admit(&mut self, node: &str, now: u64) {
        if !self.contains(node) {
            self.members.push(ReplicaMember {
                node_id: node.to_string(),
                role: ReplicaRole::Backup,
                last_seen_tick: now,
                healthy: true,
            });
        }
    }

    /// Mark any member silent for longer than `timeout` ticks as unhealthy (failure detection).
    pub fn expire(&mut self, now: u64, timeout: u64) {
        for m in self.members.iter_mut() {
            if now.saturating_sub(m.last_seen_tick) > timeout {
                m.healthy = false;
            }
        }
    }

    /// Compute the deterministic recovery plan: who to promote, where to copy the map, what to drop.
    /// Every node runs the same function over the same set + candidate view and gets the same answer,
    /// so promotion has no split brain. `candidates` are potential new replica holders (nearby players'
    /// nodes first). Call [`expire`](Self::expire) before this.
    pub fn plan(&self, candidates: &[ReplicaCandidate]) -> ReplicationPlan {
        let mut plan = ReplicationPlan { promote: None, copy_to: Vec::new(), drop: Vec::new() };

        // Dead members are retired.
        for m in &self.members {
            if !m.healthy {
                plan.drop.push(m.node_id.clone());
            }
        }

        // Promote if the primary is gone: choose the best healthy backup deterministically (highest
        // candidate score, then lexicographically smallest id as a stable tiebreak).
        let primary_ok = self.primary().map(|p| p.healthy).unwrap_or(false);
        if !primary_ok {
            let cand_score: HashMap<&str, f64> =
                candidates.iter().map(|c| (c.node_id.as_str(), c.score())).collect();
            let best = self
                .members
                .iter()
                .filter(|m| m.healthy && m.role == ReplicaRole::Backup)
                .max_by(|a, b| {
                    let sa = cand_score.get(a.node_id.as_str()).copied().unwrap_or(0.0);
                    let sb = cand_score.get(b.node_id.as_str()).copied().unwrap_or(0.0);
                    sa.total_cmp(&sb).then_with(|| b.node_id.cmp(&a.node_id))
                });
            plan.promote = best.map(|m| m.node_id.clone());
        }

        // Restore the constraint: count replicas that will be healthy after dropping the dead, then
        // copy the map to the best fresh candidates until we reach k.
        let surviving = self.healthy_count();
        let need = self.constraint.k.saturating_sub(surviving);
        if need > 0 {
            let mut fresh: Vec<&ReplicaCandidate> = candidates
                .iter()
                .filter(|c| c.alive && !self.contains(&c.node_id))
                .collect();
            // Best first; stable by id on ties.
            fresh.sort_by(|a, b| b.score().total_cmp(&a.score()).then_with(|| a.node_id.cmp(&b.node_id)));
            plan.copy_to = fresh.into_iter().take(need).map(|c| c.node_id.clone()).collect();
        }

        plan
    }

    /// Apply a plan to the in-memory set (promotion + admit fresh backups + retire dead). The host
    /// performs the side effects (snapshot copy, role handshake) via [`crate::director`]; this keeps
    /// the local view consistent.
    pub fn apply(&mut self, plan: &ReplicationPlan, now: u64) {
        // Retire dead members.
        self.members.retain(|m| !plan.drop.contains(&m.node_id));
        // Promote.
        if let Some(new_primary) = &plan.promote {
            for m in self.members.iter_mut() {
                m.role = if &m.node_id == new_primary { ReplicaRole::Primary } else { ReplicaRole::Backup };
            }
        }
        // Admit fresh backups (they will heartbeat once the copy completes).
        for node in &plan.copy_to {
            self.admit(node, now);
        }
    }
}

// ----------------------------------------------------------------------------------------------
// ANTI-CHEAT / REDUNDANCY: replicas simulate the same sector and must AGREE on its state hash.
// ----------------------------------------------------------------------------------------------

/// One replica's claim about the authoritative state at a given tick — the deterministic
/// [`state_hash`](crate::sim::Sim::state_hash) it computed. Replicas publish these; comparing them is
/// how the mesh tells an honest simulation from a host that cheated (teleported a ship, faked a kill,
/// minted resources). A cheat changes the hash, so the cheater is the odd one out.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StateProof {
    pub region: String,
    pub node: String,
    pub tick: u64,
    pub hash: u64,
}

/// The outcome of comparing replicas' [`StateProof`]s for one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreement {
    /// The hash the majority of replicas computed (the accepted truth), if a majority exists.
    pub quorum_hash: Option<u64>,
    /// Nodes whose hash matched the quorum.
    pub agree: Vec<String>,
    /// Nodes whose hash disagreed with the quorum — faulty or cheating, to be re-synced or excluded.
    pub dissent: Vec<String>,
    /// True if a strict majority of replicas agreed (the result can be trusted).
    pub has_quorum: bool,
}

/// What a replica should DO this checkpoint, derived from an [`Agreement`] for a given node. This turns
/// "the replicas disagree" from a log line into a convergent action — the heart of state merging: an
/// out-voted replica (a cheater, or one that merely desynced) adopts the agreed state, so the world
/// stays single-valued without any trusted central server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Fewer than two replicas, or no strict majority — cannot safely act this checkpoint.
    Inconclusive,
    /// This node is part of the quorum (or all agree) — nothing to do.
    Agreed,
    /// This node is out-voted by a quorum that agreed on this state hash — it must MERGE: re-sync its
    /// sim to the snapshot whose [`crate::director::SnapshotAnnounce::hash`] equals this value.
    ResyncTo(u64),
    /// This node is IN the quorum; these other node(s) diverged — they are the cheat/fault suspects.
    PeersDiverged(Vec<String>),
}

impl Agreement {
    /// The action `this_node` should take given this agreement. See [`Verdict`].
    pub fn verdict(&self, this_node: &str) -> Verdict {
        if !self.has_quorum {
            return Verdict::Inconclusive;
        }
        if self.dissent.is_empty() {
            return Verdict::Agreed;
        }
        if self.dissent.iter().any(|d| d == this_node) {
            // We lost the vote — merge to the agreed truth. quorum_hash is Some when has_quorum.
            match self.quorum_hash {
                Some(h) => Verdict::ResyncTo(h),
                None => Verdict::Inconclusive,
            }
        } else {
            Verdict::PeersDiverged(self.dissent.clone())
        }
    }
}

/// Compare the replicas' state proofs for a single tick and decide the agreed truth. The most common
/// hash wins (ties broken by the lowest hash value, deterministically); every replica that computed a
/// different hash is a **dissenter** — a host that is faulty or cheating. `has_quorum` is true only if
/// the winning hash has a strict majority, so a single honest node can never be overruled by one liar
/// and a 1-vs-1 split is reported as no-quorum rather than a false accusation.
pub fn agree(proofs: &[StateProof]) -> Agreement {
    use std::collections::BTreeMap;
    if proofs.is_empty() {
        return Agreement { quorum_hash: None, agree: vec![], dissent: vec![], has_quorum: false };
    }
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    for p in proofs {
        *counts.entry(p.hash).or_insert(0) += 1;
    }
    // Winner: highest count, then lowest hash (BTreeMap iterates hashes ascending, so this is stable).
    let (winner, top) = counts.iter().fold((0u64, 0usize), |(bh, bc), (&hash, &c)| {
        if c > bc { (hash, c) } else { (bh, bc) }
    });
    let total = proofs.len();
    let has_quorum = top * 2 > total;
    let mut agree_nodes = Vec::new();
    let mut dissent_nodes = Vec::new();
    for p in proofs {
        if p.hash == winner {
            agree_nodes.push(p.node.clone());
        } else {
            dissent_nodes.push(p.node.clone());
        }
    }
    agree_nodes.sort();
    dissent_nodes.sort();
    Agreement { quorum_hash: Some(winner), agree: agree_nodes, dissent: dissent_nodes, has_quorum }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, dist: f32, rtt: f64, alive: bool) -> ReplicaCandidate {
        ReplicaCandidate { node_id: id.into(), rtt_ms: Some(rtt), in_game_dist: dist, free_cores: 4, alive }
    }

    #[test]
    fn closer_player_node_scores_higher() {
        let near = cand("near", 100.0, 30.0, true);
        let far = cand("far", 5000.0, 10.0, true);
        assert!(near.score() > far.score(), "in-game proximity beats raw latency for replica placement");
        let dead = cand("dead", 1.0, 1.0, false);
        assert!(dead.score().is_infinite() && dead.score() < 0.0);
    }

    #[test]
    fn plan_copies_to_restore_k_replicas() {
        let now = 100;
        let mut set = ReplicaSet::new("3_4", "host", ReplicationConstraint { k: 3 }, now);
        // Only the primary exists -> need 2 more.
        let candidates = vec![
            cand("playerA", 200.0, 25.0, true),
            cand("playerB", 400.0, 40.0, true),
            cand("playerC", 9000.0, 15.0, true),
        ];
        let plan = set.plan(&candidates);
        assert!(plan.promote.is_none(), "primary healthy, no promotion");
        assert_eq!(plan.copy_to.len(), 2, "copy the map to two nearby nodes to reach k=3");
        assert_eq!(plan.copy_to[0], "playerA", "nearest player's node first");
        set.apply(&plan, now);
        assert_eq!(set.members.len(), 3);
    }

    #[test]
    fn primary_loss_promotes_a_backup_deterministically() {
        let mut set = ReplicaSet::new("0_0", "host", ReplicationConstraint { k: 3 }, 0);
        set.admit("backupA", 0);
        set.admit("backupB", 0);
        // Fresh heartbeats from the backups, but not the primary.
        set.observe("backupA", 18);
        set.observe("backupB", 18);
        set.expire(20, 5); // primary last seen at 0 -> unhealthy; backups (t=18) stay healthy
        let candidates = vec![
            cand("backupA", 150.0, 30.0, true), // closer => higher score => promoted
            cand("backupB", 800.0, 10.0, true),
        ];
        let plan = set.plan(&candidates);
        assert_eq!(plan.promote.as_deref(), Some("backupA"), "the best healthy backup takes over");
        assert!(plan.drop.contains(&"host".to_string()), "the dead primary is retired");

        // Re-running on another node with the same inputs yields the same decision (no split brain).
        let plan2 = set.plan(&candidates);
        assert_eq!(plan, plan2);
    }

    #[test]
    fn re_replicates_to_next_best_after_a_drop() {
        let mut set = ReplicaSet::new("1_1", "host", ReplicationConstraint { k: 3 }, 0);
        set.admit("a", 0);
        set.admit("b", 0);
        // 'a' goes silent.
        set.observe("host", 30);
        set.observe("b", 30);
        set.expire(40, 5);
        let candidates = vec![
            cand("host", 50.0, 5.0, true),
            cand("b", 300.0, 20.0, true),
            cand("c", 500.0, 25.0, true), // the next-best fresh node to copy the map to
        ];
        let plan = set.plan(&candidates);
        assert!(plan.drop.contains(&"a".to_string()), "the dropped replica is retired");
        assert_eq!(plan.copy_to, vec!["c".to_string()], "the high-precision map is copied to the next best node");
    }

    #[test]
    fn healthy_set_needs_no_action() {
        let mut set = ReplicaSet::new("2_2", "host", ReplicationConstraint { k: 2 }, 0);
        set.admit("a", 0);
        set.observe("host", 100);
        set.observe("a", 100);
        set.expire(101, 50);
        let plan = set.plan(&[cand("host", 10.0, 5.0, true), cand("a", 20.0, 6.0, true)]);
        assert!(plan.promote.is_none() && plan.copy_to.is_empty() && plan.drop.is_empty());
    }

    fn proof(node: &str, hash: u64) -> StateProof {
        StateProof { region: "0_0".into(), node: node.into(), tick: 42, hash }
    }

    #[test]
    fn honest_majority_outvotes_a_cheater() {
        // Three replicas agree on hash 7; one cheater reports 9. The quorum is 7 and the cheater is
        // the sole dissenter.
        let a = agree(&[proof("a", 7), proof("b", 7), proof("c", 7), proof("cheat", 9)]);
        assert_eq!(a.quorum_hash, Some(7));
        assert!(a.has_quorum);
        assert_eq!(a.agree, vec!["a", "b", "c"]);
        assert_eq!(a.dissent, vec!["cheat"]);
    }

    #[test]
    fn a_one_to_one_split_is_no_quorum_not_a_false_accusation() {
        // Two replicas, two different hashes: we must NOT pick a "cheater" — there is no majority, so
        // a single liar cannot frame a single honest node.
        let a = agree(&[proof("a", 1), proof("b", 2)]);
        assert!(!a.has_quorum, "a 1-1 split has no quorum");
    }

    #[test]
    fn unanimous_agreement_has_no_dissent() {
        let a = agree(&[proof("a", 5), proof("b", 5)]);
        assert!(a.has_quorum);
        assert!(a.dissent.is_empty());
        assert_eq!(a.quorum_hash, Some(5));
    }

    #[test]
    fn verdict_tells_an_outvoted_node_to_merge_to_the_quorum() {
        // a,b,c agree on 7; the cheater reports 9. The cheater's verdict is RESYNC to hash 7 (merge);
        // an honest node's verdict is that the cheater diverged; a quorum node otherwise does nothing.
        let a = agree(&[proof("a", 7), proof("b", 7), proof("c", 7), proof("cheat", 9)]);
        assert_eq!(a.verdict("cheat"), Verdict::ResyncTo(7), "the out-voted node merges to the agreed truth");
        assert_eq!(a.verdict("a"), Verdict::PeersDiverged(vec!["cheat".into()]), "a quorum node flags the suspect");
        let unanimous = agree(&[proof("a", 5), proof("b", 5)]);
        assert_eq!(unanimous.verdict("a"), Verdict::Agreed);
        // No strict majority -> never act (can't tell truth from a lie in a 1-1 split).
        let split = agree(&[proof("a", 1), proof("b", 2)]);
        assert_eq!(split.verdict("a"), Verdict::Inconclusive);
        assert_eq!(split.verdict("b"), Verdict::Inconclusive);
    }

    #[test]
    fn empty_proofs_is_inconclusive() {
        let a = agree(&[]);
        assert!(!a.has_quorum && a.quorum_hash.is_none());
    }
}
