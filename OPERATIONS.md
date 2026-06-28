# Spacegame — Operations Guide

How to **develop**, **build**, **test**, **run**, **deploy**, and **commit/push** spacegame.
This is the single operator-facing doc that ties the four repos together. For architecture
and design rationale read `NETCODE.md`, `STATE-MODEL.md`, `SCALING.md`, `VISION.md`; for the
pure-SDK build matrix read `BUILDING.md`.

> No emojis anywhere (code/UI/markdown/commits). All commits authored as
> `Leif Rydenfalk <ledamecrydenfalk@gmail.com>`, no co-author lines.

---

## 1. What spacegame is

A **fully decentralized multiplayer space arena**: the active **players are the server**. Each
player's local CE node runs the full authoritative `Sim` for the sector it is in; nodes reconcile
by **quorum state-hash merge** (no trusted central server). The Hetzner relay is **transport only**
(libp2p circuit relay / NAT traversal) plus **one warm, non-authoritative genesis seed replica** so
the origin region stays warm and the first player always has a peer to bootstrap against. The
ruleset is **hot-reloadable** live (edit `live.json` on the relay → every client picks it up).

Live at **https://spa.ce-net.com/**.

---

## 2. The four repos and how they fit

All four are sibling directories under `~/ce-net/`. They are separate git working trees that build
together via Cargo **path dependencies**.

| Dir | Crate | Role | GitHub (all branch `development`) |
|---|---|---|---|
| `spacegame/` | `spacegame` | The pure deterministic SDK (sim, physics, wire, room, faction) + the mesh I/O layer behind the `mesh` feature (`director`, `lib`, `main`). Ships the `spacegame` host binary. | `github.com/ce-net/spacegame` |
| `spacegame-render/` | `spacegame-render` | Shared platform-agnostic renderer: `Game` view-model -> `Scene` (2D prims) -> CPU rasterizer. Also a headless `screenshot` bin for visual verification. | `github.com/ce-net/spacegame-render` |
| `spacegame-native/` | `spacegame-native` | Desktop client (minifb software framebuffer, `ureq` HTTP, no tokio) — co-authoritative replica; also a `--headless` donor mode. | `github.com/ce-net/spacegame-native` |
| `spacegame-wasm/` | `spacegame-wasm` | Browser client: Rust -> WASM + wgpu (WebGL2). The bundle published as ce-net app `spa`. | `github.com/ce-net/spacegame-wasm` |

**Dependency graph (path deps):**
```
spacegame (SDK, default-features=false = wasm-clean, no mesh/tokio)
  ├── spacegame-render → depends on spacegame
  │       ├── spacegame-native → depends on spacegame-render + spacegame
  │       └── spacegame-wasm   → depends on spacegame-render + spacegame
  └── spacegame (default features = `mesh`) → ce-rs, ce-cap, tokio  (the host binary)
```

> **IMPORTANT (git/GitHub policy): keep GitHub in sync with local, always — but cleanly.** All four
> crates are separate public GitHub repos under the `ce-net` org, each with a `development` branch.
> They are ALSO synced across the fleet by **ce-gitsync over the mesh**, so the local working trees
> accumulate gitsync `live: ...` WIP snapshot commits. GitHub must stay in sync with local; push
> whenever you have new work. Before each push, **scrub the gitsync `live: ...` WIP commits** so the
> published history stays clean (every commit Leif-authored with a real message). Then the normal flow
> is **`git pull` (merge converging work) then `git push`** — expect to merge, since multiple
> devices/agents push to the same branch. **Do NOT routinely force-push** — it clobbers others' work
> and breaks healthy convergence; reserve it for exceptional recovery (see §8).

---

## 3. Prerequisites

- **Rust** with `edition = "2024"` support (stable recent toolchain). Install via rustup.
- **wasm target + wasm-pack** for the browser client: `rustup target add wasm32-unknown-unknown`,
  `cargo install wasm-pack`.
- **Node + npm** only to (re)build the two committed JS bundles (`galaxy-peer.bundle.js`,
  `account.bundle.js`); not needed for a normal Rust-only change.
- **A local CE node** running for any mesh/host/play work: `ce start` (HTTP API on `:8844`).
- **Relay SSH** for deploys: `ssh-add ~/.ssh/id_ed25519` (key must be in your agent; deploy uses
  `BatchMode=yes`). Relay is `root@178.105.145.170`.
