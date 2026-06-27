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

> Why do we even "predict" at all? the architecture should be we run the full backend server on our local
> mac AND relay AND on each other player for instant feedback and then merge everything - but for this we
> need to first properly sync and make sure the server ALWAYS gets all our inputs and stays in sync and rely
> on local mac backend servers for player inputs and movement with proper auto cheat merging of server
> states properly. This is very advanced. I hate sector clamping - hte sectors should ADAPT to players and
> other servers should automatically take over. Document everything i say verbatim!

(=> This supersedes "client-side prediction" as the target. The end-state is NOT predict-then-correct: it
is **replicated authority** — the full authoritative sim runs locally (Mac) AND on the relay AND on each
nearby player's node, every replica ingests every input, and the replicas **merge** their states with
anti-cheat reconciliation. "Instant feedback" comes from the LOCAL authoritative sim, not a guess.
Prerequisite #1 is INPUT SYNC: every input must reach every replica reliably and in order, and the
replicas must stay convergent. **No sector clamping / no walls** — sectors ADAPT to where players are and
neighbouring nodes AUTOMATICALLY TAKE OVER hosting as players move. See `NETCODE.md`.)

> No authored bool please thats stupid... we must have proper state merging ! people will try to cheat
> when there are millions of players. the state merging and the one server per player goes hand in hand.
> We dont even need the relay backend server - the players machines can themselves be servers and that
> should be enough so when there are no players the state is lost which is perfect for deelopment. and
> proves something: in ce-net apps scales on demand by having each user host a part of it.

> What the fuck is self state

> do proper state merging

> redo

> yes all player should run the full server locally and contribute to the game.

(=> Confirms the target: EVERY player's node runs the full authoritative `Sim` for its region and
contributes compute to the shared game — players are the servers. Build: the deterministic `Replica`
engine, integrated into browser/native/relay, merged by quorum.)

> How do i see the map of the entire mesh and all active players and server instances? do we have a
> spacegame map function to monitor the entire mesh and all servers contributing? document what i say
> verbatim

(=> Wants a live mesh-monitor view: the whole galaxy, every active player, and every node/server
instance hosting a region (who is contributing compute, which sector each hosts). See the galaxy-map
work; if it doesn't already show hosts+players live, that monitor is a build item.)

> think like this: browsers and players are the only servers. imagine that there is no relay stable
> server at all just browsers and players. woudl this system work in this situation? just player to player
> communication and server hosting together?

(=> The acid test for the architecture: ZERO trusted/stable server — only browsers/players, peer-to-peer,
hosting + communicating together. The AUTHORITY layer we built (deterministic replicas + quorum merge +
tick-tagged inputs) is server-free BY DESIGN and passes. The TRANSPORT/IDENTITY layer does NOT yet: today
every browser tunnels through the ce-serve bridge to the SAME relay node and even shares its NodeId
[galaxy-peer.js: "a million tabs collapse into one player"]. True P2P needs the in-tab libp2p peer
[galaxy-peer.js, currently a stub] wired — each browser its own key/identity + WebTransport/wss +
gossipsub — plus browser<->browser merge/proof exchange and peer snapshot bootstrap. Web reality: browsers
can't accept inbound, so SOME dumb, stateless, replaceable, NON-authoritative rendezvous/signaling +
entry URL is irreducible [like Bitcoin's DNS seeds] — but it holds no game authority. So: "no stable
server" => "no TRUSTED server; only replaceable peer-provided bootstrap." Build items: wire galaxy-peer.js
transport, browser-side merge, peer bootstrap.)

(=> BINDING: there is NO trusted authority and NO "declare your own position" (the rejected `authored`
bool / `self_state` — a client telling the host where it is, which is trivially cheatable). The model is
**deterministic replicated simulation merged by quorum**: every node in a region runs the SAME
deterministic sim from the SAME ordered input log (Stage 0), and replicas AGREE on a periodic state hash
(`Sim::state_hash` + `replication::agree`); a node whose state differs is a cheater/faulty and is
re-synced to the quorum or excluded. Movement isn't trusted — it is RE-COMPUTED by every replica from
inputs, so a liar diverges from the majority and loses. **One-server-per-player == the merge**: each
player's own machine hosts the region it's in and is one replica among the players present; remove the
relay backend as an authority. **No players in a region => its state is dropped** (perfect for solo dev:
resets), which is the whole point — it proves ce-net apps scale on demand because each user hosts a part
of the app. See `NETCODE.md`.)

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
