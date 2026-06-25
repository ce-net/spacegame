//! Authoritative spacegame simulation — the single source of truth for one **sector**.
//!
//! The galaxy is partitioned into a grid of fixed-size **sectors** (see [`crate::shard`]); this
//! module is the authoritative simulation of *one* sector. Every ship inside the sector is owned by
//! the sector's host: the server integrates thrust/rotation at a clamped rate (a client can ask to
//! turn or thrust, but only the server decides how far the ship actually moves), enforces sector
//! bounds, spawns and integrates bullets, decides hits and kills, and runs mining and upgrades.
//! Clients never mutate this state directly — they send *intents* (thrust/turn/fire/build) and the
//! server does everything else, which is what makes it cheat-resistant: a client cannot teleport,
//! exceed [`MAX_SPEED`], fabricate a kill, or mine an asteroid it is not touching.
//!
//! The asteroid field is a **deterministic hash field**: every grid cell hosts at most one asteroid
//! at a stable sub-cell position and value derived from a hash of its galaxy coordinates. Both the
//! server and the renderer compute the identical field with zero shared state, so the server only
//! tracks which asteroids are on mined-cooldown. Mining is authoritative; the field is shared.
//!
//! The simulation is pure and deterministic: same inputs in, same state out. It has no knowledge of
//! the mesh, the network, or wall-clock time — [`Sim::tick`] advances exactly one fixed step. That
//! makes it fully unit-testable (see the tests at the bottom) and makes failover seamless (a host
//! that restores a [`crate::snapshot::SectorSnapshot`] evolves identically to the one it replaced).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Side length of one sector in world units. A sector's local coordinates run `0..SECTOR_SIZE`.
pub const SECTOR_SIZE: f32 = 3000.0;
/// Grid cell size for the shared deterministic asteroid field (sector-local).
pub const ROCK_CELL: f32 = 300.0;
/// Ship collision / pickup radius.
pub const SHIP_R: f32 = 18.0;
/// Base maximum speed (world units / tick) before thruster upgrades.
pub const MAX_SPEED: f32 = 7.0;
/// Thrust acceleration per tick (before upgrades).
pub const THRUST: f32 = 0.55;
/// Per-tick velocity damping (space drag, so ships coast but settle).
pub const DAMPING: f32 = 0.94;
/// Max angular velocity (radians / tick) a ship may turn.
pub const TURN_RATE: f32 = 0.16;
/// Bullet speed (world units / tick).
pub const BULLET_SPEED: f32 = 26.0;
/// Bullet lifetime in ticks (~1.1s at 20 Hz).
pub const BULLET_TTL: u64 = 22;
/// Ticks between shots at gun level 1 (faster with more guns).
pub const FIRE_COOLDOWN: u64 = 5;
/// Base hull / max hull at spawn.
pub const BASE_HP: i32 = 100;
/// Ticks a destroyed ship stays dead before it may respawn (~3s at 20 Hz).
pub const RESPAWN_TICKS: u64 = 64;
/// Ticks a mined asteroid stays depleted before it regenerates (~30s at 20 Hz).
pub const ROCK_REGEN_TICKS: u64 = 600;
/// A ship with no input for this many ticks leaves the sector (~5s at 20 Hz).
pub const PLAYER_TTL_TICKS: u64 = 100;
/// Max name length the server accepts.
pub const MAX_NAME: usize = 16;
/// Mineral value range of an asteroid.
pub const ROCK_MIN_VAL: u32 = 5;
pub const ROCK_MAX_VAL: u32 = 30;
/// Damage one bullet deals at gun level 1 (scales with guns).
pub const BULLET_DMG: i32 = 9;

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

/// An asteroid in a sector-local grid cell, or `None` if the cell hosts none / it falls outside the
/// sector. Computed identically on client and server from the cell coordinates alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rock {
    pub x: f32,
    pub y: f32,
    /// Mineral value when mined out.
    pub val: u32,
    /// Hull of the rock (how many bullet hits / mining ticks to destroy).
    pub hp: u32,
    pub cx: i32,
    pub cy: i32,
}

/// The deterministic asteroid (if any) for sector-local grid cell `(cx, cy)`. Identical formula to
/// the frontend's `rockInCell`, so client and server see the same field with zero shared state.
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

