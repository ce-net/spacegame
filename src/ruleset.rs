//! The **Ruleset** — the entire tunable definition of the game as hot-swappable *data*, so the game
//! can be developed, balanced and expanded **while people are playing** and the change reaches every
//! live host and every live client instantly, with no restart and no dropped session.
//!
//! Everything a designer touches lives here as serializable data rather than hard-coded constants:
//! the weapon catalogue (blasters, **homing missiles**, **railguns**, **lasers**), the **tech tree**
//! that unlocks them, the physics/economy [`Tunables`], and an opaque [`Assets`] blob the *frontend*
//! interprets (shader source, sprite params, palettes). The authoritative [`crate::sim::Sim`] holds an
//! `Arc<Ruleset>` and reads it every tick, so swapping that `Arc` ([`crate::sim::Sim::apply_ruleset`])
//! changes the live game between one tick and the next — new weapon stats, a new item, a re-balanced
//! tech node, a tweaked shader all take effect immediately for ships already in flight.
//!
//! ## How a change goes live (the hot-reload path)
//!
//! 1. A designer edits a `ruleset.json` (or the dev host watches the file) and bumps [`Ruleset::version`].
//! 2. The host `put_object`s the new ruleset (content-addressed) and publishes its CID on the galaxy
//!    **config topic** (see [`crate::director::ruleset_topic`]).
//! 3. Every sector host subscribed to that topic fetches the object and calls `apply_ruleset` — the
//!    authoritative simulation re-balances live.
//! 4. Every client subscribed to the same topic fetches the same object and hot-applies the [`Assets`]
//!    (recompiling shaders, re-reading weapon visuals) without reloading the page.
//!
//! Higher [`Ruleset::version`] wins, so the swap is monotonic and order-independent across the mesh: a
//! late-arriving older version is ignored, and a node that missed an update catches up from the latest
//! published CID. Because the object is content-addressed, every node that fetched it is a CDN edge for
//! it — a million clients pulling a new shader do not hammer one origin.
//!
//! This module is pure (no clock, no mesh) and unit-tested. The mesh I/O that distributes it lives in
//! [`crate::director`]; the file-watch loop lives in [`crate::run_sector`].

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// How a weapon delivers damage. The authoritative simulation dispatches firing on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeaponKind {
    /// Ballistic projectile(s): spawn `count` bullets along the heading with `spread`, each flying
    /// straight at `speed` until `ttl` or a hit. The classic blaster / shotgun.
    Projectile,
    /// Homing missile(s): like a projectile but each round **steers toward the nearest enemy** every
    /// tick at `turn_rate` rad/tick. Slower, but it chases.
    Homing,
    /// Railgun: an **instant hitscan ray** along the heading up to `range`. The first ship on the ray
    /// takes `damage` at once; a beam from the muzzle to the hit point is emitted for rendering. High
    /// damage, long `cooldown`.
    Railgun,
    /// Laser: a **continuous beam** while the trigger is held. Every tick it deals `damage` to the
    /// first ship within `range` along the heading and emits a short beam. Low per-tick damage, near-
    /// zero `cooldown` — a sustained DPS weapon.
    Laser,
}

/// One weapon in the catalogue. All combat numbers are data so they can be tuned live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeaponDef {
    /// Stable id referenced by ships (`Ship::weapon`), the tech tree, and the frontend's art.
    pub id: String,
    /// Display name in the loadout UI.
    pub name: String,
    pub kind: WeaponKind,
    /// Ticks between shots (rate of fire). `1` ≈ every tick.
    pub cooldown: u64,
    /// Damage per projectile / per hitscan hit / per laser tick.
    pub damage: i32,
    /// Projectile speed (world units/tick) for [`WeaponKind::Projectile`] / [`WeaponKind::Homing`].
    pub speed: f32,
    /// Projectile lifetime in ticks (projectile/homing).
    pub ttl: u64,
    /// Hitscan reach in world units for [`WeaponKind::Railgun`] / [`WeaponKind::Laser`].
    pub range: f32,
    /// Rounds emitted per trigger pull (pellets / missile salvo).
    pub count: u32,
    /// Angular spread of a salvo, radians (total cone).
    pub spread: f32,
    /// Homing steer rate, radians/tick (only [`WeaponKind::Homing`]).
    pub turn_rate: f32,
    /// Visual hue offset the frontend may apply to this weapon's shots/beam.
    pub hue_shift: i32,
}

