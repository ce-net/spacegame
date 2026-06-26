//! The **free-form building system** — place objects (structure, armor, weapons, turrets, thrusters,
//! command centres, radars, sensors, storage tanks, containers, upgrades) to design a craft, and a
//! **recursive blueprint system** that turns a parametric, nested design into a concrete assembled
//! craft at runtime.
//!
//! Everything is **data** (so it is hot-reloadable through [`crate::ruleset::Ruleset`]) and reuses the
//! one [dynamic shape kernel](crate::shape):
//!
//! - [`ObjectDef`] is a buildable thing: a category, a reusable [`Shape2D`] (variable rectangle,
//!   triangle, trapezoid, disc, polygon…), mass/hp, where it may be placed, whether it can hold child
//!   objects, and a flat bag of [`ObjectStats`] (thrust, power, weapon mount, storage capacity, sensor
//!   range, armour…).
//! - [`Placement`] puts an object (or a *sub-blueprint*) at a local transform, optionally **resized**
//!   per instance, optionally with **children placed inside it** (a command centre inside a structure
//!   block), and optionally **repeated** (a ring of turrets) — all customised by parameters.
//! - [`Blueprint`] is a named, parametric design: a tree of placements plus declared [`BpParam`]s, and
//!   placements may reference **other blueprints**, so blueprints contain blueprints contain
//!   blueprints. [`resolve_blueprint`] expands the whole tree at runtime — binding parameters,
//!   composing transforms, repeating, and guarding against cycles and runaway depth — into a flat
//!   [`ResolvedCraft`]: every concrete part with its world transform and resolved shape, plus the
//!   aggregate mass, hp, thrust, power, weapon mounts and storage capacity the sim/physics consume.
//!
//! Pure and unit-tested.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::shape::Shape2D;

/// What an object is — drives placement rules and how the sim treats a resolved part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectCategory {
    /// Load-bearing block: the skeleton. Holds children (interior objects).
    Structure,
    /// Protective plating bolted to the outside.
    Armor,
    /// A fixed weapon mount (fires a ruleset weapon).
    Weapon,
    /// A rotating weapon mount.
    Turret,
    /// A light fixed gun.
    Gun,
    /// Propulsion.
    Thruster,
    /// The brain — placed inside a structure.
    CommandCenter,
    /// Detects distant contacts — placed inside a structure.
    Radar,
    /// Short-range awareness — placed inside a structure.
    Sensor,
    /// Holds a fluid/resource volume.
    StorageTank,
    /// Holds discrete cargo/items; can contain child objects.
    Container,
    /// An improvement slotted inside a structure (boosts the things around it).
    Upgrade,
}

impl ObjectCategory {
    /// Can objects be placed *inside* this one?
    pub fn holds_children(self) -> bool {
        matches!(self, ObjectCategory::Structure | ObjectCategory::Container)
    }
    /// Must this object be placed inside a structure/container?
    pub fn must_be_interior(self) -> bool {
        matches!(
            self,
            ObjectCategory::CommandCenter | ObjectCategory::Radar | ObjectCategory::Sensor | ObjectCategory::Upgrade
        )
    }
}

/// The flat, category-agnostic stat bag. Every field defaults to zero/None so a JSON object only lists
/// what it uses. New stats can be added without touching existing data.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ObjectStats {
    /// Propulsion force (thrusters).
    #[serde(default)]
    pub thrust: f32,
    /// Power generated (+) or consumed (−).
    #[serde(default)]
    pub power: f32,
    /// Weapon id this mount fires (links to the ruleset weapon catalogue).
    #[serde(default)]
    pub weapon: Option<String>,
    /// Storage volume (tanks/containers).
    #[serde(default)]
    pub capacity: f32,
    /// Detection range (radar/sensor).
    #[serde(default)]
    pub sensor_range: f32,
    /// Extra armour value.
    #[serde(default)]
    pub armor: i32,
    /// Generic upgrade magnitude (interpreted by whatever it upgrades).
    #[serde(default)]
    pub boost: f32,
}

