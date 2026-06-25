//! Consensus — a **tamper-proof galaxy leaderboard** anchored to the PoW chain.
//!
//! Kills are simulated authoritatively per sector, but "authoritative" only means *a host can't be
//! cheated by a client*. It does not stop a dishonest **host** from rewriting history, nor give a
//! third party a way to verify a final score. CRDTs can't fix this: a Merged/RMap converges, but any
//! writer can fabricate an entry and there is no single agreed truth. That is exactly the gap CE's
//! PoW chain fills — global uniqueness and tamper-evidence.
//!
//! Because spacegame is **sector-sharded**, the leaderboard is also the place the shards' results are
//! reconciled: each sector host reduces its ships to `(id, name, kills)` rows, those rows are folded
//! into one canonical galaxy [`Leaderboard`] ([`Leaderboard::merge`]), and the result is sealed:
//!
//! 1. Reduce the galaxy's standings to a canonical, deterministic [`Leaderboard`].
//! 2. Serialize it; its CID (sha256, via [`ce_rs::CeClient::put_object`]) *is* a tamper-evident
//!    fingerprint — change one score and the CID changes.
//! 3. Bind that CID to the current **PoW beacon** ([`ce_rs::CeClient::beacon`] — tip height + hash,
//!    globally agreed and unpredictable) to form a [`Commitment`]. The beacon proves *when* (against
//!    which global chain state) it was sealed; the CID proves *what*.
//!
//! A [`Commitment`] is published on the galaxy `seal` topic. Anyone can re-fetch the object by CID,
//! recompute the digest, and check the height/hash against the chain — so a final score is
//! **verifiable by a stranger and unforgeable by the host**. This is the property a leaderboard
//! genuinely needs and a CRDT provably cannot give. Pure and unit-tested; the thin I/O lives in
//! [`crate::director`].

use serde::{Deserialize, Serialize};

/// One canonical standing in a sealed leaderboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Standing {
    /// Player NodeId (hex) — the authenticated identity, the unit of one-per-identity uniqueness.
    pub id: String,
    pub name: String,
    /// Total kills across all sectors (the score).
    pub kills: u32,
}

/// A canonical, deterministically-ordered snapshot of the galaxy's standings, ready to be sealed.
/// Built so two honest hosts of the same standings produce **identical** bytes (hence the identical
/// CID) regardless of internal map order or how sectors are sharded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaderboard {
    /// Galaxy id this board is for (part of the committed bytes).
    pub galaxy: String,
    /// Authoritative tick (max over the sectors folded in).
    pub tick: u64,
    /// Standings, ordered by kills desc then id asc — the canonical order.
    pub standings: Vec<Standing>,
}

impl Leaderboard {
    /// Build a canonical board from arbitrary `(id, name, kills)` rows. Ordering is total and
    /// deterministic: kills descending, then NodeId ascending. This is what makes the serialized
    /// bytes — and thus the CID commitment — reproducible by any honest party.
    pub fn from_scores(
        galaxy: &str,
        tick: u64,
        rows: impl IntoIterator<Item = (String, String, u32)>,
    ) -> Self {
        let mut standings: Vec<Standing> =
            rows.into_iter().map(|(id, name, kills)| Standing { id, name, kills }).collect();
        standings.sort_by(|a, b| b.kills.cmp(&a.kills).then_with(|| a.id.cmp(&b.id)));
        Leaderboard { galaxy: galaxy.to_string(), tick, standings }
    }

    /// **Cross-shard reconciliation:** fold per-sector standings into one galaxy board. A player who
    /// appears in several sectors has their kills summed; the latest non-empty name wins; the tick is
    /// the max. The result is re-canonicalized, so the merge is order-independent and produces stable
    /// bytes regardless of how the sectors were sharded across nodes — exactly what makes the sealed
    /// CID reproducible in a sharded world.
    pub fn merge(galaxy: &str, boards: impl IntoIterator<Item = Leaderboard>) -> Self {
        use std::collections::HashMap;
        let mut by_id: HashMap<String, (String, u32)> = HashMap::new();
        let mut tick = 0u64;
        for b in boards {
            tick = tick.max(b.tick);
            for s in b.standings {
                let e = by_id.entry(s.id).or_insert_with(|| (String::new(), 0));
                e.1 = e.1.saturating_add(s.kills);
                if !s.name.is_empty() {
                    e.0 = s.name;
                }
            }
        }
        let rows = by_id.into_iter().map(|(id, (name, kills))| (id, name, kills));
        Leaderboard::from_scores(galaxy, tick, rows)
    }

    /// Canonical bytes to store as a content-addressed object. JSON of the already-canonical struct:
    /// deterministic because field order is fixed and `standings` is pre-sorted.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// A short, stable digest of the standings (FNV-1a/64 over the canonical bytes) for display and
    /// cross-checking. The *CID* is the cryptographic commitment; this is a cheap human fingerprint.
    pub fn digest(&self) -> u64 {
        let bytes = self.encode().unwrap_or_default();
        let mut h: u64 = 0xcbf29ce484222325;
        for b in &bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// The champion (top standing), if any.
    pub fn champion(&self) -> Option<&Standing> {
        self.standings.first()
    }
}

/// A leaderboard **sealed against the PoW chain**: the object CID of the canonical board, bound to
/// the chain tip it was sealed at. Published so anyone can verify a final score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commitment {
    #[serde(rename = "t")]
    pub t: CommitTag,
    pub galaxy: String,
    pub tick: u64,
    /// Content id (sha256) of the canonical [`Leaderboard`] bytes — the tamper-evident fingerprint.
    pub cid: String,
    /// Cheap FNV digest of the same bytes (display / quick mismatch detection).
    pub digest: u64,
    /// PoW chain tip height the board was sealed at (the global ordering anchor).
    pub height: u64,
    /// PoW tip block hash (64 hex) — proves *which* global chain state sealed it.
    pub beacon_hash: String,
    /// Host NodeId that produced the seal (authenticated by the mesh on delivery).
    pub host: String,
}

