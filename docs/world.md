# The seamless, player-following world (`src/world.rs`)

> Requirement (VISION.md #21): *"make chunks / sections seamless — no lag transitions and ideally no
> transition required at all … our own player server must of course follow us … a huge open world with
> real earth size playing as a human … if each player takes care of itself and its environment for
> other players it will scale very well. recursive aabb which follows the player and its ships and
> factions. Later used for broad phase physics."*

This is the design of the model that replaces the fixed sector grid. It is implemented, pure, and
unit-tested in [`src/world.rs`](../src/world.rs); the mesh host loop is being ported onto it (the
sector path in `shard.rs` still runs today — see the migration note at the end).

---

## 1. The problem with fixed sectors

The original galaxy is a grid of fixed `3000 × 3000` [`shard::SectorId`] tiles, each rendezvous-hashed
onto some node. Two hard ceilings:

1. **Transitions.** A ship crossing a tile edge is *handed between two unrelated hosts*. However well
   done, that is a discrete boundary where state migrates and a hitch can show. A seamless, walk-around,
   Earth-sized world cannot have a seam every 3 km.
2. **Range & precision.** Sector-local `f32` runs `0..3000`. An Earth-sized world (~4·10⁷ units around)
   in raw `f32` quantises to *metres* — useless when you are also a human standing on a surface.

## 2. The model: authority **bubbles** that follow the player

Authority is no longer a tile owned by a hashed stranger. It is a **bubble that follows you**: a
recursive AABB that tightly bounds *your ship, your fleet, and your faction*, and **your own node is its
primary**. "Each player takes care of itself and its environment for other players" is literal — you
simulate your bubble, for everyone who can see it.

Bubbles overlap and slide continuously, so:

- **There is no grid line to cross, therefore no transition.** As you fly, the set of peers you exchange
  state with changes smoothly ([`World::interest`]), and authority for any patch of space migrates with
  a **dead-band** ([`World::handoff`]) so it never flaps at a border. The hand-off is a slope, not a
  step.
- **It scales by construction.** No node ever simulates more than the players whose bubbles it owns or
  neighbours; cold space costs nothing because no bubble covers it.
- **The same AABBs are the physics broad-phase.** A bubble *is* a recursive AABB (the world index is a
  recursive `f64` AABB tree, [`BubbleTree`]); nesting the per-cell `f32` trees under each bubble gives
  the "recursive AABB that holds recursive AABBs and follows objects" the physics later queries.

## 3. Layer A — floating-origin coordinates ([`Wpos`], [`Frame`])

- The canonical world position is **`f64`** ([`Wpos`]): ~15–16 significant digits, i.e. sub-millimetre
  anywhere on a planet-sized map. No global `f32` precision cliff.
- The hot simulation still runs in **`f32`**, but **local to a [`Frame`] origin kept near the action**.
  Near the origin `f32` is dense, so the deterministic `f32` physics kernels (and therefore the
  replica-agreement / GPU-cross-compat property) are *unchanged* — they just operate in a rebased frame.
- When the focus drifts past [`REBASE_LIMIT`], the node [`Frame::rebased_to`] a fresh origin snapped to
  the [`REBASE_QUANTUM`] lattice. Because both origins are lattice multiples, the shift applied to every
  local coordinate is **exact** in `f32`, and two nodes that rebase near the same place pick the *same*
  origin — so independent replicas stay bit-identical.

Tested: a point ~1.2·10⁷ out round-trips through `f32` locals to < 1 mm; the same value stored directly
in `f32` is > 0.4 units off; a rebase leaves the world provably unmoved.

## 4. Layer B — bubbles, the world index, and the three queries ([`World`])

A node feeds bubble announcements (its own + neighbours heard on a pubsub topic) into a [`World`], then
asks three pure questions each tick:

| Query | Replaces | Answer |
|---|---|---|
| [`World::authority_for(p)`] | rendezvous hash of a tile | the owner whose bubble contains `p`, nearest-focus wins — usually you, for your own space |
| [`World::interest(view)`] | fixed 3×3 `SectorId::neighbors` | every owner whose bubble overlaps your view box — a set that *slides* as you move, no edge to cross |
| [`World::handoff(at, cur, hys)`] | sector-edge transit | the owner a body should migrate to, with a **dead-band** so a body on a boundary cannot oscillate |

Overlap is served by [`BubbleTree`] — a recursive `f64` loose-quadtree, the planet-scale analogue of
`aabb::AabbTree`, with the same "pad-by-max-half-extent" lossless prune and a brute-force completeness
test. All outputs are sorted/deterministic so every replica computes the identical set.

## 5. How it maps to the existing systems

- **LOD physics (`physics.rs`)** already keys precision off *distance to focus points*. Bubbles make the
  focus set exact: a node runs `High` LOD inside its own bubble and the overlap with neighbours, `Low`
  out to the bubble edge, `Registered` beyond — same dial, now per-bubble instead of per-sector.
- **Proximity-replica fault tolerance (`replication.rs`)** is unchanged in spirit: the replicas of *your*
  bubble are the neighbours whose bubbles overlap yours ([`World::neighbors`]) — the players already
  simulating you at `High`, so a replica is nearly free and instantly warm.
- **Anti-cheat agreement (`Sim::state_hash`)** is unchanged: bubbles are an authority-placement layer,
  not a rules change; overlapping replicas still hash and vote.

## 6. Migration status

- **Done & tested:** the whole `world.rs` module — `Wpos`/`Frame`, `WAabb`, `Bubble`, `World`,
  `BubbleTree` — with unit tests for precision, rebasing, follow, authority, interest, hand-off
  dead-band, neighbours, and broad-phase completeness.
- **Next (host loop):** announce each host's bubble on a `…/bubble` pubsub topic (mirroring the replica
  heartbeat), maintain a `World` from those announcements, and drive `interest` / `handoff` from it in
  place of `SectorId::neighbors` / the transit path. The bubble's `bounds` is computed from the owner's
  ship + `reconcile_fleets` NPCs + faction structures each tick.
- **Then (coords):** thread a `Frame` through `Sim` so ship `x/y` are frame-local `f32` and the wire
  carries the frame origin, unlocking the Earth-sized surface. Until then the sim stays sector-local and
  the two models coexist.
