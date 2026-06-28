# Entity-anchored authority — the world follows the player

> "I hate sector clamping — the sectors should ADAPT to players and other servers should automatically
> take over." — Leif (`STATE-MODEL.md`, verbatim)

This is the answer to that directive, and to "make chunks/sections seamless — no lag transitions and
ideally no transition required at all… recursive AABB which follows the player and its ships and
factions." It is the design and API of `src/domain.rs`.

## Status & remainders (2026-06-28)

**Shipped this pass (committed + pushed to GitHub `development`; live-deployed):**
- `domain.rs` — the entity-anchored recursive-AABB partition core: `Bounds` (f64 world AABB), `Domain`
  (a player's ship+fleet+faction bubble that follows them), `DomainField` + `DomainTree` (broad-phase
  overlap = interest), `claim`/`claim_sticky` (environment ownership, hysteresis). Sim bridge:
  `from_sim`, `world_pos`, `Bounds::sectors`. 11 unit tests.
- `replica.rs` — `domain_field()` + `interest_sectors()` (seamless interest: 1/2/4 sectors).
- `client.rs` — `interest_set` is domain-driven (viewport bubble → overlapping sectors).
- `galaxywire.rs` + `galaxymap.rs` + `node.rs` + `lib.rs` host loop — hosts gossip `DomainFrame`s on
  `topics::DOMAINS`; the map folds them (`MapModel::on_domains`, `player_domains`, `live_players`).
- **Clients wired** — `spacegame-wasm` and `spacegame-native` subscribe to the `/in` of every interest
  sector each frame, pre-warming the neighbour so a crossing has no subscribe round-trip; inputs are
  sector-guarded so pre-warmed neighbours never ghost-join the local sim.
- 239 pure + 248 mesh SDK tests green; both frontends compile; deployed to `spa.ce-net.com`.

**Remainders (open, in priority order):**
1. **Per-entity environment authority is not in the tick** (see ladder item 4). `claim_sticky` is built +
   tested but deliberately not driving simulation: it conflicts with the shipped quorum-merge model
   (every player runs the *whole* region sim and agrees on a state hash; per-entity ownership would make
   replicas simulate different subsets and diverge). **Decision needed:** keep full-region replication,
   or move to per-entity authority (partial sim + cross-player entity exchange) — the latter is the
   "each player simulates its environment for others" scaling win, but it *replaces* the merge.
2. **Cross-seam rendering.** Clients pre-warm neighbour `/in` (no hitch crossing) but still render only
   their own replica's sector, so you don't *see* players across a seam until you cross. Needs neighbour
   `/state` consumption, or domains replacing sector-partitioning in the sim (item 3).
3. **Domains replacing the sector sim outright.** Today domains are a layer *on top* of the
   sector-partitioned `Sim` (authority/interest/map). The full vision — the sim itself partitioned by a
   moving AABB, no sectors at all — is a large `sim.rs` rewrite, deferred (and `sim.rs` is being reworked
   concurrently).
4. **Frontend lockfile pins.** `spacegame-wasm`/`-native` pin the `spacegame` git dep to `9eda512`
   (functionally complete; the later `fb259a5` only fixes a unit test). Bump for hygiene when convenient.
5. **Subscription pruning.** Clients accumulate `/in` subscriptions as they explore; far sectors are
   never unsubscribed. Bounded, but a prune pass would tighten bandwidth on long sessions.

## The problem with a grid

The adaptive galaxy (`galaxy.rs`) is real and it scales: space is a power-of-two quadtree, hot cells
split, cold cells merge, no coordinator. But every cut is **fixed in space**. A pilot loitering on a
cell seam crosses a boundary every few seconds, and every crossing is a hand-off — a `Sim` transit, a
re-subscription, a snapshot adopt. That is the lag transition. **A grid can never be seamless _for the
owner_, because the owner moves and the grid does not.** At Earth scale, played as a single human-sized
ship, you are *always* near some seam.

## The one idea

**Anchor authority to entities, not to coordinates.** Each player owns a **domain**: a recursive AABB
that bounds the things that player is authoritative for — their ship, their fleet, their faction's
structures — and therefore **moves with them**. The player sits at the centre of their own bubble and
**never crosses a boundary**, because the boundary travels with the ship. "Your own node is the server
for you" is exactly this: your node hosts your domain, your domain is always centred on you, your
authority is always zero-RTT and is never handed off.

```text
  faction box  ────────────────────────────────────┐   the Domain's outer bound
  │   fleet box ───────────────────┐                │   (what others replicate to see you)
  │   │  ship □ (you)              │   ◦ structure  │
  │   │     ◦ wingman  ◦ wingman   │     ◦ structure│
  │   └────────────────────────────┘                │
  └─────────────────────────────────────────────────┘
```

The AABB is **recursive** — the faction box is the union of the fleet box and the structure boxes; the
fleet box is the union of the ship boxes — so the structure literally *follows the player and its ships
and factions*, each level a union of the level below.

## How it scales: each player brings its own patch of world

Two domains **overlap** exactly when their owners can see each other; overlap *is* the interest relation.
A `DomainTree` (a recursive AABB over the domains themselves, rebuilt each tick from the moving anchors)
answers "which bubbles overlap mine" in `O(log n + k)`, never `O(players²)`.

The neutral environment near you — asteroids, hazards, wreckage — is **claimed by your domain** (the
nearest anchor) and simulated **on your node, on behalf of everyone who can see it**. So each player
contributes the compute for **itself and the patch of world around it**. Empty space has no domains and
costs nothing; a crowd is its own server farm, each node carrying its own bubble. This is the
STATE-MODEL principle made literal: *compute, authority, and storage for a region live on the nodes of
the players in that region.*

