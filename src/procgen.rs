//! Recursive **procedural ship generation**. Given a set of building-block **modules** (each a
//! [`Blueprint`] tagged with a role and the **attach slots** where other modules may dock) and a
//! **placement grammar** that says which roles fill which slots, this synthesizes whole ship designs
//! by recursively attaching modules to modules — a core grows wings, the wings grow engines and weapon
//! pods, the nose grows sensors — producing a fleet of varied "cool shapes and functions" out of a few
//! reusable parts.
//!
//! The output of [`generate_ship`] is itself a plain [`Blueprint`] (a flat tree of module references at
//! computed transforms), so it flows straight back through [`crate::build::resolve_blueprint`] into a
//! concrete craft. Modules, grammars, blueprints, objects and shapes all live in the hot-reloadable
//! [`Ruleset`](crate::ruleset::Ruleset), so editing any of them and regenerating yields new ships live.
//!
//! Generation is **deterministic** in its seed (a SplitMix64 PRNG): the same `(grammar, seed)` makes
//! the same ship on every node, which keeps procedurally-built craft consistent across the replicas
//! ([`crate::replication`]) and reproducible for sharing.
//!
//! Pure and unit-tested.

use serde::{Deserialize, Serialize};

use crate::build::{Blueprint, Placement, Transform2D};

/// A docking slot on a module: where, and what *kind* of module may attach there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    /// The slot's role tag (matched by a [`SlotRule::slot_tag`]).
    pub tag: String,
    /// Where the attached module's origin goes, in this module's frame.
    #[serde(default)]
    pub at: Transform2D,
    /// If true, also place a **mirrored** copy across the x axis (for symmetric wings/pods).
    #[serde(default)]
    pub mirror: bool,
    /// Lateral step between instances when a rule attaches more than one module here.
    #[serde(default)]
    pub step: f32,
}

/// A buildable **module**: a blueprint (its geometry) plus the role tags it satisfies and the slots it
/// exposes for further attachment. This is the unit the generator places.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleDef {
    pub id: String,
    /// The blueprint that draws this module.
    pub blueprint: String,
    /// Roles this module can fill (e.g. `["core"]`, `["wing"]`, `["engine"]`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Where other modules may attach to this one.
    #[serde(default)]
    pub slots: Vec<Slot>,
}

impl ModuleDef {
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }
}

/// A rule in the grammar: how a slot of a given tag is filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotRule {
    /// Which slot tag this rule applies to.
    pub slot_tag: String,
    /// Module tags that may fill the slot (a module qualifies if it has any of these tags).
    pub options: Vec<String>,
    /// Minimum / maximum modules to attach when the slot fires.
    #[serde(default)]
    pub min: u32,
    #[serde(default = "one_u32")]
    pub max: u32,
    /// Probability the slot fires at all (0..=1).
    #[serde(default = "full_chance")]
    pub chance: f32,
}

fn one_u32() -> u32 {
    1
}
fn full_chance() -> f32 {
    1.0
}

/// The **placement grammar** — the system that defines how blueprints/modules can and should be
/// placed. It names the root role and, per slot tag, what may fill it, plus a recursion depth cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShipGrammar {
    pub id: String,
    pub name: String,
    /// The role tag of the module the ship is grown from.
    pub root_tag: String,
    pub rules: Vec<SlotRule>,
    #[serde(default = "default_depth")]
    pub max_depth: u32,
}

fn default_depth() -> u32 {
    4
}

impl ShipGrammar {
    fn rule_for(&self, slot_tag: &str) -> Option<&SlotRule> {
        self.rules.iter().find(|r| r.slot_tag == slot_tag)
    }
}

/// A generated ship: the synthesized blueprint plus the seed and how many modules it ended up with.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedShip {
    pub blueprint: Blueprint,
    pub seed: u64,
    pub module_count: u32,
}

/// Hard cap on modules per ship, so a permissive grammar can't recurse into a giant.
pub const MAX_MODULES: u32 = 96;

/// A small deterministic PRNG (SplitMix64). Seeded generation is reproducible on every node.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e3779b97f4a7c15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    /// An index in `0..n` (0 if `n == 0`).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
    /// A float in `[0, 1)`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// An inclusive integer in `[a, b]`.
    pub fn range_u32(&mut self, a: u32, b: u32) -> u32 {
        if b <= a { a } else { a + (self.next_u64() % ((b - a + 1) as u64)) as u32 }
    }
}