impl WeaponDef {
    /// A sane fallback weapon, used when a ship references a weapon id that the current ruleset no
    /// longer defines (e.g. a weapon removed by a live edit). Keeps a mid-flight ship firing rather
    /// than silently disarmed.
    pub fn fallback() -> WeaponDef {
        WeaponDef {
            id: "blaster".into(),
            name: "Blaster".into(),
            kind: WeaponKind::Projectile,
            cooldown: 5,
            damage: 9,
            speed: 26.0,
            ttl: 22,
            range: 0.0,
            count: 1,
            spread: 0.0,
            turn_rate: 0.0,
            hue_shift: 0,
        }
    }
}

/// What buying a tech node does to a ship. Effects are applied once, when the node is purchased.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum TechEffect {
    /// Unlock a weapon id so the ship may select and fire it.
    UnlockWeapon { weapon: String },
    /// Add to max hull and fully repair.
    AddHull { amount: i32 },
    /// Add thruster levels (top speed & accel).
    AddThruster { levels: u32 },
    /// Add blaster barrels (the legacy multi-gun spread), capped by [`Tunables::max_guns`].
    AddGun { count: u32 },
}

/// One node of the **tech tree**. A node is buyable with minerals once its `requires` prerequisites
/// are owned; buying it applies its `effect`. Expanding the tree is purely additive data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TechNode {
    /// Stable id (also what `Ship::unlocked` records once bought).
    pub id: String,
    pub name: String,
    /// Mineral cost.
    pub cost: u32,
    /// Tech ids that must already be owned before this node can be bought. Empty = always available.
    #[serde(default)]
    pub requires: Vec<String>,
    pub effect: TechEffect,
}

/// Physics / economy knobs. Defaults reproduce the original hard-coded constants, so an empty or
/// minimal ruleset plays exactly like the classic arena; a live edit re-tunes feel instantly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tunables {
    /// Base max speed (world units/tick) before thruster upgrades.
    pub max_speed: f32,
    /// Thrust accel per tick before upgrades.
    pub thrust: f32,
    /// Per-tick velocity damping.
    pub damping: f32,
    /// Max turn rate, radians/tick.
    pub turn_rate: f32,
    /// Each thruster level adds this fraction to speed & accel.
    pub thruster_step: f32,
    /// Hull at spawn.
    pub base_hp: i32,
    /// Respawn cooldown, ticks.
    pub respawn_ticks: u64,
    /// Mined-asteroid regen cooldown, ticks.
    pub rock_regen_ticks: u64,
    /// Idle ticks before a silent ship leaves the sector.
    pub player_ttl_ticks: u64,
    /// Max blaster barrels (the legacy gun cap).
    pub max_guns: u32,
    /// Ship↔ship collision push stiffness (0 disables ship collision physics).
    pub ship_push: f32,
    /// Max live NPC fleet ships a single faction may field at once (bounds the simulation cost a busy
    /// faction can impose; excess roster units wait until a slot frees).
    #[serde(default = "default_max_fleet")]
    pub max_fleet: u32,
}

fn default_max_fleet() -> u32 {
    24
}

impl Default for Tunables {
    fn default() -> Self {
        Tunables {
            max_speed: 7.0,
            thrust: 0.55,
            damping: 0.94,
            turn_rate: 0.16,
            thruster_step: 0.16,
            base_hp: 100,
            respawn_ticks: 64,
            rock_regen_ticks: 600,
            player_ttl_ticks: 100,
            max_guns: 5,
            ship_push: 0.5,
            max_fleet: 24,
        }
    }
}