/// A buildable object definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectDef {
    pub id: String,
    pub name: String,
    pub category: ObjectCategory,
    /// The reusable parametric shape (the whole point of [`crate::shape`]).
    pub shape: Shape2D,
    /// Base mass (scaled with area when a placement resizes the shape).
    #[serde(default = "one")]
    pub mass: f32,
    #[serde(default = "hundred")]
    pub hp: i32,
    #[serde(default)]
    pub stats: ObjectStats,
}

fn one() -> f32 {
    1.0
}
fn hundred() -> i32 {
    100
}

impl ObjectDef {
    pub fn new(id: &str, name: &str, category: ObjectCategory, shape: Shape2D) -> Self {
        ObjectDef { id: id.into(), name: name.into(), category, shape, mass: 1.0, hp: 100, stats: ObjectStats::default() }
    }
    pub fn mass(mut self, m: f32) -> Self {
        self.mass = m;
        self
    }
    pub fn hp(mut self, h: i32) -> Self {
        self.hp = h;
        self
    }
    pub fn stats(mut self, s: ObjectStats) -> Self {
        self.stats = s;
        self
    }
}

/// A 2D rigid placement transform (local to the parent).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Transform2D {
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    /// Rotation in radians.
    #[serde(default)]
    pub rot: f32,
}

impl Transform2D {
    pub fn new(x: f32, y: f32, rot: f32) -> Self {
        Transform2D { x, y, rot }
    }
    /// `self ∘ child`: place `child` in this frame, yielding `child`'s transform in `self`'s parent.
    pub fn compose(&self, child: &Transform2D) -> Transform2D {
        let (c, s) = (self.rot.cos(), self.rot.sin());
        Transform2D {
            x: self.x + c * child.x - s * child.y,
            y: self.y + s * child.x + c * child.y,
            rot: self.rot + child.rot,
        }
    }
    /// Map a local point into the parent frame.
    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        let (c, s) = (self.rot.cos(), self.rot.sin());
        [self.x + c * p[0] - s * p[1], self.y + s * p[0] + c * p[1]]
    }
}

/// Dynamic repetition of a placement — a row, ring or stack generated from one entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repeat {
    pub count: u32,
    #[serde(default)]
    pub dx: f32,
    #[serde(default)]
    pub dy: f32,
    #[serde(default)]
    pub drot: f32,
}

/// What a placement instantiates: a concrete object, or a nested blueprint (the recursion).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "place", rename_all = "snake_case")]
pub enum PlacementKind {
    /// A concrete object from the catalogue.
    Object { def: String },
    /// Another blueprint, expanded in place — blueprints within blueprints.
    Blueprint {
        id: String,
        /// Literal argument values passed to the sub-blueprint's parameters.
        #[serde(default)]
        args: BTreeMap<String, f32>,
        /// Bind a sub-blueprint parameter to a variable in the *current* scope (dynamic wiring).
        #[serde(default)]
        arg_bind: BTreeMap<String, String>,
    },
}

/// One placement in a blueprint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    #[serde(default)]
    pub at: Transform2D,
    /// What this placement instantiates (a concrete object or a nested blueprint).
    pub kind: PlacementKind,
    /// Literal numeric customisation (e.g. `w`, `h`, `r` resize the shape).
    #[serde(default)]
    pub params: BTreeMap<String, f32>,
    /// Bind a customisation key (e.g. `w`) to a variable in the current scope — dynamic sizing.
    #[serde(default)]
    pub bind: BTreeMap<String, String>,
    /// Override the object's shape for this instance.
    #[serde(default)]
    pub shape: Option<Shape2D>,
    /// Objects placed INSIDE this one (only meaningful for structures/containers) — recursion.
    #[serde(default)]
    pub children: Vec<Placement>,
    /// Instantiate this placement many times.
    #[serde(default)]
    pub repeat: Option<Repeat>,
}