/// Mirror a transform across the x axis (for symmetric placement).
fn mirror(t: &Transform2D) -> Transform2D {
    Transform2D { x: -t.x, y: t.y, rot: -t.rot }
}

/// Generate one ship design from the module set and grammar, seeded by `seed`. Returns a synthesized
/// [`Blueprint`] of module references at computed transforms (resolve it with the build resolver).
/// Errors only if the grammar has no module for its `root_tag`.
pub fn generate_ship(modules: &[ModuleDef], grammar: &ShipGrammar, seed: u64) -> Result<GeneratedShip, String> {
    let mut rng = Rng::new(seed);
    let roots: Vec<&ModuleDef> = modules.iter().filter(|m| m.has_tag(&grammar.root_tag)).collect();
    if roots.is_empty() {
        return Err(format!("grammar {} has no module tagged '{}' to root the ship", grammar.id, grammar.root_tag));
    }
    let root = roots[rng.below(roots.len())];

    let mut placements: Vec<Placement> = Vec::new();
    let mut count: u32 = 0;
    grow(modules, grammar, root, &Transform2D::default(), 0, &mut rng, &mut placements, &mut count);

    let bp = Blueprint {
        id: format!("gen-{}-{:x}", grammar.id, seed),
        name: format!("{} #{:x}", grammar.name, seed & 0xffff),
        params: Vec::new(),
        root: placements,
    };
    Ok(GeneratedShip { blueprint: bp, seed, module_count: count })
}

#[allow(clippy::too_many_arguments)]
fn grow(
    modules: &[ModuleDef],
    grammar: &ShipGrammar,
    module: &ModuleDef,
    world: &Transform2D,
    depth: u32,
    rng: &mut Rng,
    out: &mut Vec<Placement>,
    count: &mut u32,
) {
    if *count >= MAX_MODULES {
        return;
    }
    // Draw this module at its world transform (a reference to its blueprint).
    out.push(Placement::blueprint(&module.blueprint, *world));
    *count += 1;
    if depth >= grammar.max_depth {
        return;
    }

    for slot in &module.slots {
        let Some(rule) = grammar.rule_for(&slot.tag) else { continue };
        if rng.unit() >= rule.chance {
            continue;
        }
        let n = rule.range_u32_count(rng);
        // Candidate modules that satisfy this slot.
        let candidates: Vec<&ModuleDef> =
            modules.iter().filter(|m| rule.options.iter().any(|o| m.has_tag(o))).collect();
        if candidates.is_empty() {
            continue;
        }
        for i in 0..n {
            if *count >= MAX_MODULES {
                return;
            }
            let cand = candidates[rng.below(candidates.len())];
            // Slot transform, fanned by `step` for multi-attach.
            let local = Transform2D { x: slot.at.x + slot.step * i as f32, y: slot.at.y, rot: slot.at.rot };
            let child_world = world.compose(&local);
            grow(modules, grammar, cand, &child_world, depth + 1, rng, out, count);
            if slot.mirror && *count < MAX_MODULES {
                grow(modules, grammar, cand, &mirror(&child_world), depth + 1, rng, out, count);
            }
        }
    }
}

impl SlotRule {
    fn range_u32_count(&self, rng: &mut Rng) -> u32 {
        rng.range_u32(self.min, self.max.max(self.min))
    }
}

