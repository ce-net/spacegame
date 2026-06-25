//! Replication — periodic authoritative sector snapshots to **content-addressed blobs**, for
//! crash-recovery and host failover.
//!
//! A sector's authoritative state lives in one host's [`Sim`](crate::sim::Sim). If that host crashes,
//! disconnects, or is migrated, the live state is gone — unless it was **replicated**. CE gives the
//! substrate for free: the content-addressed object store ([`ce_rs::CeClient::put_object`] /
//! [`get_object`](ce_rs::CeClient::get_object)). The host serializes the full sector sim to a
//! [`SectorSnapshot`] every few seconds and `put_object`s it; the returned CID is a durable,
//! verifiable handle. Because objects are content-addressed and can be **pinned to multiple nodes**,
//! every node that holds the snapshot is effectively a CDN edge for it — so a new host taking over a
//! sector fetches the latest CID from any holder and resumes with at most one snapshot-interval of
//! loss.
//!
//! [`SectorSnapshot`] is a faithful, deterministic capture: `restore(&capture(&s))` reproduces the
//! same observable state (ships, bullets, minerals, kills, asteroid cooldowns), which is what makes
//! failover seamless. This module is pure; the snapshot-every-N-ticks I/O and the
//! `put_object`/`get_object` round-trip live in [`crate::director`] / [`crate::run_sector`].

use serde::{Deserialize, Serialize};

use crate::sim::{Bullet, Ship, Sim};

/// Format version, so a future field change can be migrated rather than mis-read.
pub const SNAPSHOT_VERSION: u32 = 1;

/// A serializable capture of one ship's authoritative state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShipSnap {
    pub id: String,
    pub name: String,
    pub hue: u32,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub a: f32,
    pub hp: i32,
    pub max_hp: i32,
    pub minerals: u32,
    pub kills: u32,
    pub speed_lv: u32,
    pub guns: u32,
    pub alive: bool,
}

/// A full, deterministic snapshot of a sector's authoritative [`Sim`]. Stored as a content-addressed
/// object; its CID is the failover handle. Round-trips faithfully.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectorSnapshot {
    pub version: u32,
    pub tick: u64,
    pub ships: Vec<ShipSnap>,
    pub bullets: Vec<Bullet>,
    /// Asteroid cells depleted: `(cx, cy, mined_at_tick)`.
    pub mined: Vec<(i32, i32, u64)>,
}

impl SectorSnapshot {
    /// Capture the full authoritative state of `sim`. Ships and mined cells are emitted in sorted
    /// order so the bytes (and hence the CID) are reproducible for identical state.
    pub fn capture(sim: &Sim) -> Self {
        let mut ships: Vec<ShipSnap> = sim
            .ships
            .iter()
            .map(|(id, s)| ShipSnap {
                id: id.clone(),
                name: s.name.clone(),
                hue: s.hue,
                x: s.x,
                y: s.y,
                vx: s.vx,
                vy: s.vy,
                a: s.a,
                hp: s.hp,
                max_hp: s.max_hp,
                minerals: s.minerals,
                kills: s.kills,
                speed_lv: s.speed_lv,
                guns: s.guns,
                alive: s.alive,
            })
            .collect();
        ships.sort_by(|a, b| a.id.cmp(&b.id));

        let mut mined: Vec<(i32, i32, u64)> =
            sim.mined_cells().into_iter().map(|((cx, cy), t)| (cx, cy, t)).collect();
        mined.sort();

        SectorSnapshot {
            version: SNAPSHOT_VERSION,
            tick: sim.tick,
            ships,
            bullets: sim.bullets.clone(),
            mined,
        }
    }

    /// Rebuild a [`Sim`] from this snapshot — what a new host calls after fetching the latest CID, so
    /// play resumes from the replicated state instead of an empty sector.
    pub fn restore(&self) -> Sim {
        let mut sim = Sim::new();
        sim.tick = self.tick;
        sim.bullets = self.bullets.clone();
        for s in &self.ships {
            sim.ships.insert(
                s.id.clone(),
                Ship::from_snap(
                    s.name.clone(),
                    s.hue,
                    s.x,
                    s.y,
                    s.vx,
                    s.vy,
                    s.a,
                    s.hp,
                    s.max_hp,
                    s.minerals,
                    s.kills,
                    s.speed_lv,
                    s.guns,
                    s.alive,
                    self.tick,
                ),
            );
        }
        sim.set_mined(self.mined.iter().map(|&(cx, cy, t)| ((cx, cy), t)));
        sim
    }