impl Placement {
    /// Place a concrete object at a transform (no children/params).
    pub fn object(def: &str, at: Transform2D) -> Self {
        Placement {
            at,
            kind: PlacementKind::Object { def: def.into() },
            params: BTreeMap::new(),
            bind: BTreeMap::new(),
            shape: None,
            children: vec![],
            repeat: None,
        }
    }
    /// Place a nested blueprint at a transform.
    pub fn blueprint(id: &str, at: Transform2D) -> Self {
        Placement {
            at,
            kind: PlacementKind::Blueprint { id: id.into(), args: BTreeMap::new(), arg_bind: BTreeMap::new() },
            params: BTreeMap::new(),
            bind: BTreeMap::new(),
            shape: None,
            children: vec![],
            repeat: None,
        }
    }
    /// Objects placed inside this one (structures/containers).
    pub fn with_children(mut self, children: Vec<Placement>) -> Self {
        self.children = children;
        self
    }
    /// A literal numeric customisation (e.g. `("w", 4.0)`).
    pub fn with_param(mut self, key: &str, v: f32) -> Self {
        self.params.insert(key.into(), v);
        self
    }
    /// Bind a customisation key to a scope variable (dynamic sizing).
    pub fn with_bind(mut self, key: &str, var: &str) -> Self {
        self.bind.insert(key.into(), var.into());
        self
    }
    /// Repeat this placement.
    pub fn with_repeat(mut self, repeat: Repeat) -> Self {
        self.repeat = Some(repeat);
        self
    }
    /// Override the shape for this instance.
    pub fn with_shape(mut self, shape: Shape2D) -> Self {
        self.shape = Some(shape);
        self
    }
    /// Pass a literal arg to a nested blueprint (only meaningful on a [`PlacementKind::Blueprint`]).
    pub fn with_arg(mut self, name: &str, v: f32) -> Self {
        if let PlacementKind::Blueprint { args, .. } = &mut self.kind {
            args.insert(name.into(), v);
        }
        self
    }
    /// Bind a nested blueprint's arg to a scope variable.
    pub fn with_arg_bind(mut self, name: &str, var: &str) -> Self {
        if let PlacementKind::Blueprint { arg_bind, .. } = &mut self.kind {
            arg_bind.insert(name.into(), var.into());
        }
        self
    }
}

/// A declared blueprint parameter (a setting / customization knob).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BpParam {
    pub name: String,
    pub default: f32,
    #[serde(default)]
    pub min: Option<f32>,
    #[serde(default)]
    pub max: Option<f32>,
}

/// A named, parametric, possibly-nested design.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blueprint {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub params: Vec<BpParam>,
    pub root: Vec<Placement>,
}

/// A read-only view over the object + blueprint catalogue (lives in the hot-reloadable ruleset).
#[derive(Debug, Clone, Copy)]
pub struct Catalog<'a> {
    pub objects: &'a [ObjectDef],
    pub blueprints: &'a [Blueprint],
}

impl<'a> Catalog<'a> {
    pub fn object(&self, id: &str) -> Option<&'a ObjectDef> {
        self.objects.iter().find(|o| o.id == id)
    }
    pub fn blueprint(&self, id: &str) -> Option<&'a Blueprint> {
        self.blueprints.iter().find(|b| b.id == id)
    }
}

/// One concrete part of a resolved craft.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPart {
    pub def: String,
    pub category: ObjectCategory,
    /// Transform of this part in the craft's root frame.
    pub world: Transform2D,
    /// The resolved (sized/overridden) shape.
    pub shape: Shape2D,
    pub mass: f32,
    pub hp: i32,
    pub stats: ObjectStats,
}

/// The fully expanded craft: every part plus the aggregate properties the sim/physics need.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCraft {
    pub parts: Vec<ResolvedPart>,
    pub total_mass: f32,
    pub total_hp: i32,
    pub total_thrust: f32,
    pub net_power: f32,
    pub weapon_mounts: Vec<String>,
    pub storage_capacity: f32,
    /// Mass-weighted centre of mass in the root frame.
    pub center_of_mass: [f32; 2],
    /// Bounding box of all parts in the root frame, `(min, max)`.
    pub bounds: ([f32; 2], [f32; 2]),
}

/// Recursion safety: a blueprint can nest at most this deep before resolution bails.
pub const MAX_BLUEPRINT_DEPTH: usize = 16;

