# spacegame

Authoritative, **sector-sharded** mesh backend for **CE Spacegame** — a real-time multiplayer space
arena, built as a flagship demonstration that **CE is a global supercomputer**, not a single shallow
game server.

The galaxy is partitioned into a grid of fixed-size **sectors**. Each sector is an *independent
authoritative simulation cell* that runs on a mesh node chosen for low latency, rendezvous-hashed so
the galaxy spreads across the mesh with no coordinator, snapshot-replicated to content-addressed blobs
for failover, with the galaxy leaderboard sealed against the PoW chain. Distinct *regions of space*
are simulated by *distinct nodes*, so the world scales horizontally: more players in more places means
more sectors, and load spreads across the mesh.

Players are CE **NodeIds** — free, unforgeable auth. The wire payload never carries a player id; the
node delivers the authenticated sender NodeId with every message, so a client cannot impersonate
another, and even the ship's color (derived from the NodeId) is unspoofable.

```
ce start                                       # the local node must be running (this is the mesh)
spacegame host --sector 0_0                    # host the origin sector here
spacegame host --sector 0_0 --sector 1_0       # host several sectors (independent cells) at once
spacegame host --sector 0_0 --autoscale        # pre-warm neighbours as the sector fills up
spacegame host --sector 0_0 --ruleset live.json# host AND hot-reload: every save of live.json re-tunes
                                               # the live game for every host + client, no restart
spacegame place --sector 1_0 --image ce-net/spacegame:latest
                                               # atlas-guided: pick the best host, deploy the cell there
spacegame ruleset init live.json               # write the built-in ruleset as an editable template
spacegame ruleset push live.json               # push an edited ruleset live to the whole mesh, now
spacegame shard   --sector 1_0                 # which node rendezvous-hash assigns this sector to
spacegame nearest --sector 1_0                 # nearest live host of this sector (client view)
```

See [`SCALING.md`](SCALING.md) for how this design holds **1,000,000+ concurrent players**, and
[`SYSTEMS.md`](SYSTEMS.md) for the LOD rigid-body physics, nested/dynamic AABBs, the always-alive
faction metagame, and the proximity-replica fault-tolerance system.

### Combat, content and the infinite map

- **Weapons are data** in a hot-reloadable [ruleset](src/ruleset.rs): the **blaster** (ballistic),
  **homing missiles** that fly and **explode** with area damage (single seeker, multi-tube **missile
  pod**, **heavy seeker**), **railguns** (instant hitscan ray) and several **laser types** — continuous
  **beam**, gated **pulse**, fan-firing **scatter**, and long-range **beam lance**. Hitscan weapons
  honour count/spread, so new fan/multi-shot variants are pure ruleset data. Add or re-balance weapons
  by editing the ruleset — no redeploy.
- **Tech tree** unlocks weapons and upgrades (hull / thrusters / guns), server-priced and gated.
- **Infinite map:** a ship that crosses a sector edge is *handed off* to the neighbouring sector
  (cross-sector transit), carrying its full loadout — one continuous galaxy, not walled arenas.
- **Ship↔ship collision physics** keep ships from stacking.
- **Hot reload:** weapons, items, tech tree, tunables and even **frontend shaders** can be changed
  *while people are playing* and the change reaches every host and client across the mesh instantly.

The frontend (`web/demos/spacegame/`) talks to these backends **only over the CE mesh**, through the
same-origin node bridge (`window.__ceNode` if an in-browser WASM node is present, else the same-origin
`/ce` proxy). It never contacts `ce-net.com`, `/db`, `/rt`, or any remote origin.

---

## Architecture

