# The Adaptive Galaxy — planet-scale spacegame on the open mesh

A design and API for one continuous galaxy that holds **millions of concurrent players**, runs on
**whatever computers the world donates** (plus cloud burst when a battle erupts), and has **no central
server, no shard list, and no operator deciding where anything runs**. The galaxy grows compute where
the crowds are and gives it back when they leave.

This is not a roadmap of someday-features. It is the API, expressed as code, of the system we want.

## The one idea

**Space is an infinite quadtree, and resolution follows population.** Each leaf cell is one
authoritative simulation on one node, sized to one node's tick budget. A cell that gets hot **splits
into four** (load halves, children scatter onto other nodes); four cold siblings **merge** back into
one. Empty space is a single giant cell on a single box; a 10,000-ship siege is a deep funnel of cells
across dozens of boxes. The split/merge verdict is a **pure function of the cell's measured load + the
chain beacon epoch**, so every node computes the same galaxy shape with no coordinator — the same trick
that makes deterministic failover work, applied to the topology itself.

## The five pieces (all new, all in `src/`)

| Module | What it is | Heart of it |
|---|---|---|
| [`galaxy.rs`](src/galaxy.rs) | the adaptive quadtree: `CellId`, `Galaxy`, `CellLoad::verdict` | `Verdict::{Split,Merge,Hold}` is *pure* — everyone agrees on the shape |
| [`fleet.rs`](src/fleet.rs) | elastic capacity: donor nodes (free, auto-enrolled) + `CloudProvider` burst | `Fleet::best_host_for` (latency-first) and `Fleet::burst` (rent the planet) |
| [`autoscale.rs`](src/autoscale.rs) | the control policy: observe load → `ScaleAction` batch | hysteresis + dwell + per-region burst cooldown, so it never flaps or stampedes |
| [`gateway.rs`](src/gateway.rs) | how millions of browsers attach — each its **own** peer | `Attachment` (in-tab peer / signed subkey) — never one shared player |
| [`orchestrator.rs`](src/orchestrator.rs) | leaderless control loop, rendezvous-owned cells | `Orchestrator::owner` (HRW) — the control plane scales like the data plane |
| [`cosmos.rs`](src/cosmos.rs) | the one-call facade: `run_node` (server) + `Player` (client) | a million-player game server in one function |

Browser side: [`spacegame-wasm/galaxy-peer.js`](../spacegame-wasm/galaxy-peer.js) brings up a
real libp2p node **inside the tab** and installs the same `window.__ceNode` the WASM client already
speaks — so every browser is its own player, and the simulation and connection tiers scale apart.

## Why each scale axis holds

- **Players (millions).** Per-client cost is `O(visible)`, never `O(population)`: a player subscribes
  only to their interest cells (`Galaxy::interest`), and a host answers each client with a
  viewport-scoped snapshot. A pilot in a 5,000-ship cell still gets ~tens of entities. The galaxy can
  hold a thousand or a billion; no machine sees them all, no client sees them all.
- **Compute (the planet's spare cores).** Capacity is donated nodes that auto-enrol by advertising in
  the atlas — a phone, a gaming PC, a rack, all equal — plus cloud burst billed to game revenue when
  the donor pool saturates, drained and destroyed when the surge passes (`Fleet::burst` / `drain`).
- **Topology (no coordinator).** Cells split/merge on a deterministic verdict; controllers own a
  rendezvous slice of the galaxy and reach identical, idempotent decisions; the shape is gossiped and
  CID-stamped. Add nodes → finer ownership, faster reaction, more host slots. Nothing to elect, nothing
  to bottleneck.
- **Connections (millions of browsers).** A stateless, autoscaled gateway tier fans browser peers into
  the mesh; each tab dials its lowest-RTT gateway and re-homes seamlessly if one drains.
- **Failure.** A host vanishing is a non-event: a proximity replica is deterministically promoted, the
  cell re-replicates to restore K, and the client — still subscribed to the cell's `/state` topic —
  sees at most one snapshot of jitter.

## The whole server in one call

```rust
use spacegame::cosmos::{run_node, NodeConfig, Roles};

// Every participating machine runs this. It hosts whatever the galaxy assigns it, joins the leaderless
// control plane, and (if public) accepts browser sessions. A thousand of these self-organise into one
// galaxy with no central anything.
run_node(&ce, NodeConfig { roles: Roles::EVERYTHING, ..Default::default() }).await;
```

## The whole client in three lines

```rust
let mut me = Player::join(&node, my_id, view_radius).await; // own peer id = own ship
for cell in me.interest() { node.subscribe(&cell.token()).await; } // a bounded handful
// move, predict locally, render snapshots; me.moved_to(x,y) hands you across cell edges seamlessly.
```

## Wiring (when the crate is green)

These are self-contained, new files. Activate them by adding to `src/lib.rs`:

```rust
pub mod galaxy;
pub mod fleet;
pub mod autoscale;
pub mod gateway;
pub mod orchestrator;
pub mod cosmos;
```

and add the daemon/CLI surface to `main.rs`:

```text
spacegame node            # run the galaxy node daemon (host + controller [+ gateway])
spacegame node --gateway  # also accept browser sessions
spacegame galaxy          # print the live galaxy shape (leaf cells + their hosts) — the heatmap
```

## What it takes to go global (the honest ladder)

1. **One public sector → one public *galaxy*.** Host the genesis cell on the relay; serve the WASM
   bundle + `galaxy-peer.js` at `spa.ce-net.com`. Real, playable, single-cell. *(Foundational.)*
2. **Self-subdivision live.** Turn on the controller loop so the genesis cell splits under load across
   relay + desktop + donor nodes. *(Needs the cell image published for `mesh_deploy`.)*
3. **Cloud burst.** Wire one `CloudProvider` (Hetzner is already scripted) so surges rent capacity.
4. **Gateway fleet + in-tab peers.** Ship `galaxy-peer.js` so browsers are distinct players, behind an
   autoscaled gateway tier. *(The one real code gap for web-multiplayer at scale.)*
5. **Load-test the gates.** Gossipsub at thousands of topics/peers, relay socket limits, burst latency
   — measured, not assumed. "Millions" is proven here, not declared.

The distributed-systems hard parts — sharding, interest management, failover, transit, deterministic
topology — are designed and in the box. What remains is deployment, the in-tab peer, and load proof.
