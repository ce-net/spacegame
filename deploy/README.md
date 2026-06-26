# Deploying spacegame — served by ce-serve (the ONLY way to serve a ce-net app)

**Architectural rule (Leif's directive): ce-serve is the only supported way to serve a ce-net demo/app
to the browser. The hub is the registry/tracker, NOT a web server.** Serving an app's files straight from
the hub (`/apps/<id>/…`) is deprecated and was a mistake: a hub-served page gets **no mesh bridge**, so the
browser has **no `window.__ceNode`**, no transport, and the game can't reach the mesh (you'd join and see
no ship). ce-serve resolves Host → a content-addressed bundle, serves each file from the node blob store
(wasm as `application/wasm`), and **auto-injects `/__ce/mesh-bridge.js`**, which gives every page
`window.__ceNode` over one same-origin **WebSocket** (`/mesh-bridge`) — the transport the WASM client speaks.

```
                       ┌──────────────────────────── relay (178.105.145.170) ────────────────────────────┐
  browser  ────────────┤  spa.ce-net.com → nginx (*.ce-net.com regex) → ce-serve :8790                    │
   (renders+predicts)  │     ce-serve resolves Host→bundle (ce-hub), serves blobs, injects the mesh bridge │
        │  WebSocket    │     window.__ceNode  ⇄  /mesh-bridge (WS)  ⇄  ce node :8844  ⇄  the mesh         │
        └───────────────┤  ce-relay (`ce start`)  ←→  spacegame-node.service (genesis host + controller +  │
                        │     :8844 mesh node           gateway; the authoritative adaptive-galaxy sim)     │
                        └────────────────────────────────────────────────────────────────────────────────┘
```

## The two halves

| Half | What runs | Where | How |
|---|---|---|---|
| **Backend** | the adaptive-galaxy node (genesis host + leaderless controller + gateway) | `spacegame-node.service` on the relay, `/opt/ce-build/spacegame-run/` | `deploy.sh backend` |
| **Frontend** | the browser client (`spacegame-wasm`: Rust→WASM + wgpu), published as a **content-addressed bundle** | ce-serve (`spa.ce-net.com`) | `deploy.sh frontend` |

Both build natively on the relay. The frontend is published with **`ce-serve-publish <dir> spa.ce-net.com spa`**:
it blob-uploads each file to the node, builds the `{spa, files}` manifest, and registers `spa.ce-net.com →
bundle` in ce-hub. ce-serve then serves it. No nginx edits, no per-file hub upload.

## Deploy

```bash
ssh-add ~/.ssh/id_ed25519                 # a relay key in your agent
bash deploy/deploy.sh                      # dns + backend + frontend(ce-serve) + unshadow + smoke
# or piecemeal:
bash deploy/deploy.sh backend              # (re)build + restart the adaptive-galaxy node service
bash deploy/deploy.sh frontend             # (re)build the wasm, ce-serve-publish the bundle
bash deploy/deploy.sh unshadow             # ensure no bespoke nginx block shadows the host (ce-serve owns it)
bash deploy/deploy.sh smoke                # POST-DEPLOY GATE: live browser path (boot + bridge + join→ship)
```

`deploy.sh` runs **`smoke.sh` as a blocking final gate** — it asserts, against the LIVE url, that the wasm
boots (growable function table), that **ce-serve is serving with the mesh bridge injected**, and that a
joining player's ship comes back over the bridge. A red gate fails the deploy. See `LIVE-STATUS.md`.

## Why a server-class node

The world only advances if a server-class node runs the authoritative sim: the browser bundle ships only
the renderer + local prediction (`default-features = false`, no mesh I/O). `spacegame-node.service` is the
genesis host + controller + gateway; clients render/predict against the state it publishes. See
`../GALAXY-SCALE.md`.

## Local play / dev

See [`../SPACEGAME-PLAY.md`](../SPACEGAME-PLAY.md) and `../play.sh` for driving the game on your own
machines over the real mesh (native window + browser, screenshots).