/// Opaque, frontend-only payload — the backend never interprets it, it just distributes it. This is
/// how a **shader tweak or new sprite goes live mid-match**: the designer changes a shader string, the
/// version bumps, the blob rides the same hot-reload path, and the client recompiles its shaders on
/// receipt. Kept as a free-form JSON map so the renderer can evolve without a backend change.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Assets {
    /// Named shader sources (e.g. `"ship"`, `"thruster"`, `"nebula"` -> GLSL/WGSL text).
    #[serde(default)]
    pub shaders: std::collections::BTreeMap<String, String>,
    /// Anything else the renderer wants (palettes, sprite atlases, particle params). Opaque JSON.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// The complete, versioned, hot-swappable game definition. The authoritative sim reads it every tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ruleset {
    /// Monotonic version. A host/client applies an incoming ruleset only if its version is **higher**
    /// than the one currently live — so updates are idempotent and order-independent across the mesh.
    pub version: u64,
    /// Human label for the build (shown in dev HUD), e.g. `"2026-06-26 missile buff"`.
    #[serde(default)]
    pub label: String,
    pub tunables: Tunables,
    /// The weapon catalogue. Order is the loadout order; `[0]` is the default starting weapon.
    pub weapons: Vec<WeaponDef>,
    /// The tech tree.
    pub tech: Vec<TechNode>,
    /// Frontend assets (shaders, art params).
    #[serde(default)]
    pub assets: Assets,
}

