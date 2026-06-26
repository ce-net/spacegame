# Scaling CE Spacegame to 1,000,000+ concurrent players

This is the deployment and capacity story for spacegame: how one seamless, infinite 2D galaxy holds a
million-plus simultaneous pilots on the CE mesh, and why latency stays flat as it fills. It is the
counterpart to the architecture in [`README.md`](README.md) — that explains *what* each module does;
this explains *how the numbers work out at scale*.

The short version: **the world is sharded into independent sector cells, each cell's tick cost is kept
near-linear by a recursive AABB broad-phase, each client's bandwidth is bounded by interest
management, and load spreads itself across the mesh with no central coordinator.** No single machine
ever sees a million players; no single client ever receives a million entities.

---

## 1. The four levers

| Lever | What it bounds | Where it lives |
|---|---|---|
| **Sector sharding** | players *per host* | `shard.rs` (rendezvous hash), `director::prewarm_neighbors` |
| **Recursive AABB broad-phase** | CPU *per tick* | `aabb.rs`, used throughout `sim.rs` |
| **Interest management** | bandwidth *per client* | `room::build_snapshot_view` |
| **Snapshot replication** | blast radius of a *host failure* | `snapshot.rs`, `director::replicate_snapshot` |

Each lever is independent. Together they make the cost of the game **per-region and per-viewer**, never
global.

---

## 2. Sharding: players per host

The galaxy is a grid of `3000 x 3000`-unit **sectors**. Each sector is an independent authoritative
`Sim` running on one mesh node, assigned by a coordinator-free **rendezvous hash** (`shard::shard_for`):
every node computes the same owner for a sector, and only `~1/N` of sectors move when a node joins or
leaves.