/// A ship's authoritative state. Position, velocity, hull, minerals and kills are owned by the
/// server. The hull/speed/guns upgrade levels gate combat power and are bought with minerals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ship {
    pub name: String,
    /// Color hue 0..360, derived from the NodeId, so it is stable and unspoofable.
    pub hue: u32,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    /// Heading in radians.
    pub a: f32,
    pub hp: i32,
    pub max_hp: i32,
    pub minerals: u32,
    pub kills: u32,
    /// Thruster upgrade level (raises max speed & accel).
    pub speed_lv: u32,
    /// Number of guns (1..=5).
    pub guns: u32,
    /// `false` while dead (respawn cooldown running).
    pub alive: bool,
    /// Tick the ship died (for respawn cooldown).
    #[serde(skip)]
    pub dead_at: u64,
    /// Tick of last successful fire (for fire cooldown).
    #[serde(skip)]
    pub last_fire: u64,
    /// Last requested thrust (0/1) and turn (-1/0/1) this tick.
    #[serde(skip)]
    pub want_thrust: bool,
    #[serde(skip)]
    pub want_turn: i32,
    #[serde(skip)]
    pub want_fire: bool,
    /// Tick of the ship's last input — used to expire silent ships.
    #[serde(skip)]
    pub last_input_tick: u64,
}

impl Ship {
    fn new(name: String, hue: u32, tick: u64) -> Self {
        // Spawn at a deterministic-ish spot from the hue so two ships rarely overlap.
        let off = (hue as f32 / 360.0 - 0.5) * SECTOR_SIZE * 0.5;
        Ship {
            name,
            hue,
            x: SECTOR_SIZE / 2.0 + off,
            y: SECTOR_SIZE / 2.0 - off,
            vx: 0.0,
            vy: 0.0,
            a: -std::f32::consts::FRAC_PI_2,
            hp: BASE_HP,
            max_hp: BASE_HP,
            minerals: 0,
            kills: 0,
            speed_lv: 0,
            guns: 1,
            alive: true,
            dead_at: 0,
            last_fire: 0,
            want_thrust: false,
            want_turn: 0,
            want_fire: false,
            last_input_tick: tick,
        }
    }

    /// Restore a ship from a replication snapshot (live-only fields reset to neutral, so a host that
    /// takes over after a failover continues exactly where the old host left off). Used by
    /// [`crate::snapshot::SectorSnapshot::restore`].
    #[allow(clippy::too_many_arguments)]
    pub fn from_snap(
        name: String,
        hue: u32,
        x: f32,
        y: f32,
        vx: f32,
        vy: f32,
        a: f32,
        hp: i32,
        max_hp: i32,
        minerals: u32,
        kills: u32,
        speed_lv: u32,
        guns: u32,
        alive: bool,
        tick: u64,
    ) -> Self {
        Ship {
            name,
            hue,
            x,
            y,
            vx,
            vy,
            a,
            hp,
            max_hp,
            minerals,
            kills,
            speed_lv,
            guns,
            alive,
            dead_at: 0,
            last_fire: 0,
            want_thrust: false,
            want_turn: 0,
            want_fire: false,
            last_input_tick: tick,
        }
    }

    /// Effective max speed after thruster upgrades.
    pub fn max_speed(&self) -> f32 {
        MAX_SPEED * (1.0 + self.speed_lv as f32 * 0.16)
    }

    /// Effective thrust accel after thruster upgrades.
    pub fn accel(&self) -> f32 {
        THRUST * (1.0 + self.speed_lv as f32 * 0.16)
    }

    /// Damage dealt per bullet at this gun level.
    pub fn bullet_dmg(&self) -> i32 {
        BULLET_DMG + (self.guns.saturating_sub(1) as i32) * 2
    }

    /// Fire cooldown (ticks) — faster with more guns.
    pub fn fire_cooldown(&self) -> u64 {
        FIRE_COOLDOWN.saturating_sub((self.guns.saturating_sub(1)) as u64).max(2)
    }
}

/// A live bullet. Owned by the firing ship; the server integrates and collides it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bullet {
    /// NodeId of the firing ship (kill attribution).
    pub owner: String,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub dmg: i32,
    pub hue: u32,
    /// Tick the bullet expires.
    pub die_at: u64,
}

/// A one-off kill event the host emits when a ship is destroyed, for the kill feed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KillEvent {
    pub killer: String,
    pub killer_name: String,
    pub victim: String,
    pub victim_name: String,
    pub tick: u64,
}