/// Generate a **fleet** of `count` distinct designs from one grammar, seeded from `base_seed`.
pub fn generate_fleet(
    modules: &[ModuleDef],
    grammar: &ShipGrammar,
    base_seed: u64,
    count: u32,
) -> Vec<GeneratedShip> {
    let mut out = Vec::new();
    for i in 0..count {
        // Decorrelate per-ship seeds with a SplitMix step so designs differ.
        let seed = {
            let mut r = Rng::new(base_seed ^ (i as u64).wrapping_mul(0x9e3779b97f4a7c15));
            r.next_u64()
        };
        if let Ok(s) = generate_ship(modules, grammar, seed) {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modules() -> Vec<ModuleDef> {
        vec![
            ModuleDef {
                id: "core".into(),
                blueprint: "bp-core".into(),
                tags: vec!["core".into()],
                slots: vec![
                    Slot { tag: "wing".into(), at: Transform2D::new(-2.0, 0.0, 0.0), mirror: true, step: 0.0 },
                    Slot { tag: "nose".into(), at: Transform2D::new(0.0, 3.0, 0.0), mirror: false, step: 0.0 },
                    Slot { tag: "engine".into(), at: Transform2D::new(0.0, -3.0, 0.0), mirror: false, step: 0.0 },
                ],
            },
            ModuleDef {
                id: "wing".into(),
                blueprint: "bp-wing".into(),
                tags: vec!["wing".into()],
                slots: vec![
                    Slot { tag: "engine".into(), at: Transform2D::new(-1.0, -1.0, 0.0), mirror: false, step: 0.0 },
                    Slot { tag: "weapon".into(), at: Transform2D::new(-1.0, 1.0, 0.0), mirror: false, step: 1.0 },
                ],
            },
            ModuleDef { id: "engine".into(), blueprint: "bp-engine".into(), tags: vec!["engine".into()], slots: vec![] },
            ModuleDef { id: "weapon".into(), blueprint: "turret-pod".into(), tags: vec!["weapon".into()], slots: vec![] },
            ModuleDef { id: "nose".into(), blueprint: "bp-nose".into(), tags: vec!["nose".into()], slots: vec![] },
        ]
    }

    fn grammar() -> ShipGrammar {
        ShipGrammar {
            id: "warship".into(),
            name: "Warship".into(),
            root_tag: "core".into(),
            rules: vec![
                SlotRule { slot_tag: "wing".into(), options: vec!["wing".into()], min: 1, max: 1, chance: 1.0 },
                SlotRule { slot_tag: "nose".into(), options: vec!["nose".into()], min: 1, max: 1, chance: 1.0 },
                SlotRule { slot_tag: "engine".into(), options: vec!["engine".into()], min: 1, max: 2, chance: 1.0 },
                SlotRule { slot_tag: "weapon".into(), options: vec!["weapon".into()], min: 1, max: 2, chance: 1.0 },
            ],
            max_depth: 4,
        }
    }

    #[test]
    fn generation_is_deterministic_in_the_seed() {
        let (m, g) = (modules(), grammar());
        let a = generate_ship(&m, &g, 12345).unwrap();
        let b = generate_ship(&m, &g, 12345).unwrap();
        assert_eq!(a.blueprint, b.blueprint, "same seed -> identical ship (replica-safe)");
        let c = generate_ship(&m, &g, 999).unwrap();
        assert_ne!(a.blueprint, c.blueprint, "a different seed makes a different ship");
    }

    #[test]
    fn a_ship_grows_recursively_from_the_core() {
        let (m, g) = (modules(), grammar());
        let ship = generate_ship(&m, &g, 7).unwrap();
        // Root core + 2 mirrored wings + nose + engines + weapons (+ wings' own engines/weapons).
        assert!(ship.module_count >= 5, "ship grew several modules, got {}", ship.module_count);
        // Mirror produced symmetric wing placements (a +x and a -x reference to the wing blueprint).
        let wing_xs: Vec<f32> = ship
            .blueprint
            .root
            .iter()
            .filter_map(|p| match &p.kind {
                crate::build::PlacementKind::Blueprint { id, .. } if id == "bp-wing" => Some(p.at.x),
                _ => None,
            })
            .collect();
        assert!(wing_xs.iter().any(|x| *x < 0.0) && wing_xs.iter().any(|x| *x > 0.0), "wings are mirrored: {wing_xs:?}");
    }

    #[test]
    fn module_count_is_capped() {
        // A grammar that always attaches the max and recurses can't blow past MAX_MODULES.
        let mut m = modules();
        // Make the wing recurse into more wings -> explosive without the cap.
        m[1].slots.push(Slot { tag: "wing".into(), at: Transform2D::new(-1.0, 0.0, 0.0), mirror: true, step: 0.0 });
        let mut g = grammar();
        g.max_depth = 20;
        for r in &mut g.rules {
            r.max = 3;
        }
        let ship = generate_ship(&m, &g, 42).unwrap();
        assert!(ship.module_count <= MAX_MODULES, "capped at {MAX_MODULES}, got {}", ship.module_count);
    }

    #[test]
    fn fleet_generation_makes_varied_designs() {
        let (m, g) = (modules(), grammar());
        let fleet = generate_fleet(&m, &g, 1, 8);
        assert_eq!(fleet.len(), 8);
        // Not all identical: at least two distinct module counts or blueprints across the fleet.
        let distinct = fleet.iter().map(|s| s.blueprint.root.len()).collect::<std::collections::BTreeSet<_>>();
        assert!(distinct.len() > 1, "the fleet has structural variety");
    }

    #[test]
    fn missing_root_module_errors() {
        let m = modules();
        let mut g = grammar();
        g.root_tag = "nonexistent".into();
        assert!(generate_ship(&m, &g, 1).is_err());
    }
}