```
src/
  sim.rs          pure authoritative simulation of ONE sector: ships, physics, weapons (blaster /
                  homing missile / railgun / laser), mining, tech tree, kills, respawns, ship<->ship
                  collision, and cross-sector transit. Deterministic: same inputs -> same state.
  aabb.rs         recursive AABB trees: a per-tick loose quadtree (broad-phase for collision, hitscan,
                  homing, viewport interest) PLUS a dynamic fat-AABB BVH that follows moving objects and
                  nests (Compound: recursive AABBs holding recursive AABBs) for compound bodies.
  physics.rs      advanced 2D rigid-body engine (mass, inertia, restitution, friction, angular impulse)
                  with level-of-detail precision: high framerate near players, coarse far away. GPU/SoA-
                  friendly so replicas stay bit-identical across CPU and GPU.
  faction.rs      your always-alive faction: factories, resources, tech, an autonomous build policy that
                  spends your resources for you, and a roster that becomes a real NPC fleet (drones/
                  fighters/haulers) you command (sim drives their AI; FactionView tracks them).
  replication.rs  proximity-replica fault tolerance: K high-precision replicas on nearby players' nodes,
                  heartbeat failure detection, deterministic takeover, re-replicate to the next best.
  ruleset.rs      the hot-reloadable game definition as DATA: weapon catalogue, tech tree, physics
                  tunables, and an opaque shader/asset blob. Versioned; swapped live with no restart.
  shard.rs        SectorId + rendezvous-hash sharding + latency-first host scoring + interest set.
  wire.rs         sector-keyed pubsub topics + ClientMsg / Snapshot JSON wire types (weapons, beams).
  room.rs         glue: authenticated mesh msg -> sim intent; sim -> wire Snapshot (full + viewport-
                  scoped for bounded per-client bandwidth). Pure.
  snapshot.rs     SectorSnapshot: faithful capture/restore of a sector (incl. loadout) for failover.
  leaderboard.rs  canonical galaxy leaderboard + cross-sector merge + PoW-anchored Commitment.
  director.rs     the ONLY mesh-I/O layer: maps the pure modules onto real ce-rs SDK calls —
                  placement, replication, consensus, transit delivery, ruleset hot-reload, autoscale.
  lib.rs          run_sector(): the authoritative tick/publish/replicate/seal/transit/hot-reload loop.
  main.rs         CLI: host / place / ruleset (init|push) / shard / nearest.
```

The `sim`, `aabb`, `ruleset`, `shard`, `wire`, `room`, `snapshot`, and `leaderboard` modules are
**pure and fully unit-tested** — no mesh, no network, no clock. `director` and `lib` hold the thin
async mesh I/O.

---

## What each showcased CE capability maps to