## Why it is seamless (no transition, by construction)

- **The owner is never transited.** Its ship is, by construction, always inside its own domain (the
  footprint is built around it). There is no seam under the player to cross, ever — at any position,
  Earth-scale or not. (`domain::tests::the_owner_is_never_outside_its_own_domain`.)
- **The only thing that changes hands is neutral environment**, and that is decided by a **pure, sticky
  function** (`DomainField::claim` / `claim_sticky`) recomputed every tick — *not* a discrete
  edge-crossing event. With hysteresis, an asteroid sitting in the overlap of two equally close bubbles
  does not flap between owners. Nothing is *scheduled* to transition, so nothing can stutter.
- **Determinism.** Domains, overlaps, and the environment owner are pure functions of entity positions
  (no clock, no float-fragile ordering — ties break on `owner` string). Every replica computes the same
  *who-simulates-what*, so the replicated-authority merge (`replication.rs`) agrees without a
  coordinator — the same trick `galaxy.rs` uses for cell shape, applied to moving bubbles.

## The API (`src/domain.rs`)

| Item | What it is |
|---|---|
| `Bounds` | `f64` world-scale AABB (`union`/`intersects`/`contains`/`expanded`/`around`). The world-frame analogue of the `f32`, sector-local `aabb::Aabb`. |
| `Owned` | trait: `owner()` + `entity_id()` + `pos()` + `radius()`. Keeps the module pure/testable, like `partition::Positioned`. The sim's `Ship` and faction entities satisfy it. |
| `Domain` | one player's bubble: `{ owner, anchor, bounds, interest, entities }`. `Domain::build` unions the owned entity boxes (the recursive footprint) and grows it by the view radius. |
| `DomainField` | all domains + the broad-phase `DomainTree`. `from_entities` groups by owner; `interest(owner)` → overlapping domains; `claim`/`claim_sticky` → environment owner. |

## Relationship to the grid quadtree

Domains **replace the grid as the unit of authority and interest** (who simulates what, who you receive).
The `galaxy.rs` `CellId` quadtree is **not deleted** — it stays as the coarse, stable **addressing /
rendezvous / bootstrap** layer: a name for a patch of space to discover peers and seed a region. You
join by resolving a cell, then live inside moving domains. Grid for *addressing*; bubbles for *play*.

## Integration ladder (status)

`domain.rs` is the pure, tested core. The wiring through the live game is now in place; what's done and
what remains:

1. **World-frame bridge — DONE.** `domain::world_pos(sector, lx, ly)` + `DomainField::from_sim(sim,
   view_radius)` turn a sector's sector-local-`f32` `Sim` into absolute-`f64` domains, grouping every
   ship by its authority owner (a human ship by its pilot, an NPC fleet unit by its faction's player) so
   a player's ship + fleet + faction fold into one bubble. `Bounds::sectors()` resolves a bubble to the
   sectors it overlaps. Tested.
2. **`Replica` interest + clients — DONE (end to end).** `Replica::domain_field()` /
   `Replica::interest_sectors(me, view_radius)` and the shared `ClientProfile::interest_set(world_x,
   world_y)` give the seamless interest set — one sector mid-sector, two on an edge, four on a corner,
   growing with the fleet — replacing the fixed ring. **Both clients now call it:** `spacegame-wasm` and
   `spacegame-native` subscribe to the `/in` of every sector their viewport bubble overlaps each frame,
   so the neighbour they are about to cross into is *already* subscribed — the hand-off has no subscribe
   round-trip at the seam. Neighbour inputs are filtered out (wasm tags each queued input with its
   sector; native already had the `sector_of(topic) == sector_tok` guard) until the replica actually
   re-homes, so a pre-warmed subscription never spawns a ghost in the local sim. Tested
   (`replica::tests::interest_grows_to_the_neighbour_as_the_bubble_nears_a_seam`,
   `client::tests::interest_follows_the_player_and_slides_across_seams`).
3. **Live map — DONE (end to end).** Hosts publish a `galaxywire::DomainFrame` (compact `DomainView`
   bubbles) on `topics::DOMAINS` once per advertise interval (`run_sector`); the node control loop and
   `observe_galaxy` subscribe and fold via `MapModel::on_domains`; `MapModel::player_domains()` and
   `MapSummary::live_players` expose the moving "who is where" dots, and the `spacegame galaxy` ASCII
   dump shows the bubble count. Tested (`galaxymap::tests::domain_frames_track_moving_player_bubbles`).
4. **Environment authority — API ready; intentionally NOT forced into the tick.** `DomainField::claim` /
   `claim_sticky` decide which single node simulates each neutral entity. But the shipped authority model
   (`NETCODE.md`) is **deterministic full-region replication merged by quorum**: every player in a region
   runs the *whole* sim and agrees on a state hash. Per-entity ownership (each player simulating only the
   neutral entities its bubble claims) is a *different* model — it would make replicas simulate different
   subsets and so diverge on the quorum hash. Wiring `claim` into the tick is therefore a real
   architectural change (partial-sim + cross-player entity exchange) that must replace, not coexist with,
   the quorum merge — and it lands on the per-tick `Sim` path another agent is actively reworking. So the
   claim primitive is built, tested, and ready, but deliberately left un-forced into the hot loop until
   that model decision is made. This is the one honest gap, and it is a design choice, not missing code.

The seam-free, deterministic, coordinator-free authority partition that follows the player — its
world-frame bridge, replica + client interest (wired through both frontends), and the live-map path — is
designed, compiled, unit-tested, and driving the clients. The remaining item (per-entity environment
authority) is gated on a deliberate model decision, documented above.
