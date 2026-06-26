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
use crate::ruleset::{Ruleset, RulesetHandle, TechEffect, Tunables, WeaponKind};
use crate::shard::SectorId;
use crate::snapshot::ShipSnap;

/// Side length of one sector in world units. A sector's local coordinates run `0..SECTOR_SIZE`.
pub const SECTOR_SIZE: f32 = 3000.0;
/// Grid cell size for the shared deterministic asteroid field (sector-local).
pub const ROCK_CELL: f32 = 300.0;
/// Ship collision / pickup radius.
pub const SHIP_R: f32 = 18.0;
/// Legacy default max speed (kept as the reference base; live value comes from [`Tunables`]).
pub const MAX_SPEED: f32 = 7.0;
/// Legacy default thrust accel (kept as a reference base).
pub const THRUST: f32 = 0.55;
/// Legacy default damping (kept as a reference base).
pub const DAMPING: f32 = 0.94;
/// Legacy default turn rate (kept as a reference base).
pub const TURN_RATE: f32 = 0.16;
/// Base hull / max hull at spawn (reference base; live value from [`Tunables::base_hp`]).
pub const BASE_HP: i32 = 100;
/// Max name length the server accepts.
pub const MAX_NAME: usize = 16;
/// Mineral value range of an asteroid.
pub const ROCK_MIN_VAL: u32 = 5;
pub const ROCK_MAX_VAL: u32 = 30;
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
    pub alive: bool,
    #[serde(skip)]
    pub dead_at: u64,
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
            minerals: 0,
            kills: 0,
            speed_lv: 0,
            guns: 1,
            weapon: default_weapon.clone(),
            weapons: vec![default_weapon],
            owned: Vec::new(),
            alive: true,
            dead_at: 0,
            last_fire: 0,
            want_thrust: false,
            want_turn: 0,
            want_fire: false,
            last_input_tick: tick,
        }
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
            minerals: snap.minerals,
            kills: snap.kills,
            speed_lv: snap.speed_lv,
            guns: snap.guns,
            weapon: if snap.weapon.is_empty() { "blaster".into() } else { snap.weapon.clone() },
            weapons: if snap.weapons.is_empty() { vec![snap.weapon.clone()] } else { snap.weapons.clone() },
            owned: snap.owned.clone(),
            alive: snap.alive,
            dead_at: 0,
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
            minerals: self.minerals,
            kills: self.kills,
            speed_lv: self.speed_lv,
            guns: self.guns,
            weapon: self.weapon.clone(),
            weapons: self.weapons.clone(),
            owned: self.owned.clone(),
            alive: self.alive,
        }
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
    /// `0` = railgun, `1` = laser.
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
    mined: HashMap<(i32, i32), u64>,
    pub kill_feed: Vec<KillEvent>,
    /// Beams emitted this tick (railgun/laser) for the wire snapshot. Cleared each tick.
    pub beams: Vec<BeamEvent>,
    /// Ships that left this sector this tick, to be delivered to neighbours. Drained by the host.
    pub transit_out: Vec<Transit>,
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
            mined: HashMap::new(),
            kill_feed: Vec::new(),
            beams: Vec::new(),
            transit_out: Vec::new(),
        }
    }
}

impl Sim {
    pub fn new() -> Self {
        Self::default()
    }

