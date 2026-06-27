# Deploying spacegame — served by ce-serve (the ONLY way to serve a ce-net app)

**Architectural rule (Leif's directive): ce-serve is the only supported way to serve a ce-net demo/app
to the browser. The hub is the registry/tracker, NOT a web server.** Serving an app's files straight from
the hub (`/apps/<id>/…`) is deprecated and was a mistake: a hub-served page gets **no mesh bridge**, so the
browser has **no `window.__ceNode`**, no transport, and the game can't reach the mesh (you'd join and see
no ship). ce-serve resolves Host → a content-addressed bundle, serves each file from the node blob store
(wasm as `application/wasm`), and **auto-injects `/__ce/mesh-bridge.js`**, which gives every page
`window.__ceNode` over one same-origin **WebSocket** (`/mesh-bridge`) — the transport the WASM client speaks.

**Decentralized: the players ARE the server.** Each player's node (the browser wasm, or a native `ce`
node) runs the FULL authoritative `Sim` for the region it is in, exchanging tick-tagged inputs on the
sector `/in` topic and reconciling by quorum state-hash merge (see `../NETCODE.md`). The relay is NOT the
game authority — it is **ce-net transport** (libp2p relay / NAT traversal, so players reach each other over
the global internet, + the `/mesh-bridge` wss + ce-serve) plus ONE warm, non-authoritative **genesis seed**
replica so the origin region is never cold.

```
                       ┌──────────────────────────── relay (178.105.145.170) ────────────────────────────┐
  browser  ────────────┤  spa.ce-net.com → nginx (*.ce-net.com regex) → ce-serve :8790                    │
   (IS a server:       │     ce-serve resolves Host→bundle (ce-hub), serves blobs, injects the mesh bridge │
    runs the full Sim) │     window.__ceNode  ⇄  /mesh-bridge (WS)  ⇄  ce node :8844  ⇄  the mesh         │
        │  WebSocket    │     players exchange tick-tagged /in inputs + quorum hashes over the mesh         │
        └───────────────┤  ce-relay (`ce start`, TRANSPORT)  ←→  spacegame-seed.service (one warm,         │
                        │     :8844 mesh node                       NON-authoritative genesis replica)      │
                        └────────────────────────────────────────────────────────────────────────────────┘
```

## The two halves

| Half | What runs | Where | How |
|---|---|---|---|
| **Seed** | one lightweight, non-authoritative `spacegame host` replica on the genesis ring (keeps origin warm; one vote in the quorum) | `spacegame-seed.service` on the relay, `/opt/ce-build/spacegame-run/` | `deploy.sh seed` |
| **Frontend** | the browser client (`spacegame-wasm`: Rust→WASM + wgpu) — a FULL self-hosting replica, published as a **content-addressed bundle** | ce-serve (`spa.ce-net.com`) | `deploy.sh frontend` |

Both build natively on the relay. The frontend is published with **`ce-serve-publish <dir> spa.ce-net.com spa`**:
it blob-uploads each file to the node, builds the `{spa, files}` manifest, and registers `spa.ce-net.com →
bundle` in ce-hub. ce-serve then serves it. No nginx edits, no per-file hub upload.

## Deploy

```bash
ssh-add ~/.ssh/id_ed25519                 # a relay key in your agent
bash deploy/deploy.sh                      # dns + seed + frontend(ce-serve) + unshadow + smoke
# or piecemeal:
bash deploy/deploy.sh seed                 # (re)build + restart the genesis seed replica (alias: backend)
bash deploy/deploy.sh frontend             # (re)build the wasm, ce-serve-publish the bundle
bash deploy/deploy.sh unshadow             # ensure no bespoke nginx block shadows the host (ce-serve owns it)
bash deploy/deploy.sh smoke                # POST-DEPLOY GATE: live browser path (boot + bridge + join→ship)
```

`deploy.sh` runs **`smoke.sh` as a blocking final gate** — it asserts, against the LIVE url, that the wasm
boots (growable function table), that **ce-serve is serving with the mesh bridge injected**, and that a
joining player's ship comes back over the bridge. A red gate fails the deploy. See `LIVE-STATUS.md`.

## Why only a seed (no server-class authority)

The world advances because the PLAYERS run it: the wasm bundle ships the full `Sim` and each browser is a
`replica::Replica` advancing to the shared wall-clock tick, exchanging tick-tagged `/in` inputs and merging
by quorum. So no server-class authority is needed — the relay's `spacegame-seed.service` is just one warm
replica near genesis so the first arrival has a peer. Empty regions away from genesis are simply dropped
(ce-net "scales on demand"). The old planet-scale adaptive node (`spacegame node`, see `../GALAXY-SCALE.md`)
is no longer deployed; that machinery remains in the binary for a future where one box must carry a hot
region before donors arrive.

## Local play / dev

See [`../SPACEGAME-PLAY.md`](../SPACEGAME-PLAY.md) and `../play.sh` for driving the game on your own
machines over the real mesh (native window + browser, screenshots).
