# Spacegame systems: LOD physics, nested AABBs, always-alive factions, replica fault tolerance

Spacegame is the development vehicle for the hard part of a real-time mesh game: **keeping critical,
high-framerate simulation correct and alive when the machines running it can vanish at any instant, and
when the same world must run on wildly different hardware (CPU and GPU).** This document covers the four
systems that make that work. Capacity/sharding is in [`SCALING.md`](SCALING.md); this is the realtime,
physics, persistence and fault-tolerance core.

---

## 1. Advanced 2D rigid-body physics with level-of-detail (`physics.rs`)

Every solid thing — ships, debris, asteroids, planets — is a [`RigidBody`] with mass, linear and
angular velocity, restitution and friction. Contacts are resolved with a **sequential-impulse solver**:
a normal impulse with restitution plus a Coulomb friction impulse applied **at the contact point**, so
bodies bounce, **spin from off-centre hits**, and grind — real rigid-body behaviour, not push-apart.
Narrow-phase handles circle/circle, circle/convex and convex/convex (SAT); broad-phase is the dynamic
AABB tree (§2).

**Level of detail is the scale dial.** Physics fidelity is *not* uniform:

| Tier | Where | Substeps × iterations |
|---|---|---|
| `High` | on your node and nearby players' nodes, right around the action | 4 × 8 |
| `Medium` | a ring out | 2 × 4 |
| `Low` | distant | 1 × 2 |
| `Registered` | beyond any interested node | integrate only, no contact solve |

`assign_lod(bodies, focus, …)` tags each body from its distance to the focus points (the local player
and the nearby players whose replicas live here). **Full physics is local; cheap physics is global** —
the reason a million-body galaxy is affordable. High precision and high framerate happen where players
actually are; far away, the world is merely *registered* so distant players still see motion.

---

## 2. Recursive AABBs that hold recursive AABBs, and that follow objects (`aabb.rs`)

Two trees, by purpose:

- **`AabbTree`** — a per-tick *static* loose quadtree, rebuilt each frame, used for the gameplay
  broad-phase (bullet/ship, hitscan, homing, viewport interest). Cheap to build, provably complete.
- **`DynamicAabbTree`** — a Box2D-style *dynamic* BVH with **fat AABBs**: a moving object only
  re-inserts when it leaves its slack box, so the tree **follows players, ships, debris, asteroids and
  planets** frame to frame at log cost instead of a full rebuild. Insertion uses a surface-area
  heuristic and the tree self-balances with rotations.

**Nesting** — `Compound<T>` is a recursive AABB that *holds another recursive AABB*: a compound object
(a ship of modules, an asteroid cluster, a planet with stations) carries its own local
`DynamicAabbTree` plus a `Transform` placing it in the world. The world tree indexes the compound as a
single fat leaf; a deep query maps into the compound's local frame and descends its nested tree, so a
railgun ray or a collision resolves right down to the struck module — and the whole nested structure
follows the parent as it translates and rotates.

---

## 3. Always-alive factions / factories / swarm (`faction.rs`)

Every player owns a [`Faction`]: harvesters, refineries, solar, factories, shipyards and turrets; a
swarm of drones/fighters/haulers; a resource stockpile; a tech list. It runs a deterministic economy
**every tick, online or offline**:

1. buildings generate and refine resources;
2. the build queue advances and completes;
3. when idle, the [`AutoPolicy`] **spends your resources for you** — it keeps the economy growing toward
   target ratios, then expands the military, then upgrades — so you return to a bigger base and a larger
   swarm you never had to micromanage.

Live mining in the arena deposits straight into your faction stockpile, bridging the twitch game to the
persistent industrial layer. Factions are serialized into the sector snapshot and replicated (§4), so a
host handover never costs you a tick of production. "Your faction is always alive" is literal: the
faction ticks independently of whether your ship is even in the sector.

---

## 4. Proximity-replica fault tolerance (`replication.rs`)

The headline reliability system. A region keeps a **replication constraint** of `k` high-precision
replicas at all times, placed on the **devices of the players who are close in-game** — they already
simulate the region at `High` LOD, so a replica is nearly free and instantly warm.

- **Detect** — replica holders heartbeat on the region topic; `ReplicaSet::expire` flags a member that
  goes silent.
- **Promote** — if the dropped member was the authoritative primary, `ReplicaSet::plan` picks the best
  healthy backup. The decision is **deterministic**, so every holder computes the same winner — no split
  brain, no coordinator.
- **Re-replicate** — when healthy replicas fall below `k`, the plan **copies the high-precision map to
  the next-best node** (the nearest remaining player's device, else a capable mesh node). The map is the
  content-addressed snapshot plus a live delta, so "copy" is a CID fetch from the nearest holder — every
  node that has it is a CDN edge.

Failure detection, promotion and re-replication are the pure, unit-tested decision layer
(`ReplicaSet::plan`); the heartbeat publish, candidate gathering and snapshot copy are thin `ce-rs`
calls in `director.rs`, driven from the host loop in `lib.rs`.

### Why this is also the GPU / cross-compatibility forcing function

For a backup to take over **instantly**, its replica must have been evolving **bit-identically** to the
primary. That forces the whole simulation to be deterministic across heterogeneous hardware — the same
result whether a node runs the physics on a CPU loop or a GPU compute shader. So `physics.rs` keeps its
hot state as plain `Copy` arrays stepped by fixed-iteration kernels (no heap, no order-dependent
floating shortcuts), which is exactly the layout a GPU wants. **Fault tolerance and GPU
cross-compatibility are the same requirement seen from two sides: every replica must agree, on any
device.** Spacegame is the project that keeps us honest about both at once.
