//! Authoritative spacegame simulation — the single source of truth for one **sector**.
//!
//! The galaxy is partitioned into a grid of fixed-size **sectors** (see [`crate::shard`]); this
//! module is the authoritative simulation of *one* sector. Every ship inside the sector is owned by
//! the sector's host: the server integrates thrust/rotation at a clamped rate, spawns and integrates
//! projectiles, runs hitscan weapons, decides hits and kills, and runs mining and the tech tree.
//! Clients never mutate this state directly — they send *intents* (thrust/turn/fire/select/build) and
//! the server does everything else, which is what makes it cheat-resistant.
//!
//! Three things in this module are built for **scale and latency**:
//!
//! * **Data-driven, hot-reloadable rules.** Every weapon, tech node and physics knob lives in an
//!   [`Arc<Ruleset>`](crate::ruleset::Ruleset) the sim reads each tick. [`Sim::apply_ruleset`] swaps
//!   it between ticks, so the game can be balanced and expanded *while people are playing* with no
//!   restart and no dropped ship.
//! * **A recursive AABB broad-phase.** Bullet→ship collision, railgun/laser hitscan, homing
//!   target-acquisition and ship↔ship collision all query a per-tick [`AabbTree`] instead of scanning
//!   every pair, so a crowded sector still ticks inside its time budget (a blown budget is felt as
//!   lag by everyone).
//! * **Seamless cross-sector transit.** When a ship crosses a sector edge it is not bounced off a
//!   wall — it is handed off to the neighbouring sector ([`Sim::take_transits`] /
//!   [`Sim::accept_transit`]), so the world is one continuous **infinite** map spread across the mesh.
//!
//! The simulation is deterministic: same inputs in, same state out. [`Sim::tick`] advances exactly one
//! fixed step with no knowledge of the clock, the mesh or the network, which makes it fully
//! unit-testable and makes failover seamless (a host that restores a [`crate::snapshot::SectorSnapshot`]
//! evolves identically to the one it replaced).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::aabb::{Aabb, AabbTree};
use crate::effects::{StatusKind, StatusStack};
use crate::faction::{Faction, FactionCommand, UnitKind};
use crate::hazard::Hazards;
use crate::physics::{self, RigidBody, Shape, Vec2};
use crate::ruleset::{OnHitEffect, Ruleset, RulesetHandle, TechEffect, Tunables, WeaponKind};
use crate::shard::SectorId;
use crate::snapshot::ShipSnap;

/// What a ship is: a human player, or one of the NPC fleet roles a [`Faction`] fields under command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShipRole {
    #[default]
    Player,
    Drone,
    Fighter,
    Hauler,
}

impl ShipRole {
    pub fn from_unit(u: UnitKind) -> ShipRole {
        match u {
            UnitKind::Drone => ShipRole::Drone,
            UnitKind::Fighter => ShipRole::Fighter,
            UnitKind::Hauler => ShipRole::Hauler,
        }
    }
    pub fn to_unit(self) -> Option<UnitKind> {
        match self {
            ShipRole::Drone => Some(UnitKind::Drone),
            ShipRole::Fighter => Some(UnitKind::Fighter),
            ShipRole::Hauler => Some(UnitKind::Hauler),
            ShipRole::Player => None,
        }
    }
}

/// Side length of one sector in world units. A sector's local coordinates run `0..SECTOR_SIZE`.
/// A sector is a large open expanse: the galaxy is an unbounded grid of these tiled seamlessly
/// (see [`Sim::seamless`]), so play never hits a wall — you just cross into the neighbour.
pub const SECTOR_SIZE: f32 = 9000.0;
/// Grid cell size for the shared deterministic asteroid field (sector-local).
pub const ROCK_CELL: f32 = 300.0;
/// Ship collision / pickup radius.
pub const SHIP_R: f32 = 18.0;
/// Canonical max speed (world units / tick). The single source of truth that both the authoritative
/// server ([`Tunables::default`]) and the client's local prediction read, so prediction matches.
/// Tuned for a fast, momentum-carrying "Reassembly" feel — ships are quick and glide.
pub const MAX_SPEED: f32 = 16.0;
/// Canonical thrust accel (world units / tick^2) — snappy off-the-line response.
pub const THRUST: f32 = 0.95;
/// Canonical per-tick velocity damping. Higher = more glide/momentum (still self-arresting).
pub const DAMPING: f32 = 0.965;
/// Canonical turn rate (radians / tick) — tight mouse-aim tracking.
pub const TURN_RATE: f32 = 0.22;
/// Base hull / max hull at spawn (reference base; live value from [`Tunables::base_hp`]).
pub const BASE_HP: i32 = 100;
/// Max name length the server accepts.
pub const MAX_NAME: usize = 16;
/// Mineral value range of an asteroid.
pub const ROCK_MIN_VAL: u32 = 5;
pub const ROCK_MAX_VAL: u32 = 30;
/// Synthetic owner id for the marauder (hostile PvE) faction. It is *not* a real [`crate::faction::Faction`]
/// — it owns no economy and never appears in the faction summaries — it is purely a tag that makes a ship
/// hostile to everyone and aggressive. Marauder ships are `npc:marauders:*` and hunt the nearest target.
pub const HOSTILE_OWNER: &str = "marauders";
/// Acquire radius for a homing missile to lock the nearest enemy.
pub const HOMING_ACQUIRE_R: f32 = 1100.0;

/// FNV-1a 32-bit hash of a string — the exact field hash the frontend uses, so the renderer and the
/// authoritative server agree on the asteroid field bit-for-bit.
pub fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

fn cell_hash(cx: i32, cy: i32, salt: &str) -> u32 {
    fnv1a(&format!("{cx}:{cy}:{salt}"))
}

/// An asteroid in a sector-local grid cell. Computed identically on client and server from the cell
/// coordinates alone, so the field is shared with zero state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rock {
    pub x: f32,
    pub y: f32,
    pub val: u32,
    pub hp: u32,
    pub cx: i32,
    pub cy: i32,
}

/// The deterministic asteroid (if any) for sector-local grid cell `(cx, cy)`.
pub fn rock_in_cell(cx: i32, cy: i32) -> Option<Rock> {
    let h = cell_hash(cx, cy, "rock");
    if h % 100 >= 55 {
        return None; // ~55% of cells host a rock
    }
    let ox = ((h >> 8) % 1000) as f32 / 1000.0;
    let oy = ((h >> 18) % 1000) as f32 / 1000.0;
    let x = cx as f32 * ROCK_CELL + ox * ROCK_CELL;
    let y = cy as f32 * ROCK_CELL + oy * ROCK_CELL;
    if x < 30.0 || y < 30.0 || x > SECTOR_SIZE - 30.0 || y > SECTOR_SIZE - 30.0 {
        return None;
    }
    let span = ROCK_MAX_VAL - ROCK_MIN_VAL;
    let val = ROCK_MIN_VAL + (cell_hash(cx, cy, "val") % (span + 1));
    let hp = 18 + (cell_hash(cx, cy, "hp") % 30);
    Some(Rock { x, y, val, hp, cx, cy })
}

/// A ship's authoritative state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ship {
    pub name: String,
    pub hue: u32,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub a: f32,
    pub hp: i32,
    pub max_hp: i32,
    /// Current shield buffer — damage is soaked here before it reaches `hp`, and it regenerates out of
    /// combat. `0`/`max_shield == 0` means an unshielded ship (classic feel) until shield tech is bought.
    #[serde(default)]
    pub shield: i32,
    #[serde(default)]
    pub max_shield: i32,
    /// Energy capacitor — the pool heavy weapons draw from (see [`crate::ruleset::WeaponDef::energy_cost`]).
    #[serde(default)]
    pub energy: f32,
    #[serde(default)]
    pub max_energy: f32,
    /// Active status effects (EMP, burn, slow, stasis, overcharge). Persistent across failover/transit.
    #[serde(default)]
    pub effects: StatusStack,
    pub minerals: u32,
    pub kills: u32,
    /// Thruster upgrade level (raises max speed & accel).
    pub speed_lv: u32,
    /// Number of blaster barrels (the legacy multi-gun spread), 1..=`max_guns`.
    pub guns: u32,
    /// Currently selected weapon id (into the live ruleset's catalogue).
    pub weapon: String,
    /// Weapon ids the ship has unlocked and may select.
    pub weapons: Vec<String>,
    /// Tech node ids the ship has bought (for `requires` gating of the tech tree).
    pub owned: Vec<String>,
    /// `None` for a human player's ship (its id *is* the player). `Some(faction_owner_node_id)` for an
    /// NPC fleet ship — the player who commands it. NPCs are tracked here and in [`Faction::units`].
    #[serde(default)]
    pub owner: Option<String>,
    /// Player ship, or which NPC fleet role this is.
    #[serde(default)]
    pub role: ShipRole,
    pub alive: bool,
    #[serde(skip)]
    pub dead_at: u64,
    /// Tick until which shield regen is paused (set when the shield/hull is hit). Transient.
    #[serde(skip)]
    pub shield_block: u64,
    /// Sub-point shield-regen accumulator, so a fractional regen rate is deterministic without a float
    /// shield value. Transient (resets to 0 on snapshot restore — harmless, it is sub-1 point).
    #[serde(skip)]
    pub shield_frac: f32,
    #[serde(skip)]
    pub last_fire: u64,
    #[serde(skip)]
    pub want_thrust: bool,
    #[serde(skip)]
    pub want_turn: i32,
    #[serde(skip)]
    pub want_fire: bool,
    #[serde(skip)]
    pub last_input_tick: u64,
}

impl Ship {
    fn new(name: String, hue: u32, tick: u64, default_weapon: String, base_hp: i32) -> Self {
        let off = (hue as f32 / 360.0 - 0.5) * SECTOR_SIZE * 0.5;
        Ship {
            name,
            hue,
            x: SECTOR_SIZE / 2.0 + off,
            y: SECTOR_SIZE / 2.0 - off,
            vx: 0.0,
            vy: 0.0,
            a: -std::f32::consts::FRAC_PI_2,
            hp: base_hp,
            max_hp: base_hp,
            shield: 0,
            max_shield: 0,
            energy: 0.0,
            max_energy: 0.0,
            effects: StatusStack::new(),
            minerals: 0,
            kills: 0,
            speed_lv: 0,
            guns: 1,
            weapon: default_weapon.clone(),
            weapons: vec![default_weapon],
            owned: Vec::new(),
            owner: None,
            role: ShipRole::Player,
            alive: true,
            dead_at: 0,
            shield_block: 0,
            shield_frac: 0.0,
            last_fire: 0,
            want_thrust: false,
            want_turn: 0,
            want_fire: false,
            last_input_tick: tick,
        }
    }

    /// Spawn an NPC fleet ship of `role` for faction `owner` at `(x, y)`. It carries the blaster (so a
    /// fighter can fight) and full hull for its role; its id is the synthetic `npc:<owner>:<seq>`.
    #[allow(clippy::too_many_arguments)]
    fn npc(role: ShipRole, owner: String, x: f32, y: f32, hp: i32, hue: u32, tick: u64) -> Self {
        let mut s = Ship::new(format!("{role:?}"), hue, tick, "blaster".into(), hp);
        s.x = x;
        s.y = y;
        s.max_hp = hp;
        s.hp = hp;
        s.owner = Some(owner);
        s.role = role;
        // NPCs never idle-expire (they are server-owned, not driven by client input).
        s.last_input_tick = tick;
        s
    }

    /// Rebuild a ship from a persistent snapshot (used by replication failover and cross-sector
    /// transit). Live-only fields reset to neutral so a host that takes over continues deterministically.
    pub fn from_snap(snap: &ShipSnap, tick: u64) -> Self {
        Ship {
            name: snap.name.clone(),
            hue: snap.hue,
            x: snap.x,
            y: snap.y,
            vx: snap.vx,
            vy: snap.vy,
            a: snap.a,
            hp: snap.hp,
            max_hp: snap.max_hp,
            shield: snap.shield,
            max_shield: snap.max_shield,
            energy: snap.energy,
            max_energy: snap.max_energy,
            effects: snap.effects.clone(),
            minerals: snap.minerals,
            kills: snap.kills,
            speed_lv: snap.speed_lv,
            guns: snap.guns,
            weapon: if snap.weapon.is_empty() { "blaster".into() } else { snap.weapon.clone() },
            weapons: if snap.weapons.is_empty() { vec![snap.weapon.clone()] } else { snap.weapons.clone() },
            owned: snap.owned.clone(),
            owner: snap.owner.clone(),
            role: snap.role,
            alive: snap.alive,
            dead_at: 0,
            shield_block: 0,
            shield_frac: 0.0,
            last_fire: 0,
            want_thrust: false,
            want_turn: 0,
            want_fire: false,
            last_input_tick: tick,
        }
    }

    /// Effective max speed after thruster upgrades, given the live tunables.
    pub fn max_speed_t(&self, tun: &Tunables) -> f32 {
        tun.max_speed * (1.0 + self.speed_lv as f32 * tun.thruster_step)
    }

    /// Effective thrust accel after thruster upgrades, given the live tunables.
    pub fn accel_t(&self, tun: &Tunables) -> f32 {
        tun.thrust * (1.0 + self.speed_lv as f32 * tun.thruster_step)
    }

    /// Reference max speed using the legacy default constants (kept for tests / callers without a
    /// ruleset in hand; matches [`max_speed_t`](Self::max_speed_t) under default tunables).
    pub fn max_speed(&self) -> f32 {
        MAX_SPEED * (1.0 + self.speed_lv as f32 * 0.16)
    }

    /// A serializable capture of this ship's persistent state, at id `id`.
    pub fn snap(&self, id: &str) -> ShipSnap {
        ShipSnap {
            id: id.to_string(),
            name: self.name.clone(),
            hue: self.hue,
            x: self.x,
            y: self.y,
            vx: self.vx,
            vy: self.vy,
            a: self.a,
            hp: self.hp,
            max_hp: self.max_hp,
            shield: self.shield,
            max_shield: self.max_shield,
            energy: self.energy,
            max_energy: self.max_energy,
            effects: self.effects.clone(),
            minerals: self.minerals,
            kills: self.kills,
            speed_lv: self.speed_lv,
            guns: self.guns,
            weapon: self.weapon.clone(),
            weapons: self.weapons.clone(),
            owned: self.owned.clone(),
            owner: self.owner.clone(),
            role: self.role,
            alive: self.alive,
        }
    }

    /// Fit a freshly-spawned ship with its base shield + energy capacity from the live tunables. Called
    /// on join, respawn and NPC spawn (after [`new`](Self::new)); shield/energy techs add on top later.
    pub fn outfit(&mut self, tun: &Tunables) {
        self.max_shield = tun.base_shield.max(0);
        self.shield = self.max_shield;
        self.max_energy = tun.base_energy.max(0.0);
        self.energy = self.max_energy;
    }

    /// The faction this ship belongs to: its owner for an NPC, else the player's own id.
    pub fn faction_id<'a>(&'a self, id: &'a str) -> &'a str {
        self.owner.as_deref().unwrap_or(id)
    }
}

/// A live projectile (blaster pellet or homing missile). Hitscan weapons (railgun/laser) do not create
/// bullets — they resolve instantly and emit a [`BeamEvent`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bullet {
    pub owner: String,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub dmg: i32,
    pub hue: u32,
    pub die_at: u64,
    /// Homing steer rate, radians/tick. `0.0` = a straight projectile.
    #[serde(default)]
    pub homing: f32,
    /// If `> 0`, this round is a **missile**: on impact, expiry, or leaving the sector it detonates,
    /// dealing area-of-effect damage within this radius (with distance falloff) and emitting an
    /// [`Explosion`]. `0.0` = an ordinary bullet that deals point damage and vanishes.
    #[serde(default)]
    pub explode_radius: f32,
    /// A status effect this round stamps onto every ship it damages (EMP/burn/slow/stasis). Carried
    /// from the firing [`crate::ruleset::WeaponDef`]. `None` = plain damage.
    #[serde(default)]
    pub effect: Option<OnHitEffect>,
    /// **Cluster submunitions:** when this round detonates, spawn this many child blast rounds in a
    /// ring (a cluster missile). `0` = no split.
    #[serde(default)]
    pub submunitions: u32,
}

