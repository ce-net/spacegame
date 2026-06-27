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

## 3. Always-alive factions — an NPC fleet under your command (`faction.rs` + `sim.rs`)

Every player owns a [`Faction`]: harvesters, refineries, solar, factories, shipyards and turrets; a
roster of drones/fighters/haulers; a resource stockpile; a tech list. It runs a deterministic economy
**every tick, online or offline**:

1. buildings generate and refine resources;
2. the build queue advances and completes;
3. when idle, the [`AutoPolicy`] **spends your resources for you** — it grows the economy toward target
   ratios, then expands the military, then upgrades — so you return to a bigger base and a larger fleet
   you never had to micromanage.

**The roster is not abstract — it is real ships in the world.** Each unit the economy builds is
reconciled into an actual NPC ship (`sim::reconcile_fleets`): drones, fighters and haulers spawn near
you, fly, collide and fight with the same authoritative simulation as players. They obey a standing
**[`FactionCommand`]** you set over the wire (`defend` / `follow` / `mine` / `hold` / `attack_nearest` /
`attack_move`): the fleet AI (`sim::drive_npcs`) makes fighters hunt enemy ships (any ship of another
faction), drones seek and mine asteroids (banking minerals straight to your stockpile), and haulers
escort. A fleet ship that dies is removed and **struck from the roster** — you lose a ship and must
build another; NPCs do not respawn.

Everything is **tracked**: per-player faction summaries (economy + live fleet count + standing order)
ride the snapshot wire as `FactionView`, and the NPC ships carry `owner`/`role` so a client and the e2e
can tell your fleet from enemies. Live mining (yours or a drone's) deposits into the owning faction.
Factions and their fleet ships are serialized into the sector snapshot and replicated (§4), so a host
handover never costs you a tick of production or a ship. "Your faction is always alive" is literal: it
ticks — builds, mines, fights — whether or not your own ship is in the sector.

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

## 5. Local-first, replicated authority — zero delay + anti-cheat (`sim::state_hash`, `replication::agree`)

Two properties fall out of the deterministic simulation, and they are what the user asked for:

**Zero delay locally.** Because [`Sim`](src/sim.rs) is a pure, deterministic function of its inputs, the
**same code runs on your own node** for the patch of space around you. Your inputs apply *instantly* to
your local copy — there is no round-trip to a server before your ship turns — while the authoritative
snapshots stream in to confirm and, if needed, gently correct. The local node predicts; the mesh
confirms. (The wire and the sim are already shared, so the in-browser WASM node runs the identical
logic — see the e2e mobile leg.)

**No one can cheat — agreement by replication.** The region is simulated by **several replicas at once**
(§4), not one server. Every replica periodically hashes its authoritative state with
[`Sim::state_hash`] — a deterministic digest (floats quantised, order-independent fields XOR-folded) —
and publishes a [`StateProof`] on the region's proof topic. [`replication::agree`] compares them: the
hash a **strict majority** computed is the accepted truth, and any replica that computed a different
hash diverged — a bug, a desync, or a host that fudged the rules (teleported a ship, faked a kill,
minted minerals). A cheater changes the hash, so it is the odd one out and gets outvoted; honest nodes
agree for free because they ran the same deterministic code. A 1-vs-1 split is reported as *no quorum*
rather than a false accusation, so one liar cannot frame one honest node. The host loop publishes and
judges these proofs every checkpoint and logs/acts on dissent; the same replica machinery (§4) then
re-syncs or excludes the divergent node.

This is the same determinism the GPU/fault-tolerance story needs, used for a third purpose:
**redundancy, anti-cheat, and instant local feel are all the deterministic-replica property seen from
different angles.**

### Why this is also the GPU / cross-compatibility forcing function

For a backup to take over **instantly**, its replica must have been evolving **bit-identically** to the
primary. That forces the whole simulation to be deterministic across heterogeneous hardware — the same
result whether a node runs the physics on a CPU loop or a GPU compute shader. So `physics.rs` keeps its
hot state as plain `Copy` arrays stepped by fixed-iteration kernels (no heap, no order-dependent
floating shortcuts), which is exactly the layout a GPU wants. **Fault tolerance and GPU
cross-compatibility are the same requirement seen from two sides: every replica must agree, on any
device.** Spacegame is the project that keeps us honest about both at once.

---

## 6. Physics-based mining: magnetic alloy loot (`sim.rs`)

Mining is **no longer instant**. A depleted asteroid does not blink its value into your wallet — it
**sheds physical alloy nuggets** ([`sim::Loot`]) at the rock, each with an outward drift. A nugget is
then **magnetised toward the nearest ship** within [`sim::LOOT_MAGNET_R`] (found through the same
per-tick AABB broad-phase as everything else), accelerates, glides (speed-capped + lightly damped), and
is **collected on contact**, crediting that ship's spendable minerals and its faction stockpile. The
"fly over the spray and vacuum it up" loop is the satisfying part the arcade instant-credit lacked.

It is fully authoritative and deterministic: nuggets are spawned from a hash of the rock cell (no rng,
so every replica spawns the identical field), folded into [`Sim::state_hash`] (a host that mints or
palms floating loot diverges from the quorum), and carried in the [`snapshot::SectorSnapshot`] so a host
hand-over never loses loot a player was about to grab. The wire carries it as
[`wire::LootView`]; a resynced renderer draws it exactly like the existing pickups (`#[serde(default)]`,
so older clients simply ignore it).

Tuning lives in `sim.rs` constants for now (`ALLOY_PER_NUGGET`, `LOOT_MAGNET_R`, `LOOT_PULL`,
`LOOT_MAX_SPEED`, `LOOT_DAMPING`, `LOOT_PICKUP_R`, `LOOT_LIFE_TICKS`) — a follow-up promotes them into
the hot-reloadable [`ruleset::Tunables`] so loot feel can be tweaked live like everything else.

See [`docs/world.md`](docs/world.md) for the seamless player-following world this mining lives in.
