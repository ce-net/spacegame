# Spacegame — feel + state model (Leif's directives, verbatim)

Leif's word is law. His requests are transcribed here **word for word, unedited**, with the architecture
each one defines underneath. Keep this file up to date as the design evolves.

---

## Verbatim requests

> time for refinement of spacegame. make it feel realtime and not laggy like now. Remove the borders -
> the world must be infinate galaxy able to handle millions of users. The ships traver very slowly. Lots
> of gameplay is not wired up to the frontend and is missing systems for it in the frontend

> Were going for more of a reassembly feeling of gameplay. fast and responsive. zero delay for inputs

> Alright make it so that my local node is the server for me - basically instant with 0 lag efficently.
> with different update rates based on latency to different places - maximum possible. because your device
> should be the server for you and those closeby and replicate it also so its always available in a minimum
> amount of places so that when youre node quits all state is not lost. Or make it so that all state is lost
> without players - no players = no compute or storage to have spacegame on so its reset every time for me
> the developer but as more player come the state continues and with millions of players its all efficently
> distributed the terrabyte save file.

> Document this verbatim

> Deploy it globally! Always!

(=> Standing rule: every spacegame iteration is deployed live via `bash deploy/deploy.sh` — backend
adaptive-galaxy node + wasm frontend published through ce-serve — gated by the live browser smoke test.
"Always" = make global deploy the default end-step of a change, not an opt-in.)

---

## What these define (the model we are building toward)

### 1. Feel: Reassembly-style — fast, responsive, ZERO input delay
- The game should feel like **Reassembly**: quick, momentum-carrying flight; the ship reacts the instant
  you press a key, never after a server round-trip.
- **Local-first prediction is mandatory**, not optional. The client runs the exact deterministic movement
  math locally and the mesh only *confirms*. (Implemented: `spacegame_render::Game::predict` + soft
  reconciliation; movement retuned for speed/momentum in `sim::{MAX_SPEED,THRUST,DAMPING,TURN_RATE}`.)

### 2. Your own device hosts you — "your node is the server for you"
- The authoritative cell covering **you and the players near you** should run on **your own node** (or the
  nearest capable one), so your latency to the authority is ~0. This is the placement goal: a player's
  home cell is hosted on the lowest-RTT node — ideally the player's own machine.
- **Different update rates by latency, maximum possible.** Each subscriber gets snapshots at a rate set by
  its RTT to the host: a co-located/LAN player gets the full tick rate; a distant player gets a coarser
  rate. The host fans out per-client cadences instead of one global rate. (`ClientProfile.snapshot_divisor`
  exists as the seed; the per-RTT, per-subscriber cadence is the work item.)

### 3. Replication to the MINIMUM number of places for availability
- State is replicated to **as few nodes as possible while still surviving a host quitting** — when your
  node leaves, the cell's state is not lost; a nearby replica is deterministically promoted. (The K-replica
  machinery exists in `replication.rs`; "minimum K that still guarantees survival" is the tuning goal.)

### 4. Population-driven persistence — "no players = no state" (for the dev), continuity as players arrive
- **No players in a region ⇒ no compute and no storage** for it: it is dropped. For the **developer playing
  solo**, this means the world **resets every time** — there is nobody to hold the state, so it is not paid
  for or kept.
- **As more players arrive, the state continues**: a region with players is held by those players' nodes.
  The more populated a region, the more durably and widely it is held.
- **At millions of players the full "terabyte save file" is efficiently distributed** across all the
  players' nodes — no single machine holds it; each holds the slice for the region it plays in. Storage
  scales with population exactly like compute does (it IS the same nodes).

This is the unifying principle: **compute, authority, and storage for a region all live on the nodes of the
players in that region.** Empty space costs nothing; a crowd brings its own servers and its own disk.

---

## Status against the model (2026-06-27)

Shipped this pass:
- Zero-delay local prediction + reconciliation (the ship moves on keypress, the mesh confirms).
- Reassembly-feel movement tuning (faster, momentum-carrying).
- Open world: bigger sectors (`SECTOR_SIZE` 3000 -> 9000) + the gateway hosts a seamless ring of
  neighbouring play sectors in-process (`--ring`, default 1) so edge-crossing never hits a wall.
- Gameplay wired to the frontend: HUD (hull/shield/energy/minerals/kills/weapon/fleet) on both native and
  web, weapon switching, build/upgrade (keys 1-6), and fleet commands (F1-F4) actually sent over the wire.

Work items toward the full model above (not yet done):
- **Placement = your own node hosts your home cell** (lowest-RTT / self-host first), not just the relay.
- **Per-subscriber snapshot cadence by RTT** (maximum rate for near players, coarser for far ones).
- **Minimum-K replication** tuned to "survive host exit" and **population-driven drop** (empty region ⇒
  released; solo-dev ⇒ resets; crowded region ⇒ continues, held by the players' nodes).
- **Distributed save** so the aggregate state is the union of per-region slices on players' nodes.
