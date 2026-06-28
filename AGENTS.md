# AGENTS.md — spacegame: lessons learned + what to ALWAYS do instead

Hard-won rules for anyone (AI or human) working on spacegame's **four repos** (`spacegame` SDK,
`spacegame-render`, `spacegame-native`, `spacegame-wasm`) and deploying to **spa.ce-net.com**. Each rule
exists because doing the opposite broke something real (2026-06-28). Read before merging or deploying.

## 1. Branches — ALWAYS check all of them; never lose work
- Before working OR deploying, enumerate **every** branch in **every** repo: `git branch -a`. The
  default branch is NOT the whole story — a `git pull` once reset `development` and stranded the entire
  seamless **domain model** (`src/domain.rs`), `coords.rs` (1:1 scale), the `Objective` AI (`ai.rs`),
  `shipyard.rs`, and the in-canvas editor onto `wip-*` backup branches. The deploy silently shipped a
  regressed build missing all of it.
- Reconcile by **merging**, never by reset/force — merge commits keep every branch's history.
- **Watch for SILENT deletes:** a 3-way merge keeps a file *deleted* on HEAD if the other side didn't
  modify it since the base. After merging, diff trees: `comm -23 <(git ls-tree -r --name-only <wip>|sort)
  <(git ls-tree -r --name-only HEAD|sort)` and `git checkout <wip> -- <file>` anything wrongly dropped.
- When two branches reimplemented the same thing, find the **superset** (`git merge-base --is-ancestor`,
  line counts, feature greps) and take it wholesale per-file; let `cargo` reconcile the rest. Keep the
  unrelated infra (deploy/, ceapp.toml, the wasm JS/account tooling).
- Tag before any risky op: `git tag pre-<op> <branch>`. Push wip backups too.

## 2. Deploy — ALWAYS a ceapp, NEVER systemd
- The seed is the `spacegame` **ceapp daemon** (`ce-publish app` + `ce app install` + `ce app daemon
  enable`, supervised by the single `ce` node). Do NOT create systemd units for CE services.
- `ce app install` **default registry** (the relay hub IS ce-net.com). Passing
  `--registry http://127.0.0.1:8970` **HANGS** the materialize step and leaves the seed DOWN mid-deploy.
- `ce app uninstall` does NOT stop the running daemon — `pkill` the old proc so the supervisor respawns
  the freshly-materialized binary.
- `deploy.sh all` aborts if `CLOUDFLARE_API_TOKEN` is unset (a `${VAR:?}` under `set -e`); run
  `seed`/`frontend`/`unshadow`/`smoke` individually when you have no token (DNS already exists).
- Frontend is global automatically: `ce-publish bundle` → ce-hub → ce-serve serves spa.ce-net.com
  worldwide. "Local vs global" only concerns seed *placement* (`--on self` vs `--on fleet/tag/nearest`).

## 3. The wasm table invariant — BOTH are required or the page won't boot
`spacegame-wasm` must keep **both**, together:
1. `Cargo.toml`: `[package.metadata.wasm-pack.profile.release] wasm-opt = false` — the relay's binaryen
   (wasm-opt v108) re-caps the wasm table's max (min==max).
2. `deploy.sh` frontend build: `RUSTFLAGS="-C link-arg=--growable-table"` — makes the closure/externref
   table growable.

If wasm-opt runs (metadata dropped — a merge did this) OR the flag is missing, the table is capped and
boot dies at instantiation: `WebAssembly.Table.grow(): failed to grow table by N` /
`Table.set(): function-typed object must be null` in `__wbindgen_init_externref_table`. Keep
reference-types ON (default); wasm-bindgen 0.2.12x needs its externref table (you cannot `-reference-types`
— the CLI then errors `failed to find __wbindgen_externref_table_dealloc`). Pin `wasm-bindgen` to the
relay's `wasm-bindgen` CLI version so glue+wasm match.

## 4. Verification — the deploy smoke is NOT enough; boot the wasm in a real browser
`deploy/smoke.sh` only checks the served wasm's *imports* + the bridge join — it does NOT instantiate the
module, so it passed a build that crashed at boot. ALWAYS also run the browser smoke after a frontend
deploy: `cd spacegame-wasm/tools/account-smoke && DISPLAY=:1 node smoke.mjs` — it asserts the wasm
actually instantiates (GPU adapter / `player id` logged) and the pilot auto-login works. Use the real GPU
(`DISPLAY=:1`, ANGLE-GL); SwiftShader can't init wgpu. The version banner `SPACEGAME build <hash>` in the
console must match the deploy's `cache-bust version stamped` line.

## 5. ce-iam identity — use the SDK, don't hand-roll or bypass
Pilots/identity go through `@ce-net/iam` `Identities` (encrypted vault, libp2p keypair, node vouch). Do
NOT reimplement storage or strip the ce-iam dependency to dodge a bug. The Vault is a SECRETS vault
(`put`/`getString`/`listSecrets`/`deleteSecret`) — it has no generic `.list()`.

## 6. boot.js version stamp
`deploy.sh` replaces **every** `__SGV__`. Never reference that literal token in logic (e.g. a dev-check)
— it gets rewritten too. Detect the unstamped placeholder by prefix (`V.startsWith("__")`).
