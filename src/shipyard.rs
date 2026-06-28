//! The **shipyard** — the bridge from "a ship is a pile of parts" to "a ship that flies".
//!
//! [`crate::build`] assembles a parametric [`Blueprint`](crate::build::Blueprint) of parts into a
//! [`ResolvedCraft`](crate::build::ResolvedCraft) with aggregate physical properties (mass, hull, thrust,
//! power, weapon mounts, cargo). That craft is the *design*. This module turns a design into a
//! [`Loadout`] — the concrete gameplay stats a [`crate::sim::Ship`] is flown with — and tells the player
//! whether the design is **flyable** and where it is weak.
//!
//! It is the rule that makes the build system *matter*: a light hull with a big engine is a fast,
//! nimble interceptor; a heavy freighter with the same engine is a slow barge. Thrust-to-weight,
//! straight out of the parts, drives speed and agility — so building a better ship is real, not cosmetic.
//!
//! Pure and deterministic (no floats compared for equality across machines beyond the sim's own
//! quantisation), and fully unit-tested.

use crate::build::{ObjectCategory, ResolvedCraft};

/// A reference craft's thrust-to-weight. A design with this exact ratio flies at the baseline
/// (multipliers `1.0`); lighter-and-punchier flies faster, heavier-and-weaker flies slower. Picked so
/// the builtin "interceptor" lands a bit above 1.0 and the "hauler" a bit below.
pub const REFERENCE_TWR: f32 = 6.0;

/// A non-fatal-to-fatal problem the designer should know about. Fatal issues make the craft
/// **unflyable** (it would be a brick); the rest are penalties or advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildIssue {
    /// No command centre — nothing to pilot the craft. **Fatal.**
    NoCommandCenter,
    /// No thruster — it cannot move. **Fatal.**
    NoThruster,
    /// No weapon mount — it cannot fight (a pure miner/hauler; allowed, just noted).
    NoWeapon,
    /// Net power is negative: the reactor cannot feed every system, so output is browned-out.
    Underpowered,
}

impl BuildIssue {
    /// Whether this issue makes the craft impossible to fly at all.
    pub fn is_fatal(&self) -> bool {
        matches!(self, BuildIssue::NoCommandCenter | BuildIssue::NoThruster)
    }
}

/// The concrete gameplay stats a built ship flies with, derived from a [`ResolvedCraft`].
#[derive(Debug, Clone, PartialEq)]
pub struct Loadout {
    /// Max hull, from the summed part hp plus any armour bonus.
    pub max_hp: i32,
    /// Total mass — heavier ships accelerate slower and shove harder in a collision.
    pub mass: f32,
    /// Max-speed multiplier vs the base ship (from thrust-to-weight).
    pub speed_mult: f32,
    /// Acceleration / agility multiplier vs the base ship (from thrust-to-weight).
    pub thrust_mult: f32,
    /// Number of weapon mounts (guns/turrets/weapon racks) — the multi-barrel spread count.
    pub guns: u32,
    /// Distinct weapon ids the mounts provide, sorted; the ship can switch between these.
    pub weapons: Vec<String>,
    /// The default selected weapon (first mount), if any.
    pub primary: Option<String>,
    /// Cargo capacity from storage tanks/containers — how much ore the ship can haul before it must bank.
    pub cargo: f32,
    /// Shield buffer from shield upgrades.
    pub shield: i32,
    /// Energy capacitor from surplus reactor power.
    pub energy: f32,
    /// Everything the designer should know — fatal first.
    pub issues: Vec<BuildIssue>,
}

impl Loadout {
    /// A flyable design has no fatal issue. An unflyable one must be rejected by the sim (the player
    /// keeps their old ship).
    pub fn is_flyable(&self) -> bool {
        !self.issues.iter().any(BuildIssue::is_fatal)
    }
}