/// Resolve a top-level blueprint by id into a concrete [`ResolvedCraft`]. `args` override the
/// blueprint's parameter defaults. Errors on an unknown id, a reference cycle, or exceeding
/// [`MAX_BLUEPRINT_DEPTH`].
pub fn resolve_blueprint(
    catalog: &Catalog,
    id: &str,
    args: &BTreeMap<String, f32>,
) -> Result<ResolvedCraft, String> {
    let mut parts = Vec::new();
    let mut path = Vec::new();
    resolve_blueprint_into(catalog, id, args, &Transform2D::default(), 0, &mut path, &mut parts)?;
    Ok(aggregate(parts))
}

fn resolve_blueprint_into(
    catalog: &Catalog,
    id: &str,
    args: &BTreeMap<String, f32>,
    parent: &Transform2D,
    depth: usize,
    path: &mut Vec<String>,
    out: &mut Vec<ResolvedPart>,
) -> Result<(), String> {
    if depth > MAX_BLUEPRINT_DEPTH {
        return Err(format!("blueprint nesting exceeded {MAX_BLUEPRINT_DEPTH} (cycle or too deep) at {id}"));
    }
    if path.iter().any(|p| p == id) {
        return Err(format!("blueprint cycle detected: {} -> {id}", path.join(" -> ")));
    }
    let bp = catalog.blueprint(id).ok_or_else(|| format!("unknown blueprint {id}"))?;

    // Build this blueprint's parameter scope: declared defaults (clamped) overridden by args.
    let mut scope: BTreeMap<String, f32> = BTreeMap::new();
    for p in &bp.params {
        let mut v = args.get(&p.name).copied().unwrap_or(p.default);
        if let Some(mn) = p.min {
            v = v.max(mn);
        }
        if let Some(mx) = p.max {
            v = v.min(mx);
        }
        scope.insert(p.name.clone(), v);
    }

    path.push(id.to_string());
    for placement in &bp.root {
        resolve_placement(catalog, placement, &scope, parent, depth, path, out)?;
    }
    path.pop();
    Ok(())
}

fn resolve_placement(
    catalog: &Catalog,
    placement: &Placement,
    scope: &BTreeMap<String, f32>,
    parent: &Transform2D,
    depth: usize,
    path: &mut Vec<String>,
    out: &mut Vec<ResolvedPart>,
) -> Result<(), String> {
    let n = placement.repeat.as_ref().map(|r| r.count.max(1)).unwrap_or(1);
    for i in 0..n {
        // Step transform for repeated instances.
        let local = if let Some(r) = &placement.repeat {
            Transform2D {
                x: placement.at.x + r.dx * i as f32,
                y: placement.at.y + r.dy * i as f32,
                rot: placement.at.rot + r.drot * i as f32,
            }
        } else {
            placement.at
        };
        let world = parent.compose(&local);

        // Effective numeric customisation: literals, then bindings from the current scope.
        let mut eff: BTreeMap<String, f32> = placement.params.clone();
        for (key, var) in &placement.bind {
            if let Some(v) = scope.get(var) {
                eff.insert(key.clone(), *v);
            }
        }

        match &placement.kind {
            PlacementKind::Object { def } => {
                let od = catalog.object(def).ok_or_else(|| format!("unknown object {def}"))?;
                let base_shape = placement.shape.clone().unwrap_or_else(|| od.shape.clone());
                let shape = base_shape.sized(eff.get("w").copied(), eff.get("h").copied(), eff.get("r").copied());
                // Mass/hp scale with the resized area, so a bigger plate weighs more.
                let area_ratio = {
                    let a0 = od.shape.area();
                    if a0 > 1e-6 { (shape.area() / a0).max(0.0) } else { 1.0 }
                };
                out.push(ResolvedPart {
                    def: od.id.clone(),
                    category: od.category,
                    world,
                    shape,
                    mass: od.mass * area_ratio.max(0.05),
                    hp: ((od.hp as f32) * area_ratio.max(0.1)).round() as i32,
                    stats: od.stats.clone(),
                });
                // Children placed inside this object (structure/container) are relative to it.
                for child in &placement.children {
                    resolve_placement(catalog, child, scope, &world, depth, path, out)?;
                }
            }
            PlacementKind::Blueprint { id, args, arg_bind } => {
                // Compose the sub-blueprint's args: literals, then bindings from this scope.
                let mut child_args = args.clone();
                for (param, var) in arg_bind {
                    if let Some(v) = scope.get(var) {
                        child_args.insert(param.clone(), *v);
                    }
                }
                resolve_blueprint_into(catalog, id, &child_args, &world, depth + 1, path, out)?;
            }
        }
    }
    Ok(())
}