| Capability | Where it is wired (real `ce-rs` SDK calls) | What a live multi-node mesh shows |
|---|---|---|
| **Distribution** — run logic on the best *other* node | `director::choose_host` reads `ce.atlas()`; `director::deploy_sector_cell` calls `ce.mesh_deploy(node_id, spec, grant)` to place a sector cell on a chosen node; the cell runs this very binary (`spacegame host --sector …`). | Run `spacegame place --sector 1_0` on node A; the authoritative sim for that region appears on node B (the latency-optimal capable host), not on A. |
| **Concurrency** — many independent cells at once | The galaxy is a grid of sectors; `shard::shard_for` rendezvous-hashes each `SectorId` to a host with no coordinator; `host_sectors` runs each sector in its own `tokio` task. | `spacegame shard --sector …` over many sectors shows them spread evenly across nodes; each sector ticks independently and concurrently. |
| **Latency** — pick the lowest-latency host; bound bandwidth | `director::gather_candidates` joins `ce.atlas()` with `ce.netgraph()` (measured RTT); `shard::best_host` scores latency-first; the client uses `director::nearest_sector_host` + `ce.find_service`; **interest management** via `SectorId::neighbors` limits a client to its 9-sector neighbourhood; realtime is the `/mesh/messages/stream` **SSE** push, never polling. | A client near node B is handed B as its sector host; a far-away galaxy region's state never reaches it (bounded per-client bandwidth at any world size). |
| **Replication** — snapshot + failover | `director::replicate_snapshot` serializes a `SectorSnapshot` and `ce.put_object()`s it (content-addressed, chunked), `ce.advertise_service`s + `ce.publish`es the CID; `adopt_latest_snapshot` + `director::restore_snapshot` (`ce.get_object`) restore it on a new host. Every node that fetches the object caches it — every node is a CDN edge. | Kill a sector host; a standby host adopts the latest snapshot CID and resumes that region with at most one snapshot-interval (~5s) of loss, instead of an empty sector. |
| **Consensus** — tamper-proof leaderboard | `leaderboard` builds a canonical, order-independent galaxy board (cross-sector `merge`); `director::seal_leaderboard` `ce.put_object()`s the canonical bytes (the CID is a tamper-evident fingerprint), binds it to `ce.beacon()` (PoW tip height + hash) into a `Commitment`, and `ce.publish`es it on the seal topic. | Any stranger re-fetches the board by CID, recomputes the digest, and checks the height/hash against the chain — a final score is verifiable and a dishonest host that edits a sealed board produces a different CID, breaking its own commitment. (CRDTs provably cannot do this.) |
| **Economy** — pay the host per session | `director::open_host_channel` calls `ce.channel_open(host, capacity, …)` then `ce.sign_receipt(...)`; `pay_host_tick` signs rising receipts for ongoing hosting — the marketplace angle: a player funds the node simulating their region of space. | A player opens a payment channel to its sector host and signs receipts as the session runs; the host redeems the highest to settle. |
| **Auth** — identity is the player | The player id is the CE NodeId the node authenticates on every pubsub delivery (`AppMessage.from`); `room::hue_for` derives the unspoofable color from it. No central auth server. | Two browsers on two nodes each appear as their own NodeId-derived ship; neither can pick another's id or color. |
| **Realtime** — SSE, gossipsub | `ce.subscribe` / `ce.publish` on the sector topics; `ce.messages_stream()` (the `/mesh/messages/stream` SSE) drives the authoritative loop and the client render. | Inputs and snapshots flow over libp2p gossipsub with push (SSE) latency, no polling. |
| **Hot reload** — change the game while it runs | `ruleset.rs` holds weapons/tech/tunables/shaders as versioned data; `director::publish_ruleset` `ce.put_object()`s it and `ce.publish`es `{cid, version}`; hosts `Sim::apply_ruleset` between ticks and clients re-fetch. `spacegame ruleset push` / `host --ruleset live.json` (file-watch). | Edit a weapon stat or a shader and save; within ~1s every running host re-tunes and every client hot-applies it — no restart, no dropped session. |
| **Scale (latency under load)** — recursive AABB | `aabb.rs` is a recursively subdivided AABB tree; `sim` queries it for bullet/hitscan/homing/ship collision and `room::build_snapshot_view` queries it for per-client viewport scoping. | A sector with thousands of entities still ticks inside its budget, and a client receives only what is on screen — bandwidth and CPU stay bounded as the galaxy fills. See `SCALING.md`. |
| **Infinite map** — seamless sectors | `Sim::take_transits` emits a `Transit` when a ship crosses a sector edge; `director::publish_transit` delivers it to the neighbour's host, which `accept_transit`s it; `director::prewarm_neighbors` autoscales hosts ahead of demand. | Fly across a sector boundary and keep going — your ship, loadout and minerals appear in the neighbouring region (often on a different node), one continuous galaxy. |
| **Local-first + anti-cheat** — agreement by replication | The deterministic `Sim` runs locally for zero-delay feel; several replicas simulate the same sector and publish `Sim::state_hash` proofs; `replication::agree` takes the majority hash as truth and flags any divergent (cheating/faulty) replica. | Your ship reacts with no server round-trip; a host that fudges the rules produces a different state hash and is outvoted by the honest replicas — and the world survives any of them dropping. |

---

## What needs a live multi-node mesh to see end-to-end

The **deterministic cores are real and unit-tested here** (the simulation tick, rendezvous-hash
sharding, interest set, latency host scoring, snapshot capture/restore + deterministic continuation,
the canonical leaderboard, cross-sector merge, and the PoW-anchored commitment math). The mesh I/O in
`director`/`lib` issues real `ce-rs` SDK calls against the **local** node.

To observe the full distributed behaviour you need several `ce` nodes running and connected (e.g. the
laptop + desktop + relay, or several `ce start` nodes on a LAN):

- **Distributed placement / failover migration** needs ≥2 capable nodes so `mesh_deploy` lands a cell
  on a *different* host and a *standby* can take over a sector after the host is killed.
- **Sector spread across the mesh** is visible once ≥2 nodes advertise capacity in the atlas.
- **The on-chain leaderboard seal** needs a node mining/synced to the PoW chain so `beacon` returns a
  live tip; the commitment is published over real gossipsub.
- **Payment channels** need an open channel between payer and host nodes.

Single-node, everything still runs: one node hosts every sector locally, snapshots to its own blob
store, and seals against its own chain tip — the same code paths, just not yet *spread*.

---

## Testing

Build and test on Hetzner (never locally — the dev laptop disk is tiny):

```
cd ~/ce-net && tools/remote-test.sh spacegame --clippy
```

All deterministic logic is covered by `#[cfg(test)]` unit tests in each pure module.
