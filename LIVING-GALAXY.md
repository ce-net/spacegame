# The Living 1:1 Galaxy — mandate, feasibility, and architecture

This document records the directive for taking spacegame from a load-partitioned grid to a
continuously-simulated, **1:1-scale, living** galaxy, and answers — honestly, with the budget math —
whether living state is possible at that scale.

It supersedes the framing in [`GALAXY-SCALE.md`](GALAXY-SCALE.md) (which solves *compute placement
over empty continuous space*) by adding the layers that make the space an actual galaxy: a coordinate
foundation that spans atmosphere-to-galaxy, a Newtonian celestial hierarchy, physical FTL travel, and
persistent living state.

---

## 1. The mandate (verbatim — every word, as given)

> We need to make it a lot smarter. this game will simulate entire 1:1 galaxies of scale -> not just a
> simple small grid using recursive aabb

**Dimensionality:**

> Stay 2D disk

**Scale & travel model:**

> No jumping. it will be actually simulated newtonianly adn you have to physically travel between them
> using technologies like warp drives - physically simulated warp drives which makes your ship travel
> faster than speed of light with advanced flight planning and realistic orbital mechanics from
> suborbital in atmosphere to galaxy star traversing

**Living state:**

> We need to have living state of course... is this possible with the scale we scope for? Document
> every single word i say verbatim

*(Verbatim including original spelling, e.g. "adn", per the instruction.)*

### What this pins down

- **2D disk.** Top-down plane; `Vec2` stays. "Scale" means range + precision + structure, not 3D volume.
- **No jump graph.** Travel is *continuous and physical*. You accelerate, orbit, and **warp** through
  real space. A warp drive is a physically-simulated FTL state, not a teleport between nodes.
- **One continuous Newtonian world** from suborbital atmospheric flight up to crossing between stars —
  realistic orbital mechanics the whole way, no scene cuts.
- **Living state is required**, not optional — the galaxy must change and persist.

---

## 2. The verdict

**Yes — a living 1:1 galaxy is possible, but only if living state is *sparse and multi-resolution*.**

You cannot full-fidelity-simulate 10^11 star systems every frame, and nothing can. You do not need to.
Three facts collapse the impossible into the affordable:

1. **Newtonian orbital motion is integrable, so dormancy is free.** A body bound to a primary has a
   closed-form position at any time `t` (Kepler's equation is the exact integral of Newtonian gravity
   for the dominant-mass case). A system with no player in it is **not ticked** — its state at time `T`
   is a pure function of `(seed, T)`, computed `O(1)` on demand, costing zero storage and zero
   background CPU. The entire dormant galaxy is therefore free.
2. **Full numerical integration is bounded to active bubbles.** Ships under thrust, weapons, collisions,
   close multi-body encounters — the genuinely non-analytic work — happens only where players are. That
   set is `O(players)`, not `O(stars)`, and is exactly what the existing per-cell sim + autoscaling
   quadtree already bound.
3. **Persistent change is sparse.** A galaxy of 10^11 systems, even wildly successful with 10^6
   concurrent players, accumulates history in maybe 10^6–10^8 systems — the ones someone touched.
   Everything else stays at its deterministic baseline. Sparse, content-addressed, chain-anchored,
   sharded across the mesh: ~100 MB–100 GB of *total* living delta across the whole network, a
   rounding error per node.

So the cost of the galaxy scales with **attention and history, not with size**. That is the whole
trick, and it is the same trick the codebase already uses for content (`worldgen.rs`) and compute
(`galaxy.rs`) — extended to time and to life.

**The honest limit:** you do **not** get 10^11 independent full-economy simulations running in real
time. You get: a deterministic baseline everywhere; *real, persistent history* wherever a player or
faction acted; *macro-fidelity evolution* of inhabited regions; and *full discrete simulation* on the
active frontier. The life is real and durable, but its **fidelity is LOD'd by attention**. That is the
only physically possible answer — and it is a good one (it is how EVE and Dwarf Fortress feel alive).

---

## 3. Core principle: resolution follows attention

Every axis of the galaxy is rendered at a resolution proportional to how closely it is being watched,
and every axis falls back to a deterministic, free baseline when unwatched.