fn aggregate(parts: Vec<ResolvedPart>) -> ResolvedCraft {
    let mut total_mass = 0.0;
    let mut total_hp = 0;
    let mut total_thrust = 0.0;
    let mut net_power = 0.0;
    let mut weapon_mounts = Vec::new();
    let mut storage_capacity = 0.0;
    let mut com = [0.0f32, 0.0];
    let mut mn = [f32::INFINITY, f32::INFINITY];
    let mut mx = [f32::NEG_INFINITY, f32::NEG_INFINITY];

    for p in &parts {
        total_mass += p.mass;
        total_hp += p.hp;
        total_thrust += p.stats.thrust;
        net_power += p.stats.power;
        storage_capacity += p.stats.capacity;
        if let Some(w) = &p.stats.weapon {
            weapon_mounts.push(w.clone());
        }
        com[0] += p.world.x * p.mass;
        com[1] += p.world.y * p.mass;
        // Bounds from the part's world-transformed outline.
        for v in p.shape.outline() {
            let wv = p.world.apply(v);
            mn[0] = mn[0].min(wv[0]);
            mn[1] = mn[1].min(wv[1]);
            mx[0] = mx[0].max(wv[0]);
            mx[1] = mx[1].max(wv[1]);
        }
    }
    if total_mass > 1e-6 {
        com[0] /= total_mass;
        com[1] /= total_mass;
    }
    if parts.is_empty() {
        mn = [0.0, 0.0];
        mx = [0.0, 0.0];
    }
    weapon_mounts.sort();
    ResolvedCraft {
        parts,
        total_mass,
        total_hp,
        total_thrust,
        net_power,
        weapon_mounts,
        storage_capacity,
        center_of_mass: com,
        bounds: (mn, mx),
    }
}

/// Validate a whole catalogue before it goes live: shapes are sane, every object/blueprint reference
/// resolves, interior-only objects are placed inside a holder, and no blueprint cycles exist. Returns
/// a human-readable error for the designer.
pub fn validate_catalog(catalog: &Catalog) -> Result<(), String> {
    for o in catalog.objects {
        o.shape.validate().map_err(|e| format!("object {}: {e}", o.id))?;
    }
    // Reference + zone + cycle checks by attempting a structural walk of each blueprint.
    for bp in catalog.blueprints {
        let mut path = Vec::new();
        check_blueprint(catalog, &bp.id, &mut path)?;
    }
    Ok(())
}

fn check_blueprint(catalog: &Catalog, id: &str, path: &mut Vec<String>) -> Result<(), String> {
    if path.iter().any(|p| p == id) {
        return Err(format!("blueprint cycle: {} -> {id}", path.join(" -> ")));
    }
    if path.len() > MAX_BLUEPRINT_DEPTH {
        return Err(format!("blueprint too deep at {id}"));
    }
    let bp = catalog.blueprint(id).ok_or_else(|| format!("unknown blueprint {id}"))?;
    path.push(id.to_string());
    for p in &bp.root {
        check_placement(catalog, p, None, path)?;
    }
    path.pop();
    Ok(())
}

