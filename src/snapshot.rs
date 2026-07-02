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
//! same observable state (ships incl. loadout/tech, bullets, minerals, kills, asteroid cooldowns),
//! which is what makes failover seamless. The snapshot also records the sector coordinate and the
//! ruleset version it ran under, so the recovering host restores into the right region and can detect
//! whether it must hot-apply a newer ruleset. This module is pure; the snapshot-every-N-ticks I/O and
//! the `put_object`/`get_object` round-trip live in [`crate::director`] / [`crate::run_sector`].

use serde::{Deserialize, Serialize};

use crate::faction::Faction;
use crate::shard::SectorId;
use crate::sim::{Bullet, Sim};

/// Format version, so a future field change can be migrated rather than mis-read. v4 adds the shield/
/// energy/status-effect ship fields plus deployed mines and dropped pickups (all `#[serde(default)]`,
/// so a v3 snapshot still decodes — its ships come back unshielded with full energy and no effects).
pub const SNAPSHOT_VERSION: u32 = 5;

/// serde default for built-design multipliers/mass on a pre-build snapshot: the stock hull is `1.0`.
fn one_f32_snap() -> f32 {
    1.0
}

/// serde default for the per-direction thrust profile on a pre-profile snapshot: full authority.
fn ones_profile_snap() -> [f32; crate::shipyard::THRUST_BINS] {
    [1.0; crate::shipyard::THRUST_BINS]
}

/// A serializable capture of one ship's authoritative, persistent state. The newer loadout/tech fields
/// carry `#[serde(default)]` so a v1 snapshot (pre-weapons) still decodes — the ship simply comes back
/// with the default blaster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShipSnap {
    pub id: String,
    pub name: String,
    pub hue: u32,
    /// Absolute galaxy position (anchored 1:1 floating-origin) — the canonical persistent position.
    pub pos: crate::coords::GalaxyPos,
    pub vx: f32,
    pub vy: f32,
    pub a: f32,
    pub hp: i32,
    pub max_hp: i32,
    /// Shield buffer + capacity (carried so a shielded ship survives failover/transit shielded).
    #[serde(default)]
    pub shield: i32,
    #[serde(default)]
    pub max_shield: i32,
    /// Energy capacitor charge + capacity.
    #[serde(default)]
    pub energy: f32,
    #[serde(default)]
    pub max_energy: f32,
    /// Active status effects (EMP/burn/slow/stasis/overcharge), preserved across failover/transit.
    #[serde(default)]
    pub effects: crate::effects::StatusStack,
    pub minerals: u32,
    pub kills: u32,
    pub speed_lv: u32,
    pub guns: u32,
    /// Built-design physical mass (`1.0` for the stock hull). Carried so a fitted ship keeps its
    /// handling across failover/transit.
    #[serde(default = "one_f32_snap")]
    pub mass: f32,
    /// Built-design max-speed multiplier.
    #[serde(default = "one_f32_snap")]
    pub speed_mult: f32,
    /// Built-design acceleration/agility multiplier.
    #[serde(default = "one_f32_snap")]
    pub thrust_mult: f32,
    /// Built-design cargo capacity.
    #[serde(default)]
    pub cargo: f32,
    /// Built-design per-direction thrust authority (craft frame, 8 bins of 45° — see
    /// [`crate::shipyard::Loadout::thrust_profile`]). Carried so directional handling survives
    /// failover/transit; defaults to full authority for a v1 snapshot.
    #[serde(default = "ones_profile_snap")]
    pub thrust_profile: [f32; crate::shipyard::THRUST_BINS],
    /// Built-design rotational agility (heavier turns slower). Defaults to stock for old snapshots.
    #[serde(default = "one_f32_snap")]
    pub turn_mult: f32,
    /// Built-design resolved part count (block damage/regrowth granularity). `0` = stock.
    #[serde(default)]
    pub part_count: u16,
    /// Blueprint id the ship was built from (`""` = stock hull).
    #[serde(default)]
    pub hull: String,
    /// Selected weapon id.
    #[serde(default)]
    pub weapon: String,
    /// Unlocked weapon ids.
    #[serde(default)]
    pub weapons: Vec<String>,
    /// Bought tech node ids.
    #[serde(default)]
    pub owned: Vec<String>,
    /// `Some(faction_owner)` for an NPC fleet ship, `None` for a human player.
    #[serde(default)]
    pub owner: Option<String>,
    /// Player or NPC fleet role.
    #[serde(default)]
    pub role: crate::sim::ShipRole,
    pub alive: bool,
}