| Axis | Baseline (free, everywhere) | High-res (bounded, where watched) |
|---|---|---|
| **Content** (`worldgen.rs`) | `(seed, CellId) → contents`, 0 bytes dormant | materialized entities in active cells |
| **Compute** (`galaxy.rs`) | one giant cell on one node | quadtree funnel across many nodes |
| **Orbits** *(new)* | closed-form Kepler at time `T` | numerically integrated near players |
| **Time** *(new)* | not ticked; sampled at `T` on demand | 20 Hz full step in the active bubble |
| **Life** *(new)* | deterministic baseline state | sparse persistent deltas + region macro-sim |

The three new axes (orbits, time, life) are what this document adds.

---

## 4. The coordinate foundation (non-negotiable, build first)

Flat `f32 x,y` absolute coordinates **cannot** span this. `f64` at galaxy radius (~1e21 m) resolves to
~1 km; `f32` is hopeless. Atmospheric flight needs sub-metre precision *simultaneously* with
galaxy-spanning range — ~21 orders of magnitude, far beyond any single float.

**Position becomes anchored + local:**

```
WorldPos = { anchor: FrameId, local: Vec2<f32> }
```

- `anchor` names a node in the **frame tree** (galaxy → star → planet → moon → station → ship).
- `local` is high-precision offset *within that frame* — metres near a planet, light-seconds in
  interstellar space.
- **Floating origin:** the active simulation always runs near `local ≈ 0` by continuously rebasing onto
  the nearest dominant body. Precision is spent where the player is, never wasted on absolute galactic
  offset.
