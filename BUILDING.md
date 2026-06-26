# Free-form building & recursive blueprints

Spacegame craft are **built**, not picked from a fixed list. You place objects — structure blocks,
armour, weapons, turrets, guns, thrusters, command centres, radars, sensors, storage tanks, containers
and upgrades — at any position and angle, with **variable shapes**, and you compose those into
**reusable, nestable blueprints** that resolve to a concrete craft at runtime. All of it is data, so all
of it is hot-reloadable (it lives in the [`Ruleset`](src/ruleset.rs) like weapons and tech).

It was built in the order the design demands, bottom-up.

## 1. The dynamic shape system (`shape.rs`) — built first, reused by everything

One geometry kernel that every block type reuses. [`Shape2D`] is a parametric, serializable shape:

- `Rect { w, h }` — any width × height
- `Triangle { w, h, skew }` — any width/height, and `skew` leans the apex so the **angles** vary freely
- `Trapezoid { top_w, bottom_w, h, top_skew }`
- `Disc { r }` (a true circle in physics)
- `RegularPolygon { sides, r }` (hexes, octagons…)
- `Polygon { verts }` — an arbitrary convex outline

Every shape yields its **outline** (centred on its centre of mass, ready for render + physics), its
**area / centroid / AABB / bounding radius / unit moment of inertia**, a **[`physics::Shape`]** for
collision, and can be **resized per placement** (`sized(w, h, r)`) or uniformly **scaled**. So one
"armour plate" definition becomes a whole family of plates just by changing parameters — no new data.

## 2. Objects & free-form placement (`build.rs`)

An [`ObjectDef`] is a buildable thing: a [`ObjectCategory`], a reused `Shape2D`, `mass`/`hp`, and a flat
[`ObjectStats`] bag (thrust, power ±, weapon mount, storage capacity, sensor range, armour, boost). New
stats are additive — no schema churn.

Placement rules fall out of the category:
- **Structure** and **Container** can hold **child objects placed inside them**.
- **Command centre, radar, sensor, upgrade** are interior-only — they *must* be placed inside a
  structure/container (the validator enforces it, exactly as asked: "inside structure blocks you can
  place command centers and radars and sensors").
- Everything else (armour, weapons, thrusters, tanks…) mounts on the hull.

A [`Placement`] puts an object at a [`Transform2D`] (x, y, rotation), optionally **resized**, optionally
with **children**, optionally **repeated** (a `Repeat` makes a row/ring/stack from one entry), and
optionally a per-instance **shape override**.

## 3. Recursive blueprints (`build.rs`)

A [`Blueprint`] is a named, parametric design: declared parameters ([`BpParam`] with defaults + min/max
= the settings/customization) and a tree of placements. A placement can be an **object** *or a reference
to another blueprint* — so **blueprints contain blueprints contain blueprints**. Parameters are wired in
dynamically: a placement can `bind` a shape size (e.g. `w`) to a blueprint parameter, and a
sub-blueprint reference can `arg_bind` its parameters to the parent's scope.

[`resolve_blueprint`] expands the whole thing **at runtime**:
1. builds each blueprint's parameter scope (defaults, overridden by args, clamped),
2. composes transforms down the tree,
3. expands `repeat`s,
4. recurses into sub-blueprints — with a **cycle guard** and a `MAX_BLUEPRINT_DEPTH` cap,
5. produces a flat [`ResolvedCraft`]: every concrete part with its **world transform** and **resolved
   shape**, plus the aggregate **mass, hp, thrust, net power, weapon mounts, storage capacity, centre of
   mass and bounding box** — precisely what the sim and the rigid-body physics consume to fly the craft.

### Built-in example

The ruleset ships a starter catalogue (every category, several shape families) and two blueprints: a
`turret-pod` (a structure block holding a turret) and a parametric `scout` that **nests two turret
pods**, places a **command centre + radar inside a structure**, **repeats** a spine of blocks, mounts
thrusters, and **sizes an armour plate from its `armor` parameter**. Resolving it:

```rust
let craft = ruleset.resolve_craft("scout", &Default::default())?;   // 13 parts, 2 weapon mounts
let armoured = ruleset.resolve_craft("scout", &BTreeMap::from([("armor".into(), 6.0)]))?; // heavier
```

Edit any of it — a new shape, a new object, a rebalanced blueprint, a new sub-assembly — and push the
ruleset; it goes live mid-match like every other piece of content.

## What's next

The data + resolution layer is complete and tested. Wiring a `ResolvedCraft` into a live ship (its mass
and moment from the parts, thrust from its thrusters, weapon mounts firing the ruleset weapons, and
per-part damage so chunks blow off) is the follow-on that makes built craft fly and fight — the
resolver already outputs exactly those aggregates and per-part shapes/transforms to make it
straightforward.