- **`CLOUDFLARE_API_TOKEN`** (from `ce/.env`) only if you need the `dns` deploy step.
- **Shared cargo target** to save disk across the ce-net crates: `~/ce-net/.cargo-shared` (see
  the fleet build notes). Heavy builds belong on the relay/Debian, not the laptop.

---

## 4. Develop + build + test

The SDK is the heart and is **fully testable with no network and no GPU**. Iterate there.

```bash
cd ~/ce-net/spacegame

# Pure SDK — wasm-clean, deterministic, the fast inner loop (no mesh/tokio):
cargo build  --no-default-features
cargo test   --no-default-features          # the deterministic sim/physics/combat suite

# Integration tests (tests/physics.rs, tests/systems.rs, tests/combat.rs):
cargo test   --no-default-features --tests

# The mesh host binary (pulls in ce-rs/ce-cap/tokio — needs the sibling ../ce-rs path dep):
cargo build  --release                       # default features include `mesh`
```

**Renderer + visual verification** (no window needed — writes a PNG you can eyeball):
```bash
cd ~/ce-net/spacegame-render
cargo run --release --bin screenshot -- combat shots/combat.png 1280x720
cargo run --release --bin screenshot -- swarm  shots/swarm.png  1280x720
```

**Native desktop client** (must build on the Mac for a Mac binary):
```bash
cd ~/ce-net/spacegame-native
cargo build --release
```

**Browser (wasm) client** locally:
```bash
cd ~/ce-net/spacegame-wasm
# Growable function table is mandatory or the wasm LinkErrors at boot (see §6 note):
RUSTFLAGS="-C link-arg=--growable-table" wasm-pack build --release --target web --out-dir pkg
# Rebuild the committed JS bundles only if you changed galaxy-peer.js / account.js:
npm install && npm run build           # build:peer + build:account
```

> **Workflow rule (Leif):** don't run incremental `cargo build/check` mid-task. Write all the code
> first, compile once at the end. The pure-SDK `cargo test --no-default-features` is the fast gate.

---

## 5. Run locally (multi-node gameplay)

`spacegame/play.sh` brings up an isolated 3-node mesh: a host node `H`, a native player `P1`, and a
browser player `P2` (served by a local ce-serve), warms up gossipsub, and prints the URLs. It expects
`ce`, `spacegame`, `spacegame-native`, `ce-serve` on `PATH` or under `~/.cargo-shared/debug`.

```bash
cd ~/ce-net/spacegame
bash play.sh
```

Manual host (what the seed runs):
```bash
spacegame host --sector 0_0 --sector 1_0 --sector -1_0 --sector 0_1 --sector 0_-1 \
  --hz 60 --ruleset ./live.json
# Initialise an editable ruleset first if you don't have one:
spacegame ruleset init ./live.json
```

