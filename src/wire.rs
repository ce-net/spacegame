//! Wire protocol between the spacegame frontend and the authoritative sector backends.
//!
//! Topics are **sector-keyed** (see [`topics`]): each sector owns a disjoint `(in, state)` topic
//! pair, so the galaxy's sectors are independent pubsub channels and a client only ever subscribes
//! to the handful of sectors in its interest set.
//!
//! - `ce-game/spacegame/<sector>/in`    — clients publish [`ClientMsg`] (input / join / build / respawn / bye).
//! - `ce-game/spacegame/<sector>/state` — the sector host publishes [`Snapshot`] every tick.
//!
//! Messages are JSON. The node delivers each pubsub message with the **authenticated sender NodeId**
//! (`AppMessage.from`), so the player id is free and unspoofable — the wire payload never carries a
//! player id, a client cannot impersonate another, and even the ship color (derived from the NodeId)
//! is unspoofable. JSON (not bincode) is deliberate: the browser publishes/decodes directly through
//! the same-origin node bridge with no codec.

use serde::{Deserialize, Serialize};

/// Topic naming for one sector. Many sectors scale horizontally because each is a disjoint set of
/// topics and a disjoint authoritative cell; the discovery service lets a client find a sector's host.
pub mod topics {
    /// The game namespace.
    pub const GAME: &str = "spacegame";

    /// Topic a client publishes inputs on for a sector (`sector` is a [`crate::shard::SectorId::token`]).
    pub fn input(sector: &str) -> String {
        format!("ce-game/{GAME}/{sector}/in")
    }

    /// Topic the sector host broadcasts authoritative snapshots on.
    pub fn state(sector: &str) -> String {
        format!("ce-game/{GAME}/{sector}/state")
    }

    /// DHT service name a backend advertises so clients can discover a sector's host.
    pub fn service(sector: &str) -> String {
        format!("ce-game/{GAME}/{sector}")
    }
}

/// A message a client publishes on a sector's `/in` topic. The player id is **not** in the payload:
/// the node authenticates it as `AppMessage.from`, so spoofing is impossible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum ClientMsg {
    /// Announce presence / set name in this sector.
    #[serde(rename = "join")]
    Join { name: String },

    /// One frame of input: thrust, turn (-1/0/+1), fire, and an optional absolute aim heading.
    #[serde(rename = "in")]
    Input {
        #[serde(default)]
        thrust: bool,
        #[serde(default)]
        turn: i32,
        #[serde(default)]
        fire: bool,
        #[serde(default)]
        aim: Option<f32>,
        #[serde(default)]
        name: Option<String>,
    },

    /// Buy from the tech tree (server-priced and gated). `kind` is a tech node id
    /// (e.g. `"hull-1" | "thruster-1" | "twin-guns" | "tech-missile" | "tech-railgun" | "tech-laser"`).
    /// The legacy tokens `"hull" | "speed" | "gun"` are also accepted and mapped to the matching node.
    #[serde(rename = "build")]
    Build { kind: String },

    /// Switch the active weapon to an unlocked one (`id` is a weapon id in the live ruleset, e.g.
    /// `"blaster" | "missile" | "railgun" | "laser"`).
    #[serde(rename = "weapon")]
    Weapon { id: String },

    /// Command your faction's NPC fleet. `order` is one of
    /// `"defend" | "follow" | "mine" | "hold" | "attack" | "attackmove"`; `attackmove` uses `x`/`y`.
    #[serde(rename = "command")]
    Command {
        order: String,
        #[serde(default)]
        x: Option<f32>,
        #[serde(default)]
        y: Option<f32>,
    },

    /// Request a respawn after death (honoured only once the cooldown elapsed).
    #[serde(rename = "respawn")]
    Respawn,

    /// Explicit leave from this sector.
    #[serde(rename = "bye")]
    Bye,
}

/// One ship as broadcast in a [`Snapshot`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShipView {
    /// Ship NodeId (hex) — the authenticated id the host saw.
    pub id: String,
    pub name: String,
    pub hue: u32,
    pub x: i32,
    pub y: i32,
    /// Heading in centi-radians (quantized) so snapshots stay compact.
    pub a: i32,
    pub hp: i32,
    pub max_hp: i32,
    /// Shield buffer + capacity (`max_shield == 0` => an unshielded ship; the HUD hides the bar).
    #[serde(default)]
    pub shield: i32,
    #[serde(default)]
    pub max_shield: i32,
    /// Energy capacitor charge + capacity (rounded to ints for the wire).
    #[serde(default)]
    pub energy: i32,
    #[serde(default)]
    pub max_energy: i32,
    /// Codes of the status effects currently active on this ship (see [`crate::effects::StatusKind::code`]):
    /// `0 = emp, 1 = burn, 2 = slow, 3 = stasis, 4 = overcharge`. The HUD draws an icon per code.
    #[serde(default)]
    pub effects: Vec<u8>,
    pub minerals: u32,
    pub kills: u32,
    pub guns: u32,
    /// Selected weapon id, so the renderer can draw the right muzzle / HUD.
    #[serde(default)]
    pub weapon: String,
    /// Unlocked weapon ids, so the loadout UI can show what this ship may switch to.
    #[serde(default)]
    pub weapons: Vec<String>,
    /// `Some(faction_owner)` if this is an NPC fleet ship, so the client can render it as a fleet unit
    /// (and as friendly/hostile relative to the viewer). `None`/absent for a human player.
    #[serde(default)]
    pub owner: Option<String>,
    /// `"player" | "drone" | "fighter" | "hauler"`.
    #[serde(default)]
    pub role: String,
    pub alive: bool,
}