/// Derive the gameplay [`Loadout`] from an assembled [`ResolvedCraft`]. Pure — the same craft always
/// yields the same loadout, on every replica.
pub fn loadout_from_craft(craft: &ResolvedCraft) -> Loadout {
    let mut issues = Vec::new();

    // --- Roles present in the parts list. ---
    let mut has_command = false;
    let mut has_thruster = false;
    let mut guns = 0u32;
    let mut armor_bonus = 0i32;
    let mut shield = 0i32;
    for p in &craft.parts {
        match p.category {
            ObjectCategory::CommandCenter => has_command = true,
            ObjectCategory::Thruster => has_thruster = true,
            ObjectCategory::Weapon | ObjectCategory::Turret | ObjectCategory::Gun => {
                if p.stats.weapon.is_some() {
                    guns += 1;
                }
            }
            ObjectCategory::Upgrade => {
                // An upgrade's `boost` reinforces the shield envelope.
                shield += p.stats.boost.max(0.0).round() as i32;
            }
            _ => {}
        }
        armor_bonus += p.stats.armor.max(0);
    }

    if !has_command {
        issues.push(BuildIssue::NoCommandCenter);
    }
    if !has_thruster || craft.total_thrust <= 0.0 {
        issues.push(BuildIssue::NoThruster);
    }
    if craft.weapon_mounts.is_empty() {
        issues.push(BuildIssue::NoWeapon);
    }

    // --- Thrust-to-weight → speed & agility. a = F/m, straight physics. ---
    let mass = craft.total_mass.max(0.05);
    let twr = craft.total_thrust / mass;
    let ratio = twr / REFERENCE_TWR;
    // Agility (acceleration) scales linearly with thrust-to-weight; top speed with its square root, so a
    // huge engine makes a craft leap off the line more than it raises the (drag-limited) terminal speed.
    let mut thrust_mult = ratio.clamp(0.4, 2.4);
    let mut speed_mult = ratio.max(0.0).sqrt().clamp(0.55, 1.8);

    // --- Power budget. A craft that can't feed its systems browns out. ---
    let mut energy = (craft.net_power.max(0.0) * 10.0).min(400.0);
    if craft.net_power < 0.0 {
        issues.push(BuildIssue::Underpowered);
        speed_mult *= 0.7;
        thrust_mult *= 0.7;
        energy = 0.0;
    }

    // --- Distinct, sorted weapon ids the mounts provide. ---
    let mut weapons = craft.weapon_mounts.clone();
    weapons.sort();
    weapons.dedup();
    let primary = weapons.first().cloned();

    issues.sort_by_key(|i| if i.is_fatal() { 0 } else { 1 });

    Loadout {
        max_hp: (craft.total_hp + armor_bonus).max(1),
        mass,
        speed_mult,
        thrust_mult,
        guns: guns.max(if weapons.is_empty() { 0 } else { 1 }),
        weapons,
        primary,
        cargo: craft.storage_capacity,
        shield: shield.max(0),
        energy,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{
        Blueprint, Catalog, ObjectCategory, ObjectDef, ObjectStats, Placement, Transform2D,
    };
    use crate::shape::Shape2D;
    use std::collections::BTreeMap;

    // A minimal parts catalogue: a hull, a command centre, an engine, a gun, a reactor and a tank.
    fn parts() -> Vec<ObjectDef> {
        use ObjectCategory::*;
        vec![
            ObjectDef::new("hull", "Hull", Structure, Shape2D::Rect { w: 2.0, h: 2.0 }).mass(2.0).hp(200),
            ObjectDef::new("cmd", "Command", CommandCenter, Shape2D::Rect { w: 1.0, h: 1.0 })
                .stats(ObjectStats { power: 6.0, ..Default::default() }),
            ObjectDef::new("engine", "Engine", Thruster, Shape2D::Rect { w: 1.0, h: 1.0 })
                .mass(1.0)
                .stats(ObjectStats { thrust: 60.0, power: -4.0, ..Default::default() }),
            ObjectDef::new("gun", "Gun", Gun, Shape2D::Rect { w: 0.5, h: 1.0 })
                .stats(ObjectStats { weapon: Some("blaster".into()), power: -1.0, ..Default::default() }),
            ObjectDef::new("reactor", "Reactor", Upgrade, Shape2D::Rect { w: 1.0, h: 1.0 })
                .stats(ObjectStats { power: 20.0, boost: 25.0, ..Default::default() }),
            ObjectDef::new("tank", "Tank", StorageTank, Shape2D::Disc { r: 1.0 })
                .stats(ObjectStats { capacity: 300.0, ..Default::default() }),
        ]
    }

    fn craft(root: Vec<Placement>) -> ResolvedCraft {
        let objects = parts();
        let bp = Blueprint { id: "d".into(), name: "D".into(), params: vec![], root };
        let blueprints = vec![bp];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        crate::build::resolve_blueprint(&cat, "d", &BTreeMap::new()).unwrap()
    }

    #[test]
    fn a_complete_design_is_flyable_and_arms_its_guns() {
        let c = craft(vec![
            Placement::object("hull", Transform2D::default()).with_children(vec![
                Placement::object("cmd", Transform2D::default()),
                Placement::object("engine", Transform2D::new(0.0, -1.0, 0.0)),
                Placement::object("gun", Transform2D::new(0.0, 1.0, 0.0)),
                Placement::object("reactor", Transform2D::new(0.5, 0.0, 0.0)),
            ]),
        ]);
        let lo = loadout_from_craft(&c);
        assert!(lo.is_flyable(), "complete craft flies: {:?}", lo.issues);
        assert_eq!(lo.guns, 1);
        assert_eq!(lo.weapons, vec!["blaster".to_string()]);
        assert_eq!(lo.primary.as_deref(), Some("blaster"));
        assert!(lo.max_hp >= 200);
        assert!(lo.energy > 0.0, "surplus reactor power charges the capacitor");
    }

    #[test]
    fn a_design_with_no_command_center_is_unflyable() {
        let c = craft(vec![Placement::object("hull", Transform2D::default()).with_children(vec![
            Placement::object("engine", Transform2D::new(0.0, -1.0, 0.0)),
        ])]);
        let lo = loadout_from_craft(&c);
        assert!(!lo.is_flyable());
        assert!(lo.issues.contains(&BuildIssue::NoCommandCenter));
    }

    #[test]
    fn a_design_with_no_engine_is_unflyable() {
        let c = craft(vec![Placement::object("hull", Transform2D::default()).with_children(vec![
            Placement::object("cmd", Transform2D::default()),
        ])]);
        let lo = loadout_from_craft(&c);
        assert!(!lo.is_flyable());
        assert!(lo.issues.contains(&BuildIssue::NoThruster));
    }

    #[test]
    fn lighter_craft_with_the_same_engine_flies_faster() {
        // Heavy: two hull blocks. Light: one. Same single engine on each.
        let light = craft(vec![Placement::object("hull", Transform2D::default()).with_children(vec![
            Placement::object("cmd", Transform2D::default()),
            Placement::object("engine", Transform2D::new(0.0, -1.0, 0.0)),
            Placement::object("reactor", Transform2D::new(0.5, 0.0, 0.0)),
        ])]);
        let heavy = craft(vec![
            Placement::object("hull", Transform2D::default()).with_children(vec![
                Placement::object("cmd", Transform2D::default()),
                Placement::object("engine", Transform2D::new(0.0, -1.0, 0.0)),
                Placement::object("reactor", Transform2D::new(0.5, 0.0, 0.0)),
            ]),
            Placement::object("hull", Transform2D::new(0.0, 3.0, 0.0)),
            Placement::object("hull", Transform2D::new(0.0, 6.0, 0.0)),
        ]);
        let lo_light = loadout_from_craft(&light);
        let lo_heavy = loadout_from_craft(&heavy);
        assert!(lo_heavy.mass > lo_light.mass);
        assert!(
            lo_light.speed_mult > lo_heavy.speed_mult,
            "lighter is faster: {} > {}",
            lo_light.speed_mult,
            lo_heavy.speed_mult
        );
        assert!(lo_light.thrust_mult > lo_heavy.thrust_mult, "lighter is more agile");
    }

    #[test]
    fn an_underpowered_design_is_flagged_and_penalised() {
        // Command (+6) + two engines (−4 each) = net −2: a brownout, but still flies.
        let c = craft(vec![Placement::object("hull", Transform2D::default()).with_children(vec![
            Placement::object("cmd", Transform2D::default()),
            Placement::object("engine", Transform2D::new(-0.5, -1.0, 0.0)),
            Placement::object("engine", Transform2D::new(0.5, -1.0, 0.0)),
        ])]);
        let lo = loadout_from_craft(&c);
        assert!(lo.is_flyable(), "underpowered is a penalty, not fatal");
        assert!(lo.issues.contains(&BuildIssue::Underpowered));
        assert_eq!(lo.energy, 0.0, "a browned-out craft has no spare capacitor charge");
    }

    #[test]
    fn cargo_capacity_comes_from_tanks() {
        let c = craft(vec![Placement::object("hull", Transform2D::default()).with_children(vec![
            Placement::object("cmd", Transform2D::default()),
            Placement::object("engine", Transform2D::new(0.0, -1.0, 0.0)),
            Placement::object("tank", Transform2D::new(0.5, 0.5, 0.0)),
        ])]);
        let lo = loadout_from_craft(&c);
        assert!((lo.cargo - 300.0).abs() < 1.0);
    }
}