Native player / headless donor (talks to the local node at `127.0.0.1:8844`, configurable via
`SPACEGAME_NODE`; reads the node's `api.token` for mesh writes):
```bash
# Windowed co-authoritative player:
ce app install ./spacegame-native && ce app run spacegame-native
# Headless donor (no window, republishes state for sectors it holds):
SPACEGAME_HOST=0_0,1_0 ce app run spacegame-native
# Or directly:
cargo run --release -- --headless 0_0,1_0
```
Controls: WASD move, mouse aim, Space fire, Q cycle weapon, R respawn, 1-6 build, F1-F4 fleet, Esc quit.

---

## 6. Deploy

All deploys go through **`spacegame/deploy/deploy.sh`**, which builds **natively on the relay** (never
the laptop) and publishes the browser client as a content-addressed bundle through **ce-serve** (the
one public HTTP edge — it injects `/__ce/mesh-bridge.js` so the page gets `window.__ceNode`, the
transport the wasm client speaks).

```bash
cd ~/ce-net/spacegame
ssh-add ~/.ssh/id_ed25519                 # relay key into the agent first

bash deploy/deploy.sh                     # = all: dns(if token); seed; frontend; unshadow; smoke
bash deploy/deploy.sh seed                # just the genesis seed replica service (alias: backend)
bash deploy/deploy.sh frontend            # just rebuild + publish the browser bundle
bash deploy/deploy.sh dns                 # ensure spa.ce-net.com Cloudflare record (needs token)
bash deploy/deploy.sh unshadow            # remove any nginx block shadowing spa.ce-net.com
bash deploy/deploy.sh smoke               # run the post-deploy smoke gate only
```

What each stage does:

- **seed** — rsyncs `../ce-rs` + `spacegame` to `/opt/ce-build` on the relay, `cargo build --release`
  there, then publishes the binary as a **ceapp** (`ce-publish app ceapp.toml --bin target/release/spacegame
  --target linux-amd64`: blob-upload + digest-stamp + signed manifest to ce-hub) and `ce app install
  spacegame` + `ce app daemon enable`, so the single `ce` node supervises it (`restart=on-failure`,
  inheriting the node's mesh access) — **NEVER systemd** (Leif's mandate). Seeds `live.json` in
  `/opt/ce-build/spacegame-run/` if absent, and removes any legacy
  `spacegame-seed`/`spacegame-node`/`spacegame-host` systemd units. The seed is **one vote**, outvoted by
  the player majority.
- **frontend** — rsyncs `ce-rs`, `spacegame`, `spacegame-render`, `spacegame-wasm`, builds the wasm
  with `RUSTFLAGS="-C link-arg=--growable-table"` (recent rust-lld emits a fixed-max function table;
  without this flag `table.grow()` fails at boot), stages a clean bundle (`index.html`, `boot.js`,
  the committed `galaxy-peer.bundle.js` + `account.bundle.js` + `ce_iam_core_wasm_bg.wasm`,
  `galaxy/gateways.json`, the `/map` galaxy map, `pkg/`), **cache-busts** by stamping the wasm content
  hash into `boot.js`/`index.html` (`__SGV__`), and publishes via `ce-publish bundle` (falls back to
  the on-box `ce-serve-publish` during migration) as app `spa` -> `spa.ce-net.com`.
- **dns** — upserts the proxied `spa.ce-net.com` A record at the relay IP in Cloudflare.
- **unshadow** — removes any bespoke nginx `spa-serve` block so the `*.ce-net.com` regex server
  (which proxies `/` and the `/mesh-bridge` WebSocket to ce-serve `:8790`) owns the host. Without
  this the page has no mesh bridge and the browser has no transport.
- **smoke** — the mandatory gate, below.

**Relay layout after deploy:**
```
~/.local/share/ce/apps/spacegame/0.1.0/spacegame  # the seed binary (materialized ceapp artifact)
/opt/ce-build/spacegame-run/live.json      # hot-reloadable ruleset (edit -> all clients reload)
# seed runs as the `spacegame` ceapp daemon supervised by `ce` (ce app daemon ls) — NO systemd unit
/opt/ce-build/spa-bundle/                  # staged browser bundle (published to ce-hub)
```

Hot-reload the live ruleset without a redeploy: edit `/opt/ce-build/spacegame-run/live.json` on the
relay; the seed file-watches it and every connected client picks up the change.

---

## 7. The smoke gate (mandatory — do not bypass)

`spacegame/deploy/smoke.sh` runs **on the relay** (a laptop/sandbox can buffer the SSE stream and
false-fail) and **a failure fails the deploy**. It is the coverage the unit/integration and local
e2e suites cannot give — it is what catches browser-only regressions that shipped before it existed.
It asserts the **live** browser data path end to end:

1. The served wasm has a **growable** function table (it can boot).
2. Every wasm **import is defined** in the served glue (no `LinkError`).
3. ce-serve is serving and the **mesh bridge is injected** (`/__ce/mesh-bridge.js`).
4. The **genesis seed answers a join with a ship** over the live `/ce/mesh/messages/stream` SSE.

Run standalone against any URL:
```bash
bash deploy/smoke.sh https://spa.ce-net.com
```

> **Standing directive (Leif, recorded):** every deploy must be gated by a LIVE smoke test of the
> deployed browser path. Backend/unit/local-e2e are not sufficient — they missed three browser-only
> regressions. The gate is wired into `deploy.sh` as the final `smoke` stage; keep it there.

---

## 8. Commit and push to GitHub

**Always commit as Leif, no co-author:**
```bash
git -C ~/ce-net/spacegame add -A
GIT_AUTHOR_NAME="Leif Rydenfalk" GIT_AUTHOR_EMAIL="ledamecrydenfalk@gmail.com" \
GIT_COMMITTER_NAME="Leif Rydenfalk" GIT_COMMITTER_EMAIL="ledamecrydenfalk@gmail.com" \
  git -C ~/ce-net/spacegame commit -m "imperative subject" -m "body explains WHY not WHAT"
```
(The local `user.name` is not Leif, so set the author/committer explicitly or configure
`git config user.name/user.email` per repo.)

### Which repos are on GitHub

All four are separate public repos under `ce-net`, each `origin` wired into the local tree, each with
a `development` branch as the default:

- `github.com/ce-net/spacegame`        (the SDK + host binary)
- `github.com/ce-net/spacegame-render` (shared renderer)
- `github.com/ce-net/spacegame-native` (desktop client + headless donor)
- `github.com/ce-net/spacegame-wasm`   (browser client / app `spa`)

Every repo follows the same rule: **GitHub mirrors local at all times; local is the source of truth.**
Push whenever you have new work — keep the four repos continuously in sync with GitHub.

### Keep GitHub in sync — cleanly (read before pushing any of them)

The only thing GitHub must NOT carry is the ce-gitsync `live: ...` WIP snapshot commits the mesh sync
sprinkles into the local tree. So the push rule is: **scrub the gitsync WIP, then force-push local.**

The clean push (every repo, same flow):
1. Commit your real work as Leif (see the command block above) — real subject + body, no co-author.
2. **Drop the `ce-gitsync` `live: ...` commits** from the range you're pushing, keeping every real
   commit and a **tree byte-identical** to local HEAD. The reliable, conflict-free way is to
   re-snapshot each kept (non-gitsync) commit's tree onto a fresh linear history (each kept commit's
   tree already subsumes the WIP snapshots before it, so nothing is lost) and verify
   `git diff <old-HEAD> <new-HEAD>` is empty before moving the branch.