Budget one sector at a comfortable **150–400 concurrent ships** (a busy arena, well within a single
core's tick budget — see §3). Then:

```
1,000,000 players / 250 players-per-sector  ≈  4,000 active sectors
4,000 sectors / 8 sectors-per-host          ≈  500 hosts
```

500 commodity nodes (or browser/worker nodes donating spare cores — this is CE) host the whole game.
That is not a fleet you provision; it is the mesh you already have. As a region fills, the
**autoscaler** (`director::prewarm_neighbors`, enabled with `spacegame host --autoscale`) places the
busy sector's still-unhosted neighbours on the lowest-latency capable nodes *before* players spill into
them, so the seamless map keeps spreading load instead of crowding one cell. Empty regions cost
nothing — a sector with no ships and no neighbours under pressure is never even hosted.

**Why it stays even:** players cluster (fights, hubs), but the rendezvous hash scatters *adjacent*
sectors onto *different* hosts, so a dense brawl spanning a few sectors is still served by several
machines, not one. Hot spots shard; cold space is free.

---

## 3. Recursive AABB broad-phase: CPU per tick

The thing that actually kills a real-time server as a cell fills is **pairwise work**: bullet→ship
collision, weapon hitscan, homing target-acquisition and ship↔ship collision are all "for each X, which
Y is near it" questions. Done naively that is `O(n·m)` per tick, and the moment a tick overruns its
`1000/hz` ms slot **every player in that sector feels lag**.

`aabb.rs` is a **recursively subdivided axis-aligned bounding-box tree** (a loose quadtree whose every
node is an AABB). It is rebuilt once per tick from final ship positions, then:

- **bullet → ship**: each bullet queries the tree around its position instead of scanning all ships;
- **railgun / laser hitscan**: the ray's bounding box is the query, so only ships near the beam are tested;
- **homing missiles**: each missile queries an acquire radius for the nearest enemy;
- **ship ↔ ship collision**: each ship queries its immediate neighbourhood.

This turns the per-tick cost from `O(n·m)` into roughly `O((n + bullets) · log n + hits)`. The query is
**provably complete** (it never misses a true overlap — see the brute-force cross-check test in
`aabb.rs`) and returns candidates in deterministic order, so collision resolution stays deterministic
and snapshot failover remains reproducible.

Concretely: a 400-ship sector firing thousands of rounds is a few hundred-thousand cheap tree
descents per tick, not 400×thousands of distance checks — comfortably inside a 20 Hz (50 ms) budget on
one core, with headroom to raise the per-sector cap.

---

## 4. Interest management: bandwidth per client

A client must never receive the whole galaxy, or even a whole crowded sector. Two bounds apply:

1. **Sector interest set** (`shard::SectorId::neighbors`): a client subscribes only to its sector and
   its 8 neighbours. The far side of the galaxy never reaches it, at any world size.
2. **Viewport scoping** (`room::build_snapshot_view`): within a sector, the snapshot is scoped to the
   entities inside the client's viewport, found with an AABB query. A pilot in a 5,000-ship sector
   still receives only the ~tens of ships on screen.

So per-client downstream is `O(visible)`, independent of total population. At 20 Hz with a few dozen
visible entities, that is a few KB/s per client — the same whether the galaxy holds 1,000 or
1,000,000 players. Realtime is the node's **SSE push** stream, never polling.

Cross-sector edges are seamless: a ship that flies off an edge is **handed to the neighbouring sector**
(`Sim::take_transits` / `accept_transit`, delivered over the mesh by `director::publish_transit`),
carrying its full state (loadout, tech, minerals, kills). The map is one continuous infinite world, not
a grid of walled arenas.

---

## 5. Replication: surviving host failure

Each sector host snapshots its authoritative `Sim` to a **content-addressed blob** every few seconds
(`director::replicate_snapshot`) and announces the CID. If a host dies, a standby adopts the latest CID
and resumes with at most one snapshot interval (~5 s) of loss instead of an empty sector. Because blobs
are content-addressed and pinned to many nodes, **every node that fetched a snapshot is a CDN edge for
it** — recovery does not stampede one origin. The snapshot records the sector coordinate and the
ruleset version, so a recovering host restores into the right region under the right rules.

---

## 6. Hot reload at scale (zero-downtime content updates)

The entire game definition — weapons, the tech tree, physics tunables, and the frontend shader/asset
blob — is **data** in a versioned `Ruleset` (`ruleset.rs`). Pushing an edit
(`spacegame ruleset push live.json`, or just saving a watched file under `spacegame host --ruleset
live.json`) stores it as a content-addressed object and announces `{cid, version}` on the galaxy config
topic. Every host and every client subscribes; **a higher version wins**, so the update is idempotent
and order-independent across the mesh. Hosts call `Sim::apply_ruleset` between ticks (ships in flight
keep their state); clients re-fetch and recompile shaders. A million clients pulling a new shader pull
it from the nearest CDN-edge holder, not from one server.

The net effect: balance changes, new items, new weapons, an expanded tech tree and shader tweaks all go
live **while people are playing**, with no restart and no dropped session.

---

## 7. Operating it

```bash
# A capable node hosts a cluster of sectors and autoscale-spreads as they fill.
spacegame host --sector 0_0 --sector 1_0 --sector 0_1 --autoscale

# A designer iterates on balance/shaders live; every save reaches the whole mesh.
spacegame ruleset init live.json     # editable template
spacegame host --sector 0_0 --ruleset live.json
#   ...edit live.json, save -> hot reload galaxy-wide in <1s...

# Or push a vetted ruleset once, from anywhere on the mesh.
spacegame ruleset push live.json

# Place a sector's cell on the latency-optimal node instead of hosting locally.
spacegame place --sector 5_3 --image ce-net/spacegame:latest
```

**Capacity rule of thumb:** scale by *active regions*, not by players. Provision (or let the mesh
donate) ~1 host per ~2,000 concurrent players, keep the per-sector cap where the 20 Hz tick has
headroom (a few hundred ships), and let sharding + autoscale + interest management do the rest. The
ceiling is the size of the mesh, not the size of any one machine.