/// An upgrade the server may apply, bought with minerals. Costs are server-enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upgrade {
    /// +40 max hull, full repair.
    Hull,
    /// +16% top speed & accel.
    Thruster,
    /// +1 gun (faster fire), capped at 5.
    Gun,
}

impl Upgrade {
    /// Parse the wire token used by the frontend's build menu.
    pub fn from_token(s: &str) -> Option<Upgrade> {
        match s {
            "hull" => Some(Upgrade::Hull),
            "speed" | "thruster" => Some(Upgrade::Thruster),
            "gun" => Some(Upgrade::Gun),
            _ => None,
        }
    }

    /// Mineral cost given the ship's current level of this upgrade. Server-authoritative pricing.
    pub fn cost(self, ship: &Ship) -> u32 {
        match self {
            Upgrade::Hull => {
                let lv = ((ship.max_hp - BASE_HP) / 40).max(0) as u32;
                30 + lv * 25
            }
            Upgrade::Thruster => 25 + ship.speed_lv * 22,
            Upgrade::Gun => {
                let lv = ship.guns.saturating_sub(1);
                40 + lv * 40
            }
        }
    }

    /// True if the ship is already maxed out on this upgrade.
    pub fn maxed(self, ship: &Ship) -> bool {
        match self {
            Upgrade::Hull => ship.max_hp >= BASE_HP + 40 * 6,
            Upgrade::Thruster => ship.speed_lv >= 6,
            Upgrade::Gun => ship.guns >= 5,
        }
    }
}

/// A single ship's intent for the next tick. Movement magnitude, firing, and upgrade legality are
/// all decided by the server, not the client.
#[derive(Debug, Clone, Default)]
pub struct Intent {
    pub thrust: bool,
    /// -1 (left), 0, or +1 (right).
    pub turn: i32,
    pub fire: bool,
    /// Absolute heading the client wants (mouse aim); the server clamps the turn toward it.
    pub aim: Option<f32>,
    pub name: Option<String>,
}

fn sanitize_name(name: &str) -> String {
    let n: String = name.trim().chars().take(MAX_NAME).collect();
    if n.is_empty() { "pilot".to_string() } else { n }
}

/// The authoritative state of one sector.
#[derive(Debug, Clone, Default)]
pub struct Sim {
    pub tick: u64,
    /// Ship id (NodeId hex) -> ship state.
    pub ships: HashMap<String, Ship>,
    /// Live bullets.
    pub bullets: Vec<Bullet>,
    /// Asteroid cell `(cx, cy)` -> tick it was mined out (regen cooldown).
    mined: HashMap<(i32, i32), u64>,
    /// Kill events emitted since the last drain (for the wire snapshot feed).
    pub kill_feed: Vec<KillEvent>,
}

impl Sim {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn player_count(&self) -> usize {
        self.ships.len()
    }

    /// Register or update a ship's identity (called on join). `hue` is derived from the NodeId by
    /// the caller, so it cannot be spoofed.
    pub fn join(&mut self, id: &str, name: &str, hue: u32) {
        let tick = self.tick;
        let name = sanitize_name(name);
        match self.ships.get_mut(id) {
            Some(s) => {
                s.name = name;
                s.hue = hue;
                s.last_input_tick = tick;
            }
            None => {
                self.ships.insert(id.to_string(), Ship::new(name, hue, tick));
            }
        }
    }

    pub fn leave(&mut self, id: &str) {
        self.ships.remove(id);
    }