impl Ruleset {
    /// The built-in default ruleset: the four flagship weapons (blaster, homing missile, railgun,
    /// laser) and a tech tree that unlocks them, plus the classic hull/thruster/gun upgrades. This is
    /// what a host runs until a live edit is pushed.
    pub fn builtin() -> Ruleset {
        Ruleset {
            version: 1,
            label: "builtin".into(),
            tunables: Tunables::default(),
            weapons: vec![
                WeaponDef {
                    id: "blaster".into(),
                    name: "Blaster".into(),
                    kind: WeaponKind::Projectile,
                    cooldown: 5,
                    damage: 9,
                    speed: 26.0,
                    ttl: 22,
                    range: 0.0,
                    count: 1,
                    spread: 0.12,
                    turn_rate: 0.0,
                    hue_shift: 0,
                },
                WeaponDef {
                    id: "missile".into(),
                    name: "Homing Missile".into(),
                    kind: WeaponKind::Homing,
                    cooldown: 28,
                    damage: 34,
                    speed: 15.0,
                    ttl: 90,
                    range: 0.0,
                    count: 1,
                    spread: 0.0,
                    turn_rate: 0.08,
                    hue_shift: 20,
                },
                WeaponDef {
                    id: "railgun".into(),
                    name: "Railgun".into(),
                    kind: WeaponKind::Railgun,
                    cooldown: 40,
                    damage: 70,
                    speed: 0.0,
                    ttl: 0,
                    range: 1600.0,
                    count: 1,
                    spread: 0.0,
                    turn_rate: 0.0,
                    hue_shift: 200,
                },
                WeaponDef {
                    id: "laser".into(),
                    name: "Laser".into(),
                    kind: WeaponKind::Laser,
                    cooldown: 1,
                    damage: 4,
                    speed: 0.0,
                    ttl: 0,
                    range: 520.0,
                    count: 1,
                    spread: 0.0,
                    turn_rate: 0.0,
                    hue_shift: 320,
                },
                // --- homing missile launchers ---
                WeaponDef {
                    // A multi-tube launcher: a salvo of homing missiles in a spread.
                    id: "missile-pod".into(),
                    name: "Missile Pod".into(),
                    kind: WeaponKind::Homing,
                    cooldown: 36,
                    damage: 18,
                    speed: 16.0,
                    ttl: 80,
                    range: 0.0,
                    count: 4,
                    spread: 0.5,
                    turn_rate: 0.09,
                    hue_shift: 12,
                },
                WeaponDef {
                    // A single heavy seeker: slow, hard-turning, hits like a truck.
                    id: "heavy-seeker".into(),
                    name: "Heavy Seeker".into(),
                    kind: WeaponKind::Homing,
                    cooldown: 70,
                    damage: 90,
                    speed: 11.0,
                    ttl: 160,
                    range: 0.0,
                    count: 1,
                    spread: 0.0,
                    turn_rate: 0.13,
                    hue_shift: 350,
                },
                // --- laser weapon types ---
                WeaponDef {
                    // Pulsed laser: gated bursts, heavier per-hit than the continuous beam.
                    id: "pulse-laser".into(),
                    name: "Pulse Laser".into(),
                    kind: WeaponKind::Laser,
                    cooldown: 6,
                    damage: 18,
                    speed: 0.0,
                    ttl: 0,
                    range: 640.0,
                    count: 1,
                    spread: 0.0,
                    turn_rate: 0.0,
                    hue_shift: 290,
                },
                WeaponDef {
                    // Scatter laser: a fan of short beams — devastating up close, weak at range.
                    id: "scatter-laser".into(),
                    name: "Scatter Laser".into(),
                    kind: WeaponKind::Laser,
                    cooldown: 3,
                    damage: 4,
                    speed: 0.0,
                    ttl: 0,
                    range: 440.0,
                    count: 3,
                    spread: 0.32,
                    turn_rate: 0.0,
                    hue_shift: 260,
                },
                WeaponDef {
                    // Lance: a long, slow, high-damage focused beam pulse.
                    id: "lance".into(),
                    name: "Beam Lance".into(),
                    kind: WeaponKind::Laser,
                    cooldown: 30,
                    damage: 55,
                    speed: 0.0,
                    ttl: 0,
                    range: 1300.0,
                    count: 1,
                    spread: 0.0,
                    turn_rate: 0.0,
                    hue_shift: 180,
                },
            ],
            tech: vec![
                TechNode {
                    id: "hull-1".into(),
                    name: "Reinforced Hull".into(),
                    cost: 30,
                    requires: vec![],
                    effect: TechEffect::AddHull { amount: 40 },
                },
                TechNode {
                    id: "thruster-1".into(),
                    name: "Overdrive Thrusters".into(),
                    cost: 25,
                    requires: vec![],
                    effect: TechEffect::AddThruster { levels: 1 },
                },
                TechNode {
                    id: "twin-guns".into(),
                    name: "Twin Cannons".into(),
                    cost: 40,
                    requires: vec![],
                    effect: TechEffect::AddGun { count: 1 },
                },
                TechNode {
                    id: "tech-missile".into(),
                    name: "Missile Bay".into(),
                    cost: 120,
                    requires: vec![],
                    effect: TechEffect::UnlockWeapon { weapon: "missile".into() },
                },
                TechNode {
                    id: "tech-railgun".into(),
                    name: "Railgun Mount".into(),
                    cost: 220,
                    requires: vec!["twin-guns".into()],
                    effect: TechEffect::UnlockWeapon { weapon: "railgun".into() },
                },
                TechNode {
                    id: "tech-laser".into(),
                    name: "Laser Array".into(),
                    cost: 180,
                    requires: vec!["thruster-1".into()],
                    effect: TechEffect::UnlockWeapon { weapon: "laser".into() },
                },
                // Advanced missile launchers (branch off the missile bay).
                TechNode {
                    id: "tech-missile-pod".into(),
                    name: "Missile Pod".into(),
                    cost: 260,
                    requires: vec!["tech-missile".into()],
                    effect: TechEffect::UnlockWeapon { weapon: "missile-pod".into() },
                },
                TechNode {
                    id: "tech-heavy-seeker".into(),
                    name: "Heavy Seeker".into(),
                    cost: 320,
                    requires: vec!["tech-missile".into()],
                    effect: TechEffect::UnlockWeapon { weapon: "heavy-seeker".into() },
                },
                // Laser weapon types (branch off the laser array).
                TechNode {
                    id: "tech-pulse-laser".into(),
                    name: "Pulse Laser".into(),
                    cost: 240,
                    requires: vec!["tech-laser".into()],
                    effect: TechEffect::UnlockWeapon { weapon: "pulse-laser".into() },
                },
                TechNode {
                    id: "tech-scatter-laser".into(),
                    name: "Scatter Laser".into(),
                    cost: 260,
                    requires: vec!["tech-laser".into()],
                    effect: TechEffect::UnlockWeapon { weapon: "scatter-laser".into() },
                },
                TechNode {
                    id: "tech-lance".into(),
                    name: "Beam Lance".into(),
                    cost: 360,
                    requires: vec!["tech-laser".into(), "tech-railgun".into()],
                    effect: TechEffect::UnlockWeapon { weapon: "lance".into() },
                },
            ],
            assets: Assets::default(),
        }
    }