/// A compact view of one player's faction, so the HUD can **track factions** — your industry and your
/// fleet — and so the e2e can assert the always-alive economy and the NPC ships under command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FactionView {
    /// Owning player NodeId.
    pub owner: String,
    pub minerals: u64,
    pub energy: u64,
    pub alloys: u64,
    /// Building count.
    pub buildings: u32,
    /// Roster unit counts.
    pub drones: u32,
    pub fighters: u32,
    pub haulers: u32,
    /// Live NPC fleet ships currently in the world (under command).
    pub fleet_alive: u32,
    /// Coarse strength rating.
    pub power: u64,
    /// Standing fleet order (e.g. `"defend"`, `"attack_nearest"`, `"attack_move"`).
    pub command: String,
}

/// One live bullet in a snapshot. `homing` lets the renderer draw a missile trail vs a pellet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BulletView {
    pub x: i32,
    pub y: i32,
    pub vx: i32,
    pub vy: i32,
    pub hue: u32,
    #[serde(default)]
    pub homing: bool,
}

/// One beam a hitscan weapon emitted this tick (railgun shot / laser sweep / lightning arc), for the
/// renderer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BeamView {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub hue: u32,
    /// `0` = railgun, `1` = laser, `2` = arc / chain lightning.
    pub kind: u8,
}

/// One deployed proximity mine in a snapshot, for the renderer to draw (and pulse once armed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MineView {
    pub x: i32,
    pub y: i32,
    pub hue: u32,
    /// Trigger radius (so the client can hint the danger zone).
    pub r: i32,
    /// Whether the mine is live (armed) yet.
    pub armed: bool,
}

/// One floating pickup in a snapshot, for the renderer to draw a loot icon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PickupView {
    pub x: i32,
    pub y: i32,
    pub hue: u32,
    /// Pickup kind code (see [`crate::sim::PickupKind::code`]):
    /// `0 = repair, 1 = shield, 2 = energy, 3 = overcharge, 4 = minerals`.
    pub kind: u8,
}

/// A missile detonation this tick, for the renderer to flash a blast of radius `r`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExplosionView {
    pub x: i32,
    pub y: i32,
    pub r: i32,
    pub hue: u32,
}

/// One piece of rigid-body wreckage in a snapshot, for the renderer to tumble.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DebrisView {
    pub x: i32,
    pub y: i32,
    /// Heading in centi-radians (quantized), so the client can spin it.
    pub a: i32,
    pub r: i32,
}

/// One kill-feed entry in a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KillView {
    pub killer: String,
    pub victim: String,
}

/// The authoritative state a sector host broadcasts each tick on the sector's `/state` topic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    /// Constant frame tag so the frontend can switch on `t`.
    #[serde(rename = "t")]
    pub t: SnapshotTag,
    /// Sector token this snapshot is for.
    pub sector: String,
    /// Host NodeId (hex) — who authored this snapshot.
    pub host: String,
    /// Authoritative tick number.
    pub tick: u64,
    /// Every live ship in the sector.
    pub ships: Vec<ShipView>,
    /// Live bullets.
    pub bullets: Vec<BulletView>,
    /// Hitscan beams emitted this tick (railgun/laser).
    #[serde(default)]
    pub beams: Vec<BeamView>,
    /// Missile detonations this tick.
    #[serde(default)]
    pub explosions: Vec<ExplosionView>,
    /// Deployed proximity mines in this sector.
    #[serde(default)]
    pub mines: Vec<MineView>,
    /// Floating powerup pickups in this sector.
    #[serde(default)]
    pub pickups: Vec<PickupView>,
    /// Rigid-body wreckage drifting in this sector.
    #[serde(default)]
    pub debris: Vec<DebrisView>,
    /// Per-player faction summaries (economy + fleet), so clients can track factions.
    #[serde(default)]
    pub factions: Vec<FactionView>,
    /// Asteroid cells currently depleted: `[cx, cy]`. Clients hide these rocks.
    pub depleted: Vec<[i32; 2]>,
    /// Kill events emitted this tick (for the kill feed).
    pub kills: Vec<KillView>,
    /// The live ruleset version in force on the host. When a client sees this number rise it knows a
    /// hot reload happened and re-fetches the ruleset (new weapon stats, new shaders) — instant,
    /// no page reload.
    #[serde(default)]
    pub ruleset: u64,
    /// Host wall-clock millis when produced (clients estimate RTT from it).
    pub ts: u64,
}

