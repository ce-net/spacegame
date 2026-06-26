# spacegame — live deployment status (spa.ce-net.com)

The **adaptive galaxy** (GALAXY-SCALE.md) is wired, built, and deployed live. This file records exactly
what is running, what is proven, and the honest remaining rungs to literal "millions."

## Live now

- **URL:** https://spa.ce-net.com/ (also https://ce-net.com/apps/spa/). Page, `boot.js`, the wgpu WASM
  bundle (`application/wasm`), and the gateway directory (`/galaxy/gateways.json`) all serve 200.
- **Backend:** `spacegame-node.service` on the relay (`spacegame node --gateway --region eu-central
  --hz 20 --max-depth 3 --split-players 60 --ruleset live.json`). It runs the real adaptive-galaxy
  daemon: hosts the **genesis cell** in-process (on the legacy `0_0` play token so the shipped client
  connects unchanged), runs the **leaderless controller** (rendezvous-HRW ownership → `Autoscaler::decide`
  → split/merge over real `mesh_deploy` + gossiped `ShapeCommit`s), heartbeats the control plane, and
  serves as a browser **gateway**. Hot-reloads `live.json`.
- **Transport:** the WASM client reaches the genesis cell through the same-origin `/ce` mesh bridge (nginx
  → relay CE node, token injected server-side). The node **self-delivers** its own `/state` snapshots to
  the bridged client.
- **`spacegame galaxy`** prints the live quadtree shape from the shape/control gossip.

## Proven (the hard distributed-systems parts)

All in the build and unit-tested (192 lib + 23 integration tests green; the one red test,
`sim::shield_regenerates`, is a **pre-existing** mismatch with shield unlock-gating from commit 2afcff4,
unrelated to this work): the deterministic split/merge verdict, rendezvous ownership, interest set,
quadtree addressing/partition, world generation, the elastic-fleet/autoscaler decision logic, the gossip
protocol (shape commits + load frames + heartbeats), and the live galaxy map. The controller issues real
`mesh_deploy` for remote child cells and hosts assigned children in-process; a hot genesis cell splits
into four across the relay's cores (and onto donor nodes as they join), bounded by `--max-depth`.

## Honest remaining rungs to literal millions

1. **Snapshot rate single-node.** With no remote peer subscribed to a cell's `/state` topic, the relay's
   self-delivery to the bridged browser is gossipsub-heartbeat-paced (~1 Hz), not 20 Hz. Client-side
   prediction keeps play smooth; full authoritative rate returns the moment a second node (desktop/donor)
   or an in-tab libp2p peer subscribes to the cell. This is a node-transport property, not a sim issue.
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
bash deploy/deploy.sh                     # dns + backend + frontend
```

Runtime artifacts install to `/opt/ce-build/spacegame-run/` (binary + `live.json`) — OUTSIDE the synced
source tree, so a `frontend` re-sync can't clobber the running node. The node's API token is injected via
a systemd drop-in (`/etc/systemd/system/spacegame-node.service.d/api-token.conf`), kept out of the repo.
`spa.ce-net.com` is served by a dedicated nginx block (`/etc/nginx/sites-available/spa-serve`) that maps
the host to the hub's per-file app store plus the `/ce` bridge.