    /// The weapon with `id`, if present.
    pub fn weapon(&self, id: &str) -> Option<&WeaponDef> {
        self.weapons.iter().find(|w| w.id == id)
    }

    /// The default starting weapon id (first in the catalogue, or `"blaster"` if empty).
    pub fn default_weapon(&self) -> String {
        self.weapons.first().map(|w| w.id.clone()).unwrap_or_else(|| "blaster".into())
    }

    /// The tech node with `id`, if present.
    pub fn tech_node(&self, id: &str) -> Option<&TechNode> {
        self.tech.iter().find(|t| t.id == id)
    }

    /// A content fingerprint of the ruleset (FNV-1a/64 over the canonical JSON). Distinct rulesets get
    /// distinct fingerprints; identical ones match — handy for "did this actually change?" checks and
    /// for tests. (The mesh handle is the `put_object` CID; this is a cheap local digest.)
    pub fn fingerprint(&self) -> u64 {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        let mut h: u64 = 0xcbf29ce484222325;
        for b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Validate basic invariants a live edit must satisfy, so a typo in a hand-edited ruleset is
    /// rejected instead of breaking a running match. Returns a human-readable error to surface to the
    /// designer.
    pub fn validate(&self) -> Result<(), String> {
        if self.weapons.is_empty() {
            return Err("ruleset has no weapons".into());
        }
        for w in &self.weapons {
            if w.id.trim().is_empty() {
                return Err("a weapon has an empty id".into());
            }
            if w.cooldown == 0 {
                return Err(format!("weapon {} has cooldown 0", w.id));
            }
        }
        // Every tech that unlocks a weapon must point at a defined weapon.
        for t in &self.tech {
            if let TechEffect::UnlockWeapon { weapon } = &t.effect
                && self.weapon(weapon).is_none()
            {
                return Err(format!("tech {} unlocks unknown weapon {weapon}", t.id));
            }
            for req in &t.requires {
                if self.tech_node(req).is_none() {
                    return Err(format!("tech {} requires unknown tech {req}", t.id));
                }
            }
        }
        if self.tunables.damping <= 0.0 || self.tunables.damping > 1.0 {
            return Err("damping must be in (0, 1]".into());
        }
        Ok(())
    }

    /// Serialize for `put_object` / file write.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// Parse from `get_object` / file bytes, validating before returning so a bad edit can't go live.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Ruleset> {
        let r: Ruleset = serde_json::from_slice(bytes)?;
        r.validate().map_err(|e| anyhow::anyhow!("invalid ruleset: {e}"))?;
        Ok(r)
    }
}

impl Default for Ruleset {
    fn default() -> Self {
        Ruleset::builtin()
    }
}

