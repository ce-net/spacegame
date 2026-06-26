# Deploying spacegame — a first-class ce-net app, not a demo

Spacegame is deployed **exactly like any other system on ce-net** (ce-hub, ce-serve, drift): a
server-class backend running as a systemd service on the relay, plus a browser client published to the
hub under its own app id and served on its own domain. It is **not** a bundled page in
`web/demos/` — that pipeline (`spa.ce-net.com/game`) is retired in favour of this.

```
                          ┌─────────────────────────── relay (178.105.145.170) ───────────────────────────┐
  browser / native  ──────┤  spacegame.ce-net.com  →  nginx *.ce-net.com  →  ce-hub :8970  (app "spacegame")│
  (renders + predicts)    │                                                   serves index.html + pkg/*.wasm │
        │ mesh            │                                                                                  │
        └────────────────┤  ce-relay (`ce start`)  ←→  spacegame-host.service  (authoritative sim)          │
                          │     :8844 mesh node            real Rust `spacegame` binary, pinned sectors      │
                          └──────────────────────────────────────────────────────────────────────────────┘
```

## The two halves

| Half | What runs | Where | How |
|---|---|---|---|
| **Backend** | the real Rust `spacegame` binary hosting pinned sectors (authoritative sim, hot-reloadable ruleset) | `spacegame-host.service` on the relay, `/opt/ce-build/spacegame` | `deploy.sh backend` |
| **Frontend** | the browser client (`spacegame-wasm`: Rust→WASM + wgpu) | hub app `spacegame` → `spacegame.ce-net.com` | `deploy.sh frontend` |

Both are built **natively on the relay** (the deploy target), never on the laptop — same dogfooding
rule as `web/deploy/ce-build.sh`.

## Deploy

```bash
ssh-add ~/.ssh/id_ed25519                 # the relay key must be in your agent
bash deploy/deploy.sh                      # dns + backend + frontend
# or piecemeal:
bash deploy/deploy.sh backend              # (re)build + restart the authoritative host
bash deploy/deploy.sh frontend             # (re)build the wasm client, publish to the hub
bash deploy/deploy.sh dns                   # ensure spacegame.ce-net.com (needs CLOUDFLARE_API_TOKEN)
```

## Why a pinned authoritative host

Like drift, the world only advances if a **server-class** node runs the sim: the browser bundle ships
only the renderer + local prediction (`default-features = false`, no mesh I/O), so a browser-elected
host cannot be authoritative. `spacegame-host.service` pins the relay as the authoritative host for a
plus-shaped block of sectors around the origin; every client renders and predicts against the state it
publishes, and failover/replication (`replication.rs`) promotes another node if it drops. Grow the
pinned region by editing the `--sector` flags in the unit, or place neighbours on other mesh nodes with
`spacegame place` as more hosts join.

## Hot reload in production

The service runs `host --ruleset /opt/ce-build/spacegame/live.json`. Edit that file on the relay
(weapons, tech tree, tunables, **shaders** — the whole `Ruleset`) and the watcher pushes it live to the
entire galaxy with no restart and no dropped players; every client re-fetches and hot-applies it. Seed
or reset it with `spacegame ruleset init live.json`.

## Local play / dev

For driving the game on your own machines over the real mesh (native window + browser, screenshots),
see [`../SPACEGAME-PLAY.md`](../SPACEGAME-PLAY.md) and `../play.sh`.