/// The single snapshot tag value (`"st"`), so it serializes to a fixed string the frontend checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SnapshotTag {
    #[serde(rename = "st")]
    #[default]
    St,
}

impl ClientMsg {
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
}

impl Snapshot {
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

    #[test]
    fn topics_are_sector_keyed_and_distinct() {
        assert_eq!(topics::input("0_0"), "ce-game/spacegame/0_0/in");
        assert_eq!(topics::state("1_2"), "ce-game/spacegame/1_2/state");
        assert_eq!(topics::service("1_2"), "ce-game/spacegame/1_2");
        assert_ne!(topics::input("0_0"), topics::input("1_0"));
    }

    #[test]
    fn client_join_roundtrips_and_tags() {
        let m = ClientMsg::Join { name: "Ace".into() };
        let bytes = m.encode().unwrap();
        assert_eq!(ClientMsg::decode(&bytes).unwrap(), m);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["t"], "join");
    }

    #[test]
    fn client_input_roundtrips_with_defaults() {
        let m = ClientMsg::Input { thrust: true, turn: -1, fire: true, aim: Some(1.5), name: None };
        let bytes = m.encode().unwrap();
        assert_eq!(ClientMsg::decode(&bytes).unwrap(), m);
        // Missing fields default (the frontend may omit them).
        let sparse = serde_json::from_str::<ClientMsg>(r#"{"t":"in","thrust":true}"#).unwrap();
        assert_eq!(sparse, ClientMsg::Input { thrust: true, turn: 0, fire: false, aim: None, name: None });
    }

    #[test]
    fn build_weapon_respawn_bye_roundtrip() {
        for m in [
            ClientMsg::Build { kind: "tech-railgun".into() },
            ClientMsg::Weapon { id: "missile".into() },
            ClientMsg::Respawn,
            ClientMsg::Bye,
        ] {
            let bytes = m.encode().unwrap();
            assert_eq!(ClientMsg::decode(&bytes).unwrap(), m);
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(ClientMsg::decode(b"not json").is_err());
        assert!(ClientMsg::decode(br#"{"t":"nope"}"#).is_err());
    }

    #[test]
    fn snapshot_roundtrips_and_tags() {
        let snap = Snapshot {
            t: SnapshotTag::St,
            sector: "0_0".into(),
            host: "host123".into(),
            tick: 42,
            ships: vec![ShipView {
                id: "p1".into(),
                name: "Ace".into(),
                hue: 120,
                x: 1500,
                y: 1200,
                a: 157,
                hp: 80,
                max_hp: 100,
                shield: 40,
                max_shield: 60,
                energy: 72,
                max_energy: 100,
                effects: vec![2, 4],
                minerals: 35,
                kills: 2,
                guns: 3,
                weapon: "railgun".into(),
                weapons: vec!["blaster".into(), "railgun".into()],
                owner: None,
                role: "player".into(),
                alive: true,
            }],
            bullets: vec![BulletView { x: 1, y: 2, vx: 26, vy: 0, hue: 120, homing: true }],
            beams: vec![BeamView { x0: 0, y0: 0, x1: 100, y1: 0, hue: 200, kind: 0 }],
            explosions: vec![ExplosionView { x: 50, y: 50, r: 80, hue: 20 }],
            mines: vec![MineView { x: 700, y: 700, hue: 40, r: 150, armed: true }],
            pickups: vec![PickupView { x: 900, y: 200, hue: 200, kind: 1 }],
            debris: vec![DebrisView { x: 5, y: 6, a: 31, r: 4 }],
            factions: vec![FactionView {
                owner: "p1".into(),
                minerals: 120,
                energy: 60,
                alloys: 20,
                buildings: 3,
                drones: 2,
                fighters: 1,
                haulers: 0,
                fleet_alive: 3,
                power: 42,
                command: "defend".into(),
            }],
            depleted: vec![[3, 4]],
            kills: vec![KillView { killer: "p1".into(), victim: "p2".into() }],
            ruleset: 7,
            ts: 1_700_000_000_000,
        };
        let bytes = snap.encode().unwrap();
        assert_eq!(Snapshot::decode(&bytes).unwrap(), snap);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["t"], "st", "frontend switches on t === 'st'");
        assert_eq!(v["ships"][0]["kills"], 2);
    }
}
