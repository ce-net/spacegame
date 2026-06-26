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

use crate::build::{
    Blueprint, BpParam, Catalog, ObjectCategory, ObjectDef, ObjectStats, Placement, Repeat, Transform2D,
};
use crate::procgen::{GeneratedShip, ModuleDef, ShipGrammar, Slot, SlotRule};
use crate::shape::{NamedShape, Shape2D};
use crate::shapedef::{MaterialDef, ShapeBlueprint, ShapeKind, ShapeLibrary, ShapePart, Xform};

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
    /// Named, reusable primitive shape library — shapes as first-class hot-reloadable entities.
    #[serde(default)]
    pub shapes: Vec<NamedShape>,
    /// Material palette (authoring form) — hot-reloadable; referenced by shape blueprints.
    #[serde(default)]
    pub materials: Vec<MaterialDef>,
    /// Recursive **shape blueprints** (shapes composed of shapes) with materials — defined, saved and
    /// edited live; flattened to a GPU mesh + root AABB on demand.
    #[serde(default)]
    pub shape_blueprints: Vec<ShapeBlueprint>,
    /// The buildable object/block catalogue (structure, armor, weapons, thrusters, command centres,
    /// radars, sensors, tanks, containers, upgrades…) — hot-reloadable like everything else.
    #[serde(default)]
    pub objects: Vec<ObjectDef>,
    /// Reusable, recursive blueprints designers build from the object catalogue.
    #[serde(default)]
    pub blueprints: Vec<Blueprint>,
    /// Procedural-generation modules: blueprints tagged with a role + the slots where others attach.
    #[serde(default)]
    pub modules: Vec<ModuleDef>,
    /// Placement grammars that drive the recursive procedural ship generator.
    #[serde(default)]
    pub grammars: Vec<ShipGrammar>,
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
            shapes: builtin_shapes(),
            materials: builtin_materials(),
            shape_blueprints: builtin_shape_blueprints(),
            objects: builtin_objects(),
            blueprints: builtin_blueprints(),
            modules: builtin_modules(),
            grammars: builtin_grammars(),
            assets: Assets::default(),
        }
    }

    /// A read-only view of the buildable catalogue for [`crate::build::resolve_blueprint`].
    pub fn catalog(&self) -> Catalog<'_> {
        Catalog { objects: &self.objects, blueprints: &self.blueprints }
    }

    /// Resolve a blueprint by id (with optional parameter overrides) into a concrete assembled craft —
    /// the bridge from a design to the mass/thrust/weapons/shape the sim and physics consume.
    pub fn resolve_craft(
        &self,
        id: &str,
        args: &std::collections::BTreeMap<String, f32>,
    ) -> Result<crate::build::ResolvedCraft, String> {
        crate::build::resolve_blueprint(&self.catalog(), id, args)
    }

    /// Look up a named primitive shape from the hot-reloadable shape library.
    pub fn shape(&self, id: &str) -> Option<&Shape2D> {
        self.shapes.iter().find(|s| s.id == id).map(|s| &s.def)
    }

    /// A read-only view of the recursive shape-blueprint / primitive / material libraries.
    pub fn shape_library(&self) -> ShapeLibrary<'_> {
        ShapeLibrary { blueprints: &self.shape_blueprints, prims: &self.shapes, materials: &self.materials }
    }

    /// Flatten a recursive shape blueprint to a GPU-ready mesh (triangulated geometry, material
    /// palette, root AABB) — the bridge from a defined shape to the graphics layer.
    pub fn flatten_shape(&self, id: &str) -> Result<crate::shapedef::GpuMesh, String> {
        crate::shapedef::flatten_gpu(&self.shape_library(), id)
    }

    /// Resolve a recursive shape blueprint into its world-placed primitives (for collision/physics).
    pub fn resolve_shape(&self, id: &str) -> Result<Vec<crate::shapedef::FlatPrim>, String> {
        crate::shapedef::resolve_shape(&self.shape_library(), id)
    }

    /// **Procedurally generate a ship** from a grammar id + seed: recursively attaches modules per the
    /// grammar into a synthesized blueprint. Deterministic in the seed (same ship on every node).
    pub fn generate_ship(&self, grammar_id: &str, seed: u64) -> Result<GeneratedShip, String> {
        let g = self
            .grammars
            .iter()
            .find(|g| g.id == grammar_id)
            .ok_or_else(|| format!("unknown grammar {grammar_id}"))?;
        crate::procgen::generate_ship(&self.modules, g, seed)
    }

    /// Generate a whole fleet of distinct designs from one grammar.
    pub fn generate_fleet(&self, grammar_id: &str, base_seed: u64, count: u32) -> Result<Vec<GeneratedShip>, String> {
        let g = self
            .grammars
            .iter()
            .find(|g| g.id == grammar_id)
            .ok_or_else(|| format!("unknown grammar {grammar_id}"))?;
        Ok(crate::procgen::generate_fleet(&self.modules, g, base_seed, count))
    }

    /// Resolve a procedurally-generated ship's synthesized blueprint into a concrete craft (mass,
    /// thrust, weapon mounts, parts) against this ruleset's catalogue.
    pub fn resolve_generated(&self, ship: &GeneratedShip) -> Result<crate::build::ResolvedCraft, String> {
        crate::build::resolve_design(&self.catalog(), &ship.blueprint, &std::collections::BTreeMap::new())
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
        // The buildable catalogue: shapes sane, references resolve, no blueprint cycles, interior-only
        // objects placed inside a holder.
        crate::build::validate_catalog(&self.catalog())?;
        // Named shape library: each entry is sane geometry.
        for ns in &self.shapes {
            ns.def.validate().map_err(|e| format!("shape {}: {e}", ns.id))?;
        }
        // Recursive shape blueprints: references resolve, materials exist, no cycles.
        crate::shapedef::validate_shape_library(&self.shape_library())?;
        // Procgen modules reference real blueprints; grammars reference a rootable module tag.
        for m in &self.modules {
            if self.blueprints.iter().all(|b| b.id != m.blueprint) {
                return Err(format!("module {} references unknown blueprint {}", m.id, m.blueprint));
            }
        }
        for g in &self.grammars {
            if !self.modules.iter().any(|m| m.has_tag(&g.root_tag)) {
                return Err(format!("grammar {} has no module tagged '{}' to root a ship", g.id, g.root_tag));
            }
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

/// The built-in buildable object catalogue — a starter set spanning every category and several shape
/// families (rectangles, triangles, trapezoids, discs, hex plates). Designers extend or rebalance this
/// live by editing the ruleset.
fn builtin_objects() -> Vec<ObjectDef> {
    use ObjectCategory::*;
    let s = |stats: ObjectStats| stats;
    vec![
        ObjectDef::new("struct-block", "Structure Block", Structure, Shape2D::Rect { w: 2.0, h: 2.0 }).mass(2.0).hp(220),
        ObjectDef::new("struct-corner", "Corner Brace", Structure, Shape2D::Triangle { w: 2.0, h: 2.0, skew: 0.0 })
            .mass(1.4)
            .hp(160),
        ObjectDef::new("container", "Cargo Container", Container, Shape2D::Rect { w: 2.0, h: 2.0 })
            .mass(1.5)
            .hp(140)
            .stats(s(ObjectStats { capacity: 200.0, ..Default::default() })),
        ObjectDef::new("armor-plate", "Armor Plate", Armor, Shape2D::Rect { w: 2.0, h: 1.0 })
            .mass(1.2)
            .hp(260)
            .stats(s(ObjectStats { armor: 60, ..Default::default() })),
        ObjectDef::new("armor-wedge", "Armor Wedge", Armor, Shape2D::Triangle { w: 2.0, h: 2.0, skew: 0.0 })
            .mass(1.0)
            .hp(200)
            .stats(s(ObjectStats { armor: 45, ..Default::default() })),
        ObjectDef::new("armor-hex", "Hex Plate", Armor, Shape2D::RegularPolygon { sides: 6, r: 1.2 })
            .mass(1.3)
            .hp(240)
            .stats(s(ObjectStats { armor: 55, ..Default::default() })),
        ObjectDef::new("gun", "Auto Gun", Gun, Shape2D::Rect { w: 0.8, h: 1.4 })
            .mass(0.6)
            .hp(60)
            .stats(s(ObjectStats { weapon: Some("blaster".into()), power: -1.0, ..Default::default() })),
        ObjectDef::new("turret", "Turret", Turret, Shape2D::Disc { r: 0.9 })
            .mass(0.9)
            .hp(90)
            .stats(s(ObjectStats { weapon: Some("blaster".into()), power: -2.0, ..Default::default() })),
        ObjectDef::new("missile-rack", "Missile Rack", Weapon, Shape2D::Rect { w: 1.2, h: 1.4 })
            .mass(1.1)
            .hp(80)
            .stats(s(ObjectStats { weapon: Some("missile".into()), power: -3.0, ..Default::default() })),
        ObjectDef::new("thruster", "Thruster", Thruster, Shape2D::Trapezoid { top_w: 0.6, bottom_w: 1.4, h: 1.2, top_skew: 0.0 })
            .mass(0.8)
            .hp(70)
            .stats(s(ObjectStats { thrust: 60.0, power: -6.0, ..Default::default() })),
        ObjectDef::new("command-center", "Command Center", CommandCenter, Shape2D::Rect { w: 1.2, h: 1.2 })
            .mass(1.0)
            .hp(120)
            .stats(s(ObjectStats { power: 16.0, ..Default::default() })),
        ObjectDef::new("radar", "Radar", Radar, Shape2D::Disc { r: 0.7 })
            .mass(0.5)
            .hp(50)
            .stats(s(ObjectStats { sensor_range: 4000.0, power: -3.0, ..Default::default() })),
        ObjectDef::new("sensor", "Sensor", Sensor, Shape2D::Rect { w: 0.8, h: 0.8 })
            .mass(0.3)
            .hp(40)
            .stats(s(ObjectStats { sensor_range: 1200.0, power: -1.0, ..Default::default() })),
        ObjectDef::new("tank-round", "Round Tank", StorageTank, Shape2D::Disc { r: 1.1 })
            .mass(1.0)
            .hp(100)
            .stats(s(ObjectStats { capacity: 600.0, ..Default::default() })),
        ObjectDef::new("tank-rect", "Box Tank", StorageTank, Shape2D::Rect { w: 2.0, h: 1.4 })
            .mass(1.1)
            .hp(110)
            .stats(s(ObjectStats { capacity: 700.0, ..Default::default() })),
        ObjectDef::new("reactor", "Reactor Upgrade", Upgrade, Shape2D::Rect { w: 1.0, h: 1.0 })
            .mass(0.8)
            .hp(80)
            .stats(s(ObjectStats { power: 40.0, ..Default::default() })),
        ObjectDef::new("targeting-amp", "Targeting Amp", Upgrade, Shape2D::Rect { w: 0.8, h: 0.8 })
            .mass(0.3)
            .hp(50)
            .stats(s(ObjectStats { boost: 0.25, ..Default::default() })),
    ]
}

/// Built-in blueprints showing the recursive system: a `turret-pod` (a structure holding a turret) and
/// a parametric `scout` that nests two turret pods, places a command centre + radar inside a structure,
/// repeats a spine of blocks, and sizes an armour plate from its `armor` parameter.
fn builtin_blueprints() -> Vec<Blueprint> {
    let pod = Blueprint {
        id: "turret-pod".into(),
        name: "Turret Pod".into(),
        params: vec![],
        root: vec![Placement::object("struct-block", Transform2D::default())
            .with_children(vec![Placement::object("turret", Transform2D::default())])],
    };
    let scout = Blueprint {
        id: "scout".into(),
        name: "Scout".into(),
        params: vec![BpParam { name: "armor".into(), default: 2.0, min: Some(1.0), max: Some(6.0) }],
        root: vec![
            // A spine of three structure blocks.
            Placement::object("struct-block", Transform2D::new(0.0, 0.0, 0.0)).with_repeat(Repeat {
                count: 3,
                dx: 0.0,
                dy: 2.0,
                drot: 0.0,
            }),
            // The brain + radar live inside a structure block.
            Placement::object("struct-block", Transform2D::new(0.0, 2.0, 0.0)).with_children(vec![
                Placement::object("command-center", Transform2D::default()),
                Placement::object("radar", Transform2D::new(0.0, 0.4, 0.0)),
            ]),
            // Two thrusters at the tail.
            Placement::object("thruster", Transform2D::new(-0.8, -1.6, 0.0)),
            Placement::object("thruster", Transform2D::new(0.8, -1.6, 0.0)),
            // Two turret pods on the wings — blueprint within blueprint.
            Placement::blueprint("turret-pod", Transform2D::new(-2.6, 1.0, 0.0)),
            Placement::blueprint("turret-pod", Transform2D::new(2.6, 1.0, 0.0)),
            // A parametric armour plate: its width follows the `armor` parameter (dynamic customisation).
            Placement::object("armor-plate", Transform2D::new(0.0, 5.0, 0.0)).with_bind("w", "armor"),
        ],
    };

    // --- Module blueprints the procedural generator attaches (each is a small functional chunk). ---
    let bp = |id: &str, name: &str, root: Vec<Placement>| Blueprint {
        id: id.into(),
        name: name.into(),
        params: vec![],
        root,
    };
    let core = bp(
        "bp-core",
        "Core",
        vec![Placement::object("struct-block", Transform2D::default()).with_children(vec![
            Placement::object("command-center", Transform2D::default()),
            Placement::object("radar", Transform2D::new(0.0, 0.5, 0.0)),
        ])],
    );
    let wing = bp(
        "bp-wing",
        "Wing",
        vec![
            Placement::object("struct-corner", Transform2D::new(0.0, 0.0, 0.0)),
            Placement::object("armor-wedge", Transform2D::new(0.0, 0.6, 0.0)),
        ],
    );
    let engine = bp(
        "bp-engine",
        "Engine Pod",
        vec![Placement::object("struct-block", Transform2D::default()).with_children(vec![
            Placement::object("thruster", Transform2D::new(-0.5, -0.6, 0.0)),
            Placement::object("thruster", Transform2D::new(0.5, -0.6, 0.0)),
        ])],
    );
    let nose = bp(
        "bp-nose",
        "Nose",
        vec![Placement::object("struct-corner", Transform2D::default()).with_children(vec![
            Placement::object("sensor", Transform2D::default()),
        ])],
    );

    vec![pod, scout, core, wing, engine, nose]
}

/// A small hot-reloadable shape library, referenced by id or edited to restyle many blocks at once.
fn builtin_shapes() -> Vec<NamedShape> {
    vec![
        NamedShape { id: "wedge".into(), def: Shape2D::Triangle { w: 2.0, h: 2.0, skew: 0.0 } },
        NamedShape { id: "hex".into(), def: Shape2D::RegularPolygon { sides: 6, r: 1.2 } },
        NamedShape { id: "long-plate".into(), def: Shape2D::Rect { w: 4.0, h: 1.0 } },
        NamedShape { id: "round".into(), def: Shape2D::Disc { r: 1.0 } },
        NamedShape { id: "fin".into(), def: Shape2D::Triangle { w: 1.4, h: 3.0, skew: 1.0 } },
    ]
}

/// A hot-reloadable material palette referenced by shape blueprints.
fn builtin_materials() -> Vec<MaterialDef> {
    let m = |id: &str, color: [f32; 4], emissive: [f32; 3], es: f32, metallic: f32, roughness: f32| MaterialDef {
        id: id.into(),
        color,
        emissive,
        emissive_strength: es,
        metallic,
        roughness,
    };
    vec![
        m("hull-steel", [0.55, 0.58, 0.66, 1.0], [0.0; 3], 0.0, 0.9, 0.45),
        m("armor-ceramic", [0.82, 0.80, 0.74, 1.0], [0.0; 3], 0.0, 0.1, 0.7),
        m("canopy-glass", [0.3, 0.6, 0.9, 0.5], [0.05, 0.1, 0.2], 0.4, 0.2, 0.1),
        m("engine-glow", [0.2, 0.7, 1.0, 1.0], [0.2, 0.8, 1.0], 4.0, 0.0, 0.3),
        m("gold-trim", [1.0, 0.82, 0.35, 1.0], [0.1, 0.07, 0.0], 0.6, 1.0, 0.25),
    ]
}

/// Built-in recursive shape blueprints: a detailed `hull-plate` (steel slab + a glowing strip + a gold
/// rivet) and a `ship-skin` that recursively composes plates plus a glass canopy — shapes within shapes
/// with materials, flattenable to a GPU mesh + root AABB.
fn builtin_shape_blueprints() -> Vec<ShapeBlueprint> {
    let plate = ShapeBlueprint {
        id: "hull-plate".into(),
        name: "Hull Plate".into(),
        material: Some("hull-steel".into()),
        root: vec![
            ShapePart { at: Xform::default(), kind: ShapeKind::Prim { shape: Shape2D::Rect { w: 4.0, h: 1.2 } }, material: None },
            ShapePart {
                at: Xform::new(0.0, 0.0, 0.0, 1.0),
                kind: ShapeKind::Prim { shape: Shape2D::Rect { w: 4.0, h: 0.18 } },
                material: Some("engine-glow".into()),
            },
            ShapePart {
                at: Xform::new(1.7, 0.4, 0.0, 1.0),
                kind: ShapeKind::PrimRef { shape: "round".into() },
                material: Some("gold-trim".into()),
            },
        ],
    };
    let skin = ShapeBlueprint {
        id: "ship-skin".into(),
        name: "Ship Skin".into(),
        material: Some("armor-ceramic".into()),
        root: vec![
            ShapePart { at: Xform::new(0.0, 1.4, 0.0, 1.0), kind: ShapeKind::Ref { blueprint: "hull-plate".into() }, material: None },
            ShapePart { at: Xform::new(0.0, -1.4, 0.0, 1.0), kind: ShapeKind::Ref { blueprint: "hull-plate".into() }, material: None },
            ShapePart {
                at: Xform::new(0.0, 0.0, 0.0, 1.0),
                kind: ShapeKind::Prim { shape: Shape2D::Disc { r: 0.8 } },
                material: Some("canopy-glass".into()),
            },
        ],
    };
    vec![plate, skin]
}

/// Procgen modules: each wraps a module blueprint with a role tag and the slots others dock to.
fn builtin_modules() -> Vec<ModuleDef> {
    vec![
        ModuleDef {
            id: "core".into(),
            blueprint: "bp-core".into(),
            tags: vec!["core".into()],
            slots: vec![
                Slot { tag: "wing".into(), at: Transform2D::new(-2.2, 0.0, 0.0), mirror: true, step: 0.0 },
                Slot { tag: "nose".into(), at: Transform2D::new(0.0, 2.4, 0.0), mirror: false, step: 0.0 },
                Slot { tag: "engine".into(), at: Transform2D::new(0.0, -2.4, 0.0), mirror: false, step: 0.0 },
                Slot { tag: "weapon".into(), at: Transform2D::new(0.0, 1.0, 0.0), mirror: true, step: 0.0 },
            ],
        },
        ModuleDef {
            id: "wing".into(),
            blueprint: "bp-wing".into(),
            tags: vec!["wing".into()],
            slots: vec![
                Slot { tag: "engine".into(), at: Transform2D::new(-1.4, -1.0, 0.0), mirror: false, step: 0.0 },
                Slot { tag: "weapon".into(), at: Transform2D::new(-1.4, 1.0, 0.0), mirror: false, step: 1.2 },
            ],
        },
        ModuleDef { id: "engine".into(), blueprint: "bp-engine".into(), tags: vec!["engine".into()], slots: vec![] },
        ModuleDef { id: "weapon".into(), blueprint: "turret-pod".into(), tags: vec!["weapon".into()], slots: vec![] },
        ModuleDef { id: "nose".into(), blueprint: "bp-nose".into(), tags: vec!["nose".into()], slots: vec![] },
    ]
}

/// Built-in placement grammars for the generator.
fn builtin_grammars() -> Vec<ShipGrammar> {
    vec![ShipGrammar {
        id: "warship".into(),
        name: "Warship".into(),
        root_tag: "core".into(),
        rules: vec![
            SlotRule { slot_tag: "wing".into(), options: vec!["wing".into()], min: 1, max: 1, chance: 1.0 },
            SlotRule { slot_tag: "nose".into(), options: vec!["nose".into()], min: 1, max: 1, chance: 0.85 },
            SlotRule { slot_tag: "engine".into(), options: vec!["engine".into()], min: 1, max: 2, chance: 1.0 },
            SlotRule { slot_tag: "weapon".into(), options: vec!["weapon".into()], min: 1, max: 2, chance: 0.9 },
        ],
        max_depth: 4,
    }]
}

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

    #[test]
    fn builtin_catalog_and_blueprints_validate_and_resolve() {
        let r = Ruleset::builtin();
        assert!(r.validate().is_ok(), "builtin ruleset incl. the buildable catalogue validates");
        // The recursive `scout` expands: spine(3) + (struct+command+radar=3) + 2 thrusters +
        // 2 turret-pods×(struct+turret=2)=4 + 1 armor plate = 13 parts.
        let craft = r.resolve_craft("scout", &std::collections::BTreeMap::new()).unwrap();
        assert_eq!(craft.parts.len(), 13, "scout resolved to all nested parts");
        assert!(craft.total_thrust > 0.0, "it has thrusters");
        assert!(craft.weapon_mounts.len() >= 2, "the two turret pods mount weapons");
        assert!(craft.parts.iter().any(|p| p.category == crate::build::ObjectCategory::CommandCenter));
        // The `armor` parameter dynamically widens the armour plate (and adds mass).
        let wide = r
            .resolve_craft("scout", &std::collections::BTreeMap::from([("armor".to_string(), 6.0)]))
            .unwrap();
        assert!(wide.total_mass > craft.total_mass, "a wider armour parameter makes a heavier craft");
    }

    #[test]
    fn procedural_generator_makes_varied_buildable_ships() {
        let r = Ruleset::builtin();
        // A deterministic ship from the warship grammar resolves to a real craft.
        let ship = r.generate_ship("warship", 0xC0FFEE).unwrap();
        assert!(ship.module_count >= 4, "the ship grew several modules");
        let craft = r.resolve_generated(&ship).unwrap();
        assert!(!craft.parts.is_empty(), "the generated design resolves to concrete parts");
        assert!(craft.total_thrust > 0.0, "it has engines");
        assert!(!craft.weapon_mounts.is_empty(), "it has weapons");

        // The same seed reproduces the same ship on any node (replica-safe).
        let again = r.generate_ship("warship", 0xC0FFEE).unwrap();
        assert_eq!(ship.blueprint, again.blueprint);

        // A fleet has structural variety.
        let fleet = r.generate_fleet("warship", 1, 6).unwrap();
        let sizes: std::collections::BTreeSet<usize> = fleet.iter().map(|s| s.blueprint.root.len()).collect();
        assert!(sizes.len() > 1, "different seeds yield different designs");
        // Every generated ship resolves.
        for s in &fleet {
            assert!(r.resolve_generated(s).is_ok());
        }
    }

    #[test]
    fn builtin_shape_blueprints_resolve_and_flatten_to_gpu() {
        let r = Ruleset::builtin();
        assert!(r.validate().is_ok(), "builtin shape blueprints + materials validate");
        // ship-skin -> 2× hull-plate (rect + strip + rivet = 3) + 1 canopy = 7 primitives.
        let prims = r.resolve_shape("ship-skin").unwrap();
        assert_eq!(prims.len(), 7, "recursive shape flattened to all primitives");
        // It flattens to a GPU mesh with triangulated geometry, a material palette, and a root AABB.
        let mesh = r.flatten_shape("ship-skin").unwrap();
        assert!(!mesh.vertices.is_empty() && mesh.indices.len() % 3 == 0, "triangulated");
        assert!(mesh.materials.len() >= 3, "material palette built (default + used)");
        assert!(mesh.aabb[2] > mesh.aabb[0] && mesh.aabb[3] > mesh.aabb[1], "non-degenerate root AABB");
        // And a pointer-free raw form for upload.
        let raw = mesh.to_raw();
        assert_eq!(raw.vertices.len(), mesh.vertices.len() * 5);
    }
}