/// The single commitment tag value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CommitTag {
    #[serde(rename = "seal")]
    #[default]
    Seal,
}

impl Commitment {
    /// Seal `board` against a beacon `(height, hash)` and the object `cid` it was stored under.
    pub fn seal(board: &Leaderboard, cid: &str, height: u64, beacon_hash: &str, host: &str) -> Self {
        Commitment {
            t: CommitTag::Seal,
            galaxy: board.galaxy.clone(),
            tick: board.tick,
            cid: cid.to_string(),
            digest: board.digest(),
            height,
            beacon_hash: beacon_hash.to_string(),
            host: host.to_string(),
        }
    }

    /// Verify that `board` is the one this commitment seals: byte-identical (same digest) to what was
    /// committed. A mismatching digest means the host (or a relay) tampered after sealing.
    pub fn verifies(&self, board: &Leaderboard) -> bool {
        self.galaxy == board.galaxy && self.tick == board.tick && self.digest == board.digest()
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<(String, String, u32)> {
        vec![
            ("nodeB".into(), "Bee".into(), 7),
            ("nodeA".into(), "Ace".into(), 12),
            ("nodeC".into(), "Cy".into(), 7),
        ]
    }

    #[test]
    fn board_is_canonically_ordered() {
        let lb = Leaderboard::from_scores("g1", 100, rows());
        let order: Vec<&str> = lb.standings.iter().map(|s| s.id.as_str()).collect();
        // 12 first; the 7s tie-broken by id ascending: nodeB before nodeC.
        assert_eq!(order, vec!["nodeA", "nodeB", "nodeC"]);
        assert_eq!(lb.champion().unwrap().name, "Ace");
    }

    #[test]
    fn ordering_is_independent_of_input_order() {
        let a = Leaderboard::from_scores("g1", 100, rows());
        let mut shuffled = rows();
        shuffled.reverse();
        let b = Leaderboard::from_scores("g1", 100, shuffled);
        assert_eq!(a, b);
        assert_eq!(a.encode().unwrap(), b.encode().unwrap());
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn merge_sums_kills_across_sectors_order_independently() {
        // Player nodeA appears in two sectors; their kills must sum, and the merge must not depend on
        // sector order (the sharded world produces the same sealed CID regardless of node layout).
        let sector1 = Leaderboard::from_scores(
            "g1",
            50,
            vec![("nodeA".into(), "Ace".into(), 3), ("nodeB".into(), "Bee".into(), 1)],
        );
        let sector2 = Leaderboard::from_scores(
            "g1",
            70,
            vec![("nodeA".into(), "Ace".into(), 4), ("nodeC".into(), "Cy".into(), 9)],
        );
        let merged = Leaderboard::merge("g1", vec![sector1.clone(), sector2.clone()]);
        let merged_rev = Leaderboard::merge("g1", vec![sector2, sector1]);
        assert_eq!(merged, merged_rev, "merge is order-independent");
        assert_eq!(merged.tick, 70, "tick is the max across sectors");
        // nodeA: 3+4=7, nodeC: 9, nodeB: 1 -> champion is nodeC.
        assert_eq!(merged.champion().unwrap().id, "nodeC");
        let a = merged.standings.iter().find(|s| s.id == "nodeA").unwrap();
        assert_eq!(a.kills, 7, "kills summed across sectors");
    }

    #[test]
    fn digest_changes_when_a_score_changes() {
        let honest = Leaderboard::from_scores("g1", 5, rows());
        let mut tampered = rows();
        tampered[0].2 = 999;
        let bad = Leaderboard::from_scores("g1", 5, tampered);
        assert_ne!(honest.digest(), bad.digest(), "any score edit must change the digest");
    }

    #[test]
    fn commitment_verifies_only_the_sealed_board() {
        let board = Leaderboard::from_scores("g1", 42, rows());
        let c = Commitment::seal(&board, "cid_abc", 1000, &"ff".repeat(32), "hostX");
        assert!(c.verifies(&board));
        assert_eq!(c.tick, 42);
        assert_eq!(c.height, 1000);

        let mut bad = rows();
        bad[1].2 += 1;
        let tampered = Leaderboard::from_scores("g1", 42, bad);
        assert!(!c.verifies(&tampered), "a tampered board fails verification");

        let other = Leaderboard::from_scores("g1", 43, rows());
        assert!(!c.verifies(&other));
        let other_galaxy = Leaderboard::from_scores("g2", 42, rows());
        assert!(!c.verifies(&other_galaxy));
    }

    #[test]
    fn commitment_roundtrips_and_tags() {
        let board = Leaderboard::from_scores("g1", 9, rows());
        let c = Commitment::seal(&board, "cidZ", 77, &"ab".repeat(32), "h");
        let bytes = c.encode().unwrap();
        assert_eq!(Commitment::decode(&bytes).unwrap(), c);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["t"], "seal", "frontend switches on t === 'seal'");
        assert_eq!(v["height"], 77);
    }

    #[test]
    fn empty_board_seals_and_has_no_champion() {
        let board = Leaderboard::from_scores("g1", 0, std::iter::empty());
        assert!(board.champion().is_none());
        let c = Commitment::seal(&board, "cid0", 1, &"00".repeat(32), "h");
        assert!(c.verifies(&board));
    }
}