    /// A sim for a specific sector with a specific ruleset.
    pub fn for_sector(sector: SectorId, rules: RulesetHandle) -> Self {
        Sim { sector, rules, ..Self::default() }
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

    pub fn player_count(&self) -> usize {
        self.ships.len()
    }

    fn tun(&self) -> Tunables {
        self.rules.tunables.clone()
    }

    /// Register or update a ship's identity (called on join).
    pub fn join(&mut self, id: &str, name: &str, hue: u32) {
        let tick = self.tick;
        let name = sanitize_name(name);
        let dw = self.rules.default_weapon();
        let base_hp = self.rules.tunables.base_hp;
        match self.ships.get_mut(id) {
            Some(s) => {
                s.name = name;
                s.hue = hue;
                s.last_input_tick = tick;
            }
            None => {
                self.ships.insert(id.to_string(), Ship::new(name, hue, tick, dw, base_hp));
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
            self.ships
                .insert(id.to_string(), Ship::new(sanitize_name(&name), hue_fallback, tick, dw, base_hp));
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
                // Turn (button steering; mouse-aim already applied in apply_intent).
                s.a = (s.a + s.want_turn as f32 * tun.turn_rate).rem_euclid(std::f32::consts::TAU);
                // Thrust.
                if s.want_thrust {
                    let acc = s.accel_t(&tun) * dt_scale;
                    s.vx += s.a.cos() * acc;
                    s.vy += s.a.sin() * acc;
                }
                // Damping + clamp to max speed.
                s.vx *= tun.damping;
                s.vy *= tun.damping;
                let spd = (s.vx * s.vx + s.vy * s.vy).sqrt();
                let max = s.max_speed_t(&tun);
                if spd > max {
                    let k = max / spd;
                    s.vx *= k;
                    s.vy *= k;
                }
                // Integrate position.
                s.x += s.vx * dt_scale;
                s.y += s.vy * dt_scale;

                let out = s.x < 0.0 || s.y < 0.0 || s.x >= SECTOR_SIZE || s.y >= SECTOR_SIZE;
                if out && self.seamless {
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
                if let Some(s) = self.ships.get_mut(id) {
                    s.minerals = s.minerals.saturating_add(val);
                }
            }
        }

        // --- Build the per-tick AABB broad-phase over alive ships (final positions). ---
        let tree = self.build_ship_tree();

        // --- Pass 2: weapon firing (projectile/homing spawn bullets; railgun/laser hitscan). ---
        let firing: Vec<String> = {
            let mut v: Vec<String> = self
                .ships
                .iter()
                .filter(|(_, s)| s.alive && s.want_fire)
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

    /// Fire ship `id`'s selected weapon, dispatching on its kind. Reads the live ruleset, so a hot
    /// reload changes weapon behaviour on the next shot.
    fn fire_weapon(&mut self, id: &str, now: u64, tree: &AabbTree<String>) {
        let rules = self.rules.clone();
        let (wx, wy, wa, wvx, wvy, hue0, guns, weapon) = {
            let Some(s) = self.ships.get(id) else { return };
            (s.x, s.y, s.a, s.vx, s.vy, s.hue, s.guns, s.weapon.clone())
        };
        let def = rules.weapon(&weapon).cloned().unwrap_or_else(crate::ruleset::WeaponDef::fallback);

        // Cooldown: the blaster fires faster with more barrels; other weapons use their own cooldown.
        let cooldown = if def.kind == WeaponKind::Projectile && def.id == "blaster" {
            def.cooldown.saturating_sub(guns.saturating_sub(1) as u64).max(2)
        } else {
            def.cooldown.max(1)
        };
        {
            let s = self.ships.get(id).expect("present");
            if !(s.last_fire == 0 || now.saturating_sub(s.last_fire) >= cooldown) {
                return;
            }
        }
        if let Some(s) = self.ships.get_mut(id) {
            s.last_fire = now;
        }
        let hue = ((hue0 as i32 + def.hue_shift).rem_euclid(360)) as u32;

        match def.kind {
            WeaponKind::Projectile | WeaponKind::Homing => {
                let count = if def.id == "blaster" { guns.max(1) } else { def.count.max(1) };
                let spread = def.spread;
                let homing = if def.kind == WeaponKind::Homing { def.turn_rate } else { 0.0 };
                let dmg = def.damage + if def.id == "blaster" { (guns.saturating_sub(1) as i32) * 2 } else { 0 };
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
                    });
                }
            }
            WeaponKind::Railgun => {
                let (hit, end) = self.hitscan(id, wx, wy, wa, def.range, tree);
                self.beams.push(BeamEvent { owner: id.to_string(), x0: wx, y0: wy, x1: end.0, y1: end.1, hue, kind: 0 });
                if let Some(victim) = hit {
                    self.apply_damage(&victim, def.damage, id, now);
                }
            }
            WeaponKind::Laser => {
                let (hit, end) = self.hitscan(id, wx, wy, wa, def.range, tree);
                self.beams.push(BeamEvent { owner: id.to_string(), x0: wx, y0: wy, x1: end.0, y1: end.1, hue, kind: 1 });
                if let Some(victim) = hit {
                    self.apply_damage(&victim, def.damage, id, now);
                }
            }
        }
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
            if now >= b.die_at {
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
            b.x += b.vx * dt_scale;
            b.y += b.vy * dt_scale;
            if b.x < 0.0 || b.y < 0.0 || b.x > SECTOR_SIZE || b.y > SECTOR_SIZE {
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
                self.apply_damage(&victim, b.dmg, &b.owner, now);
                continue; // bullet consumed
            }
            surviving.push(b);
        }
        self.bullets = surviving;
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

    /// Apply `dmg` from `attacker` to `victim`, handling kill, mineral drop, kill credit and feed.
    fn apply_damage(&mut self, victim: &str, dmg: i32, attacker: &str, now: u64) {
        let (killed, victim_name) = {
            let Some(v) = self.ships.get_mut(victim) else { return };
            if !v.alive {
                return;
            }
            v.hp -= dmg;
            (v.hp <= 0, v.name.clone())
        };
        if !killed {
            return;
        }
        {
            let v = self.ships.get_mut(victim).expect("present");
            v.alive = false;
            v.hp = 0;
            v.dead_at = now;
            v.minerals = 0;
            v.vx = 0.0;
            v.vy = 0.0;
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
                if d2 >= min_d * min_d || d2 <= 1e-6 {
                    continue;
                }
                let d = d2.sqrt();
                let overlap = (min_d - d) * 0.5 * tun.ship_push;
                let nx = dx / d;
                let ny = dy / d;
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
}
