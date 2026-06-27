# spacegame — live deployment status (spa.ce-net.com)

**DECENTRALIZED (2026-06-27):** spacegame has no relay game authority — the active PLAYERS are the server.
Each player's node runs the full authoritative `Sim` for its region and they reconcile by quorum
state-hash merge (NETCODE.md). The relay was demoted from the planet-scale adaptive node to (a) ce-net
TRANSPORT and (b) one warm, non-authoritative genesis SEED replica. This file records what is running,
what is proven, and the honest remaining rungs.

## Live now

- **URL:** https://spa.ce-net.com/ (also https://ce-net.com/apps/spa/). Page, `boot.js`, the wgpu WASM
  bundle (`application/wasm`), and the gateway directory (`/galaxy/gateways.json`) all serve 200.
- **Players are the server:** the wasm client ships the full `Sim` and runs it as a `replica::Replica`,
  advancing to the shared wall-clock tick and rendering from its OWN state — it does not depend on any
  relay-published `/state`. Players exchange tick-tagged inputs on the sector `/in` topic and merge by
  quorum hash. Crossing a sector edge re-homes the replica onto the neighbour (no more teleport-to-centre).
- **Seed:** `spacegame-seed.service` on the relay (`spacegame host --sector 0_0 1_0 -1_0 0_1 0_-1
  --hz 60 --ruleset live.json`) — one lightweight, non-authoritative replica that keeps the genesis ring
  warm and is outvoted by the player majority. The planet-scale `spacegame node` (gateway + leaderless
  controller + autoscale) is **no longer deployed**. Hot-reloads `live.json`.
- **Transport:** players reach each other (and the seed) through the same-origin `/ce` mesh bridge (nginx
  → relay CE node, token injected server-side) over the relay's libp2p transport / NAT traversal.

## Proven (the hard distributed-systems parts)

All in the build and unit-tested (192 lib + 23 integration tests green; the one red test,
`sim::shield_regenerates`, is a **pre-existing** mismatch with shield unlock-gating from commit 2afcff4,
unrelated to this work): the deterministic split/merge verdict, rendezvous ownership, interest set,
quadtree addressing/partition, world generation, the elastic-fleet/autoscaler decision logic, the gossip
protocol (shape commits + load frames + heartbeats), and the live galaxy map. The controller issues real
`mesh_deploy` for remote child cells and hosts assigned children in-process; a hot genesis cell splits
into four across the relay's cores (and onto donor nodes as they join), bounded by `--max-depth`.

## Honest remaining rungs to literal millions

1. **Snapshot rate single-node — FIXED.** Originally the relay's genesis snapshots reached bridged
   browsers only via a (now-removed) stray host, because gossipsub never echoes a publish back to its own
   publisher, so the relay's own genesis was invisible to the browsers it bridges. The CE node now
   self-delivers app pub/sub to its local subscribers (ce commit "node: self-deliver app pub/sub to local
   subscribers"), so the relay's genesis `/state` reaches bridged browsers at the full 20 Hz. The genesis
   cell is anchored by the GATEWAY only (no split-brain across nodes).
2. **In-tab libp2p peer (distinct identity per browser).** `spacegame-wasm/galaxy-peer.js` is the
   scaffold; `startPeer` still needs `@libp2p/*` bundled into the page. Until then every browser shares
   the relay node's identity through the `/ce` bridge (fine for play, not yet "a million distinct
   players"). The gateway directory (`/galaxy/gateways.json`) is live for when it lands.
3. **Cloud burst.** `cloud_hetzner` is wired as a `CloudProvider` but `provision`/`destroy` are stubs;
   the autoscaler logs unmet demand instead of renting nodes. Donor capacity still absorbs growth.
4. **Per-cell worldgen in the sim.** Each leaf is currently an independent arena on the proven sector sim;
   wiring `worldgen::CellGenesis` into the sim per `CellId` makes cells visually distinct.
5. **Load proof.** "Millions" is designed and the mechanism is live; a real load test (gossipsub at
   thousands of topics/peers, burst latency) remains to be measured, not declared.

## Redeploy

```
ssh-add ~/.ssh/id_ed25519                # or any relay key in your agent
bash deploy/deploy.sh                     # dns + seed + frontend
```

Runtime artifacts install to `/opt/ce-build/spacegame-run/` (binary + `live.json`) — OUTSIDE the synced
source tree, so a `frontend` re-sync can't clobber the running seed. The seed's API token is injected via
a systemd drop-in (`/etc/systemd/system/spacegame-seed.service.d/api-token.conf`), kept out of the repo.
The `seed` deploy step also disables + removes the old `spacegame-node` / `spacegame-host` units on the
relay, so the demotion to a seed is idempotent.

**Serving: ce-serve only.** `spa.ce-net.com` is served by **ce-serve** from a content-addressed bundle
(`ce-serve-publish`), NOT the hub. ce-serve injects `/__ce/mesh-bridge.js`, so the page gets
`window.__ceNode` over a WebSocket (`/mesh-bridge`) — the transport the client needs. The earlier custom
nginx block that served straight from the hub was removed: a hub-served page has no mesh bridge, so the
browser had no transport (you'd join and see no ship). The hub is the registry/tracker, not a web server.