/// A shared, cheaply-clonable handle to the live ruleset. The sim stores one of these and reads it
/// each tick; a hot reload swaps the pointed-to value.
pub type RulesetHandle = Arc<Ruleset>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_is_valid_and_has_the_four_weapons() {
        let r = Ruleset::builtin();
        assert!(r.validate().is_ok(), "builtin must validate");
        for id in ["blaster", "missile", "railgun", "laser"] {
            assert!(r.weapon(id).is_some(), "builtin has {id}");
        }
        assert_eq!(r.default_weapon(), "blaster");
        // Each kind is represented.
        assert_eq!(r.weapon("missile").unwrap().kind, WeaponKind::Homing);
        assert_eq!(r.weapon("railgun").unwrap().kind, WeaponKind::Railgun);
        assert_eq!(r.weapon("laser").unwrap().kind, WeaponKind::Laser);
    }

    #[test]
    fn roundtrips_through_json() {
        let r = Ruleset::builtin();
        let bytes = r.encode().unwrap();
        let back = Ruleset::decode(&bytes).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn fingerprint_changes_when_a_weapon_is_tuned() {
        let mut r = Ruleset::builtin();
        let f0 = r.fingerprint();
        r.weapons[0].damage += 1; // a live balance tweak
        assert_ne!(r.fingerprint(), f0, "tuning a weapon changes the fingerprint");
    }

    #[test]
    fn validate_rejects_dangling_unlock_and_requires() {
        let mut r = Ruleset::builtin();
        r.tech.push(TechNode {
            id: "bad".into(),
            name: "Bad".into(),
            cost: 1,
            requires: vec![],
            effect: TechEffect::UnlockWeapon { weapon: "plasma-cannon".into() },
        });
        assert!(r.validate().is_err(), "unlock of an undefined weapon is rejected");

        let mut r2 = Ruleset::builtin();
        r2.tech.push(TechNode {
            id: "bad2".into(),
            name: "Bad2".into(),
            cost: 1,
            requires: vec!["does-not-exist".into()],
            effect: TechEffect::AddHull { amount: 1 },
        });
        assert!(r2.validate().is_err(), "requires of an undefined tech is rejected");
    }

    #[test]
    fn validate_rejects_zero_cooldown_and_bad_damping() {
        let mut r = Ruleset::builtin();
        r.weapons[0].cooldown = 0;
        assert!(r.validate().is_err());
        let mut r2 = Ruleset::builtin();
        r2.tunables.damping = 1.5;
        assert!(r2.validate().is_err());
    }

    #[test]
    fn fallback_weapon_is_a_blaster() {
        let w = WeaponDef::fallback();
        assert_eq!(w.kind, WeaponKind::Projectile);
        assert!(w.cooldown > 0);
    }

    #[test]
    fn empty_minimal_ruleset_uses_default_tunables_feel() {
        // A minimal hand-authored ruleset (just one weapon) still decodes and plays with classic feel.
        let json = r#"{
            "version": 7,
            "tunables": {
                "max_speed": 7.0, "thrust": 0.55, "damping": 0.94, "turn_rate": 0.16,
                "thruster_step": 0.16, "base_hp": 100, "respawn_ticks": 64,
                "rock_regen_ticks": 600, "player_ttl_ticks": 100, "max_guns": 5, "ship_push": 0.5
            },
            "weapons": [{
                "id": "blaster", "name": "Blaster", "kind": "projectile", "cooldown": 5,
                "damage": 9, "speed": 26.0, "ttl": 22, "range": 0.0, "count": 1,
                "spread": 0.0, "turn_rate": 0.0, "hue_shift": 0
            }],
            "tech": []
        }"#;
        let r = Ruleset::decode(json.as_bytes()).unwrap();
        assert_eq!(r.version, 7);
        assert_eq!(r.default_weapon(), "blaster");
        assert_eq!(r.tunables.max_speed, 7.0);
    }
}