    /// Record a ship's intent for the upcoming tick. An intent from an unknown ship auto-joins it.
    pub fn apply_intent(&mut self, id: &str, intent: Intent, hue_fallback: u32) {
        let tick = self.tick;
        if !self.ships.contains_key(id) {
            let name = intent.name.clone().unwrap_or_else(|| "pilot".into());
            self.ships.insert(id.to_string(), Ship::new(sanitize_name(&name), hue_fallback, tick));
        }
        if let Some(s) = self.ships.get_mut(id) {
            if let Some(n) = intent.name {
                s.name = sanitize_name(&n);
            }
            s.want_thrust = intent.thrust;
            s.want_turn = intent.turn.clamp(-1, 1);
            s.want_fire = intent.fire;
            if let Some(aim) = intent.aim {
                // Turn toward the requested heading at the clamped turn rate (server decides how far).
                let mut d = (aim - s.a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
                    - std::f32::consts::PI;
                d = d.clamp(-TURN_RATE, TURN_RATE);
                s.a = (s.a + d).rem_euclid(std::f32::consts::TAU);
            }
            s.last_input_tick = tick;
        }
    }

    /// Buy an upgrade for a ship. Server-enforced: must afford it, must not be maxed. Returns true on
    /// success. A client that asks for an upgrade it cannot pay for is simply ignored.
    pub fn buy(&mut self, id: &str, up: Upgrade) -> bool {
        let Some(s) = self.ships.get_mut(id) else { return false };
        if up.maxed(s) {
            return false;
        }
        let cost = up.cost(s);
        if s.minerals < cost {
            return false;
        }
        s.minerals -= cost;
        match up {
            Upgrade::Hull => {
                s.max_hp += 40;
                s.hp = s.max_hp;
            }
            Upgrade::Thruster => {
                s.speed_lv += 1;
            }
            Upgrade::Gun => {
                s.guns = (s.guns + 1).min(5);
            }
        }
        true
    }

    /// Request a respawn for a dead ship whose cooldown has elapsed. Returns true if respawned.
    pub fn respawn(&mut self, id: &str) -> bool {
        let now = self.tick;
        let Some(s) = self.ships.get_mut(id) else { return false };
        if s.alive {
            return false;
        }
        if now.saturating_sub(s.dead_at) < RESPAWN_TICKS {
            return false;
        }
        let hue = s.hue;
        let name = s.name.clone();
        let kills = s.kills;
        // Respawn keeps identity and kill count but resets ship and bankrupts on death (minerals are
        // already cleared at death time), upgrades reset to base — a clean restart.
        let mut fresh = Ship::new(name, hue, now);
        fresh.kills = kills;
        *s = fresh;
        true
    }

    /// Advance the simulation exactly one fixed tick. The authoritative loop always passes 1.0.
    pub fn tick(&mut self, dt_scale: f32) {
        self.tick += 1;
        let now = self.tick;
        self.kill_feed.clear();

        // 1) Integrate ship motion, expire silent ships, fire bullets, mine asteroids.
        let ids: Vec<String> = self.ships.keys().cloned().collect();
        for id in &ids {
            let drop = {
                let s = &self.ships[id];
                now.saturating_sub(s.last_input_tick) > PLAYER_TTL_TICKS
            };
            if drop {
                self.ships.remove(id);
                continue;
            }

            let mut new_bullets: Vec<Bullet> = Vec::new();
            let mut mined_now: Vec<(i32, i32, u32, f32, f32)> = Vec::new();
            {
                let s = self.ships.get_mut(id).expect("present");
                if !s.alive {
                    continue;
                }
                // Turn (button steering; mouse-aim already applied in apply_intent).
                s.a = (s.a + s.want_turn as f32 * TURN_RATE).rem_euclid(std::f32::consts::TAU);
                // Thrust.
                if s.want_thrust {
                    let acc = s.accel() * dt_scale;
                    s.vx += s.a.cos() * acc;
                    s.vy += s.a.sin() * acc;
                }
                // Damping + clamp to max speed.
                s.vx *= DAMPING;
                s.vy *= DAMPING;
                let spd = (s.vx * s.vx + s.vy * s.vy).sqrt();
                let max = s.max_speed();
                if spd > max {
                    let k = max / spd;
                    s.vx *= k;
                    s.vy *= k;
                }
                // Integrate position; ships bounce off sector walls (sector is its own arena).
                s.x += s.vx * dt_scale;
                s.y += s.vy * dt_scale;
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

                // Fire. A ship that has never fired (`last_fire == 0`) may fire immediately;
                // otherwise the per-gun cooldown gates the rate.
                if s.want_fire && (s.last_fire == 0 || now.saturating_sub(s.last_fire) >= s.fire_cooldown()) {
                    s.last_fire = now;
                    let dmg = s.bullet_dmg();
                    let guns = s.guns.max(1);
                    let spread = if guns > 1 { 0.12 } else { 0.0 };
                    for g in 0..guns {
                        let off = if guns > 1 {
                            (g as f32 - (guns as f32 - 1.0) / 2.0) * spread
                        } else {
                            0.0
                        };
                        let a = s.a + off;
                        new_bullets.push(Bullet {
                            owner: id.clone(),
                            x: s.x + a.cos() * (SHIP_R + 4.0),
                            y: s.y + a.sin() * (SHIP_R + 4.0),
                            vx: a.cos() * BULLET_SPEED + s.vx,
                            vy: a.sin() * BULLET_SPEED + s.vy,
                            dmg,
                            hue: s.hue,
                            die_at: now + BULLET_TTL,
                        });
                    }
                }

                // Mining: a ship overlapping a live asteroid mines it out and banks its minerals.
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
                            && now.saturating_sub(t) < ROCK_REGEN_TICKS
                        {
                            continue; // still depleted
                        }
                        let ddx = r.x - sx;
                        let ddy = r.y - sy;
                        if ddx * ddx + ddy * ddy <= reach * reach {
                            mined_now.push((cx, cy, r.val, r.x, r.y));
                        }
                    }
                }

                // One-shot inputs: each client input frame applies to exactly one tick. Resetting
                // here means a ship with no fresh input coasts (no sticky thrust/fire) and — crucially
                // — a snapshot-restored ship (whose live input fields reset to neutral) evolves
                // identically to the original, which is what makes failover deterministic.
                s.want_thrust = false;
                s.want_turn = 0;
                s.want_fire = false;
            }

            self.bullets.extend(new_bullets);
            for (cx, cy, val, _, _) in mined_now {
                self.mined.insert((cx, cy), now);
                if let Some(s) = self.ships.get_mut(id) {
                    s.minerals = s.minerals.saturating_add(val);
                }
            }
        }