/// A full, deterministic snapshot of a sector's authoritative [`Sim`]. Stored as a content-addressed
/// object; its CID is the failover handle. Round-trips faithfully.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectorSnapshot {
    pub version: u32,
    /// Sector coordinate this snapshot belongs to.
    #[serde(default)]
    pub sx: i32,
    #[serde(default)]
    pub sy: i32,
    /// The ruleset version the sector was running under (so a recovering host knows whether to fetch a
    /// newer one before resuming).
    #[serde(default)]
    pub ruleset_version: u64,
    pub tick: u64,
    pub ships: Vec<ShipSnap>,
    pub bullets: Vec<Bullet>,
    /// Deployed proximity mines drifting in the sector (persistent ordnance — survives failover).
    #[serde(default)]
    pub mines: Vec<crate::sim::Mine>,
    /// Dropped powerup pickups floating in the sector.
    #[serde(default)]
    pub pickups: Vec<crate::sim::Pickup>,
    /// Asteroid cells depleted: `(cx, cy, mined_at_tick)`.
    pub mined: Vec<(i32, i32, u64)>,
    /// Rocks being mined but not yet shattered: `(cx, cy, remaining_hp)`. Carried so a host taking over
    /// resumes a half-mined rock instead of resetting it to full. `#[serde(default)]` — a v4 snapshot
    /// (no field) restores with no in-progress damage, which is harmless.
    #[serde(default)]
    pub rock_dmg: Vec<(i32, i32, u32)>,
    /// Always-alive player factions, so a host taking over keeps everyone's economy building without
    /// missing a beat. (Ephemeral debris is not snapshotted — it is cosmetic and bounded.)
    #[serde(default)]
    pub factions: Vec<Faction>,
    /// Parked (idle-dropped) players' full ship state — progress persists across host restarts too.
    #[serde(default)]
    pub parked: Vec<ShipSnap>,
}

impl SectorSnapshot {
    /// Capture the full authoritative state of `sim`. Ships and mined cells are emitted in sorted
    /// order so the bytes (and hence the CID) are reproducible for identical state.
    pub fn capture(sim: &Sim) -> Self {
        let mut ships: Vec<ShipSnap> = sim.ships.iter().map(|(id, s)| s.snap(id)).collect();
        ships.sort_by(|a, b| a.id.cmp(&b.id));

        let mut mined: Vec<(i32, i32, u64)> =
            sim.mined_cells().into_iter().map(|((cx, cy), t)| (cx, cy, t)).collect();
        mined.sort();

        let mut rock_dmg = sim.rock_damage();
        rock_dmg.sort();

        let mut factions: Vec<Faction> = sim.factions.values().cloned().collect();
        factions.sort_by(|a, b| a.owner.cmp(&b.owner));

        let mut parked: Vec<ShipSnap> = sim.parked.values().cloned().collect();
        parked.sort_by(|a, b| a.id.cmp(&b.id));

        SectorSnapshot {
            version: SNAPSHOT_VERSION,
            sx: sim.sector.sx,
            sy: sim.sector.sy,
            ruleset_version: sim.rules.version,
            tick: sim.tick,
            ships,
            bullets: sim.bullets.clone(),
            mines: sim.mines.clone(),
            pickups: sim.pickups.clone(),
            mined,
            rock_dmg,
            factions,
            parked,
        }
    }

