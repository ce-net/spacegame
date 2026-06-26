# Frontend integration — distributed, fault-tolerant, insanely scalable

How the three frontends — a **native app**, a **desktop browser**, and a **mobile browser** — talk to
spacegame efficiently when the world is sharded across the mesh, hosts can vanish at any instant, and
there may be a million players online. The backend (`src/`) already provides every primitive this needs;
this is the contract the client builds on. The small shared tuning lives in
[`src/client.rs`](src/client.rs); the connection/scale story is here.

The thesis in one line: **the client talks only to a CE node over the mesh, predicts locally for zero
delay, subscribes to a tiny slice of the world, and never needs to know which machine is hosting it.**

---

## 1. One transport, three clients

Every client speaks the *same wire* ([`wire.rs`](src/wire.rs)) to a CE node — never to a central server:

- subscribe/publish on **sector-keyed pubsub topics** (`ce-game/spacegame/<sector>/in|state`),
- receive authoritative `Snapshot`s over the node's **SSE push** stream (`/mesh/messages/stream`),
- send `ClientMsg` inputs (move/fire/build/weapon/command) with payloads as `payload_hex`.

The player id is the node's authenticated **NodeId** — free, unspoofable identity, no auth server.

What differs per platform is only *where the node is*:

| Client | The node it uses | Notes |
|---|---|---|
| **Native app** | a full CE node in-process | most capable; mines/hosts/donates compute too |
| **Desktop browser** | an in-page **WASM node** (`window.__ceNode`), else the same-origin `/ce` proxy | full peer in the tab |
| **Mobile browser** | a **WASM-only** node over `wss` to the relay | a real mesh peer on a tight budget |

Because the transport is identical, one client codebase serves all three; only the **budget** changes.

---

## 2. Budgets per platform (`client.rs`)

`Platform::{Native, DesktopBrowser, MobileBrowser}::profile()` is the shared source of truth both the
client and the host honour:

| | view radius | max entities | snapshot rate | interest sectors | prefetch assets |
|---|---|---|---|---|---|
| Native | 2200 | 4000 | every tick | 9 (full ring) | yes |
| Desktop browser | 1600 | 1500 | every tick | 9 | yes |
| Mobile browser | 1000 | 400 | every **2nd** tick | 5 (plus-shape) | deferred |

All three **predict locally** (zero delay). Mobile subsamples snapshots, renders a smaller viewport,
caps entities, drops to a 5-sector interest set, and defers shader/asset prefetch to protect first-load
and battery — without changing the protocol.

---

## 3. Interest management — bounded per-client cost (the scale lever)

A client never receives the galaxy, or even a full crowded sector. Two cuts, both already in the backend:

1. **Sector interest set** — `ClientProfile::interest_set(x, y)` subscribes only to the player's sector
   and its neighbours (`SectorId::neighbors`). The far side of the galaxy never reaches the client, at
   any world size.
2. **Viewport scoping** — the host answers each client with `room::build_snapshot_view(sim, …,
   ClientProfile::viewport(x, y))`, an **AABB query** that returns only the entities on screen. A pilot
   in a 5000-ship sector still receives ~tens of entities.

So **per-client downstream is `O(visible)`, independent of total population** — a few KB/s whether the
galaxy holds 1,000 or 1,000,000 players. This, plus sector sharding across the mesh (`SCALING.md`), is
why a million concurrent clients is affordable: no machine sees them all, no client sees them all.

---

## 4. Local-first prediction — zero delay

The simulation ([`sim.rs`](src/sim.rs)) is a **pure, deterministic** function of its inputs, so the
*same code* runs on the client's own node for the patch of space around the player:

1. apply the local player's input to a local `Sim` **immediately** (the ship turns with no round-trip);
2. the host's authoritative `Snapshot`s stream in;
3. the client **reconciles** — snaps confirmed entities to the authoritative state and re-applies any
   unacknowledged local inputs.

Other players/NPCs are interpolated between snapshots. Because the core is deterministic and ships as
WASM, the in-browser node runs the identical logic — the client *predicts*, the mesh *confirms*.

---

## 5. Distribution — the client never picks a server

- **Nearest host:** to render a sector the client resolves its host with `director::nearest_sector_host`
  (the lowest-RTT node currently advertising that sector) and reads its `/state`. Different sectors are
  hosted on different nodes; the client just subscribes to topics — the mesh routes.
- **Following a player across sectors** is transparent: when the local player crosses a sector edge the
  ship **transits** to the neighbour's host (`Sim::take_transits`/`accept_transit`); the client simply
  shifts its interest set by one sector and keeps reading topics. No reconnect, one continuous galaxy.

---

## 6. Fault tolerance — a host can vanish mid-fight

The client is built to **not care which machine is authoritative**:

- A region is simulated by **K high-precision replicas** on nearby players' nodes
  (`replication.rs`). If the host drops, the best healthy replica is **promoted deterministically**
  (every node computes the same winner — no split brain) and the map is **re-replicated** to the next
  best node to restore K.
- The client keeps subscribing to the sector's `/state` topic; whoever is authoritative publishes it, so
  the takeover is **invisible** beyond at most one snapshot interval. A recovering host **adopts the
  latest content-addressed snapshot** (`snapshot.rs`), so play resumes mid-state, not empty.
- If the client's own stream drops, it reconnects with `client::reconnect_backoff_ms` (capped
  exponential) and re-subscribes — its local prediction covers the gap.

The client needs zero special handling: read the topic, render the snapshots, predict locally.

---

## 7. Anti-cheat — agreement by replication

The client's local sim is **advisory** — pretty, instant, but never authoritative. Truth is what the
replicas agree on: each publishes a deterministic `Sim::state_hash` and `replication::agree` takes the
**majority** hash as truth, flagging any divergent (cheating/faulty) host. A client can verify the
sector's published commitment/hash itself, so a tampered host is caught by the mesh, not trusted by the
client. Determinism is also what makes the client's prediction line up with the authoritative result.

---

## 8. Hot reload — content changes mid-session

The whole game definition (weapons, tech, tunables, the buildable catalogue, **shaders/assets**) is a
versioned `Ruleset`. When the client sees the `ruleset` version rise in a `Snapshot` (or an announce on
the config topic), it **re-fetches the content-addressed ruleset from the nearest CDN-edge holder** and
hot-applies it — recompiling shaders, re-reading weapon/shape data — **without a page reload**. A million
clients pulling a new shader pull it from the nearest node that already has it, not from one origin.

---

## 9. Rendering — shapes to the GPU, cheaply

- A whole ship is collapsed to **one** shape blueprint and flattened to **one** GPU mesh + root AABB
  (`Ruleset::ship_mesh` / `generated_ship_mesh`), **cached** with `shapedef::MeshCache` and invalidated
  on hot reload — so a ship is built once, drawn in one pass, and culled by its cached AABB.
- Shapes flatten to packed `#[repr(C)]` vertex/index/material buffers with `to_raw()` for a pointer-free
  upload (`shapedef.rs`).
- Far entities/regions render at lower physics LOD (`physics::Lod`), matching the host's compute LOD.

---

## 10. Connection lifecycle (pseudocode)

```text
node     = window.__ceNode ?? sameOriginProxy("/ce")     // native: in-process node
me       = node.GET("/status").node_id                   // my unspoofable player id
profile  = Platform.detect().profile()                    // budgets for this device
ruleset  = fetchRulesetFromMesh()                          // content-addressed; shaders/assets
localSim = Sim(ruleset)                                    // for prediction

loop forever:
  sectors = profile.interest_set(player.x, player.y)
  for s in sectors: node.subscribe("ce-game/spacegame/"+s+"/state")
  host    = director.nearest_sector_host(currentSector)    // lowest-RTT advertiser (may change on failover)

  on input:                       publish ClientMsg.Input to "<currentSector>/in"; apply to localSim now
  on snapshot(s) (SSE push):      if profile.should_process(s.tick): reconcile(localSim, s); render(view)
  on s.ruleset > ruleset.version: ruleset = refetch(); hotApply(ruleset)     // shaders, stats, shapes
  on stream drop:                 sleep(reconnect_backoff_ms(n++)); resubscribe()   // prediction covers gap
  on cross-sector:                shift interest set; keep reading topics            // transit is transparent
```

No step references a specific server. The client subscribes to *places*, predicts locally, and renders
whatever the mesh delivers.

---

## 11. Per-platform summary

| Concern | Native app | Desktop browser | Mobile browser |
|---|---|---|---|
| Node | full in-process node | WASM node / `/ce` proxy | WASM-only over `wss` |
| Prediction | yes | yes | yes |
| Snapshot rate | every tick | every tick | every 2nd tick |
| Viewport / entities | large / 4000 | medium / 1500 | small / 400 |
| Interest sectors | 9 | 9 | 5 |
| Assets | prefetch | prefetch | deferred |
| Also hosts/donates | yes | optional | no (consumer peer) |

## 12. Native mobile (next)

The mobile **browser** path works today. **Native mobile** (a packaged iOS/Android app shipping the CE
node — WASM in a webview or a native node build — plus the renderer) reuses everything above unchanged:
same wire, same `Platform::MobileBrowser`-class budgets (or a `Native` profile if the device is capable),
same prediction and failover. Only the app shell and the GPU backend are new; the protocol is identical,
so it interops with the browser and native-desktop players in the same sectors from day one.