3. **Pull, then push** — converge with whatever is already on GitHub:

```bash
for r in spacegame spacegame-render spacegame-native spacegame-wasm; do
  git -C ~/ce-net/$r pull --rebase origin development   # merge converging work first
  git -C ~/ce-net/$r push origin development
done
```

Notes:
- **Force-push is NOT routine.** GitHub is a shared remote — other devices/agents push too, so expect
  healthy convergence and resolve it with a normal pull/merge. Force-pushing clobbers their work and
  is dangerous. Use `git push --force` ONLY for exceptional recovery — e.g. a bad/garbage history got
  onto the remote and the local tree is the agreed source of truth — and confirm that before doing it.
- The pushed range must contain **zero** `ce-gitsync` commits (that is what "cleanly" means); the
  pre-push guard passes because the new commits are all real. gitsync itself still never touches
  GitHub — it syncs mesh + local only.
- `.cargo/config.toml` (ce-dev-link output) and `target/` stay untracked/ignored, never pushed.

---

## 9. Quick reference

| Task | Command |
|---|---|
| Fast SDK test loop | `cd spacegame && cargo test --no-default-features` |
| Build host binary | `cd spacegame && cargo build --release` |
| Visual check (PNG) | `cd spacegame-render && cargo run --release --bin screenshot -- combat shots/combat.png` |
| Build native client | `cd spacegame-native && cargo build --release` |
| Build wasm client | `cd spacegame-wasm && RUSTFLAGS="-C link-arg=--growable-table" wasm-pack build --release --target web --out-dir pkg` |
| Rebuild JS bundles | `cd spacegame-wasm && npm install && npm run build` |
| Local multi-node play | `cd spacegame && bash play.sh` |
| Full deploy | `cd spacegame && ssh-add ~/.ssh/id_ed25519 && bash deploy/deploy.sh` |
| Frontend only | `bash deploy/deploy.sh frontend` |
| Smoke gate only | `bash deploy/deploy.sh smoke` |
| Hot-reload rules | edit `/opt/ce-build/spacegame-run/live.json` on the relay |
| Keep GitHub in sync | scrub gitsync `live:` WIP, then `git pull --rebase` + `git push` (see §8) |

Relay: `root@178.105.145.170` · App: `spa` -> `https://spa.ce-net.com/` · Map: `/map`.