        // 2) Integrate bullets and resolve ship hits (authoritative combat + kills).
        let mut surviving: Vec<Bullet> = Vec::with_capacity(self.bullets.len());
        // Take ownership so we can mutate ships while iterating bullets.
        let bullets = std::mem::take(&mut self.bullets);
        for mut b in bullets {
            if now >= b.die_at {
                continue;
            }
            b.x += b.vx * dt_scale;
            b.y += b.vy * dt_scale;
            // Out of the sector: drop it (cross-sector bullets are not modeled; sectors are
            // independent cells — a clean concurrency boundary).
            if b.x < 0.0 || b.y < 0.0 || b.x > SECTOR_SIZE || b.y > SECTOR_SIZE {
                continue;
            }
            // Collide with the nearest hittable ship (not the owner, alive).
            let mut hit_target: Option<String> = None;
            for (sid, s) in self.ships.iter() {
                if !s.alive || *sid == b.owner {
                    continue;
                }
                let dx = s.x - b.x;
                let dy = s.y - b.y;
                if dx * dx + dy * dy <= (SHIP_R + 4.0) * (SHIP_R + 4.0) {
                    hit_target = Some(sid.clone());
                    break;
                }
            }
            if let Some(victim_id) = hit_target {
                let (killed, victim_name) = {
                    let v = self.ships.get_mut(&victim_id).expect("present");
                    v.hp -= b.dmg;
                    (v.hp <= 0, v.name.clone())
                };
                if killed {
                    // Mark victim dead, drop their minerals.
                    {
                        let v = self.ships.get_mut(&victim_id).expect("present");
                        v.alive = false;
                        v.hp = 0;
                        v.dead_at = now;
                        v.minerals = 0;
                        v.vx = 0.0;
                        v.vy = 0.0;
                    }
                    // Credit the killer (if still present).
                    let killer_name = if let Some(k) = self.ships.get_mut(&b.owner) {
                        k.kills += 1;
                        k.name.clone()
                    } else {
                        "unknown".to_string()
                    };
                    self.kill_feed.push(KillEvent {
                        killer: b.owner.clone(),
                        killer_name,
                        victim: victim_id,
                        victim_name,
                        tick: now,
                    });
                }
                // Bullet is consumed on hit.
                continue;
            }
            surviving.push(b);
        }
        self.bullets = surviving;