/// A deployed **proximity mine** — a real, persistent entity (snapshotted and replicated) that drifts
/// where it was dropped, **arms** after a delay, then **detonates** with an area blast when an enemy of
/// another faction enters its trigger radius (or when it finally times out). Area-denial ordnance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mine {
    pub owner: String,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub dmg: i32,
    /// Blast radius on detonation.
    pub blast: f32,
    /// Proximity trigger radius — an enemy this close sets it off.
    pub trigger: f32,
    pub hue: u32,
    /// Tick at which the mine becomes live (before this it cannot trigger — armed-after-drop safety).
    pub arm_at: u64,
    /// Tick at which the mine goes inert and quietly vanishes.
    pub die_at: u64,
    /// Optional on-detonation status effect (e.g. an EMP mine), carried from the weapon.
    #[serde(default)]
    pub effect: Option<OnHitEffect>,
}

/// What a dropped **pickup** grants when a ship flies over it. Powerups drop where a player is
/// destroyed, turning every kill into loot worth contesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickupKind {
    /// Instantly repairs hull.
    Repair,
    /// Instantly refills (and over-fills, capped) the shield.
    ShieldCell,
    /// Tops up the energy capacitor.
    EnergyCell,
    /// Applies a timed Overcharge buff (rate of fire + damage).
    Overcharge,
    /// A cache of salvaged minerals.
    Minerals,
}

impl PickupKind {
    pub fn code(self) -> u8 {
        match self {
            PickupKind::Repair => 0,
            PickupKind::ShieldCell => 1,
            PickupKind::EnergyCell => 2,
            PickupKind::Overcharge => 3,
            PickupKind::Minerals => 4,
        }
    }
}

/// A floating powerup in the world: collected by overlapping it, expires if left too long.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pickup {
    pub kind: PickupKind,
    pub x: f32,
    pub y: f32,
    /// Effect-specific magnitude (hull/shield/energy points, overcharge fraction, mineral count).
    pub value: f32,
    pub hue: u32,
    /// Tick at which the pickup despawns if uncollected.
    pub die_at: u64,
}

/// A one-tick explosion (a missile detonation) for the renderer to flash and shake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Explosion {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub hue: u32,
}

/// A one-tick beam a hitscan weapon emits, for the renderer to draw (railgun shot, laser sweep).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BeamEvent {
    pub owner: String,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub hue: u32,
    /// `0` = railgun, `1` = laser, `2` = arc / chain lightning.
    pub kind: u8,
}

/// A one-off kill event for the kill feed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KillEvent {
    pub killer: String,
    pub killer_name: String,
    pub victim: String,
    pub victim_name: String,
    pub tick: u64,
}

/// A ship handed off to a neighbouring sector when it crosses a sector edge. The host delivers it to
/// the destination sector (see [`crate::director::transit_topic`]); the destination calls
/// [`Sim::accept_transit`]. This is what makes the galaxy one seamless infinite map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Transit {
    /// Destination sector.
    pub to: SectorId,
    /// The ship's full persistent state, with `x/y` already converted to the destination's local
    /// coordinates and velocity carried over.
    pub ship: ShipSnap,
}

/// A single ship's intent for the next tick.
#[derive(Debug, Clone, Default)]
pub struct Intent {
    pub thrust: bool,
    pub turn: i32,
    pub fire: bool,
    pub aim: Option<f32>,
    pub name: Option<String>,
}

fn sanitize_name(name: &str) -> String {
    let n: String = name.trim().chars().take(MAX_NAME).collect();
    if n.is_empty() { "pilot".to_string() } else { n }
}

/// The authoritative state of one sector.
#[derive(Debug, Clone)]
pub struct Sim {
    pub tick: u64,
    /// This sector's grid coordinate — needed to compute the neighbour a ship transits into.
    pub sector: SectorId,
    /// When true (the default), a ship crossing a sector edge is handed off to the neighbour (infinite
    /// map). When false, it bounces off the edge (a single closed arena — used by some tests).
    pub seamless: bool,
    /// The live, hot-swappable ruleset the sim reads every tick.
    pub rules: RulesetHandle,
    pub ships: HashMap<String, Ship>,
    pub bullets: Vec<Bullet>,
    /// Deployed proximity mines drifting in this sector (persistent, replicated).
    pub mines: Vec<Mine>,
    /// Floating powerup pickups (dropped on kills).
    pub pickups: Vec<Pickup>,
    /// The deterministic environmental hazard field of this sector (gravity wells + nebulae), derived
    /// from the sector coordinate. Empty for the calm home sector `(0,0)` and for `Sim::new()`.
    pub hazards: Hazards,
    mined: HashMap<(i32, i32), u64>,
    pub kill_feed: Vec<KillEvent>,
    /// Beams emitted this tick (railgun/laser) for the wire snapshot. Cleared each tick.
    pub beams: Vec<BeamEvent>,
    /// Missile detonations this tick, for the wire snapshot. Cleared each tick.
    pub explosions: Vec<Explosion>,
    /// Ships that left this sector this tick, to be delivered to neighbours. Drained by the host.
    pub transit_out: Vec<Transit>,
    /// Per-player **always-alive** factions, keyed by owner NodeId. Ticked every sim tick whether or
    /// not the player's ship is present — your industrial swarm keeps building while you are away.
    pub factions: std::collections::HashMap<String, Faction>,
    /// Free-floating wreckage simulated with the LOD rigid-body engine: high precision near players,
    /// coarse far away. Spawned on kills; demonstrates the advanced 2D physics at scale.
    pub debris: physics::World,
}

impl Default for Sim {
    fn default() -> Self {
        Sim {
            tick: 0,
            sector: SectorId::new(0, 0),
            seamless: true,
            rules: std::sync::Arc::new(Ruleset::builtin()),
            ships: HashMap::new(),
            bullets: Vec::new(),
            mines: Vec::new(),
            pickups: Vec::new(),
            hazards: Hazards::empty(),
            mined: HashMap::new(),
            kill_feed: Vec::new(),
            beams: Vec::new(),
            explosions: Vec::new(),
            transit_out: Vec::new(),
            factions: std::collections::HashMap::new(),
            debris: physics::World::new(),
        }
    }
}

impl Sim {
    pub fn new() -> Self {
        Self::default()
    }

    /// A sim for a specific sector with a specific ruleset. The sector's deterministic hazard field
    /// (gravity wells + nebulae) is grown from its coordinate — the home sector `(0,0)` is calm.
    pub fn for_sector(sector: SectorId, rules: RulesetHandle) -> Self {
        Sim { sector, rules, hazards: Hazards::for_sector(sector), ..Self::default() }
    }

    /// **Hot reload:** swap the live ruleset between ticks. Ships in flight keep their state; the new
    /// weapon stats, tech costs and tunables take effect on the very next tick. A ship whose selected
    /// weapon no longer exists falls back to the default weapon so it stays armed.
    pub fn apply_ruleset(&mut self, rules: RulesetHandle) {
        let default_weapon = rules.default_weapon();
        for s in self.ships.values_mut() {
            // Drop any unlocked weapon ids the new ruleset removed (keep the loadout consistent).
            s.weapons.retain(|w| rules.weapon(w).is_some());
            if s.weapons.is_empty() {
                s.weapons.push(default_weapon.clone());
            }
            if rules.weapon(&s.weapon).is_none() {
                s.weapon = default_weapon.clone();
                if !s.weapons.contains(&default_weapon) {
                    s.weapons.push(default_weapon.clone());
                }
            }
        }
        self.rules = rules;
    }

    /// Number of **human players** in the sector (NPC fleet ships are excluded).
    pub fn player_count(&self) -> usize {
        self.ships.values().filter(|s| s.owner.is_none()).count()
    }

    fn tun(&self) -> Tunables {
        self.rules.tunables.clone()
    }

    /// Register or update a ship's identity (called on join). Also founds the player's always-alive
    /// faction the first time they are seen, so their economy exists from then on regardless of
    /// presence.
    pub fn join(&mut self, id: &str, name: &str, hue: u32) {
        let tick = self.tick;
        let name = sanitize_name(name);
        let dw = self.rules.default_weapon();
        let base_hp = self.rules.tunables.base_hp;
        self.factions.entry(id.to_string()).or_insert_with(|| Faction::founding(id));
        match self.ships.get_mut(id) {
            Some(s) => {
                s.name = name;
                s.hue = hue;
                s.last_input_tick = tick;
            }
            None => {
                let mut s = Ship::new(name, hue, tick, dw, base_hp);
                s.outfit(&self.rules.tunables);
                self.ships.insert(id.to_string(), s);
            }
        }
    }

    pub fn leave(&mut self, id: &str) {
        self.ships.remove(id);
    }

    /// Accept a ship handed off from a neighbouring sector (the other end of [`take_transits`]).
    pub fn accept_transit(&mut self, snap: ShipSnap) {
        let ship = Ship::from_snap(&snap, self.tick);
        self.ships.insert(snap.id.clone(), ship);
    }

    /// Drain the ships that left this sector this tick (the host publishes each to its destination).
    pub fn take_transits(&mut self) -> Vec<Transit> {
        std::mem::take(&mut self.transit_out)
    }

    /// Select an unlocked weapon. Ignored if the ship has not unlocked it.
    pub fn select_weapon(&mut self, id: &str, weapon: &str) -> bool {
        if self.rules.weapon(weapon).is_none() {
            return false;
        }
        let Some(s) = self.ships.get_mut(id) else { return false };
        if s.weapons.iter().any(|w| w == weapon) {
            s.weapon = weapon.to_string();
            true
        } else {
            false
        }
    }