    /// Serialize for `put_object`.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserialize from `get_object` bytes.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let snap: SectorSnapshot = serde_json::from_slice(bytes)?;
        if snap.version != SNAPSHOT_VERSION {
            anyhow::bail!("unsupported sector snapshot version {}", snap.version);
        }
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::apply_client_msg;
    use crate::sim::rock_in_cell;
    use crate::wire::ClientMsg;

    /// Build a non-trivial live sector: ships that moved, mined, fired, and bought upgrades.
    fn busy_sim() -> Sim {
        let mut s = Sim::new();
        apply_client_msg(&mut s, "nodeA", ClientMsg::Join { name: "Ace".into() });
        apply_client_msg(&mut s, "nodeB", ClientMsg::Join { name: "Bee".into() });
        // Drop Ace onto a live rock so it banks minerals (creating a mined-cooldown entry).
        let rock = (0..40)
            .flat_map(|cx| (0..40).map(move |cy| (cx, cy)))
            .find_map(|(cx, cy)| rock_in_cell(cx, cy))
            .unwrap();
        if let Some(sh) = s.ships.get_mut("nodeA") {
            sh.x = rock.x;
            sh.y = rock.y;
        }
        apply_client_msg(&mut s, "nodeA", ClientMsg::Input { thrust: true, turn: 0, fire: true, aim: Some(0.0), name: None });
        apply_client_msg(&mut s, "nodeB", ClientMsg::Input { thrust: true, turn: 1, fire: false, aim: Some(1.0), name: None });
        s.tick(1.0);
        s.tick(1.0);
        // Buy an upgrade for Ace.
        s.ships.get_mut("nodeA").unwrap().minerals += 100;
        apply_client_msg(&mut s, "nodeA", ClientMsg::Build { kind: "speed".into() });
        s.tick(1.0);
        s
    }

    #[test]
    fn capture_then_restore_is_faithful() {
        let original = busy_sim();
        let snap = SectorSnapshot::capture(&original);
        let restored = snap.restore();

        assert_eq!(restored.tick, original.tick);
        assert_eq!(restored.player_count(), original.player_count());
        for (id, p) in &original.ships {
            let q = restored.ships.get(id).expect("ship restored");
            assert_eq!(q.minerals, p.minerals);
            assert_eq!(q.kills, p.kills);
            assert_eq!(q.guns, p.guns);
            assert_eq!(q.speed_lv, p.speed_lv);
            assert_eq!(q.hp, p.hp);
            assert_eq!(q.alive, p.alive);
            assert!((q.x - p.x).abs() < 1e-3 && (q.y - p.y).abs() < 1e-3);
        }
        // Same depleted-rock set survives the round-trip.
        let mut a: Vec<_> = original.mined_cells();
        let mut b: Vec<_> = restored.mined_cells();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn restored_sim_continues_deterministically() {
        // The strong failover property: a restored host advancing the sim produces the SAME future
        // the original would have. Snapshot at T, advance both K ticks (same inputs = none), equal.
        let mut original = busy_sim();
        let snap = SectorSnapshot::capture(&original);
        let mut restored = snap.restore();
        for _ in 0..30 {
            original.tick(1.0);
            restored.tick(1.0);
        }
        assert_eq!(original.tick, restored.tick);
        let cap_a = SectorSnapshot::capture(&original);
        let cap_b = SectorSnapshot::capture(&restored);
        assert_eq!(cap_a, cap_b, "restored host must evolve identically to the original");
    }

    #[test]
    fn encode_roundtrips_and_cid_is_stable() {
        let snap = SectorSnapshot::capture(&busy_sim());
        let bytes = snap.encode().unwrap();
        assert_eq!(SectorSnapshot::decode(&bytes).unwrap(), snap);
        // Same state -> same bytes (so put_object yields the same CID): re-capture matches.
        let again = SectorSnapshot::capture(&snap.restore());
        assert_eq!(again.encode().unwrap(), bytes);
    }

    #[test]
    fn decode_rejects_wrong_version() {
        let mut snap = SectorSnapshot::capture(&busy_sim());
        snap.version = 999;
        let bytes = snap.encode().unwrap();
        assert!(SectorSnapshot::decode(&bytes).is_err());
    }
}