        // 3) GC the mined-cooldown map so it can't grow without bound.
        if self.mined.len() > 4096 {
            self.mined.retain(|_, &mut t| now.saturating_sub(t) < ROCK_REGEN_TICKS);
        }
    }

    /// The full mined-cooldown map as `((cx, cy), mined_at_tick)` pairs (including expired entries,
    /// GC'd lazily). Captured verbatim by [`crate::snapshot::SectorSnapshot`] so a restored host
    /// treats the same asteroids as depleted. Distinct from [`depleted_cells`], which filters to the
    /// still-active cooldowns for the wire snapshot.
    pub fn mined_cells(&self) -> Vec<((i32, i32), u64)> {
        self.mined.iter().map(|(&k, &t)| (k, t)).collect()
    }

    /// Replace the mined-cooldown map wholesale — inverse of [`mined_cells`], used by snapshot
    /// restore to rebuild authoritative state after failover.
    pub fn set_mined(&mut self, entries: impl IntoIterator<Item = ((i32, i32), u64)>) {
        self.mined = entries.into_iter().collect();
    }

    /// Asteroid cells still depleted (so clients dim/hide them): `(cx, cy, ticks_remaining)`.
    pub fn depleted_cells(&self) -> Vec<(i32, i32, u64)> {
        let now = self.tick;
        self.mined
            .iter()
            .filter(|&(_, &t)| now.saturating_sub(t) < ROCK_REGEN_TICKS)
            .map(|(&(cx, cy), &t)| (cx, cy, ROCK_REGEN_TICKS - now.saturating_sub(t)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rock_field_is_deterministic() {
        for cx in -4..8 {
            for cy in -4..8 {
                assert_eq!(rock_in_cell(cx, cy), rock_in_cell(cx, cy));
            }
        }
    }

    #[test]
    fn rock_field_has_rocks_and_gaps() {
        let mut rocks = 0;
        let mut gaps = 0;
        for cx in 0..20 {
            for cy in 0..20 {
                if rock_in_cell(cx, cy).is_some() {
                    rocks += 1;
                } else {
                    gaps += 1;
                }
            }
        }
        assert!(rocks > 0 && gaps > 0);
    }

    #[test]
    fn join_then_tick_keeps_ship() {
        let mut s = Sim::new();
        s.join("nodeA", "Ace", 120);
        s.tick(1.0);
        assert_eq!(s.player_count(), 1);
        assert_eq!(s.ships["nodeA"].name, "Ace");
        assert_eq!(s.ships["nodeA"].hue, 120);
    }

    #[test]
    fn server_clamps_max_speed() {
        // Thrusting forever can never exceed the ship's max speed: the server clamps it.
        let mut s = Sim::new();
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
    fn ship_stays_inside_sector() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        // Aim down-right and burn for a long time; the ship must never leave the sector.
        for _ in 0..400 {
            s.apply_intent("n", Intent { thrust: true, aim: Some(0.7), ..Default::default() }, 0);
            s.tick(1.0);
            let p = &s.ships["n"];
            assert!(p.x >= 0.0 && p.x <= SECTOR_SIZE);
            assert!(p.y >= 0.0 && p.y <= SECTOR_SIZE);
        }
    }

    #[test]
    fn mining_an_asteroid_banks_its_value() {
        let mut s = Sim::new();
        let rock = (0..40)
            .flat_map(|cx| (0..40).map(move |cy| (cx, cy)))
            .find_map(|(cx, cy)| rock_in_cell(cx, cy))
            .expect("a rock exists");
        s.join("n", "p", 0);
        {
            let p = s.ships.get_mut("n").unwrap();
            p.x = rock.x;
            p.y = rock.y;
        }
        s.tick(1.0);
        assert_eq!(s.ships["n"].minerals, rock.val, "should bank the rock's value");
        // Same rock is now depleted and is not re-mined immediately.
        s.tick(1.0);
        assert_eq!(s.ships["n"].minerals, rock.val, "depleted rock is not mined again");
    }

    #[test]
    fn buying_upgrades_is_server_priced_and_gated() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        // No minerals -> cannot buy.
        assert!(!s.buy("n", Upgrade::Gun));
        s.ships.get_mut("n").unwrap().minerals = 1000;
        // Buying a gun raises the gun count and debits the exact server cost.
        let before = s.ships["n"].minerals;
        let cost = Upgrade::Gun.cost(&s.ships["n"]);
        assert!(s.buy("n", Upgrade::Gun));
        assert_eq!(s.ships["n"].guns, 2);
        assert_eq!(s.ships["n"].minerals, before - cost);
        // Hull buy fully repairs and raises max hull.
        s.ships.get_mut("n").unwrap().hp = 10;
        assert!(s.buy("n", Upgrade::Hull));
        assert_eq!(s.ships["n"].hp, s.ships["n"].max_hp);
        assert_eq!(s.ships["n"].max_hp, BASE_HP + 40);
    }

    #[test]
    fn gun_caps_at_five() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        s.ships.get_mut("n").unwrap().minerals = 100000;
        for _ in 0..10 {
            s.buy("n", Upgrade::Gun);
        }
        assert_eq!(s.ships["n"].guns, 5);
        assert!(Upgrade::Gun.maxed(&s.ships["n"]));
    }

    #[test]
    fn firing_spawns_bullets_and_respects_cooldown() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        s.apply_intent("n", Intent { fire: true, ..Default::default() }, 0);
        s.tick(1.0);
        assert_eq!(s.bullets.len(), 1, "one gun fires one bullet");
        // Immediate refire is on cooldown -> no new bullet this tick (the first may still be alive).
        let before = s.bullets.len();
        s.apply_intent("n", Intent { fire: true, ..Default::default() }, 0);
        s.tick(1.0);
        // No *additional* bullet was fired (count did not increase beyond surviving ones).
        assert!(s.bullets.len() <= before, "fire cooldown prevents a second immediate shot");
    }

    #[test]
    fn bullets_kill_and_credit_the_shooter() {
        let mut s = Sim::new();
        s.join("killer", "K", 10);
        s.join("victim", "V", 20);
        // Place victim right in front of the killer, low on hull, and fire repeatedly.
        {
            let k = s.ships.get_mut("killer").unwrap();
            k.x = 1000.0;
            k.y = 1000.0;
            k.a = 0.0;
            k.vx = 0.0;
            k.vy = 0.0;
        }
        {
            let v = s.ships.get_mut("victim").unwrap();
            v.x = 1000.0 + SHIP_R + 8.0;
            v.y = 1000.0;
            v.hp = 5;
            v.vx = 0.0;
            v.vy = 0.0;
        }
        // Keep killer steady and firing until the kill registers.
        let mut killed = false;
        for _ in 0..40 {
            {
                let k = s.ships.get_mut("killer").unwrap();
                k.x = 1000.0;
                k.y = 1000.0;
                k.a = 0.0;
            }
            {
                let v = s.ships.get_mut("victim").unwrap();
                v.x = 1000.0 + SHIP_R + 8.0;
                v.y = 1000.0;
            }
            s.apply_intent("killer", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
            s.tick(1.0);
            if !s.kill_feed.is_empty() {
                killed = true;
                break;
            }
        }
        assert!(killed, "the victim should be destroyed by sustained fire");
        assert_eq!(s.ships["killer"].kills, 1, "killer is credited");
        assert!(!s.ships["victim"].alive, "victim is dead");
        let ev = &s.kill_feed[0];
        assert_eq!(ev.killer, "killer");
        assert_eq!(ev.victim, "victim");
    }

    #[test]
    fn a_ship_cannot_shoot_itself() {
        let mut s = Sim::new();
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
    fn dead_ship_respawns_only_after_cooldown() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        {
            let p = s.ships.get_mut("n").unwrap();
            p.alive = false;
            p.dead_at = s.tick;
            p.kills = 3;
        }
        assert!(!s.respawn("n"), "cannot respawn during cooldown");
        for _ in 0..(RESPAWN_TICKS + 1) {
            s.tick(1.0);
        }
        assert!(s.respawn("n"), "respawns after cooldown");
        assert!(s.ships["n"].alive);
        assert_eq!(s.ships["n"].kills, 3, "kills carry across respawn");
        assert_eq!(s.ships["n"].hp, s.ships["n"].max_hp);
    }

    #[test]
    fn silent_ship_is_expired() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        for _ in 0..(PLAYER_TTL_TICKS + 2) {
            s.tick(1.0);
        }
        assert_eq!(s.player_count(), 0, "silent ship leaves the sector");
    }

    #[test]
    fn bullets_expire() {
        let mut s = Sim::new();
        s.join("n", "p", 0);
        s.apply_intent("n", Intent { fire: true, ..Default::default() }, 0);
        s.tick(1.0);
        assert!(!s.bullets.is_empty());
        for _ in 0..(BULLET_TTL + 2) {
            s.tick(1.0);
        }
        assert!(s.bullets.is_empty(), "bullets expire after their TTL");
    }
}