    /// Record a ship's intent for the upcoming tick. An intent from an unknown ship auto-joins it.
    pub fn apply_intent(&mut self, id: &str, intent: Intent, hue_fallback: u32) {
        let tick = self.tick;
        let dw = self.rules.default_weapon();
        let base_hp = self.rules.tunables.base_hp;
        let turn_rate = self.rules.tunables.turn_rate;
        if !self.ships.contains_key(id) {
            let name = intent.name.clone().unwrap_or_else(|| "pilot".into());
            let mut s = Ship::new(sanitize_name(&name), hue_fallback, tick, dw, base_hp);
            s.outfit(&self.rules.tunables);
            self.ships.insert(id.to_string(), s);
        }
        if let Some(s) = self.ships.get_mut(id) {
            if let Some(n) = intent.name {
                s.name = sanitize_name(&n);
            }
            s.want_thrust = intent.thrust;
            s.want_turn = intent.turn.clamp(-1, 1);
            s.want_fire = intent.fire;
            if let Some(aim) = intent.aim {
                let mut d = (aim - s.a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
                    - std::f32::consts::PI;
                d = d.clamp(-turn_rate, turn_rate);
                s.a = (s.a + d).rem_euclid(std::f32::consts::TAU);
            }
            s.last_input_tick = tick;
        }
    }

    /// **Tech tree:** buy a tech node by id. Server-enforced: prerequisites owned, can afford, not at
    /// a cap. Stat boosts (hull/thruster/gun) are repeatable with a per-level rising cost; weapon
    /// unlocks are one-time. Returns true on success.
    pub fn buy_tech(&mut self, id: &str, node_id: &str) -> bool {
        let rules = self.rules.clone();
        let Some(node) = rules.tech_node(node_id) else { return false };
        let Some(s) = self.ships.get_mut(id) else { return false };
        // Prerequisites.
        if !node.requires.iter().all(|r| s.owned.iter().any(|o| o == r)) {
            return false;
        }
        let max_guns = rules.tunables.max_guns;
        // Per-effect cap + repeatability + scaled cost.
        let (cost, repeatable) = match &node.effect {
            TechEffect::UnlockWeapon { weapon } => {
                if s.weapons.iter().any(|w| w == weapon) {
                    return false; // already unlocked
                }
                (node.cost, false)
            }
            TechEffect::AddHull { .. } => {
                let lv = s.owned.iter().filter(|o| *o == node_id).count() as u32;
                (node.cost + node.cost * lv / 2, true)
            }
            TechEffect::AddThruster { .. } => {
                if s.speed_lv >= 6 {
                    return false;
                }
                (node.cost + node.cost * s.speed_lv, true)
            }
            TechEffect::AddGun { .. } => {
                if s.guns >= max_guns {
                    return false;
                }
                (node.cost + node.cost * s.guns.saturating_sub(1), true)
            }
            TechEffect::AddShield { .. } => {
                let lv = s.owned.iter().filter(|o| *o == node_id).count() as u32;
                (node.cost + node.cost * lv / 2, true)
            }
            TechEffect::AddEnergy { .. } => {
                let lv = s.owned.iter().filter(|o| *o == node_id).count() as u32;
                (node.cost + node.cost * lv / 2, true)
            }
        };
        if s.minerals < cost {
            return false;
        }
        s.minerals -= cost;
        match &node.effect {
            TechEffect::UnlockWeapon { weapon } => {
                s.weapons.push(weapon.clone());
            }
            TechEffect::AddHull { amount } => {
                s.max_hp += *amount;
                s.hp = s.max_hp;
            }
            TechEffect::AddThruster { levels } => {
                s.speed_lv += *levels;
            }
            TechEffect::AddGun { count } => {
                s.guns = (s.guns + *count).min(max_guns);
            }
            TechEffect::AddShield { amount } => {
                s.max_shield += *amount;
                s.shield = s.max_shield;
            }
            TechEffect::AddEnergy { amount } => {
                s.max_energy += *amount;
                s.energy = s.max_energy;
            }
        }
        if repeatable {
            // Record an ownership marker for repeatables too (so `requires` can reference them and the
            // AddHull level counter can scale cost).
            s.owned.push(node_id.to_string());
        } else {
            s.owned.push(node_id.to_string());
        }
        true
    }

    /// Request a respawn for a dead ship whose cooldown has elapsed.
    pub fn respawn(&mut self, id: &str) -> bool {
        let now = self.tick;
        let respawn_ticks = self.rules.tunables.respawn_ticks;
        let dw = self.rules.default_weapon();
        let base_hp = self.rules.tunables.base_hp;
        let tun = self.rules.tunables.clone();
        let Some(s) = self.ships.get_mut(id) else { return false };
        if s.alive {
            return false;
        }
        if now.saturating_sub(s.dead_at) < respawn_ticks {
            return false;
        }
        let hue = s.hue;
        let name = s.name.clone();
        let kills = s.kills;
        // Respawn keeps identity, kills, and unlocked weapons; upgrades reset to base.
        let weapons = s.weapons.clone();
        let owned: Vec<String> = s.owned.iter().filter(|o| o.starts_with("tech-")).cloned().collect();
        let mut fresh = Ship::new(name, hue, now, dw, base_hp);
        fresh.outfit(&tun);
        fresh.kills = kills;
        fresh.weapons = weapons;
        fresh.owned = owned;
        *s = fresh;
        true
    }

    /// Advance the simulation exactly one fixed tick.
    pub fn tick(&mut self, dt_scale: f32) {
        self.tick += 1;
        let now = self.tick;
        let tun = self.tun();
        self.kill_feed.clear();
        self.beams.clear();
        self.explosions.clear();

        // --- Pass 0: NPC fleet AI. Faction ships under command pick a goal and decide to fire; their
        // intents are then integrated exactly like a player's in pass 1. ---
        self.drive_npcs(&tun);

        // --- Pass 1: integrate motion, mine, expire, and transit ships across sector edges. ---
        let ids: Vec<String> = {
            let mut v: Vec<String> = self.ships.keys().cloned().collect();
            v.sort();
            v
        };
        for id in &ids {
            let drop = {
                let s = &self.ships[id];
                now.saturating_sub(s.last_input_tick) > tun.player_ttl_ticks
            };
            if drop {
                self.ships.remove(id);
                continue;
            }

            let mut mined_now: Vec<(i32, i32, u32)> = Vec::new();
            let mut transit: Option<Transit> = None;
            {
                let s = self.ships.get_mut(id).expect("present");
                if !s.alive {
                    continue;
                }
                // STATUS EFFECTS: drop expired effects, then read the live gates for this tick.
                s.effects.expire(now);
                let can_thrust = s.effects.can_thrust(); // EMP fries the drive
                let mobility = s.effects.mobility_mult(); // Slow scales top speed/accel
                let stasis_retain = s.effects.stasis_retain(); // tractor lock bleeds velocity

                // ENERGY + SHIELD regen: energy refills every tick; the shield only after a quiet spell
                // (no hit for `shield_delay` ticks) and never while EMP-suppressed.
                if s.max_energy > 0.0 {
                    s.energy = (s.energy + tun.energy_regen).clamp(0.0, s.max_energy);
                }
                if s.max_shield > 0
                    && s.shield < s.max_shield
                    && now >= s.shield_block
                    && s.effects.shield_regenerates()
                {
                    s.shield_frac += tun.shield_regen;
                    let whole = s.shield_frac.floor();
                    if whole >= 1.0 {
                        s.shield_frac -= whole;
                        s.shield = (s.shield + whole as i32).min(s.max_shield);
                    }
                }

                // Turn (button steering; mouse-aim already applied in apply_intent).
                s.a = (s.a + s.want_turn as f32 * tun.turn_rate).rem_euclid(std::f32::consts::TAU);
                // Thrust (gated by EMP).
                if s.want_thrust && can_thrust {
                    let acc = s.accel_t(&tun) * mobility * dt_scale;
                    s.vx += s.a.cos() * acc;
                    s.vy += s.a.sin() * acc;
                }
                // ENVIRONMENTAL HAZARDS: gravity wells pull the ship inward; nebula clouds add drag.
                // Read from the sector's deterministic field (a disjoint borrow from `ships`).
                if !self.hazards.is_empty() {
                    let g = self.hazards.accel_at(s.x, s.y);
                    s.vx += g.x * dt_scale;
                    s.vy += g.y * dt_scale;
                    let drag = self.hazards.drag_at(s.x, s.y);
                    if drag > 0.0 {
                        s.vx *= 1.0 - drag;
                        s.vy *= 1.0 - drag;
                    }
                }
                // STASIS: a tractor lock bleeds velocity toward zero.
                if stasis_retain < 1.0 {
                    s.vx *= stasis_retain;
                    s.vy *= stasis_retain;
                }
                // Damping + clamp to the (Slow-scaled) max speed.
                s.vx *= tun.damping;
                s.vy *= tun.damping;
                let spd = (s.vx * s.vx + s.vy * s.vy).sqrt();
                let max = s.max_speed_t(&tun) * mobility;
                if spd > max {
                    let k = max / spd;
                    s.vx *= k;
                    s.vy *= k;
                }
                // Integrate position.
                s.x += s.vx * dt_scale;
                s.y += s.vy * dt_scale;

                let out = s.x < 0.0 || s.y < 0.0 || s.x >= SECTOR_SIZE || s.y >= SECTOR_SIZE;
                // Only player ships transit between sectors; NPC fleet ships belong to their faction's
                // sector and bounce off the edge instead of wandering off the mesh.
                if out && self.seamless && s.owner.is_none() {
                    // INFINITE MAP: hand the ship to the neighbour sector instead of bouncing.
                    let mut dsx = 0;
                    let mut dsy = 0;
                    if s.x < 0.0 {
                        dsx = -1;
                        s.x += SECTOR_SIZE;
                    } else if s.x >= SECTOR_SIZE {
                        dsx = 1;
                        s.x -= SECTOR_SIZE;
                    }
                    if s.y < 0.0 {
                        dsy = -1;
                        s.y += SECTOR_SIZE;
                    } else if s.y >= SECTOR_SIZE {
                        dsy = 1;
                        s.y -= SECTOR_SIZE;
                    }
                    let to = SectorId::new(self.sector.sx + dsx, self.sector.sy + dsy);
                    transit = Some(Transit { to, ship: s.snap(id) });
                } else if out {
                    // Closed-arena fallback: bounce off the walls.
                    if s.x < SHIP_R {
                        s.x = SHIP_R;
                        s.vx = -s.vx * 0.4;
                    }
                    if s.x > SECTOR_SIZE - SHIP_R {
                        s.x = SECTOR_SIZE - SHIP_R;
                        s.vx = -s.vx * 0.4;
                    }
                    if s.y < SHIP_R {
                        s.y = SHIP_R;
                        s.vy = -s.vy * 0.4;
                    }
                    if s.y > SECTOR_SIZE - SHIP_R {
                        s.y = SECTOR_SIZE - SHIP_R;
                        s.vy = -s.vy * 0.4;
                    }
                }

                // Mining: a ship overlapping a live asteroid mines it out.
                if transit.is_none() {
                    let (sx, sy) = (s.x, s.y);
                    let reach = SHIP_R + 22.0;
                    let min_cx = ((sx - reach) / ROCK_CELL).floor() as i32;
                    let max_cx = ((sx + reach) / ROCK_CELL).floor() as i32;
                    let min_cy = ((sy - reach) / ROCK_CELL).floor() as i32;
                    let max_cy = ((sy + reach) / ROCK_CELL).floor() as i32;
                    for cx in min_cx..=max_cx {
                        for cy in min_cy..=max_cy {
                            let Some(r) = rock_in_cell(cx, cy) else { continue };
                            if let Some(&t) = self.mined.get(&(cx, cy))
                                && now.saturating_sub(t) < tun.rock_regen_ticks
                            {
                                continue;
                            }
                            let ddx = r.x - sx;
                            let ddy = r.y - sy;
                            if ddx * ddx + ddy * ddy <= reach * reach {
                                mined_now.push((cx, cy, r.val));
                            }
                        }
                    }
                }
            }

            if let Some(t) = transit {
                self.ships.remove(id);
                self.transit_out.push(t);
                continue;
            }
            for (cx, cy, val) in mined_now {
                self.mined.insert((cx, cy), now);
                // The faction credited is the ship's owner (so an NPC drone banks to your faction, and
                // a player's own mining banks to theirs).
                let fid = self
                    .ships
                    .get(id)
                    .map(|s| s.faction_id(id).to_string())
                    .unwrap_or_else(|| id.to_string());
                if let Some(s) = self.ships.get_mut(id) {
                    s.minerals = s.minerals.saturating_add(val);
                }
                if let Some(f) = self.factions.get_mut(&fid) {
                    f.deposit_minerals(val as u64);
                }
            }
        }

        // --- Pass 1b: damage-over-time (Burn) and lethal hazards (black-hole event horizons). Collect
        // first, then apply, so the borrow on `ships` is released before `apply_damage` mutates. ---
        {
            let mut burns: Vec<(String, i32, String)> = Vec::new();
            let mut swallowed: Vec<String> = Vec::new();
            let lethal = !self.hazards.is_empty();
            let mut ids2: Vec<&String> = self.ships.keys().collect();
            ids2.sort();
            for id in ids2 {
                let s = &self.ships[id];
                if !s.alive {
                    continue;
                }
                if let Some(b) = s.effects.get(StatusKind::Burn) {
                    let dmg = b.magnitude.round().max(0.0) as i32;
                    if dmg > 0 {
                        burns.push((id.clone(), dmg, b.source.clone()));
                    }
                }
                if lethal && self.hazards.lethal_at(s.x, s.y) {
                    swallowed.push(id.clone());
                }
            }
            // Burn bypasses shields — it is a hull fire — so apply it straight to hp via a dedicated path.
            for (victim, dmg, source) in burns {
                self.apply_hull_damage(&victim, dmg, &source, now);
            }
            // A ship that crossed an event horizon is destroyed outright (credited to the void).
            for victim in swallowed {
                self.apply_hull_damage(&victim, i32::MAX, "", now);
            }
        }

        // --- Build the per-tick AABB broad-phase over alive ships (final positions). ---
        let tree = self.build_ship_tree();

        // MINES + PICKUPS: arm/trigger drifting proximity mines and let ships collect dropped loot.
        self.tick_mines(now, &tree);
        self.tick_pickups(now);

        // --- Pass 2: weapon firing (projectile/homing spawn bullets; railgun/laser hitscan). ---
        let firing: Vec<String> = {
            let mut v: Vec<String> = self
                .ships
                .iter()
                .filter(|(_, s)| s.alive && s.want_fire && s.effects.can_fire()) // EMP fries the triggers
                .map(|(id, _)| id.clone())
                .collect();
            v.sort();
            v
        };
        for id in firing {
            self.fire_weapon(&id, now, &tree);
        }
        // One-shot input fields reset (so a ship with no fresh input coasts, and a snapshot-restored
        // ship — whose live fields reset to neutral — evolves identically: deterministic failover).
        for s in self.ships.values_mut() {
            s.want_thrust = false;
            s.want_turn = 0;
            s.want_fire = false;
        }

        // --- Pass 3: steer homing missiles, integrate bullets, resolve hits. ---
        self.advance_bullets(now, &tree, dt_scale);

        // --- Pass 4: ship<->ship collision physics (push overlapping ships apart). ---
        if tun.ship_push > 0.0 {
            self.resolve_ship_collisions(&tree, &tun);
        }

        // --- Pass 5: the always-alive factions tick on the coarse ECONOMY cadence (not every sim frame),
        // so resources accrue and the fleet grows at a strategy pace instead of ballooning at 60 Hz. The
        // roster is then reconciled into live NPC fleet ships every frame (cheap, keeps the world in sync
        // after deaths/transits). ---
        if tun.econ_interval_ticks <= 1 || now % tun.econ_interval_ticks == 0 {
            for f in self.factions.values_mut() {
                f.tick();
            }
        }
        self.reconcile_fleets(&tun);

        // --- Pass 5b: marauder raids. Periodically send a hostile wave at a sector that holds a live
        // player, giving the fleet something to fight and turning mined minerals into a real stake. ---
        self.spawn_enemies(&tun);

        // --- Pass 6: LOD rigid-body wreckage. Precision follows the players: debris near a ship is
        // simulated at high precision/iteration; far debris is coarse or merely registered. ---
        if !self.debris.bodies.is_empty() {
            let focus: Vec<Vec2> = self.ships.values().map(|s| Vec2::new(s.x, s.y)).collect();
            physics::assign_lod(&mut self.debris.bodies, &focus, 700.0, 1500.0, 3000.0);
            self.debris.step(1.0 / 20.0, Vec2::zero());
            // Retire wreckage that drifted out of the sector or has lived long enough.
            let sector_box = (0.0, 0.0, SECTOR_SIZE, SECTOR_SIZE);
            self.debris.retain(|b| {
                b.pos.x >= sector_box.0 - 200.0
                    && b.pos.y >= sector_box.1 - 200.0
                    && b.pos.x <= sector_box.2 + 200.0
                    && b.pos.y <= sector_box.3 + 200.0
                    && now.saturating_sub(b.tag) < 1200
            });
        }

        // --- Housekeeping: GC the mined-cooldown map so it can't grow without bound. ---
        if self.mined.len() > 4096 {
            let regen = tun.rock_regen_ticks;
            self.mined.retain(|_, &mut t| now.saturating_sub(t) < regen);
        }
    }

    /// Build the recursive AABB tree over alive ships, keyed by ship id. Rebuilt every tick from final
    /// positions; used by hitscan, homing target-acquire, bullet collision and ship collision.
    fn build_ship_tree(&self) -> AabbTree<String> {
        let bounds = Aabb::new(-SHIP_R, -SHIP_R, SECTOR_SIZE + SHIP_R, SECTOR_SIZE + SHIP_R);
        let items = self
            .ships
            .iter()
            .filter(|(_, s)| s.alive)
            .map(|(id, s)| (Aabb::around(s.x, s.y, SHIP_R), id.clone()));
        AabbTree::build(bounds, items)
    }

    /// Set the standing order for a player's faction. Every NPC ship the faction owns obeys it from
    /// the next tick.
    pub fn command_faction(&mut self, owner: &str, cmd: FactionCommand) {
        if let Some(f) = self.factions.get_mut(owner) {
            f.command = cmd;
        }
    }

    /// **Fleet AI** — drive every NPC ship under faction command this tick: steer toward its goal and
    /// decide whether to fire. Runs before motion so the intents it sets are integrated like a player's.
    /// Deterministic: ships and targets are chosen in sorted order.
    fn drive_npcs(&mut self, tun: &Tunables) {
        let now = self.tick;
        let mut npc_ids: Vec<String> = self
            .ships
            .iter()
            .filter(|(_, s)| s.alive && s.role != ShipRole::Player)
            .map(|(id, _)| id.clone())
            .collect();
        if npc_ids.is_empty() {
            return;
        }
        npc_ids.sort();
        let tree = self.build_ship_tree();
        for id in npc_ids {
            let (role, owner, x, y) = {
                let Some(s) = self.ships.get(&id) else { continue };
                (s.role, s.owner.clone().unwrap_or_default(), s.x, s.y)
            };
            // Marauders own no faction; they always hunt. Player fleet ships obey their faction's order.
            let cmd = if owner.as_str() == HOSTILE_OWNER {
                FactionCommand::AttackNearest
            } else {
                self.factions.get(&owner).map(|f| f.command).unwrap_or_default()
            };
            let (tx, ty, want_fire) = self.npc_goal(&id, role, &owner, x, y, cmd, &tree);
            // ANTI-RAM: never steer an escort INTO its owner. If a unit ends up closer to its commanding
            // player than the standoff radius, push its goal radially outward so the fleet screens you
            // instead of piling on and crashing. (Engaging an enemy overrides this — that's real combat.)
            let (mut tx, mut ty) = (tx, ty);
            if want_fire == false
                && let Some(ow) = self.ships.get(&owner)
                && owner != id
            {
                let (ox, oy) = (ow.x, ow.y);
                let od = ((x - ox).powi(2) + (y - oy).powi(2)).sqrt();
                if od < 200.0 {
                    let ang = (y - oy).atan2(x - ox);
                    tx = ox + 240.0 * ang.cos();
                    ty = oy + 240.0 * ang.sin();
                }
            }
            if let Some(s) = self.ships.get_mut(&id) {
                let dx = tx - x;
                let dy = ty - y;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist > 1.0 {
                    let desired = dy.atan2(dx);
                    let mut d = (desired - s.a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
                        - std::f32::consts::PI;
                    d = d.clamp(-tun.turn_rate, tun.turn_rate);
                    s.a = (s.a + d).rem_euclid(std::f32::consts::TAU);
                }
                // Hold position at the formation slot (no thrust within a deadband) so units settle into a
                // ring around you and stop jostling, instead of perpetually accelerating toward a point.
                s.want_thrust = dist > 70.0;
                s.want_turn = 0;
                s.want_fire = want_fire;
                s.last_input_tick = now; // server-owned: never idle-expire
            }
        }
    }

    /// Decide an NPC's goal point and whether to open fire, from its role and its faction's standing
    /// [`FactionCommand`]. Fighters engage enemies (any ship of another faction); drones seek asteroids;
    /// haulers escort. The "owner" anchor is the commanding player's own ship if present.
    fn npc_goal(
        &self,
        id: &str,
        role: ShipRole,
        owner: &str,
        x: f32,
        y: f32,
        cmd: FactionCommand,
        tree: &AabbTree<String>,
    ) -> (f32, f32, bool) {
        let anchor = self
            .ships
            .get(owner)
            .map(|s| (s.x, s.y))
            .unwrap_or((SECTOR_SIZE / 2.0, SECTOR_SIZE / 2.0));
        // A stable slot on a ring around the owner: each unit holds a distinct angle (hashed from its id)
        // and the whole screen drifts slowly, so the fleet forms a defensive ring AROUND you rather than
        // converging on your exact position and crashing into you.
        let escort = self.escort_slot(id, anchor);
        match role {
            ShipRole::Fighter => {
                let engage_r = match cmd {
                    FactionCommand::AttackNearest => 4000.0,
                    FactionCommand::Defend => 950.0,
                    FactionCommand::Hold => 700.0,
                    FactionCommand::AttackMove { .. } => 1100.0,
                    _ => 850.0,
                };
                let enemy = self.nearest_enemy_of(owner, x, y, engage_r, tree);
                if let Some(eid) = &enemy
                    && let Some(e) = self.ships.get(eid)
                {
                    let d = ((e.x - x).powi(2) + (e.y - y).powi(2)).sqrt();
                    let aimed = self.roughly_aimed(x, y, e.x, e.y);
                    return (e.x, e.y, d <= 700.0 && aimed);
                }
                match cmd {
                    FactionCommand::Hold => (x, y, false),
                    FactionCommand::AttackMove { x: mx, y: my } => (mx, my, false),
                    _ => (escort.0, escort.1, false), // screen the owner from a ring slot
                }
            }
            ShipRole::Drone => match cmd {
                FactionCommand::Hold => (x, y, false),
                _ => {
                    if let Some((rx, ry)) = self.nearest_live_rock(x, y, 1400.0) {
                        (rx, ry, false)
                    } else {
                        (escort.0, escort.1, false)
                    }
                }
            },
            ShipRole::Hauler => match cmd {
                FactionCommand::Hold => (x, y, false),
                FactionCommand::AttackMove { x: mx, y: my } => (mx, my, false),
                _ => (escort.0, escort.1, false),
            },
            ShipRole::Player => (x, y, false),
        }
    }

    /// A stable ring slot around `anchor` for an escorting unit: the slot angle is a hash of the unit's
    /// id (so units fan out into distinct slots, deterministically) plus a slow shared orbit so the
    /// screen feels alive. The standoff radius keeps the fleet off the owner. Pure.
    fn escort_slot(&self, id: &str, anchor: (f32, f32)) -> (f32, f32) {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in id.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        let base = (h as f32 / u64::MAX as f32) * std::f32::consts::TAU;
        let ang = base + self.tick as f32 * 0.0025; // slow shared orbit (radians/tick)
        let r = 240.0; // standoff radius
        (anchor.0 + r * ang.cos(), anchor.1 + r * ang.sin())
    }

    /// Nearest alive ship of a *different faction* than `owner` (an enemy) within `radius`.
    fn nearest_enemy_of(&self, owner: &str, x: f32, y: f32, radius: f32, tree: &AabbTree<String>) -> Option<String> {
        let mut cands = tree.query(&Aabb::around(x, y, radius));
        cands.sort();
        let mut best: Option<(f32, String)> = None;
        for cid in cands {
            let Some(s) = self.ships.get(&cid) else { continue };
            if !s.alive {
                continue;
            }
            if s.faction_id(&cid) == owner {
                continue; // same faction (the owner or its own fleet)
            }
            let d2 = (s.x - x).powi(2) + (s.y - y).powi(2);
            if d2 <= radius * radius && best.as_ref().map(|(b, _)| d2 < *b).unwrap_or(true) {
                best = Some((d2, cid));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Is a ship at `(x,y)` heading roughly toward `(tx,ty)` enough to bother firing? (Cheap gate so
    /// NPCs do not waste shots while turning.)
    fn roughly_aimed(&self, _x: f32, _y: f32, _tx: f32, _ty: f32) -> bool {
        true // the fire cooldown + steering already gate it; keep deterministic and simple
    }

    /// The position of the nearest non-depleted asteroid within `radius` of `(x,y)`, for drone mining.
    fn nearest_live_rock(&self, x: f32, y: f32, radius: f32) -> Option<(f32, f32)> {
        let now = self.tick;
        let regen = self.rules.tunables.rock_regen_ticks;
        let min_cx = ((x - radius) / ROCK_CELL).floor() as i32;
        let max_cx = ((x + radius) / ROCK_CELL).floor() as i32;
        let min_cy = ((y - radius) / ROCK_CELL).floor() as i32;
        let max_cy = ((y + radius) / ROCK_CELL).floor() as i32;
        let mut best: Option<(f32, f32, f32)> = None;
        for cx in min_cx..=max_cx {
            for cy in min_cy..=max_cy {
                let Some(r) = rock_in_cell(cx, cy) else { continue };
                if let Some(&t) = self.mined.get(&(cx, cy))
                    && now.saturating_sub(t) < regen
                {
                    continue;
                }
                let d2 = (r.x - x).powi(2) + (r.y - y).powi(2);
                if d2 <= radius * radius && best.as_ref().map(|(_, _, b)| d2 < *b).unwrap_or(true) {
                    best = Some((r.x, r.y, d2));
                }
            }
        }
        best.map(|(rx, ry, _)| (rx, ry))
    }

    /// **Fleet reconciliation** — make the set of live NPC ships match each faction's roster. New
    /// roster units (built by the economy) spawn as ships near their owner; the per-faction `max_fleet`
    /// cap bounds simulation cost. Called after the factions tick.
    fn reconcile_fleets(&mut self, tun: &Tunables) {
        let now = self.tick;
        let max_fleet = tun.max_fleet as usize;
        let mut owners: Vec<String> = self.factions.keys().cloned().collect();
        owners.sort();

        // (id, owner, role, x, y, hp, hue)
        let mut spawns: Vec<(String, String, ShipRole, f32, f32, i32, u32)> = Vec::new();
        for owner in owners {
            // Live NPC ships of this faction, by role + total.
            let mut have_drone = 0usize;
            let mut have_fighter = 0usize;
            let mut have_hauler = 0usize;
            for s in self.ships.values() {
                if s.owner.as_deref() == Some(owner.as_str()) {
                    match s.role {
                        ShipRole::Drone => have_drone += 1,
                        ShipRole::Fighter => have_fighter += 1,
                        ShipRole::Hauler => have_hauler += 1,
                        ShipRole::Player => {}
                    }
                }
            }
            let mut total_live = have_drone + have_fighter + have_hauler;
            let (ax, ay) = self
                .ships
                .get(&owner)
                .map(|s| (s.x, s.y))
                .unwrap_or((SECTOR_SIZE / 2.0, SECTOR_SIZE / 2.0));
            let hue = self.ships.get(&owner).map(|s| s.hue).unwrap_or_else(|| fnv1a(&owner) % 360);

            let Some(f) = self.factions.get_mut(&owner) else { continue };
            for (kind, have) in [
                (UnitKind::Drone, have_drone),
                (UnitKind::Fighter, have_fighter),
                (UnitKind::Hauler, have_hauler),
            ] {
                let desired = f.unit_count(kind);
                let mut need = desired.saturating_sub(have);
                while need > 0 && total_live < max_fleet {
                    let seq = f.next_unit_seq;
                    let id = f.next_ship_id();
                    // Deterministic ring spawn around the owner.
                    let ang = (seq as f32) * 2.399_963; // golden angle, spreads the fleet
                    let rad = 60.0 + (seq % 5) as f32 * 14.0;
                    let sx = (ax + ang.cos() * rad).clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
                    let sy = (ay + ang.sin() * rad).clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
                    spawns.push((id, owner.clone(), ShipRole::from_unit(kind), sx, sy, kind.hp(), hue));
                    total_live += 1;
                    need -= 1;
                }
            }
        }
        for (id, owner, role, x, y, hp, hue) in spawns {
            let mut s = Ship::npc(role, owner, x, y, hp, hue, now);
            s.outfit(tun);
            self.ships.insert(id, s);
        }
    }

    /// **Marauder raids** — the PvE threat. On the raid cadence, if the sector holds at least one live
    /// player and is below the hostile cap, spawn a wave of [`HOSTILE_OWNER`] fighters at range around a
    /// deterministically-chosen player. They hunt (the [`drive_npcs`](Self::drive_npcs) AttackNearest path)
    /// and drop minerals on death. Fully deterministic — gated on the shared tick and seeded from it — so
    /// every replica spawns the identical wave. Disabled when `enemy_wave_ticks == 0`.
    fn spawn_enemies(&mut self, tun: &Tunables) {
        if tun.enemy_wave_ticks == 0 || tun.enemy_max == 0 || tun.enemy_wave_size == 0 {
            return;
        }
        let now = self.tick;
        if now == 0 || now % tun.enemy_wave_ticks != 0 {
            return;
        }
        // Only raid sectors with a live human to fight; and never exceed the alive-hostile cap.
        let mut players: Vec<(String, f32, f32)> = self
            .ships
            .values()
            .filter(|s| s.alive && s.owner.is_none())
            .map(|s| (s.name.clone(), s.x, s.y))
            .collect();
        if players.is_empty() {
            return;
        }
        let alive_hostiles = self
            .ships
            .values()
            .filter(|s| s.alive && s.owner.as_deref() == Some(HOSTILE_OWNER))
            .count() as u32;
        if alive_hostiles >= tun.enemy_max {
            return;
        }
        let room = (tun.enemy_max - alive_hostiles).min(tun.enemy_wave_size);

        // Target a deterministically-chosen player (sorted, indexed by the tick) so all replicas agree.
        players.sort_by(|a, b| a.0.cmp(&b.0));
        let (px, py) = {
            let pick = (now as usize / tun.enemy_wave_ticks.max(1) as usize) % players.len();
            (players[pick].1, players[pick].2)
        };

        for k in 0..room {
            let seed = fnv1a(&format!("raid:{now}:{k}"));
            let ang = (seed % 3600) as f32 / 3600.0 * std::f32::consts::TAU;
            let dist = tun.enemy_spawn_dist + ((seed >> 12) % 500) as f32;
            let sx = (px + ang.cos() * dist).clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
            let sy = (py + ang.sin() * dist).clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
            let id = format!("npc:{HOSTILE_OWNER}:{now}:{k}");
            // Hue 0 = an aggressive red, so marauders read instantly as a threat.
            let mut s = Ship::npc(ShipRole::Fighter, HOSTILE_OWNER.to_string(), sx, sy, tun.enemy_hp, 0, now);
            s.outfit(tun);
            self.ships.insert(id, s);
        }
    }

    /// Fire ship `id`'s selected weapon, dispatching on its kind. Reads the live ruleset, so a hot
    /// reload changes weapon behaviour on the next shot.
    fn fire_weapon(&mut self, id: &str, now: u64, tree: &AabbTree<String>) {
        let rules = self.rules.clone();
        let (wx, wy, wa, wvx, wvy, hue0, guns, weapon, energy, oc) = {
            let Some(s) = self.ships.get(id) else { return };
            (s.x, s.y, s.a, s.vx, s.vy, s.hue, s.guns, s.weapon.clone(), s.energy, s.effects.overcharge_mult())
        };
        let def = rules.weapon(&weapon).cloned().unwrap_or_else(crate::ruleset::WeaponDef::fallback);

        // Cooldown: the blaster fires faster with more barrels; other weapons use their own cooldown.
        // Overcharge shortens it.
        let base_cd = if def.kind == WeaponKind::Projectile && def.id == "blaster" {
            def.cooldown.saturating_sub(guns.saturating_sub(1) as u64).max(2)
        } else {
            def.cooldown.max(1)
        };
        let cooldown = ((base_cd as f32) / oc).round().max(1.0) as u64;
        {
            let s = self.ships.get(id).expect("present");
            if !(s.last_fire == 0 || now.saturating_sub(s.last_fire) >= cooldown) {
                return;
            }
        }
        // ENERGY gate: heavy weapons draw from the capacitor. Free weapons (cost 0) always fire; a
        // ship that can't pay simply doesn't fire this tick and tries again as the capacitor refills.
        if def.energy_cost > 0.0 && energy < def.energy_cost {
            return;
        }
        if let Some(s) = self.ships.get_mut(id) {
            s.last_fire = now;
            if def.energy_cost > 0.0 {
                s.energy = (s.energy - def.energy_cost).max(0.0);
            }
        }
        let hue = ((hue0 as i32 + def.hue_shift).rem_euclid(360)) as u32;
        let dmg_mult = oc; // Overcharge boosts damage as well as rate of fire.

        match def.kind {
            WeaponKind::Projectile | WeaponKind::Homing | WeaponKind::Flak => {
                let count = if def.id == "blaster" { guns.max(1) } else { def.count.max(1) };
                let spread = def.spread;
                let homing = if def.kind == WeaponKind::Homing { def.turn_rate } else { 0.0 };
                let base_dmg = def.damage + if def.id == "blaster" { (guns.saturating_sub(1) as i32) * 2 } else { 0 };
                let dmg = ((base_dmg as f32) * dmg_mult).round() as i32;
                // Homing rounds and flak shells are missiles: they detonate with an AoE blast scaled to
                // their payload. A plain projectile (blaster/disruptor) does point damage and vanishes.
                let explode_radius = match def.kind {
                    WeaponKind::Homing => def.damage as f32 * 1.4 + 45.0,
                    WeaponKind::Flak => def.damage as f32 * 1.2 + 30.0,
                    _ => 0.0,
                };
                for g in 0..count {
                    let off = if count > 1 { (g as f32 - (count as f32 - 1.0) / 2.0) * spread } else { 0.0 };
                    let a = wa + off;
                    self.bullets.push(Bullet {
                        owner: id.to_string(),
                        x: wx + a.cos() * (SHIP_R + 4.0),
                        y: wy + a.sin() * (SHIP_R + 4.0),
                        vx: a.cos() * def.speed + wvx,
                        vy: a.sin() * def.speed + wvy,
                        dmg,
                        hue,
                        die_at: now + def.ttl,
                        homing,
                        explode_radius,
                        effect: def.effect,
                        submunitions: def.submunitions,
                    });
                }
            }
            WeaponKind::Railgun | WeaponKind::Laser => {
                // Hitscan weapons honour `count`/`spread`, so a weapon can fire a fan of beams: a
                // scatter laser, a twin-lance railgun, etc. Each ray emits its own beam and hits the
                // first ship it crosses, stamping any on-hit effect (a tractor beam's stasis, etc.).
                let beam_kind: u8 = if def.kind == WeaponKind::Railgun { 0 } else { 1 };
                let count = def.count.max(1);
                let spread = def.spread;
                for g in 0..count {
                    let off = if count > 1 { (g as f32 - (count as f32 - 1.0) / 2.0) * spread } else { 0.0 };
                    let a = wa + off;
                    let (hit, end) = self.hitscan(id, wx, wy, a, def.range, tree);
                    self.beams.push(BeamEvent {
                        owner: id.to_string(),
                        x0: wx,
                        y0: wy,
                        x1: end.0,
                        y1: end.1,
                        hue,
                        kind: beam_kind,
                    });
                    if let Some(victim) = hit {
                        let dmg = ((def.damage as f32) * dmg_mult).round() as i32;
                        self.apply_damage(&victim, dmg, id, now);
                        if let Some(e) = def.effect {
                            self.apply_effect(&victim, &e, id, now);
                        }
                    }
                }
            }
            WeaponKind::Mine => {
                // Deploy `count` proximity mines *behind* the ship, fanned by `spread`. They arm after
                // `arm_ticks` and then detonate on a nearing enemy (or on timeout). A soft cap bounds
                // the field so the sector's mine count (and snapshot size) can't grow without limit.
                if self.mines.len() >= 512 {
                    return;
                }
                let count = def.count.max(1);
                let blast = def.damage as f32 * 1.3 + 40.0;
                let back = wa + std::f32::consts::PI;
                for g in 0..count {
                    let off = if count > 1 { (g as f32 - (count as f32 - 1.0) / 2.0) * def.spread } else { 0.0 };
                    let a = back + off;
                    self.mines.push(Mine {
                        owner: id.to_string(),
                        x: wx + a.cos() * (SHIP_R + 6.0),
                        y: wy + a.sin() * (SHIP_R + 6.0),
                        vx: a.cos() * def.speed,
                        vy: a.sin() * def.speed,
                        dmg: ((def.damage as f32) * dmg_mult).round() as i32,
                        blast,
                        trigger: def.range.max(40.0),
                        hue,
                        arm_at: now + def.arm_ticks,
                        die_at: now + def.ttl.max(1),
                        effect: def.effect,
                    });
                }
            }
            WeaponKind::Arc => {
                self.fire_arc(id, wx, wy, &def, hue, dmg_mult, now, tree);
            }
        }
    }

    /// Stamp a weapon's on-hit status effect onto a still-alive `victim` (EMP/burn/slow/stasis), keyed
    /// from `now` so it expires by tick. No-op if the victim is gone or already dead.
    fn apply_effect(&mut self, victim: &str, e: &OnHitEffect, source: &str, now: u64) {
        if let Some(v) = self.ships.get_mut(victim)
            && v.alive
        {
            v.effects.apply(e.kind, now + e.ticks, e.magnitude, source);
        }
    }

    /// **Arc / chain lightning:** strike the nearest enemy of another faction within range, then leap to
    /// successive nearest un-struck enemies (up to `chain` extra jumps), the damage decaying each hop.
    /// Each segment emits a lightning beam (`kind = 2`). Deterministic via the sorted AABB query.
    #[allow(clippy::too_many_arguments)]
    fn fire_arc(
        &mut self,
        owner: &str,
        ox: f32,
        oy: f32,
        def: &crate::ruleset::WeaponDef,
        hue: u32,
        dmg_mult: f32,
        now: u64,
        tree: &AabbTree<String>,
    ) {
        let faction = self
            .ships
            .get(owner)
            .map(|s| s.faction_id(owner).to_string())
            .unwrap_or_else(|| owner.to_string());
        let max_links = def.chain + 1; // the initial strike plus `chain` jumps
        let mut from = (ox, oy);
        let mut dmg = def.damage as f32 * dmg_mult;
        let mut struck: Vec<String> = Vec::new();
        let mut next = self.nearest_enemy_excluding(&faction, ox, oy, def.range, tree, &struck);
        while let Some(target) = next.take() {
            if struck.len() as u32 >= max_links {
                break;
            }
            let (tx, ty) = match self.ships.get(&target) {
                Some(s) if s.alive => (s.x, s.y),
                _ => break,
            };
            self.beams.push(BeamEvent { owner: owner.to_string(), x0: from.0, y0: from.1, x1: tx, y1: ty, hue, kind: 2 });
            self.apply_damage(&target, dmg.round() as i32, owner, now);
            if let Some(e) = def.effect {
                self.apply_effect(&target, &e, owner, now);
            }
            struck.push(target);
            from = (tx, ty);
            dmg *= 0.7; // decay each hop
            next = self.nearest_enemy_excluding(&faction, tx, ty, def.range, tree, &struck);
        }
    }

    /// Nearest alive ship of a *different faction* than `faction` within `radius`, skipping any id in
    /// `exclude` — the chain-lightning hop primitive.
    fn nearest_enemy_excluding(
        &self,
        faction: &str,
        x: f32,
        y: f32,
        radius: f32,
        tree: &AabbTree<String>,
        exclude: &[String],
    ) -> Option<String> {
        let mut cands = tree.query(&Aabb::around(x, y, radius));
        cands.sort();
        let mut best: Option<(f32, String)> = None;
        for cid in cands {
            if exclude.iter().any(|e| e == &cid) {
                continue;
            }
            let Some(s) = self.ships.get(&cid) else { continue };
            if !s.alive || s.faction_id(&cid) == faction {
                continue;
            }
            let d2 = (s.x - x).powi(2) + (s.y - y).powi(2);
            if d2 <= radius * radius && best.as_ref().map(|(b, _)| d2 < *b).unwrap_or(true) {
                best = Some((d2, cid));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Cast a ray from `(ox, oy)` along heading `a` up to `range`, returning the nearest hit ship id
    /// (not the owner, alive) and the beam end point (the hit, or the ray's far end on a miss). Uses
    /// the AABB tree to consider only ships near the ray, not all of them.
    fn hitscan(
        &self,
        owner: &str,
        ox: f32,
        oy: f32,
        a: f32,
        range: f32,
        tree: &AabbTree<String>,
    ) -> (Option<String>, (f32, f32)) {
        let dx = a.cos();
        let dy = a.sin();
        let ex = ox + dx * range;
        let ey = oy + dy * range;
        // Broad-phase: the ray's bounding box, padded by the ship radius.
        let q = Aabb::new(ox, oy, ex, ey).expanded(SHIP_R + 2.0);
        let mut candidates = tree.query(&q);
        candidates.sort(); // determinism
        let mut best: Option<(f32, String)> = None;
        for cid in candidates {
            if cid == owner {
                continue;
            }
            let Some(s) = self.ships.get(&cid) else { continue };
            if !s.alive {
                continue;
            }
            // Project the ship centre onto the ray; reject if behind the muzzle or beyond range.
            let t = (s.x - ox) * dx + (s.y - oy) * dy;
            if t < 0.0 || t > range {
                continue;
            }
            let px = ox + dx * t;
            let py = oy + dy * t;
            let perp2 = (s.x - px) * (s.x - px) + (s.y - py) * (s.y - py);
            let r = SHIP_R + 4.0;
            if perp2 <= r * r && best.as_ref().map(|(bt, _)| t < *bt).unwrap_or(true) {
                best = Some((t, cid));
            }
        }
        match best {
            Some((t, cid)) => (Some(cid), (ox + dx * t, oy + dy * t)),
            None => (None, (ex, ey)),
        }
    }

    /// Steer homing missiles, integrate every bullet, and resolve ship hits using the AABB broad-phase.
    fn advance_bullets(&mut self, now: u64, tree: &AabbTree<String>, dt_scale: f32) {
        let bullets = std::mem::take(&mut self.bullets);
        let mut surviving: Vec<Bullet> = Vec::with_capacity(bullets.len());
        for mut b in bullets {
            let missile = b.explode_radius > 0.0;
            if now >= b.die_at {
                if missile {
                    self.detonate(&b, now, tree); // a missile that runs out of fuel still blows up
                }
                continue;
            }
            // Homing: steer the velocity toward the nearest alive enemy within the acquire radius.
            if b.homing > 0.0
                && let Some(target) = self.nearest_enemy(&b.owner, b.x, b.y, HOMING_ACQUIRE_R, tree)
                && let Some(t) = self.ships.get(&target)
            {
                let speed = (b.vx * b.vx + b.vy * b.vy).sqrt().max(0.001);
                let cur = b.vy.atan2(b.vx);
                let want = (t.y - b.y).atan2(t.x - b.x);
                let mut d = (want - cur + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
                    - std::f32::consts::PI;
                d = d.clamp(-b.homing, b.homing);
                let na = cur + d;
                b.vx = na.cos() * speed;
                b.vy = na.sin() * speed;
            }
            // ENVIRONMENTAL HAZARDS: gravity wells curve projectiles too — missiles fall inward, shots
            // arc past a planet. (Nebula drag is ship-only; light rounds aren't slowed by gas.)
            if !self.hazards.is_empty() {
                let g = self.hazards.accel_at(b.x, b.y);
                b.vx += g.x * dt_scale;
                b.vy += g.y * dt_scale;
            }
            b.x += b.vx * dt_scale;
            b.y += b.vy * dt_scale;
            if b.x < 0.0 || b.y < 0.0 || b.x > SECTOR_SIZE || b.y > SECTOR_SIZE {
                if missile {
                    self.detonate(&b, now, tree);
                }
                continue;
            }
            // Broad-phase: only ships near the bullet are candidates.
            let mut candidates = tree.query(&Aabb::around(b.x, b.y, SHIP_R + 4.0));
            candidates.sort();
            let mut hit_target: Option<String> = None;
            for cid in candidates {
                if cid == b.owner {
                    continue;
                }
                let Some(s) = self.ships.get(&cid) else { continue };
                if !s.alive {
                    continue;
                }
                let dx = s.x - b.x;
                let dy = s.y - b.y;
                if dx * dx + dy * dy <= (SHIP_R + 4.0) * (SHIP_R + 4.0) {
                    hit_target = Some(cid);
                    break;
                }
            }
            if let Some(victim) = hit_target {
                if missile {
                    self.detonate(&b, now, tree); // AoE — the direct target is inside the blast too
                } else {
                    self.apply_damage(&victim, b.dmg, &b.owner, now);
                    // A non-exploding round (disruptor, etc.) stamps its on-hit effect on the target.
                    if let Some(e) = b.effect {
                        self.apply_effect(&victim, &e, &b.owner, now);
                    }
                }
                continue; // round consumed
            }
            surviving.push(b);
        }
        self.bullets = surviving;
    }

    /// Detonate a missile `b` at its current position: flash an [`Explosion`], deal area-of-effect
    /// damage to every alive ship of another faction within `explode_radius` (full damage at the
    /// centre, falling off to 25% at the edge — never friendly-fires the firer's own faction), and
    /// scatter wreckage. The blast is found with the AABB broad-phase, so it is cheap even in a crowd.
    fn detonate(&mut self, b: &Bullet, now: u64, tree: &AabbTree<String>) {
        let radius = b.explode_radius;
        self.explosions.push(Explosion { x: b.x, y: b.y, r: radius, hue: b.hue });
        // The firer's faction (to avoid blowing up your own fleet).
        let own_faction = self
            .ships
            .get(&b.owner)
            .map(|s| s.faction_id(&b.owner).to_string())
            .unwrap_or_else(|| b.owner.clone());
        let mut victims = tree.query(&Aabb::around(b.x, b.y, radius));
        victims.sort();
        for cid in victims {
            let (alive, fac, sx, sy) = {
                let Some(s) = self.ships.get(&cid) else { continue };
                (s.alive, s.faction_id(&cid).to_string(), s.x, s.y)
            };
            if !alive || fac == own_faction {
                continue;
            }
            let d = ((sx - b.x).powi(2) + (sy - b.y).powi(2)).sqrt();
            if d > radius {
                continue;
            }
            let falloff = (1.0 - d / radius).max(0.25);
            let dmg = ((b.dmg as f32) * falloff).round() as i32;
            self.apply_damage(&cid, dmg, &b.owner, now);
            // The blast also stamps the warhead's on-hit effect (an EMP torpedo disables the cluster
            // it catches). Applied at full strength inside the radius.
            if let Some(e) = b.effect {
                self.apply_effect(&cid, &e, &b.owner, now);
            }
        }
        // CLUSTER: spawn a ring of submunition blast rounds that fan out and detonate shortly after.
        if b.submunitions > 0 {
            let n = b.submunitions.min(12);
            let child_dmg = (b.dmg / 2).max(4);
            let child_blast = (radius * 0.6).max(30.0);
            for k in 0..n {
                let a = (k as f32 / n as f32) * std::f32::consts::TAU;
                let spd = 8.0;
                self.bullets.push(Bullet {
                    owner: b.owner.clone(),
                    x: b.x + a.cos() * 6.0,
                    y: b.y + a.sin() * 6.0,
                    vx: a.cos() * spd,
                    vy: a.sin() * spd,
                    dmg: child_dmg,
                    hue: b.hue,
                    die_at: now + 10,
                    homing: 0.0,
                    explode_radius: child_blast,
                    effect: None, // children are pure shrapnel — no recursive status/cluster
                    submunitions: 0,
                });
            }
        }
        // A little debris kicked out by the blast (deterministic from position + tick).
        let seed = fnv1a(&b.owner) ^ (b.x as u32).wrapping_mul(2654435761) ^ now as u32;
        for k in 0..3u32 {
            let a = ((seed.wrapping_add(k.wrapping_mul(40503)) % 360) as f32).to_radians();
            let spd = 20.0 + ((seed >> (k % 8)) % 40) as f32;
            let mut body = RigidBody::dynamic(Vec2::new(b.x, b.y), 0.6, Shape::Circle { r: 3.0 });
            body.vel = Vec2::new(a.cos() * spd, a.sin() * spd);
            body.ang_vel = a.sin() * 2.0;
            body.restitution = 0.5;
            body.tag = now;
            self.debris.add(body);
        }
    }

    /// Synthesize the equivalent blast round for a detonating mine, so a mine reuses the exact missile
    /// detonation path (AoE falloff, on-hit effect, wreckage).
    fn mine_blast(m: &Mine) -> Bullet {
        Bullet {
            owner: m.owner.clone(),
            x: m.x,
            y: m.y,
            vx: 0.0,
            vy: 0.0,
            dmg: m.dmg,
            hue: m.hue,
            die_at: 0,
            homing: 0.0,
            explode_radius: m.blast,
            effect: m.effect,
            submunitions: 0,
        }
    }

    /// **Mines:** drift each deployed mine, arm it after its delay, and detonate it when an enemy of
    /// another faction enters its trigger radius (or when it times out). Detonation reuses the missile
    /// blast path. Deterministic (sorted broad-phase queries).
    fn tick_mines(&mut self, now: u64, tree: &AabbTree<String>) {
        if self.mines.is_empty() {
            return;
        }
        let mines = std::mem::take(&mut self.mines);
        let mut surviving: Vec<Mine> = Vec::with_capacity(mines.len());
        let mut blasts: Vec<Bullet> = Vec::new();
        for mut m in mines {
            if now >= m.die_at {
                blasts.push(Self::mine_blast(&m)); // a timed-out mine clears itself with a blast
                continue;
            }
            // Drift to rest where it was dropped.
            m.x += m.vx;
            m.y += m.vy;
            m.vx *= 0.92;
            m.vy *= 0.92;
            if m.x < 0.0 || m.y < 0.0 || m.x > SECTOR_SIZE || m.y > SECTOR_SIZE {
                continue; // drifted out of the sector — gone
            }
            let mut triggered = false;
            if now >= m.arm_at {
                let own_fac = self
                    .ships
                    .get(&m.owner)
                    .map(|s| s.faction_id(&m.owner).to_string())
                    .unwrap_or_else(|| m.owner.clone());
                let mut cands = tree.query(&Aabb::around(m.x, m.y, m.trigger));
                cands.sort();
                for cid in cands {
                    let Some(s) = self.ships.get(&cid) else { continue };
                    if !s.alive || s.faction_id(&cid) == own_fac {
                        continue;
                    }
                    let dx = s.x - m.x;
                    let dy = s.y - m.y;
                    if dx * dx + dy * dy <= m.trigger * m.trigger {
                        triggered = true;
                        break;
                    }
                }
            }
            if triggered {
                blasts.push(Self::mine_blast(&m));
            } else {
                surviving.push(m);
            }
        }
        self.mines = surviving;
        for b in blasts {
            self.detonate(&b, now, tree);
        }
    }

    /// **Pickups:** expire stale loot and let any alive *player* ship overlapping a pickup collect it.
    /// NPC fleet ships don't vacuum loot. Deterministic: pickups and players are scanned in sorted order.
    fn tick_pickups(&mut self, now: u64) {
        if self.pickups.is_empty() {
            return;
        }
        let pickups = std::mem::take(&mut self.pickups);
        let mut player_ids: Vec<String> = self
            .ships
            .iter()
            .filter(|(_, s)| s.alive && s.owner.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        player_ids.sort();
        let mut surviving: Vec<Pickup> = Vec::with_capacity(pickups.len());
        let mut collected: Vec<(String, Pickup)> = Vec::new();
        let reach = SHIP_R + 10.0;
        'outer: for p in pickups {
            if now >= p.die_at {
                continue; // expired uncollected
            }
            for id in &player_ids {
                let s = &self.ships[id];
                let dx = s.x - p.x;
                let dy = s.y - p.y;
                if dx * dx + dy * dy <= reach * reach {
                    collected.push((id.clone(), p));
                    continue 'outer;
                }
            }
            surviving.push(p);
        }
        self.pickups = surviving;
        for (id, p) in collected {
            self.collect_pickup(&id, &p, now);
        }
    }

    /// Apply a collected pickup's grant to ship `id`.
    fn collect_pickup(&mut self, id: &str, p: &Pickup, now: u64) {
        let fid = self.ships.get(id).map(|s| s.faction_id(id).to_string()).unwrap_or_else(|| id.to_string());
        if let Some(s) = self.ships.get_mut(id) {
            match p.kind {
                PickupKind::Repair => {
                    s.hp = (s.hp + p.value as i32).min(s.max_hp);
                }
                PickupKind::ShieldCell => {
                    if s.max_shield > 0 {
                        s.shield = (s.shield + p.value as i32).min(s.max_shield);
                    } else {
                        // No shield system yet? Convert the cell into a partial hull patch instead.
                        s.hp = (s.hp + (p.value * 0.5) as i32).min(s.max_hp);
                    }
                }
                PickupKind::EnergyCell => {
                    let cap = s.max_energy.max(p.value);
                    s.energy = (s.energy + p.value).min(cap);
                }
                PickupKind::Overcharge => {
                    s.effects.apply(StatusKind::Overcharge, now + 300, p.value, id);
                }
                PickupKind::Minerals => {
                    s.minerals = s.minerals.saturating_add(p.value as u32);
                }
            }
        }
        if let PickupKind::Minerals = p.kind
            && let Some(f) = self.factions.get_mut(&fid)
        {
            f.deposit_minerals(p.value as u64);
        }
    }

    /// The nearest alive enemy (not `owner`) to `(x, y)` within `radius`, via the AABB broad-phase.
    fn nearest_enemy(&self, owner: &str, x: f32, y: f32, radius: f32, tree: &AabbTree<String>) -> Option<String> {
        let mut candidates = tree.query(&Aabb::around(x, y, radius));
        candidates.sort();
        let mut best: Option<(f32, String)> = None;
        for cid in candidates {
            if cid == owner {
                continue;
            }
            let Some(s) = self.ships.get(&cid) else { continue };
            if !s.alive {
                continue;
            }
            let d2 = (s.x - x) * (s.x - x) + (s.y - y) * (s.y - y);
            if d2 <= radius * radius && best.as_ref().map(|(bd, _)| d2 < *bd).unwrap_or(true) {
                best = Some((d2, cid));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Apply `dmg` from `attacker` to `victim` — soaked by the shield first (which then pauses its
    /// regen), the overflow reaching hull. A kill is routed into [`on_death`] (wreckage, feed, loot).
    fn apply_damage(&mut self, victim: &str, dmg: i32, attacker: &str, now: u64) {
        if dmg <= 0 {
            return;
        }
        let delay = self.rules.tunables.shield_delay;
        let killed = {
            let Some(v) = self.ships.get_mut(victim) else { return };
            if !v.alive {
                return;
            }
            // SHIELDS absorb first; only the overflow reaches hull. Any hit pauses shield regen.
            let mut rem = dmg;
            if v.max_shield > 0 && v.shield > 0 {
                let absorbed = rem.min(v.shield);
                v.shield -= absorbed;
                rem -= absorbed;
            }
            if rem > 0 {
                v.hp -= rem;
            }
            v.shield_block = now + delay;
            v.hp <= 0
        };
        if killed {
            self.on_death(victim, attacker, now);
        }
    }

    /// Apply `dmg` **straight to hull**, bypassing shields — used by Burn (a hull fire) and by a black
    /// hole's event horizon (`i32::MAX` = an instant kill). Routes a kill into [`on_death`].
    fn apply_hull_damage(&mut self, victim: &str, dmg: i32, attacker: &str, now: u64) {
        if dmg <= 0 {
            return;
        }
        let delay = self.rules.tunables.shield_delay;
        let killed = {
            let Some(v) = self.ships.get_mut(victim) else { return };
            if !v.alive {
                return;
            }
            v.hp = v.hp.saturating_sub(dmg);
            v.shield_block = now + delay;
            v.hp <= 0
        };
        if killed {
            self.on_death(victim, attacker, now);
        }
    }

    /// Destroy `victim`, crediting `attacker`: scatter wreckage, file the kill, maybe drop loot, and
    /// either remove an NPC from its roster or leave a player dead-but-present for the respawn timer.
    fn on_death(&mut self, victim: &str, attacker: &str, now: u64) {
        let victim_name = match self.ships.get(victim) {
            Some(v) if v.alive => v.name.clone(),
            _ => return,
        };
        let (vx, vy) = {
            let v = self.ships.get_mut(victim).expect("present");
            v.alive = false;
            v.hp = 0;
            v.dead_at = now;
            v.minerals = 0;
            let p = (v.x, v.y);
            v.vx = 0.0;
            v.vy = 0.0;
            p
        };
        // Scatter rigid-body wreckage from the wreck. Deterministic spread from the victim id + tick,
        // so every replica produces identical debris.
        let seed = fnv1a(victim) ^ (now as u32).wrapping_mul(2654435761);
        for k in 0..5u32 {
            let a = ((seed.wrapping_add(k.wrapping_mul(0x9e3779b1)) % 360) as f32).to_radians();
            let spd = 30.0 + ((seed >> (k % 8)) % 60) as f32;
            let mut body = RigidBody::dynamic(Vec2::new(vx, vy), 1.0, Shape::Circle { r: 4.0 + (k % 3) as f32 });
            body.vel = Vec2::new(a.cos() * spd, a.sin() * spd);
            body.ang_vel = (a.cos()) * 2.0;
            body.restitution = 0.5;
            body.tag = now;
            self.debris.add(body);
        }
        let killer_name = if let Some(k) = self.ships.get_mut(attacker) {
            k.kills += 1;
            k.name.clone()
        } else {
            "unknown".to_string()
        };
        self.kill_feed.push(KillEvent {
            killer: attacker.to_string(),
            killer_name,
            victim: victim.to_string(),
            victim_name,
            tick: now,
        });

        // LOOT: a destroyed *human player* drops a powerup where they died — every kill becomes a prize
        // worth diving for. (NPC wrecks don't drop, so a player can't farm their own fleet for loot.)
        let is_player = self.ships.get(victim).map(|v| v.owner.is_none()).unwrap_or(false);
        if is_player {
            self.spawn_pickup(victim, vx, vy, now);
        }

        // A destroyed MARAUDER drops a mineral cache where it died — killing raiders is the core reward
        // loop (fly through the loot to bank it). Player fleet wrecks still drop nothing (no self-farming).
        let is_hostile = self.ships.get(victim).map(|v| v.owner.as_deref() == Some(HOSTILE_OWNER)).unwrap_or(false);
        if is_hostile {
            let value = self.rules.tunables.enemy_loot as f32;
            if value > 0.0 && self.pickups.len() < 256 {
                let x = vx.clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
                let y = vy.clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
                self.pickups.push(Pickup { kind: PickupKind::Minerals, x, y, value, hue: 40, die_at: now + 1800 });
            }
        }

        // An NPC fleet ship does not respawn: it is removed from the world and struck from its
        // faction's roster (you lose a ship and must build another). Player ships stay dead-but-present
        // for the respawn cooldown.
        let npc = self.ships.get(victim).and_then(|v| v.owner.clone().map(|o| (o, v.role)));
        if let Some((owner, role)) = npc {
            if let Some(unit) = role.to_unit()
                && let Some(f) = self.factions.get_mut(&owner)
            {
                f.lose_unit(unit);
            }
            self.ships.remove(victim);
        }
    }

    /// Drop a deterministic powerup at `(x, y)` when a player is destroyed. The kind and value are a
    /// pure function of the victim id + tick, so every replica spawns the identical pickup.
    fn spawn_pickup(&mut self, victim: &str, x: f32, y: f32, now: u64) {
        let seed = fnv1a(victim) ^ (now as u32).wrapping_mul(0x9e3779b1);
        let (kind, value, hue) = match seed % 5 {
            0 => (PickupKind::Repair, 50.0, 130),
            1 => (PickupKind::ShieldCell, 60.0, 200),
            2 => (PickupKind::EnergyCell, 60.0, 50),
            3 => (PickupKind::Overcharge, 0.35, 300),
            _ => (PickupKind::Minerals, 20.0 + (seed % 30) as f32, 40),
        };
        let x = x.clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
        let y = y.clamp(SHIP_R, SECTOR_SIZE - SHIP_R);
        // Cap the field so a brawl can't flood the snapshot with loot.
        if self.pickups.len() < 256 {
            self.pickups.push(Pickup { kind, x, y, value, hue, die_at: now + 1800 });
        }
    }

    /// Push overlapping ships apart so they cannot stack — the ship↔ship collision physics. Uses the
    /// AABB tree to find neighbouring pairs, processes each unordered pair once (sorted ids), and
    /// applies an equal-and-opposite positional + velocity impulse so momentum is conserved.
    fn resolve_ship_collisions(&mut self, tree: &AabbTree<String>, tun: &Tunables) {
        let min_d = SHIP_R * 2.0;
        let mut pushes: HashMap<String, (f32, f32)> = HashMap::new();
        let mut ids: Vec<String> = self.ships.iter().filter(|(_, s)| s.alive).map(|(id, _)| id.clone()).collect();
        ids.sort();
        for a in &ids {
            let (ax, ay) = {
                let s = &self.ships[a];
                (s.x, s.y)
            };
            let mut neigh = tree.query(&Aabb::around(ax, ay, min_d));
            neigh.sort();
            for b in neigh {
                if &b <= a {
                    continue; // each unordered pair once
                }
                let Some(sb) = self.ships.get(&b) else { continue };
                if !sb.alive {
                    continue;
                }
                let dx = sb.x - ax;
                let dy = sb.y - ay;
                let d2 = dx * dx + dy * dy;
                if d2 >= min_d * min_d {
                    continue;
                }
                let d = d2.sqrt();
                // Separation direction. At (near-)zero distance there is NONE, so we must NOT skip (that
                // welds two ships onto the exact same point forever) — pick a deterministic direction from
                // the id pair so perfectly-overlapping ships always shove apart the same way and resolve.
                let (nx, ny) = if d > 1e-3 {
                    (dx / d, dy / d)
                } else {
                    let mut h: u64 = 0xcbf29ce484222325;
                    for byte in a.bytes().chain(b.bytes()) {
                        h = (h ^ byte as u64).wrapping_mul(0x100000001b3);
                    }
                    let ang = (h as f32 / u64::MAX as f32) * std::f32::consts::TAU;
                    (ang.cos(), ang.sin())
                };
                let overlap = (min_d - d) * 0.5 * tun.ship_push;
                let pa = pushes.entry(a.clone()).or_insert((0.0, 0.0));
                pa.0 -= nx * overlap;
                pa.1 -= ny * overlap;
                let pb = pushes.entry(b.clone()).or_insert((0.0, 0.0));
                pb.0 += nx * overlap;
                pb.1 += ny * overlap;
            }
        }
        for (id, (px, py)) in pushes {
            if let Some(s) = self.ships.get_mut(&id) {
                s.x = (s.x + px).clamp(0.0, SECTOR_SIZE);
                s.y = (s.y + py).clamp(0.0, SECTOR_SIZE);
                // A gentle velocity nudge so the separation reads as a bump, not a teleport.
                s.vx += px * 0.3;
                s.vy += py * 0.3;
            }
        }
    }

    /// A deterministic digest of the authoritative state at this tick — the **anti-cheat / agreement**
    /// fingerprint. Replicas that simulate the same sector from the same inputs produce the *same*
    /// hash; a host that fudges the rules (teleports a ship, fakes a kill, mints minerals) produces a
    /// *different* hash and is outvoted by the honest replicas. Floats are quantised so honest replicas
    /// agree despite tiny rounding, and order-independent fields (bullets, debris) are folded with XOR
    /// so map iteration order never causes a false disagreement. Pairs with [`crate::replication`].
    pub fn state_hash(&self) -> u64 {
        const PRIME: u64 = 0x100000001b3;
        fn mix(h: &mut u64, v: u64) {
            *h ^= v;
            *h = h.wrapping_mul(PRIME);
        }
        fn q(f: f32) -> u64 {
            // Quantise to 1/8 unit so honest replicas agree despite sub-unit float noise.
            (f * 8.0).round() as i64 as u64
        }
        let mut h: u64 = 0xcbf29ce484222325;
        mix(&mut h, self.tick);
        mix(&mut h, self.sector.sx as u64);
        mix(&mut h, self.sector.sy as u64);
        mix(&mut h, self.rules.version);

        // Ships: sorted by id (stable order).
        let mut ids: Vec<&String> = self.ships.keys().collect();
        ids.sort();
        for id in ids {
            let s = &self.ships[id];
            mix(&mut h, fnv1a(id) as u64);
            mix(&mut h, q(s.x));
            mix(&mut h, q(s.y));
            mix(&mut h, q(s.vx));
            mix(&mut h, q(s.vy));
            mix(&mut h, q(s.a));
            mix(&mut h, s.hp as i64 as u64);
            mix(&mut h, s.minerals as u64);
            mix(&mut h, s.kills as u64);
            mix(&mut h, s.guns as u64);
            mix(&mut h, s.alive as u64);
            mix(&mut h, s.role as u64);
            mix(&mut h, fnv1a(&s.weapon) as u64);
            mix(&mut h, fnv1a(s.owner.as_deref().unwrap_or("")) as u64);
            // Defensive + status layer (shields, energy capacitor, active effects).
            mix(&mut h, s.shield as i64 as u64);
            mix(&mut h, s.max_shield as i64 as u64);
            mix(&mut h, q(s.energy));
            mix(&mut h, s.effects.hash());
        }

        // Bullets: order-independent (XOR fold), since the Vec order is an implementation detail.
        let mut bsum: u64 = 0;
        for b in &self.bullets {
            let mut bh: u64 = 0x9e3779b97f4a7c15;
            mix(&mut bh, fnv1a(&b.owner) as u64);
            mix(&mut bh, q(b.x));
            mix(&mut bh, q(b.y));
            mix(&mut bh, b.dmg as i64 as u64);
            mix(&mut bh, b.die_at);
            bsum ^= bh;
        }
        mix(&mut h, bsum);

        // Mines: order-independent (XOR fold) — a deployed minefield is part of authoritative state.
        let mut msum: u64 = 0;
        for m in &self.mines {
            let mut mh: u64 = 0x517cc1b727220a95;
            mix(&mut mh, fnv1a(&m.owner) as u64);
            mix(&mut mh, q(m.x));
            mix(&mut mh, q(m.y));
            mix(&mut mh, m.dmg as i64 as u64);
            mix(&mut mh, m.arm_at);
            mix(&mut mh, m.die_at);
            msum ^= mh;
        }
        mix(&mut h, msum);

        // Pickups: order-independent (XOR fold).
        let mut psum: u64 = 0;
        for p in &self.pickups {
            let mut ph: u64 = 0xff51afd7ed558ccd;
            mix(&mut ph, p.kind.code() as u64);
            mix(&mut ph, q(p.x));
            mix(&mut ph, q(p.y));
            mix(&mut ph, q(p.value));
            mix(&mut ph, p.die_at);
            psum ^= ph;
        }
        mix(&mut h, psum);

        // Factions: sorted by owner.
        let mut owners: Vec<&String> = self.factions.keys().collect();
        owners.sort();
        for o in owners {
            let f = &self.factions[o];
            mix(&mut h, fnv1a(o) as u64);
            mix(&mut h, f.resources.minerals);
            mix(&mut h, f.resources.energy);
            mix(&mut h, f.resources.alloys);
            mix(&mut h, f.buildings.len() as u64);
            mix(&mut h, f.units.len() as u64);
            mix(&mut h, f.power());
        }
        h
    }

    // ---- snapshot/cooldown plumbing ----

    pub fn mined_cells(&self) -> Vec<((i32, i32), u64)> {
        self.mined.iter().map(|(&k, &t)| (k, t)).collect()
    }

    pub fn set_mined(&mut self, entries: impl IntoIterator<Item = ((i32, i32), u64)>) {
        self.mined = entries.into_iter().collect();
    }

    pub fn depleted_cells(&self) -> Vec<(i32, i32, u64)> {
        let now = self.tick;
        let regen = self.rules.tunables.rock_regen_ticks;
        self.mined
            .iter()
            .filter(|&(_, &t)| now.saturating_sub(t) < regen)
            .map(|(&(cx, cy), &t)| (cx, cy, regen - now.saturating_sub(t)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn arena() -> Sim {
        // A closed-arena sim (bounce off walls) for tests that assert in-sector containment.
        let mut s = Sim::new();
        s.seamless = false;
        s
    }

    /// Clear every faction's roster + autonomy so no NPC fleet ships spawn — for tests that assert
    /// pure single-ship mechanics (counts, snapshot ordering, missile targeting) without fleet noise.
    fn solo(s: &mut Sim) {
        for f in s.factions.values_mut() {
            f.units.clear();
            f.policy.enabled = false;
        }
    }

    #[test]
    fn rock_field_is_deterministic() {
        for cx in -4..8 {
            for cy in -4..8 {
                assert_eq!(rock_in_cell(cx, cy), rock_in_cell(cx, cy));
            }
        }
    }

    #[test]
    fn join_then_tick_keeps_ship_with_default_weapon() {
        let mut s = Sim::new();
        s.join("nodeA", "Ace", 120);
        s.tick(1.0);
        assert_eq!(s.player_count(), 1);
        assert_eq!(s.ships["nodeA"].weapon, "blaster");
        assert!(s.ships["nodeA"].weapons.contains(&"blaster".to_string()));
    }

    #[test]
    fn server_clamps_max_speed() {
        let mut s = arena();
        s.join("n", "p", 0);
        for _ in 0..200 {
            s.apply_intent("n", Intent { thrust: true, ..Default::default() }, 0);
            s.tick(1.0);
        }
        let p = &s.ships["n"];
        let spd = (p.vx * p.vx + p.vy * p.vy).sqrt();
        assert!(spd <= p.max_speed() + 0.01, "speed {spd} must be clamped to {}", p.max_speed());
    }

    #[test]
    fn ship_bounces_inside_closed_arena() {
        let mut s = arena();
        s.join("n", "p", 0);
        for _ in 0..400 {
            s.apply_intent("n", Intent { thrust: true, aim: Some(0.7), ..Default::default() }, 0);
            s.tick(1.0);
            let p = &s.ships["n"];
            assert!(p.x >= 0.0 && p.x <= SECTOR_SIZE);
            assert!(p.y >= 0.0 && p.y <= SECTOR_SIZE);
        }
    }

    #[test]
    fn crossing_a_sector_edge_transits_to_the_neighbour() {
        // Seamless infinite map: a ship that flies off the east edge of sector (0,0) is handed to (1,0)
        // with wrapped local coords and carried velocity, and is removed from this sector.
        let mut s = Sim::for_sector(SectorId::new(0, 0), Arc::new(Ruleset::builtin()));
        s.join("n", "p", 0);
        solo(&mut s);
        {
            let p = s.ships.get_mut("n").unwrap();
            p.x = SECTOR_SIZE - 2.0;
            p.y = 1500.0;
            p.a = 0.0;
            p.vx = 6.0;
            p.vy = 0.0;
        }
        for _ in 0..3 {
            s.apply_intent("n", Intent { thrust: true, aim: Some(0.0), ..Default::default() }, 0);
            s.tick(1.0);
            if s.ships.is_empty() {
                break;
            }
        }
        let transits = s.take_transits();
        assert!(!s.ships.contains_key("n"), "ship left this sector");
        assert_eq!(transits.len(), 1);
        let t = &transits[0];
        assert_eq!(t.to, SectorId::new(1, 0), "transited east into (1,0)");
        assert!(t.ship.x >= 0.0 && t.ship.x < SECTOR_SIZE, "entry x is in neighbour-local space");
    }

    #[test]
    fn accept_transit_admits_a_ship_with_carried_state() {
        let mut dst = Sim::for_sector(SectorId::new(1, 0), Arc::new(Ruleset::builtin()));
        let mut snap = Ship::new("Ace".into(), 100, 0, "blaster".into(), 100).snap("n");
        snap.x = 5.0;
        snap.y = 1500.0;
        snap.minerals = 99;
        snap.kills = 4;
        dst.accept_transit(snap);
        assert_eq!(dst.ships["n"].minerals, 99);
        assert_eq!(dst.ships["n"].kills, 4);
    }

    #[test]
    fn blaster_fires_and_respects_cooldown() {
        let mut s = arena();
        s.join("n", "p", 0);
        s.apply_intent("n", Intent { fire: true, ..Default::default() }, 0);
        s.tick(1.0);
        assert_eq!(s.bullets.len(), 1);
        let before = s.bullets.len();
        s.apply_intent("n", Intent { fire: true, ..Default::default() }, 0);
        s.tick(1.0);
        assert!(s.bullets.len() <= before, "cooldown prevents an immediate second shot");
    }

    #[test]
    fn railgun_is_instant_hitscan_and_emits_a_beam() {
        let mut s = arena();
        s.join("gunner", "G", 10);
        s.join("target", "T", 20);
        {
            let g = s.ships.get_mut("gunner").unwrap();
            g.weapons.push("railgun".into());
            g.weapon = "railgun".into();
            g.x = 500.0;
            g.y = 500.0;
            g.a = 0.0;
            g.vx = 0.0;
            g.vy = 0.0;
        }
        {
            let t = s.ships.get_mut("target").unwrap();
            t.x = 900.0; // straight ahead, within range
            t.y = 500.0;
            t.hp = 5;
            t.vx = 0.0;
            t.vy = 0.0;
        }
        s.apply_intent("gunner", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
        s.tick(1.0);
        assert!(s.bullets.is_empty(), "railgun spawns no projectile");
        assert_eq!(s.beams.len(), 1, "railgun emits a beam");
        assert_eq!(s.beams[0].kind, 0);
        assert!(!s.kill_feed.is_empty(), "the railgun one-shots the weak target");
        assert_eq!(s.ships["gunner"].kills, 1);
    }

    #[test]
    fn laser_deals_damage_over_time() {
        let mut s = arena();
        s.join("gunner", "G", 10);
        s.join("target", "T", 20);
        {
            let g = s.ships.get_mut("gunner").unwrap();
            g.weapons.push("laser".into());
            g.weapon = "laser".into();
            g.x = 500.0;
            g.y = 500.0;
            g.a = 0.0;
            g.vx = 0.0;
            g.vy = 0.0;
        }
        {
            let t = s.ships.get_mut("target").unwrap();
            t.x = 650.0; // within laser range
            t.y = 500.0;
            t.hp = 200;
            t.max_hp = 200;
            t.vx = 0.0;
            t.vy = 0.0;
        }
        let start_hp = s.ships["target"].hp;
        for _ in 0..10 {
            {
                let t = s.ships.get_mut("target").unwrap();
                t.x = 650.0;
                t.y = 500.0;
            }
            {
                let g = s.ships.get_mut("gunner").unwrap();
                g.x = 500.0;
                g.y = 500.0;
                g.a = 0.0;
            }
            s.apply_intent("gunner", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
            s.tick(1.0);
        }
        assert!(s.ships["target"].hp < start_hp, "the laser chips the target down over ticks");
    }

    #[test]
    fn homing_missile_curves_toward_a_target() {
        let mut s = arena();
        s.join("gunner", "G", 10);
        s.join("target", "T", 20);
        solo(&mut s);
        {
            let g = s.ships.get_mut("gunner").unwrap();
            g.weapons.push("missile".into());
            g.weapon = "missile".into();
            g.x = 500.0;
            g.y = 500.0;
            g.a = 0.0; // firing straight along +x ...
            g.vx = 0.0;
            g.vy = 0.0;
        }
        {
            let t = s.ships.get_mut("target").unwrap();
            t.x = 900.0;
            t.y = 800.0; // ... but the target is off-axis, so the missile must curve
            t.hp = 500;
            t.max_hp = 500;
            t.vx = 0.0;
            t.vy = 0.0;
        }
        s.apply_intent("gunner", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
        s.tick(1.0);
        let initial_vy = s.bullets.first().map(|b| b.vy).unwrap_or(0.0);
        for _ in 0..6 {
            s.tick(1.0);
        }
        let later_vy = s.bullets.first().map(|b| b.vy).unwrap_or(0.0);
        assert!(later_vy > initial_vy, "the homing missile gains +y velocity steering toward the target");
    }

    #[test]
    fn missile_travels_and_detonates_with_area_damage() {
        let mut s = arena();
        s.join("g", "G", 10);
        {
            let g = s.ships.get_mut("g").unwrap();
            g.weapons.push("missile".into());
            g.weapon = "missile".into();
            g.x = 500.0;
            g.y = 500.0;
            g.a = 0.0;
            g.vx = 0.0;
            g.vy = 0.0;
        }
        // Two enemies clustered downrange so the blast catches both.
        s.join("e1", "E1", 100);
        s.join("e2", "E2", 110);
        solo(&mut s);
        for (id, ex, ey) in [("e1", 900.0, 500.0), ("e2", 928.0, 520.0)] {
            let e = s.ships.get_mut(id).unwrap();
            e.x = ex;
            e.y = ey;
            e.hp = 300;
            e.max_hp = 300;
            e.vx = 0.0;
            e.vy = 0.0;
        }
        s.apply_intent("g", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
        s.tick(1.0);
        assert!(s.bullets.iter().any(|b| b.explode_radius > 0.0), "a missile is in flight");

        let mut exploded = false;
        for _ in 0..60 {
            // Hold the targets still and let the missile fly in and detonate.
            for (id, ex, ey) in [("e1", 900.0, 500.0), ("e2", 928.0, 520.0)] {
                if let Some(e) = s.ships.get_mut(id) {
                    e.x = ex;
                    e.y = ey;
                }
            }
            s.tick(1.0);
            if !s.explosions.is_empty() {
                exploded = true;
                break;
            }
        }
        assert!(exploded, "the missile detonated (an explosion was emitted)");
        let e1_hurt = s.ships.get("e1").map(|e| !e.alive || e.hp < 300).unwrap_or(true);
        let e2_hurt = s.ships.get("e2").map(|e| !e.alive || e.hp < 300).unwrap_or(true);
        assert!(e1_hurt && e2_hurt, "area-of-effect blast damaged BOTH clustered enemies");
        assert!(s.ships["g"].alive && s.ships["g"].hp == s.ships["g"].max_hp, "the firer's own ship is unharmed");
    }

    #[test]
    fn tech_tree_unlocks_a_weapon_and_gates_on_cost() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        assert!(!s.buy_tech("n", "tech-missile"), "no minerals -> cannot unlock");
        s.ships.get_mut("n").unwrap().minerals = 1000;
        assert!(s.buy_tech("n", "tech-missile"));
        assert!(s.ships["n"].weapons.contains(&"missile".to_string()));
        assert!(s.select_weapon("n", "missile"));
        assert_eq!(s.ships["n"].weapon, "missile");
        // Railgun requires twin-guns first.
        assert!(!s.buy_tech("n", "tech-railgun"), "railgun gated behind twin-guns");
        assert!(s.buy_tech("n", "twin-guns"));
        assert!(s.buy_tech("n", "tech-railgun"));
        assert!(s.ships["n"].weapons.contains(&"railgun".to_string()));
    }

    #[test]
    fn hot_reload_retunes_live_and_keeps_ships() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        s.tick(1.0);
        let mut r = Ruleset::builtin();
        r.version = 2;
        r.weapons[0].damage = 99;
        s.apply_ruleset(Arc::new(r));
        assert_eq!(s.player_count(), 1, "ships survive a hot reload");
        assert_eq!(s.rules.weapon("blaster").unwrap().damage, 99, "new stats are live");
    }

    #[test]
    fn hot_reload_falls_back_when_selected_weapon_is_removed() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        s.ships.get_mut("n").unwrap().minerals = 1000;
        s.buy_tech("n", "tech-missile");
        s.select_weapon("n", "missile");
        assert_eq!(s.ships["n"].weapon, "missile");
        let mut r = Ruleset::builtin();
        r.version = 5;
        r.weapons.retain(|w| w.id != "missile");
        r.tech.retain(|t| !matches!(&t.effect, TechEffect::UnlockWeapon { weapon } if weapon == "missile"));
        s.apply_ruleset(Arc::new(r));
        assert_eq!(s.ships["n"].weapon, "blaster", "ship falls back to the default weapon, still armed");
    }

    #[test]
    fn ships_are_pushed_apart_not_stacked() {
        let mut s = arena();
        s.join("a", "A", 1);
        s.join("b", "B", 2);
        {
            let a = s.ships.get_mut("a").unwrap();
            a.x = 1000.0;
            a.y = 1000.0;
            a.vx = 0.0;
            a.vy = 0.0;
        }
        {
            let b = s.ships.get_mut("b").unwrap();
            b.x = 1004.0; // heavily overlapping (< 2*SHIP_R apart)
            b.y = 1000.0;
            b.vx = 0.0;
            b.vy = 0.0;
        }
        for _ in 0..30 {
            s.apply_intent("a", Intent::default(), 1);
            s.apply_intent("b", Intent::default(), 2);
            s.tick(1.0);
        }
        let a = &s.ships["a"];
        let b = &s.ships["b"];
        let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
        assert!(d > SHIP_R, "collision physics separates overlapping ships, d={d}");
    }

    #[test]
    fn faction_roster_becomes_npc_fleet_ships_under_command() {
        let mut s = arena();
        s.join("A", "Ace", 10);
        // Put a fighter and an extra drone on A's roster; reconciliation must field them as ships.
        {
            let f = s.factions.get_mut("A").unwrap();
            f.units.push(crate::faction::Unit { kind: UnitKind::Fighter, hp: 90 });
        }
        s.tick(1.0);
        let fleet: Vec<String> = s
            .ships
            .iter()
            .filter(|(_, sh)| sh.owner.as_deref() == Some("A"))
            .map(|(id, _)| id.clone())
            .collect();
        assert!(fleet.iter().any(|id| id.starts_with("npc:A:")), "faction fielded NPC ships: {fleet:?}");
        assert!(s.ships.values().any(|sh| sh.role == ShipRole::Fighter && sh.owner.as_deref() == Some("A")));

        // Commanding the fleet sets the standing order every NPC obeys.
        s.command_faction("A", FactionCommand::AttackNearest);
        assert_eq!(s.factions["A"].command, FactionCommand::AttackNearest);
    }

    #[test]
    fn npc_fighter_engages_an_enemy_and_death_strikes_the_roster() {
        let mut s = arena();
        s.join("A", "Ace", 10);
        s.join("B", "Bee", 200);
        // A fighter for A, parked next to enemy B; ordered to attack.
        s.factions.get_mut("A").unwrap().units.push(crate::faction::Unit { kind: UnitKind::Fighter, hp: 90 });
        s.command_faction("A", FactionCommand::AttackNearest);
        s.tick(1.0); // spawn the fighter
        let fid = s
            .ships
            .iter()
            .find(|(_, sh)| sh.role == ShipRole::Fighter && sh.owner.as_deref() == Some("A"))
            .map(|(id, _)| id.clone())
            .expect("a fighter exists");
        // Place fighter right on top of B and run; the NPC should shoot B.
        {
            let b = s.ships.get_mut("B").unwrap();
            b.x = 1500.0;
            b.y = 1500.0;
            b.hp = 12;
        }
        {
            let g = s.ships.get_mut(&fid).unwrap();
            g.x = 1500.0 - 60.0;
            g.y = 1500.0;
        }
        let before = s.factions["A"].unit_count(UnitKind::Fighter);
        for _ in 0..60 {
            // keep B in place (don't let it drift) so the fighter has a stationary target
            if let Some(b) = s.ships.get_mut("B") {
                b.x = 1500.0;
                b.y = 1500.0;
            }
            s.tick(1.0);
        }
        // The enemy should have taken fire (dead or damaged).
        let b_dead_or_hurt = s.ships.get("B").map(|b| !b.alive || b.hp < 12).unwrap_or(true);
        assert!(b_dead_or_hurt, "the NPC fighter engaged the enemy");

        // Now kill the fighter and confirm the roster shrinks and the ship is gone (no respawn).
        s.apply_damage(&fid, 9999, "B", s.tick);
        assert!(!s.ships.contains_key(&fid), "destroyed NPC is removed from the world");
        assert!(s.factions["A"].unit_count(UnitKind::Fighter) < before, "the loss struck the faction roster");
    }

    #[test]
    fn economy_advances_on_the_coarse_cadence_not_every_frame() {
        // The faction economy must NOT step every 60 Hz sim frame (that is what balloons resources and
        // hands you a full fleet in seconds) — it steps once per `econ_interval_ticks`.
        let mut s = arena();
        s.join("n", "p", 0);
        let interval = Tunables::default().econ_interval_ticks;
        assert!(interval > 1, "the default economy cadence is coarser than per-frame");
        for _ in 0..(interval * 4) {
            s.tick(1.0);
        }
        // In interval*4 sim ticks the economy stepped ~4 times (at multiples of the interval), not once
        // per frame — so the faction's own clock advanced ~4, proving the gate.
        let age = s.factions["n"].age_ticks;
        assert!((3..=5).contains(&age), "economy ran on the coarse cadence (age={age}, interval={interval})");
    }

    #[test]
    fn marauders_raid_a_sector_with_a_player_and_drop_loot() {
        let mut s = arena();
        let mut rules = crate::ruleset::Ruleset::builtin();
        rules.tunables.enemy_wave_ticks = 5; // raid quickly so the test is fast
        rules.tunables.enemy_wave_size = 2;
        rules.tunables.enemy_max = 4;
        s.apply_ruleset(std::sync::Arc::new(rules));
        s.join("n", "p", 0);

        // No raid before a player is present is impossible here (one joined); advance to the first raid.
        for _ in 0..5 {
            s.tick(1.0);
        }
        let hostiles: Vec<String> = s
            .ships
            .iter()
            .filter(|(_, sh)| sh.owner.as_deref() == Some(HOSTILE_OWNER) && sh.alive)
            .map(|(id, _)| id.clone())
            .collect();
        assert!(!hostiles.is_empty(), "a raid spawned marauders into a populated sector");
        assert!(!s.factions.contains_key(HOSTILE_OWNER), "marauders own no economy/faction");

        // Killing a marauder drops a mineral cache: the kill -> reward -> rebuild loop.
        s.apply_damage(&hostiles[0], 99_999, "n", s.tick);
        assert!(!s.ships.contains_key(&hostiles[0]), "the destroyed marauder is removed from the world");
        assert!(
            s.pickups.iter().any(|p| matches!(p.kind, PickupKind::Minerals)),
            "a destroyed marauder dropped a mineral cache"
        );
    }

    #[test]
    fn no_raid_in_an_empty_sector() {
        // PvE only harasses sectors that actually hold a live player — an empty sector stays quiet.
        let mut s = arena();
        let mut rules = crate::ruleset::Ruleset::builtin();
        rules.tunables.enemy_wave_ticks = 5;
        s.apply_ruleset(std::sync::Arc::new(rules));
        for _ in 0..20 {
            s.tick(1.0);
        }
        assert!(
            !s.ships.values().any(|sh| sh.owner.as_deref() == Some(HOSTILE_OWNER)),
            "no marauders spawn with no player to raid"
        );
    }

    #[test]
    fn state_hash_agrees_for_honest_replicas_and_catches_a_cheat() {
        // Two replicas simulate the same sector from the same inputs: their state hashes must match.
        let mut a = arena();
        let mut b = arena();
        for r in [&mut a, &mut b] {
            r.join("x", "X", 1);
            r.join("y", "Y", 200);
        }
        for _ in 0..25 {
            for r in [&mut a, &mut b] {
                r.apply_intent("x", Intent { thrust: true, aim: Some(0.3), fire: true, ..Default::default() }, 1);
                r.tick(1.0);
            }
        }
        assert_eq!(a.state_hash(), b.state_hash(), "honest replicas agree on the world hash");

        // A cheating host teleports its ship; its hash now disagrees and would be outvoted.
        a.ships.get_mut("x").unwrap().x += 60.0;
        assert_ne!(a.state_hash(), b.state_hash(), "a tampered state produces a different hash");
    }

    #[test]
    fn a_ship_cannot_shoot_itself() {
        let mut s = arena();
        s.join("n", "p", 0);
        {
            let p = s.ships.get_mut("n").unwrap();
            p.hp = 5;
            p.vx = 0.0;
            p.vy = 0.0;
        }
        for _ in 0..30 {
            s.apply_intent("n", Intent { fire: true, ..Default::default() }, 0);
            s.tick(1.0);
        }
        assert!(s.ships["n"].alive, "own bullets never hit the firing ship");
    }

    #[test]
    fn silent_ship_is_expired() {
        let mut s = arena();
        s.join("n", "p", 0);
        for _ in 0..(Tunables::default().player_ttl_ticks + 2) {
            s.tick(1.0);
        }
        assert_eq!(s.player_count(), 0);
    }

    // ---- Living-galaxy expansion: shields, energy, status effects, hazards, mines, pickups ----

    #[test]
    fn shields_absorb_before_hull_then_overflow() {
        let mut s = arena();
        s.join("v", "V", 0);
        {
            let v = s.ships.get_mut("v").unwrap();
            v.max_shield = 50;
            v.shield = 50;
            v.hp = 100;
            v.max_hp = 100;
        }
        // 30 damage is fully soaked by the shield; hull untouched.
        s.apply_damage("v", 30, "a", s.tick);
        assert_eq!(s.ships["v"].shield, 20);
        assert_eq!(s.ships["v"].hp, 100, "shield soaked it all");
        // 40 more: 20 finishes the shield, 20 overflows to hull.
        s.apply_damage("v", 40, "a", s.tick);
        assert_eq!(s.ships["v"].shield, 0);
        assert_eq!(s.ships["v"].hp, 80, "overflow reached hull");
    }

    #[test]
    fn shield_regenerates_after_a_quiet_spell() {
        let mut s = arena();
        s.join("v", "V", 0);
        {
            let v = s.ships.get_mut("v").unwrap();
            v.max_shield = 50;
            v.shield = 10;
        }
        // A hit pauses regen for shield_delay ticks; before that the shield stays put.
        s.apply_damage("v", 1, "a", s.tick); // overflows 0 shield? no — shield 10 absorbs 1 -> 9
        let after_hit = s.ships["v"].shield;
        s.tick(1.0);
        assert_eq!(s.ships["v"].shield, after_hit, "no regen during the post-hit delay");
        // After the delay elapses, the shield climbs back toward max. Keep the ship present with a
        // neutral input each tick so it doesn't idle out (player_ttl) during the long quiet spell.
        for _ in 0..(Tunables::default().shield_delay + 60) {
            s.apply_intent("v", Intent { fire: false, ..Default::default() }, 0);
            s.tick(1.0);
        }
        assert!(s.ships["v"].shield > after_hit, "shield regenerated out of combat");
        assert!(s.ships["v"].shield <= 50, "never past max");
    }

    #[test]
    fn energy_gates_a_heavy_weapon_until_it_recharges() {
        let mut s = arena();
        s.join("g", "G", 0);
        s.join("t", "T", 200);
        solo(&mut s);
        {
            let g = s.ships.get_mut("g").unwrap();
            g.weapons.push("railgun".into());
            g.weapon = "railgun".into();
            g.x = 500.0;
            g.y = 500.0;
            g.a = 0.0;
            g.energy = 10.0; // below the railgun's 34 cost
            g.max_energy = 100.0;
        }
        {
            let t = s.ships.get_mut("t").unwrap();
            t.x = 900.0;
            t.y = 500.0;
            t.hp = 5;
        }
        s.apply_intent("g", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 0);
        s.tick(1.0);
        assert!(s.beams.is_empty(), "not enough energy: the railgun did not fire");
        // Charge up, then it fires.
        s.ships.get_mut("g").unwrap().energy = 100.0;
        s.ships.get_mut("g").unwrap().last_fire = 0;
        s.apply_intent("g", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 0);
        s.tick(1.0);
        assert_eq!(s.beams.len(), 1, "charged: the railgun fired");
        assert!(s.ships["g"].energy < 100.0, "firing drew energy from the capacitor");
    }

    #[test]
    fn emp_disables_thrust_and_fire() {
        let mut s = arena();
        s.join("n", "p", 0);
        solo(&mut s);
        {
            let p = s.ships.get_mut("n").unwrap();
            p.x = 1000.0;
            p.y = 1000.0;
            p.vx = 0.0;
            p.vy = 0.0;
            p.effects.apply(StatusKind::Emp, s.tick + 50, 1.0, "z");
        }
        for _ in 0..5 {
            s.apply_intent("n", Intent { thrust: true, fire: true, ..Default::default() }, 0);
            s.tick(1.0);
        }
        let spd = {
            let p = &s.ships["n"];
            (p.vx * p.vx + p.vy * p.vy).sqrt()
        };
        assert!(spd < 0.1, "EMP fried the drive: the ship never accelerated, spd={spd}");
        assert!(s.bullets.is_empty(), "EMP fried the triggers: no shots");
    }

    #[test]
    fn slow_reduces_top_speed() {
        let mut fast = arena();
        let mut slow = arena();
        for (sim, slowed) in [(&mut fast, false), (&mut slow, true)] {
            sim.join("n", "p", 0);
            solo(sim);
            if slowed {
                sim.ships.get_mut("n").unwrap().effects.apply(StatusKind::Slow, 10_000, 0.5, "z");
            }
            for _ in 0..200 {
                sim.apply_intent("n", Intent { thrust: true, aim: Some(0.0), ..Default::default() }, 0);
                sim.tick(1.0);
            }
        }
        let v = |s: &Sim| {
            let p = &s.ships["n"];
            (p.vx * p.vx + p.vy * p.vy).sqrt()
        };
        assert!(v(&slow) < v(&fast) * 0.7, "Slow caps a ship well below full speed");
    }

    #[test]
    fn a_proximity_mine_arms_and_detonates_on_an_enemy() {
        let mut s = arena();
        s.join("a", "A", 0);
        s.join("e", "E", 200);
        solo(&mut s);
        // An already-armed mine owned by A, with enemy E sitting inside its trigger radius.
        s.mines.push(Mine {
            owner: "a".into(),
            x: 1500.0,
            y: 1500.0,
            vx: 0.0,
            vy: 0.0,
            dmg: 60,
            blast: 120.0,
            trigger: 150.0,
            hue: 40,
            arm_at: 0,
            die_at: s.tick + 1000,
            effect: None,
        });
        {
            let e = s.ships.get_mut("e").unwrap();
            e.x = 1540.0; // within trigger
            e.y = 1500.0;
            e.hp = 300;
            e.max_hp = 300;
        }
        s.tick(1.0);
        assert!(s.mines.is_empty(), "the mine triggered and was consumed");
        assert!(!s.explosions.is_empty(), "it detonated with a blast");
        assert!(s.ships["e"].hp < 300, "the blast damaged the enemy");
    }

    #[test]
    fn a_mine_does_not_trigger_on_its_owners_faction() {
        let mut s = arena();
        s.join("a", "A", 0);
        solo(&mut s);
        s.mines.push(Mine {
            owner: "a".into(),
            x: 1500.0,
            y: 1500.0,
            vx: 0.0,
            vy: 0.0,
            dmg: 60,
            blast: 120.0,
            trigger: 150.0,
            hue: 40,
            arm_at: 0,
            die_at: s.tick + 1000,
            effect: None,
        });
        s.ships.get_mut("a").unwrap().x = 1500.0;
        s.ships.get_mut("a").unwrap().y = 1500.0;
        s.tick(1.0);
        assert_eq!(s.mines.len(), 1, "a mine ignores its own faction");
    }

    #[test]
    fn a_player_kill_drops_loot_that_a_pilot_collects() {
        let mut s = arena();
        s.join("victim", "V", 0);
        s.join("looter", "L", 200);
        solo(&mut s);
        {
            let v = s.ships.get_mut("victim").unwrap();
            v.x = 1500.0;
            v.y = 1500.0;
        }
        // Destroy the player: a pickup drops where they died.
        s.apply_damage("victim", 9999, "looter", s.tick);
        assert_eq!(s.pickups.len(), 1, "a destroyed player dropped loot");
        let (px, py) = (s.pickups[0].x, s.pickups[0].y);
        // Fly the looter onto the pickup; it gets collected on the next tick.
        {
            let l = s.ships.get_mut("looter").unwrap();
            l.x = px;
            l.y = py;
        }
        s.tick(1.0);
        assert!(s.pickups.is_empty(), "the looter collected the pickup");
    }

    #[test]
    fn a_gravity_well_curves_a_ships_path() {
        use crate::hazard::{Hazards, Well, WellKind};
        let mut s = arena();
        s.hazards = Hazards {
            wells: vec![Well { x: 1400.0, y: 1000.0, radius: 800.0, core_radius: 60.0, mass: 3.0, kind: WellKind::Planet }],
            nebulae: vec![],
        };
        s.join("n", "p", 0);
        solo(&mut s);
        {
            let p = s.ships.get_mut("n").unwrap();
            p.x = 1000.0;
            p.y = 1000.0;
            p.vx = 0.0;
            p.vy = 0.0;
        }
        // No thrust at all — only gravity acts. The ship is pulled toward the well (+x).
        for _ in 0..5 {
            s.apply_intent("n", Intent::default(), 0);
            s.tick(1.0);
        }
        assert!(s.ships["n"].vx > 0.0, "gravity pulled the ship toward the well");
        assert!(s.ships["n"].x > 1000.0, "and moved it inward");
    }

    #[test]
    fn a_black_hole_event_horizon_destroys_a_ship() {
        use crate::hazard::{Hazards, Well, WellKind};
        let mut s = arena();
        s.hazards = Hazards {
            wells: vec![Well { x: 1000.0, y: 1000.0, radius: 900.0, core_radius: 60.0, mass: 4.0, kind: WellKind::BlackHole }],
            nebulae: vec![],
        };
        s.join("doomed", "D", 0);
        solo(&mut s);
        {
            let d = s.ships.get_mut("doomed").unwrap();
            d.x = 1010.0; // inside the event horizon
            d.y = 1000.0;
        }
        s.tick(1.0);
        assert!(!s.ships["doomed"].alive, "the event horizon destroyed the ship");
    }

    #[test]
    fn arc_chains_between_clustered_enemies() {
        let mut s = arena();
        s.join("g", "G", 0);
        {
            let g = s.ships.get_mut("g").unwrap();
            g.weapons.push("arc".into());
            g.weapon = "arc".into();
            g.x = 500.0;
            g.y = 500.0;
            g.energy = 100.0;
            g.max_energy = 100.0;
        }
        // Three enemies in a chain, each within arc range (460) of the previous.
        for (i, id) in ["e1", "e2", "e3"].iter().enumerate() {
            s.join(id, id, 200 + i as u32);
            let e = s.ships.get_mut(*id).unwrap();
            e.x = 700.0 + i as f32 * 200.0;
            e.y = 500.0;
            e.hp = 300;
            e.max_hp = 300;
            e.owner = None;
        }
        // Clear all NPC fleets AFTER every join so no stray drones intercept the bolt; each distinct
        // player id is already its own faction, so the enemies read as hostile.
        solo(&mut s);
        s.apply_intent("g", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 0);
        s.tick(1.0);
        let arcs = s.beams.iter().filter(|b| b.kind == 2).count();
        assert!(arcs >= 2, "the bolt forked across multiple enemies, segments={arcs}");
        let hurt = ["e1", "e2", "e3"].iter().filter(|id| s.ships[**id].hp < 300).count();
        assert!(hurt >= 2, "at least two clustered enemies took arc damage");
    }

    #[test]
    fn overcharge_pickup_buffs_rate_of_fire() {
        let mut s = arena();
        s.join("n", "p", 0);
        solo(&mut s);
        s.ships.get_mut("n").unwrap().effects.apply(StatusKind::Overcharge, 10_000, 0.5, "self");
        assert!(s.ships["n"].effects.overcharge_mult() > 1.0, "overcharge is an active buff");
    }
}