    /// Rebuild a [`Sim`] from this snapshot — what a new host calls after fetching the latest CID. The
    /// restored sim carries the snapshot's sector; the caller (the host) hot-applies the live ruleset
    /// afterwards if a newer one is in force, so a ship returns into the right region under the right
    /// rules.
    pub fn restore(&self) -> Sim {
        let mut sim = Sim::for_sector(SectorId::new(self.sx, self.sy), std::sync::Arc::new(crate::ruleset::Ruleset::builtin()));
        sim.tick = self.tick;
        sim.bullets = self.bullets.clone();
        sim.mines = self.mines.clone();
        sim.pickups = self.pickups.clone();
        for s in &self.ships {
            sim.ships.insert(s.id.clone(), crate::sim::Ship::from_snap(s, self.tick));
        }
        sim.set_mined(self.mined.iter().map(|&(cx, cy, t)| ((cx, cy), t)));
        sim.set_rock_damage(self.rock_dmg.iter().copied());
        for f in &self.factions {
            sim.factions.insert(f.owner.clone(), f.clone());
        }
        for p in &self.parked {
            sim.parked.insert(p.id.clone(), p.clone());
        }
        sim
    }

    /// Serialize for `put_object`.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserialize from `get_object` bytes.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let snap: SectorSnapshot = serde_json::from_slice(bytes)?;
        if snap.version > SNAPSHOT_VERSION {
            anyhow::bail!("unsupported sector snapshot version {} (this build reads up to {})", snap.version, SNAPSHOT_VERSION);
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

    /// Build a non-trivial live sector: ships that moved, mined, fired, bought tech, swapped weapons.
    fn busy_sim() -> Sim {
        let mut s = Sim::new();
        s.seamless = false; // keep ships inside one sector for a clean round-trip comparison
        apply_client_msg(&mut s, "nodeA", ClientMsg::Join { name: "Ace".into(), cap: None });
        apply_client_msg(&mut s, "nodeB", ClientMsg::Join { name: "Bee".into(), cap: None });
        let rock = (0..40)
            .flat_map(|cx| (0..40).map(move |cy| (cx, cy)))
            .find_map(|(cx, cy)| rock_in_cell(cx, cy))
            .unwrap();
        if let Some(sh) = s.ships.get_mut("nodeA") {
            sh.pos.x = rock.x;
            sh.pos.y = rock.y;
            sh.minerals = 500;
        }
        apply_client_msg(&mut s, "nodeA", ClientMsg::Build { kind: "tech-missile".into() });
        apply_client_msg(&mut s, "nodeA", ClientMsg::Weapon { id: "missile".into() });
        apply_client_msg(&mut s, "nodeA", ClientMsg::Input { thrust: true, turn: 0, fire: true, aim: Some(0.0), name: None, strafe_x: 0, strafe_y: 0 });
        apply_client_msg(&mut s, "nodeB", ClientMsg::Input { thrust: true, turn: 1, fire: false, aim: Some(1.0), name: None, strafe_x: 0, strafe_y: 0 });
        s.tick(1.0);
        s.tick(1.0);
        s
    }

    #[test]
    fn capture_then_restore_is_faithful_incl_loadout() {
        let original = busy_sim();
        let snap = SectorSnapshot::capture(&original);
        let restored = snap.restore();

        assert_eq!(restored.tick, original.tick);
        assert_eq!(restored.sector, original.sector);
        assert_eq!(restored.player_count(), original.player_count());
        for (id, p) in &original.ships {
            let q = restored.ships.get(id).expect("ship restored");
            assert_eq!(q.minerals, p.minerals);
            assert_eq!(q.kills, p.kills);
            assert_eq!(q.guns, p.guns);
            assert_eq!(q.weapon, p.weapon, "selected weapon survives failover");
            assert_eq!(q.weapons, p.weapons, "unlocked loadout survives failover");
            assert!((q.pos.x - p.pos.x).abs() < 1e-3 && (q.pos.y - p.pos.y).abs() < 1e-3);
        }
    }

    #[test]
    fn restored_sim_continues_deterministically() {
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
        let again = SectorSnapshot::capture(&snap.restore());
        assert_eq!(again.encode().unwrap(), bytes);
    }

    #[test]
    fn snapshot_records_sector_and_ruleset_version() {
        let mut s = Sim::for_sector(SectorId::new(3, -2), std::sync::Arc::new(crate::ruleset::Ruleset::builtin()));
        s.join("n", "p", 0);
        let snap = SectorSnapshot::capture(&s);
        assert_eq!((snap.sx, snap.sy), (3, -2));
        assert_eq!(snap.ruleset_version, 1);
    }
}