fn check_placement(
    catalog: &Catalog,
    p: &Placement,
    parent_cat: Option<ObjectCategory>,
    path: &mut Vec<String>,
) -> Result<(), String> {
    match &p.kind {
        PlacementKind::Object { def } => {
            let od = catalog.object(def).ok_or_else(|| format!("unknown object {def}"))?;
            if od.category.must_be_interior() && !parent_cat.map(|c| c.holds_children()).unwrap_or(false) {
                return Err(format!(
                    "{def} ({:?}) must be placed inside a structure/container",
                    od.category
                ));
            }
            if let Some(s) = &p.shape {
                s.validate().map_err(|e| format!("placement of {def}: {e}"))?;
            }
            if !p.children.is_empty() && !od.category.holds_children() {
                return Err(format!("{def} ({:?}) cannot hold child objects", od.category));
            }
            for c in &p.children {
                check_placement(catalog, c, Some(od.category), path)?;
            }
        }
        PlacementKind::Blueprint { id, .. } => {
            check_blueprint(catalog, id, path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_objects() -> Vec<ObjectDef> {
        vec![
            ObjectDef::new("hull-1", "Hull Block", ObjectCategory::Structure, Shape2D::Rect { w: 2.0, h: 2.0 })
                .mass(2.0)
                .hp(200),
            ObjectDef::new("armor-tri", "Armor Wedge", ObjectCategory::Armor, Shape2D::Triangle { w: 2.0, h: 2.0, skew: 0.0 })
                .mass(1.5)
                .hp(150),
            ObjectDef::new("thruster", "Thruster", ObjectCategory::Thruster, Shape2D::Rect { w: 1.0, h: 1.5 })
                .stats(ObjectStats { thrust: 50.0, power: -5.0, ..Default::default() }),
            ObjectDef::new("turret", "Turret", ObjectCategory::Turret, Shape2D::Disc { r: 0.8 })
                .stats(ObjectStats { weapon: Some("blaster".into()), power: -2.0, ..Default::default() }),
            ObjectDef::new("command", "Command Center", ObjectCategory::CommandCenter, Shape2D::Rect { w: 1.0, h: 1.0 })
                .stats(ObjectStats { power: 10.0, ..Default::default() }),
            ObjectDef::new("tank", "Storage Tank", ObjectCategory::StorageTank, Shape2D::Disc { r: 1.2 })
                .stats(ObjectStats { capacity: 500.0, ..Default::default() }),
        ]
    }

    #[test]
    fn structure_holds_a_command_center_inside_it() {
        let objects = sample_objects();
        let bp = Blueprint {
            id: "core".into(),
            name: "Core".into(),
            params: vec![],
            root: vec![Placement {
                at: Transform2D::default(),
                kind: PlacementKind::Object { def: "hull-1".into() },
                params: BTreeMap::new(),
                bind: BTreeMap::new(),
                shape: None,
                children: vec![Placement {
                    at: Transform2D::new(0.2, 0.0, 0.0),
                    kind: PlacementKind::Object { def: "command".into() },
                    params: BTreeMap::new(),
                    bind: BTreeMap::new(),
                    shape: None,
                    children: vec![],
                    repeat: None,
                }],
                repeat: None,
            }],
        };
        let blueprints = vec![bp];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        assert!(validate_catalog(&cat).is_ok(), "valid: command center is inside a structure");
        let craft = resolve_blueprint(&cat, "core", &BTreeMap::new()).unwrap();
        assert_eq!(craft.parts.len(), 2);
        assert!(craft.parts.iter().any(|p| p.category == ObjectCategory::CommandCenter));
        // The command center inherits the hull's frame.
        let cc = craft.parts.iter().find(|p| p.def == "command").unwrap();
        assert!((cc.world.x - 0.2).abs() < 1e-4);
    }

    #[test]
    fn command_center_outside_a_structure_is_rejected() {
        let objects = sample_objects();
        let blueprints = vec![Blueprint {
            id: "bad".into(),
            name: "Bad".into(),
            params: vec![],
            root: vec![Placement {
                at: Transform2D::default(),
                kind: PlacementKind::Object { def: "command".into() }, // interior-only, but at top level
                params: BTreeMap::new(),
                bind: BTreeMap::new(),
                shape: None,
                children: vec![],
                repeat: None,
            }],
        }];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        assert!(validate_catalog(&cat).is_err(), "interior-only object must be inside a holder");
    }

    #[test]
    fn blueprints_nest_inside_blueprints() {
        let objects = sample_objects();
        // A "pod" blueprint: one hull with a turret child.
        let pod = Blueprint {
            id: "pod".into(),
            name: "Pod".into(),
            params: vec![],
            root: vec![Placement {
                at: Transform2D::default(),
                kind: PlacementKind::Object { def: "hull-1".into() },
                params: BTreeMap::new(),
                bind: BTreeMap::new(),
                shape: None,
                children: vec![Placement {
                    at: Transform2D::default(),
                    kind: PlacementKind::Object { def: "turret".into() },
                    params: BTreeMap::new(),
                    bind: BTreeMap::new(),
                    shape: None,
                    children: vec![],
                    repeat: None,
                }],
                repeat: None,
            }],
        };
        // A "wing" blueprint references the pod twice (blueprint within blueprint).
        let wing = Blueprint {
            id: "wing".into(),
            name: "Wing".into(),
            params: vec![],
            root: vec![
                Placement {
                    at: Transform2D::new(-3.0, 0.0, 0.0),
                    kind: PlacementKind::Blueprint { id: "pod".into(), args: BTreeMap::new(), arg_bind: BTreeMap::new() },
                    params: BTreeMap::new(),
                    bind: BTreeMap::new(),
                    shape: None,
                    children: vec![],
                    repeat: None,
                },
                Placement {
                    at: Transform2D::new(3.0, 0.0, 0.0),
                    kind: PlacementKind::Blueprint { id: "pod".into(), args: BTreeMap::new(), arg_bind: BTreeMap::new() },
                    params: BTreeMap::new(),
                    bind: BTreeMap::new(),
                    shape: None,
                    children: vec![],
                    repeat: None,
                },
            ],
        };
        let blueprints = vec![pod, wing];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        assert!(validate_catalog(&cat).is_ok());
        let craft = resolve_blueprint(&cat, "wing", &BTreeMap::new()).unwrap();
        // Two pods × (hull + turret) = 4 parts.
        assert_eq!(craft.parts.len(), 4);
        assert_eq!(craft.weapon_mounts, vec!["blaster", "blaster"], "two turrets mount the blaster");
        // The two pods are placed left and right.
        let xs: Vec<f32> = craft.parts.iter().filter(|p| p.def == "hull-1").map(|p| p.world.x).collect();
        assert!(xs.iter().any(|x| (*x + 3.0).abs() < 1e-4) && xs.iter().any(|x| (*x - 3.0).abs() < 1e-4));
    }

    #[test]
    fn parameters_drive_dynamic_customization() {
        let objects = sample_objects();
        // A parametric armor strip: its width is set by the blueprint parameter `span`.
        let bp = Blueprint {
            id: "strip".into(),
            name: "Armor Strip".into(),
            params: vec![BpParam { name: "span".into(), default: 2.0, min: Some(1.0), max: Some(20.0) }],
            root: vec![Placement {
                at: Transform2D::default(),
                kind: PlacementKind::Object { def: "armor-tri".into() },
                params: BTreeMap::new(),
                bind: BTreeMap::from([("w".into(), "span".into())]), // bind shape width to `span`
                shape: None,
                children: vec![],
                repeat: None,
            }],
        };
        let blueprints = vec![bp];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        let small = resolve_blueprint(&cat, "strip", &BTreeMap::new()).unwrap();
        let big = resolve_blueprint(&cat, "strip", &BTreeMap::from([("span".into(), 10.0)])).unwrap();
        let w_small = part_width(&small.parts[0]);
        let w_big = part_width(&big.parts[0]);
        assert!(w_big > w_small * 3.0, "the armor widened with the `span` parameter: {w_small} -> {w_big}");
        assert!(big.total_mass > small.total_mass, "the bigger plate weighs more");
    }

    fn part_width(p: &ResolvedPart) -> f32 {
        let (mn, mx) = p.shape.aabb();
        mx[0] - mn[0]
    }

    #[test]
    fn repeat_generates_a_row_of_objects() {
        let objects = sample_objects();
        let bp = Blueprint {
            id: "battery".into(),
            name: "Turret Battery".into(),
            params: vec![],
            root: vec![Placement {
                at: Transform2D::new(0.0, 0.0, 0.0),
                kind: PlacementKind::Object { def: "hull-1".into() }, // structure to hold them
                params: BTreeMap::new(),
                bind: BTreeMap::new(),
                shape: None,
                children: vec![Placement {
                    at: Transform2D::new(-4.0, 0.0, 0.0),
                    kind: PlacementKind::Object { def: "turret".into() },
                    params: BTreeMap::new(),
                    bind: BTreeMap::new(),
                    shape: None,
                    children: vec![],
                    repeat: Some(Repeat { count: 5, dx: 2.0, dy: 0.0, drot: 0.0 }),
                }],
                repeat: None,
            }],
        };
        let blueprints = vec![bp];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        let craft = resolve_blueprint(&cat, "battery", &BTreeMap::new()).unwrap();
        let turrets = craft.parts.iter().filter(|p| p.def == "turret").count();
        assert_eq!(turrets, 5, "repeat generated a row of 5 turrets");
        assert_eq!(craft.weapon_mounts.len(), 5);
    }

    #[test]
    fn cycle_and_depth_are_guarded() {
        let objects = sample_objects();
        // a -> b -> a
        let a = Blueprint {
            id: "a".into(),
            name: "A".into(),
            params: vec![],
            root: vec![Placement {
                at: Transform2D::default(),
                kind: PlacementKind::Blueprint { id: "b".into(), args: BTreeMap::new(), arg_bind: BTreeMap::new() },
                params: BTreeMap::new(),
                bind: BTreeMap::new(),
                shape: None,
                children: vec![],
                repeat: None,
            }],
        };
        let b = Blueprint {
            id: "b".into(),
            name: "B".into(),
            params: vec![],
            root: vec![Placement {
                at: Transform2D::default(),
                kind: PlacementKind::Blueprint { id: "a".into(), args: BTreeMap::new(), arg_bind: BTreeMap::new() },
                params: BTreeMap::new(),
                bind: BTreeMap::new(),
                shape: None,
                children: vec![],
                repeat: None,
            }],
        };
        let blueprints = vec![a, b];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        assert!(validate_catalog(&cat).is_err(), "a cycle is caught at validation");
        assert!(resolve_blueprint(&cat, "a", &BTreeMap::new()).is_err(), "and at resolution");
    }

    #[test]
    fn aggregate_sums_mass_thrust_power_and_storage() {
        let objects = sample_objects();
        let bp = Blueprint {
            id: "ship".into(),
            name: "Ship".into(),
            params: vec![],
            root: vec![Placement {
                at: Transform2D::default(),
                kind: PlacementKind::Object { def: "hull-1".into() },
                params: BTreeMap::new(),
                bind: BTreeMap::new(),
                shape: None,
                children: vec![
                    Placement {
                        at: Transform2D::new(0.0, -1.0, 0.0),
                        kind: PlacementKind::Object { def: "thruster".into() },
                        params: BTreeMap::new(),
                        bind: BTreeMap::new(),
                        shape: None,
                        children: vec![],
                        repeat: None,
                    },
                    Placement {
                        at: Transform2D::new(0.0, 0.0, 0.0),
                        kind: PlacementKind::Object { def: "command".into() },
                        params: BTreeMap::new(),
                        bind: BTreeMap::new(),
                        shape: None,
                        children: vec![],
                        repeat: None,
                    },
                    Placement {
                        at: Transform2D::new(0.5, 0.5, 0.0),
                        kind: PlacementKind::Object { def: "tank".into() },
                        params: BTreeMap::new(),
                        bind: BTreeMap::new(),
                        shape: None,
                        children: vec![],
                        repeat: None,
                    },
                ],
                repeat: None,
            }],
        };
        let blueprints = vec![bp];
        let cat = Catalog { objects: &objects, blueprints: &blueprints };
        let craft = resolve_blueprint(&cat, "ship", &BTreeMap::new()).unwrap();
        assert!((craft.total_thrust - 50.0).abs() < 1e-3, "one thruster -> 50 thrust");
        assert!((craft.net_power - (10.0 - 5.0)).abs() < 1e-3, "command +10, thruster -5 => +5 net");
        assert!((craft.storage_capacity - 500.0).abs() < 1e-3, "the tank contributes 500 capacity");
        assert!(craft.total_mass > 2.0, "hull + parts mass");
    }
}