- Frame transitions (entering a planet's sphere of influence, leaving a system) are seamless rebases,
  not loading screens — the patched-conics frame tree *is* the LOD frame tree.

This replaces flat `Vec2` absolute coords throughout `sim.rs`/`physics.rs`. It is the foundation; the
celestial hierarchy, warp, and seamless atmosphere↔galaxy all stand on it.

---

## 5. The Newtonian model (honoring "actually simulated newtonianly")

Two regimes that **agree where they meet**, so there is one consistent physics, not a fake one:

- **Dormant = analytic Kepler (patched conics).** Each body orbits one primary on a fixed ellipse;
  sphere-of-influence handoffs between primaries. This is not a cheat — it is the *exact closed-form
  solution* of the Newtonian two-body problem, which is why it can be sampled at any `T` for free and
  deterministically. This is the model KSP uses; it is "realistic orbital mechanics."
- **Active = full numerical integration.** Inside a player's bubble, ships under thrust, close
  encounters, and multi-body perturbations are integrated step-by-step (the existing `physics.rs` loop,
  extended with gravity from in-frame bodies). It reduces to the Kepler baseline when one mass
  dominates and nothing is thrusting — so a ship coasting through a dormant region and a ship the engine
  is actively integrating see the *same* trajectory.

**Warp drive = a physical FTL state, not a teleport.** The ship carries a velocity (or metric-bubble
term) with `v ≫ c` through *real* space; it still has position, heading, and a trajectory you plan.
"Advanced flight planning" = computing intercepts and transfer/warp corridors against the analytic
ephemeris (where will that planet *be* when I arrive). At warp you sweep thousands of dormant
systems per second — handled by **transit LOD**: only gravitationally significant bodies near your line
are resolved (analytically); full discrete sim re-engages as you decelerate toward a destination. This
is why floating origin + multi-rate time are prerequisites, not polish.

---

## 6. Time LOD and the living tiers

Each region of the galaxy sits in one of three tiers, and is promoted/demoted as players come and go:

1. **Dormant (analytic).** No tick. State at `T` = `f(seed, T) ⊕ sparse_deltas`. Cost: zero until queried.
2. **Macro (living, low-rate).** Inhabited-but-unobserved regions evolve at *aggregate* fidelity —
   populations, prices, faction control, fleet counts — stepped once per chain epoch (~10 s) or slower,
   per *region* (a coarse quadtree cell), **not per entity**. Bounded by region count with stakes, which
   is bounded by player/faction footprint, not by star count.
3. **Active (discrete, 20 Hz).** A player is present: the region's macro-state is *expanded* into
   concrete entities (NPC ships, market orders, station inventories) seeded from `(seed, macro-state)`,
   and the full real-time sim runs. On departure, discrete state is *folded back* into macro-state +
   sparse deltas.

Promotion (macro→discrete) and demotion (discrete→macro) are the critical seams: they must be
information-preserving and deterministic so two nodes expanding the same region agree, and so a region
that empties and refills doesn't lose its history.

---

## 7. Living state at scale — how it's stored and why it fits

State = **deterministic baseline ⊕ sparse event-sourced deltas**.

- **Baseline:** `worldgen.rs` extended to the full celestial hierarchy + ephemeris. Free, everywhere,
  reproducible, tamper-evident (seed = chain genesis hash). No storage.
- **Deltas:** every durable change (a star mined out, a station built, territory claimed, a wreck left,
  a price moved by trade) is an event keyed by `(CellId / BodyId, epoch)`, content-addressed, gossiped,
  and chain-anchored — exactly like `frontier.rs` discoveries today. A system's *live* state is its
  baseline replayed under its deltas. Untouched systems carry **zero** deltas and cost nothing.
- **Macro-sim:** the per-region aggregate evolution writes deltas at coarse granularity; it is the
  engine that makes the galaxy "live" between visits.

### Budget math (why this is affordable)

- Stars in a 1:1 Milky Way: ~1–4 × 10^11. **Dormant cost: 0** (analytic, on-demand).
- Concurrent players even at huge success: ~10^6. **Active discrete sim: O(players)** — the existing
  quadtree already places and scales this across donor + burst nodes.
- Systems ever carrying a delta: ~10^6–10^8 (history is sparse). At ~100 B–1 KB each →
  **~10^8–10^11 bytes total**, sharded across the whole mesh and content-addressed. Per node: trivial.
- Macro-sim regions with stakes: bounded by faction/player footprint (~10^4–10^6), stepped at ~0.1 Hz.
  **O(inhabited regions), not O(stars).**

Every cost line is bounded by **players, history, or inhabited regions** — never by the 10^11 stars.
That is why a living 1:1 galaxy fits.

---

## 8. What changes in the existing code

> **Status (2026-06-28): the coordinate foundation is built and proven.** `src/coords.rs` lands the
> anchored, galaxy-scale (`i64` anchor + `f32` local) `GalaxyPos`/`Anchor` types with an origin-invariant
> canonical key (`fixed8`). It is wired into `Sim::state_hash` (positions now fold the *global* anchored
> key, so re-anchoring is no longer a false divergence) plus `Sim::galaxy_frame`/`galaxy_pos`/
> `ship_galaxy_pos`. Proven by `tests/floating_origin.rs` — five end-to-end *disagreeing-server* tests:
> different origins agree on a ship, transit is globally continuous, a botched re-anchor is caught by the
> hash, the whole-world hash is origin-invariant, and precision survives at galactic distance. Full suite
> green (294 tests). Still open below: the per-body frame tree / `physics.rs` integration, and re-keying
> deterministic *content* (hazards/asteroids) onto global coordinates so full-sim origin-invariance holds.

- **New foundation (done):** anchored/floating-origin coordinates (`coords::GalaxyPos`/`Anchor`). Next:
  thread a per-body frame tree through `physics.rs` so the active integrator runs near `local ≈ 0`.
- **`worldgen.rs` → celestial hierarchy:** from per-cell biome/asteroid content to a deterministic
  star-catalog → system → planet/moon/belt/station tree with orbital elements (the analytic ephemeris).
- **New `orbit` layer:** Kepler propagation (closed-form), sphere-of-influence handoff, the dormant↔active
  bridge. Today a "planet" is only a static gravity `Well` in `hazard.rs` — that becomes a body on a real orbit.
- **New `warp`/flight-planning layer:** physical FTL state + trajectory/intercept planning against the
  ephemeris; transit LOD.
- **Time LOD in the tick loop (`lib.rs`):** dormant (no tick) / macro (epoch-rate) / active (20 Hz) tiers
  with deterministic promotion/demotion.
- **Living state:** generalize `frontier.rs`'s event-sourced, chain-anchored deltas into a full
  per-body/per-region delta log; add the region macro-sim (extending `faction.rs`/`treasury.rs`/`ai.rs`
  from local-only to aggregate galaxy-wide).
- **Keep as-is:** the load quadtree (`galaxy.rs`), procedural determinism, replication/failover,
  interest-scoped snapshots, the donor+burst fleet. These remain correct; the new layers sit on top.

The key architectural move: **decouple the *content/celestial* tree from the *compute/load* quadtree.**
Today they are the same tree. At galaxy scale they must be two: the quadtree distributes CPU; a
separate sparse celestial hierarchy + delta log holds the cosmos and its history.

---

## 8b. Directive — map visualizer, benchmarks, and the two decouplings (verbatim, 2026-06-28)

> Create a map visualizer on the backend side to connect all users together and show a map of the whole
> galaxy - for debugging, visualization from ship scale to galaxy scale, show aabb nodes... i want to have
> a slider / number you can change which dictates how much out the solver should look - we cant fetch the
> whole galaxy at once so this is how much of the view we fetch to build the map - since its fully
> distributed. We need benchmarks for everything. record verbatim. - Per-body frame tree in physics.rs so
> the active integrator runs near local ≈ 0 (full floating-origin rebasing during flight, not just at
> sector seams).
> - Re-key deterministic content (hazards/asteroids in hazard.rs) onto global coordinates. They're still
> sector-keyed, which is why test #4 isolates the coordinate system rather than ticking a full sim — true
> full-sim origin-invariance needs content anchored globally too. That's the content/compute-decoupling
> from the design doc. - why didnt the tests catch these also? test driven development is key. tests SHOULD
> fail before the implementations are fully done. write more tests and implement everything i said

**On "why didn't the tests catch these":** they didn't because the prior pass *scoped them out* and wrote
test #4 to **isolate** the coordinate system (asserting the canonical hash without ticking), instead of
writing a failing **full-sim** invariance test first. Green hid the gap. The fix is TDD discipline: each
item below ships a test written to **fail first** (the property is unrepresentable or wrong on the old
code), watched go red, then made green. Also surfaced en route: a real bug — `rock_at` keys the asteroid
field on **sector-local** cells `0..30`, so *every sector has an identical asteroid field*. Global-cell
keying fixes that and is the content decoupling.

## 8c. Galaxy map (distributed, multi-server) — status 2026-06-28

A live galaxy map for "play with friends, each on their own server": `src/mapview.rs` aggregates a
`CellReport` from every host into a bounded, camera-relative `GalaxyView` (the **fetch-reach slider** is
`ViewQuery::reach` — you can't fetch the whole galaxy, so it bounds the pull; AABB nodes + entity dots at
ship scale, cell summaries at galaxy scale). All view/report types are serde-serializable (JSON to the
web, and over the mesh). Each server publishes its report on the galaxy-wide `mapview::MAP_TOPIC`
(`"spacegame/map"`) via `director::publish_map_report`; there is **no central aggregator** — the browser
folds every server's reports together, like the rest of the mesh. The web frontend is
`spacegame-wasm/map.html` (intended URL `spa.ce-net.com/map`): it subscribes through the same-origin mesh
bridge, assembles the galaxy, and renders it zoomable from ship to galaxy scale with the reach slider, a
per-server legend, player dots, and the broad-phase AABB boxes — with a synthesised demo galaxy when no
server is reporting yet. Tests: `mapview` (reach/budget/LOD/precision/**multi-server merge + JSON
round-trip**). **Remaining to go fully live:** call `publish_map_report` on each host's broadcast cadence
in the node loop, and serve `map.html` at the public URL alongside the wasm bundle.

## 9. Honest open problems (called out, not hidden)

- **Promotion/demotion determinism.** Folding discrete state back to macro and re-expanding it without
  drift or lost history is the hardest correctness problem here. Needs a precise, tested contract.
- **Macro-sim is new game-design surface,** not just engineering: the rules by which unobserved regions
  evolve (economy, war, migration) must be authored and balanced, and they must be deterministic enough
  to anchor to the chain.
- **Warp + floating origin under sustained FTL** stresses the rebasing/transit-LOD path; precision and
  hand-off jitter need real measurement.
- **Delta log growth over the galaxy's lifetime** needs compaction (fold old deltas into checkpoint
  baselines) so history stays bounded.
- **"Living everywhere at full fidelity simultaneously" remains impossible** — and we should never claim
  it. We claim: deterministic everywhere, persistent where touched, macro-evolving where inhabited,
  full-fidelity where watched.
