# Status — VISION.md #21 (seamless world, mining, AI, building, trust)

Maps each instruction in the latest brief to what is implemented now and what remains. Items are kept
small and pointed so the next session can pick any one up.

| # | Ask | Status | Where |
|---|---|---|---|
| 1 | Seamless chunks/sections, **no transitions**, your server **follows you** | **Done (model + tests)**, host-loop port pending | `src/world.rs`, `docs/world.md` |
| 2 | Earth-sized world, **play as a human** (range + precision) | **Done (floating-origin coords)**, sim still sector-local | `world::Wpos`/`Frame` |
| 3 | Chunking **follows player/ships/factions**, simulate-for-others, scale by each-player-self | **Done (bubbles)** | `world::Bubble`/`World` |
| 4 | **Recursive AABB** following entities, later **broad-phase physics** | **Done (`BubbleTree`)**; nest per-cell trees next | `world::BubbleTree` |
| 5 | **Procedural asteroids → mine → "alloys"**, not instant, **glide & collect**, satisfying, **physics-based** | **Done (authoritative + tested)**; renderer hookup pending | `sim::Loot`, `Sim::advance_loot`, `SYSTEMS.md §6` |
| 6 | `"By playing this you donate your compute to science!"` | **Done** | `README.md`, CLI `--help` (`src/main.rs`) |
| 7 | **Reporting system** | TODO | — |
| 8 | **Brutal karma & trust**, rebuildable via **trana** over time/energy | TODO (design) | integrate `../trana` |
| 9 | **Smarter AI** with objectives & goals | TODO | `sim::drive_npcs`, `faction::AutoPolicy` |
| 10 | **Real build system** — ships & blueprints | Partial (already substantial) | `src/build.rs`, `src/shapedef.rs`, `src/procgen.rs` |

## Done this pass (compiles, `cargo test` green: 166 tests, no regressions)

### Seamless player-following world — `src/world.rs` (new, pure, 10 tests)
- `Wpos` (f64 canonical) + `Frame` floating-origin: planet-scale range at sub-mm precision; exact,
  lattice-snapped rebasing that keeps deterministic `f32` replicas bit-identical.
- `Bubble` = a player's authority footprint that follows ship+fleet+faction; `World` answers
  `authority_for` / `interest` / `handoff` (dead-band, no flapping) — the seamless replacements for the
  fixed sector grid's hash + 3×3 neighbours + edge transit.
- `BubbleTree` = recursive f64 AABB broad-phase over bubbles (brute-force completeness test).
- Full rationale + migration plan in `docs/world.md`.

### Physics-based mining + magnetic alloy loot — `sim.rs` (+ wire/room/snapshot, 3 tests)
- Mining sheds `Loot` nuggets (deterministic, hashed spawn) instead of instant credit.
- `Sim::advance_loot`: nearest-ship magnet via the per-tick AABB tree, glide + speed cap + damping,
  collect-on-contact crediting ship minerals + faction; lifetime/bounds GC.
- Folded into `state_hash` (anti-cheat), persisted in `SectorSnapshot` (failover), carried as
  `wire::LootView` (`#[serde(default)]`).

## Reconciliation pass (2026-06-28): all four repos now compile together

The clients (`spacegame-render`, `-native`, `-wasm`) had drifted **ahead** of the SDK wire — a partial
mesh-sync merge brought newer client code without the matching `wire.rs`/`sim.rs`. Extended the SDK to
the clients' contract so the whole set builds:

- `wire::ShipView` += `shield/max_shield/energy/max_energy/effects/input_ack` (all `#[serde(default)]`).
- `wire::ClientMsg::Join` += `cap: Option<String>` (ce-iam vouch; accepted + ignored by this SDK).
- `wire::Snapshot` += `mines`/`pickups` (+ `MineView`/`PickupView`).
- New netcode: `wire::{ClientPacket, SeqMsg, TaggedInput}` + `room::apply_client_packet` (reliable,
  contiguous-ack input path); `lib.rs` host loop decodes `ClientPacket` with a `ClientMsg` fallback.
- New **`src/replica.rs`** — the co-authoritative lockstep `Replica` the native/wasm clients run
  (`tick_at`, `TickInput`, `INPUT_DELAY`, `schedule`/`advance_to`/`apply_local_now`/`rehome_local_player`),
  with a determinism test (two replicas + same inputs in different orders → identical `state_hash`).
- **Loot is rendered** end to end: `spacegame-render` `Fx.loot` + `scene.rs` draws glinting gold
  nuggets that streak when magnetised; demo snapshot seeded so the screenshot tool shows them.

Builds verified: SDK + render + native (host target), wasm on `wasm32-unknown-unknown`; **170 SDK tests
green**, mesh-feature build green.

Honest gaps (wire-present, not yet simulated): shields, energy, status effects, mines, pickups emit
neutral/empty values — the systems aren't in the `Sim` yet. The loot system IS fully simulated.

## Notes for the next session
- **Host-loop port (world.md §6)** is the highest-leverage next step: announce per-host bubbles on a
  `…/bubble` topic, maintain a `World`, drive interest/hand-off from it.
- **Simulate the wire-present systems** (shields/energy/effects) in `Sim` so the HUD bars are real, and
  spawn mines/pickups so those views carry data.
- **Trust/karma + reporting** should reuse `../trana` (`trana_core::karma` fuses social karma with
  on-chain compute trust) rather than a bespoke system — exactly the "you still have access to trana so
  you can build it up again" hook.
