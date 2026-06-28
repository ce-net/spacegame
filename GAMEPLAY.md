# Spacegame — gameplay pass (build system, mining, AI, physics)

This document records the gameplay pass added on 2026-06-28: a **real ship build system**, **asteroid
mining into alloy loot**, **objective-driven AI**, and **more physics-based movement** — across the SDK
and all three clients — plus what is left to do.

It is meant to be read alongside `SYSTEMS.md` (the systems overview) and the per-module rustdoc.

---

## 1. What shipped

### 1.1 Build system — a blueprint becomes the ship you fly

The SDK already had a data-driven, recursive parts/blueprint kernel (`src/build.rs`): an `ObjectDef`
is a part (a shape + mass/hp + an `ObjectStats` bag — thrust, power, weapon mount, capacity, armour…),
a `Blueprint` is a parametric, nestable tree of `Placement`s, and `resolve_blueprint` expands a design
into a `ResolvedCraft` (aggregate mass, hp, thrust, net power, weapon mounts, cargo). Before this pass
that was used **only for visuals** (the procedural ship shapes).

New module **`src/shipyard.rs`** turns a `ResolvedCraft` into the concrete gameplay stats a ship flies
with — a `Loadout`:

- **Thrust-to-weight drives handling** (`a = F/m`): `speed_mult` scales with `sqrt(twr)`, `thrust_mult`
  (acceleration/agility) scales linearly with `twr`, both relative to `REFERENCE_TWR`. A light hull with
  a big engine is a darting interceptor; a heavy freighter with the same engine is a slow barge.
- **Validity**: a design with no command centre or no engine is **unflyable** and rejected; a design
  whose `net_power < 0` is flagged `Underpowered` and browns out (speed/agility penalised, no spare
  capacitor charge).
- Parts also set `max_hp` (+ armour), `guns`/`weapons` (from weapon mounts), `cargo` (from tanks/
  containers), `shield` (from upgrade boost) and `energy` (from surplus power).

Wiring:

- `Ship` gained `mass`, `speed_mult`, `thrust_mult`, `cargo`, `hull` (the blueprint id). `max_speed_t`
  and `accel_t` now multiply in the build multipliers, so a built ship genuinely flies differently.
- `Ship::apply_loadout(&Loadout, hull)` re-fits a ship to a design (heals to the new max, unions in the
  design's weapons, keeps minerals/kills/tech/position).
- `Sim::fit_blueprint(id, blueprint)` resolves a ruleset blueprint → `Loadout` → applies it **only if
  flyable** (you can never strand yourself in a brick). Authoritative and deterministic.
- New wire message **`ClientMsg::Fit { blueprint }`** (handled in `room::apply_client_msg`).
- Three builtin flyable ship designs in `ruleset.rs::builtin_blueprints`: **`interceptor`** (light,
  fast), **`gunship`** (heavy, twin turrets + missile), **`hauler`** (slow, big cargo). `scout` is also
  flyable.
- `snapshot.rs` bumped to **v5**: the new ship build fields ride failover/transit (all `serde(default)`,
  so v4 snapshots still decode).

### 1.2 Mining — gradual, shatters into alloy nuggets you collect

Mining used to be instant (overlap a rock → minerals appear). Now:

- Asteroids have hull (the deterministic `Rock.hp`). A ship in mining reach **grinds the rock down**
  `mine_rate` hp/tick (a tunable), tracked in `Sim::rock_dmg`. Direct weapon fire also chips rocks
  (`rock_hit` in `advance_bullets`; missiles fly over — they are anti-ship ordnance).
- When a rock's hull hits zero it **shatters** (`shatter_rock`) into a fan of **`PickupKind::Alloy`**
  nuggets that fly outward, summing to the rock's value.
- Nuggets **magnetise and glide** to the nearest ship (`tick_pickups`): within `magnet_radius` they
  accelerate toward the ship (pull strengthens as they close) up to `magnet_max_speed`, and snap in on
  contact. Alloy is collectible by **any** ship (a drone hauls ore too); other powerups stay player-only.
- Collected alloy banks to the ship's haul (`minerals`) and the faction's `resources.alloys`
  (`faction::deposit_alloys`).
- New `Tunables`: `mine_rate`, `magnet_radius`, `magnet_accel`, `magnet_max_speed` (hot-reloadable).
- `rock_dmg` rides the v5 failover snapshot; it is GC'd and (like `mined`) intentionally **not** in
  `state_hash` (rock field state is derived identically on every replica).

### 1.3 AI — objective-driven brains

New module **`src/ai.rs`**: NPCs hold an `Objective` (`Idle`/`Mine`/`Engage`/`Retreat`/`Escort`/`Move`)
they **commit** to, instead of recomputing a fresh goal every tick (which made them dither).

- `next_objective(role, command, current, senses)` is a pure policy: a fighter acquires and **holds** a
  target (hysteresis out to `engage_r * ENGAGE_KEEP`), a wounded fighter **retreats** until it recovers,
  a drone **strips a vein** until it is dry.
- `lead_target` solves the intercept quadratic so fighters **lead** a moving target using their weapon's
  muzzle speed.
- `sim.rs` gathers each NPC's `Senses` (nearest enemy with velocity, nearest mineable rock cell), calls
  the policy, persists the chosen objective on the ship (transient), and steers to it. The old
  role-based `npc_goal` is replaced by `npc_decide`/`objective_goal`.

### 1.4 Physics — more momentum, less arcade

- `DAMPING` raised `0.965 → 0.984`: ships coast on their momentum (you fly by managing inertia) instead
  of stopping dead when you release thrust.
- `resolve_ship_collisions` is now **mass-aware**: the separation impulse is split by inverse mass, so a
  heavy hauler bulls through light drones and momentum is conserved.
- Built-ship `mass` (from the parts) feeds both the collision response and, via thrust-to-weight, the
  acceleration — so the build system and the physics reinforce each other.

### 1.5 Clients consume it

- **`spacegame-render`** (shared scene + view-model): `pickup_color`/the pickup loop render **alloy
  nuggets (kind 5)** as a bright metallic spark; `ship_slots()` + `InputState.fit_slot` emit
  `ClientMsg::Fit` (mirrors `build_slots`/`fleet_orders`).
- **`spacegame-wasm`**: **Shift+1..4** builds & fits a ship (Interceptor/Gunship/Hauler/Scout); the HUD
  gains a `SHIPYARD` line next to `BUILD`. (Alloy count was already shown in the fleet HUD line.)
- **`spacegame-native`**: **Shift+1..4** latches the same fit. Plain digits still buy tech on both.

### 1.6 Tests

- `shipyard.rs` and `ai.rs` are fully unit-tested (loadout derivation, validity, lighter-is-faster;
  objective transitions, retreat/recover, vein commitment, target leading).
- `sim.rs` integration tests: build-a-lighter-ship-is-faster, reject unflyable/unknown fit, mining is
  gradual then shatters into collectible alloy, alloy magnetises to a ship, a marauder locks onto a
  player.
- Whole repo green: **248 mesh lib + 239 pure lib + 23 integration** tests; `cargo check` (mesh) clean;
  clients build (wasm32 + native).
- Re-fixed two test-only breakages that shipped on `origin/development` (a `ClientMsg::Join` cap arg and
  a shield-regen idle-expiry).

---

## 2. What was pushed (origin/development, author Leif Rydenfalk)

| Repo | Commit | Contents |
|---|---|---|
| `spacegame` | `9eda512` | gameplay (shipyard/ai/sim/ruleset/wire/room/snapshot/faction) + atlas's entity-anchored authority groundwork |
| `spacegame-render` | `9e644dc` | HDR raster pipeline (vega) + alloy-nugget render + `ship_slots`/`fit_slot` |
| `spacegame-native` | `ec26af7` | Shift+digit fit binding |
| `spacegame-wasm` | `6a1e0e9` | fit UI + HUD + HDR GPU render path (vega); dropped the local render `[patch]` |

`/target` `.gitignore`s were added to render/native/wasm (render needed one force-push to drop
accidentally-committed build artifacts). Clients git-dep `spacegame`/`spacegame-render` from
`development`, so they pick up the SDK + render directly from GitHub (no local `[patch]`).

---

## 3. Remainders / follow-ups

Build system
- **No in-client ship editor yet.** The clients expose four fixed presets (`ship_slots()`); the SDK
  already supports arbitrary nested blueprints, so a real "place parts → blueprint → Fit" UI is the
  natural next step. `ship_slots()` is hardcoded in `spacegame-render`; consider a data-driven list
  (e.g. a `Ruleset::fittable_ships()` helper that returns blueprints whose `Loadout` is flyable).
- **Fitting is free.** Consider gating `fit_blueprint` on a cost in faction `alloys` (a real sink for
  the mining loop) and/or only at a station/when stationary.
- **Native has no SHIPYARD HUD line** (only wasm does) — add the hint to `spacegame-native`'s HUD text.

Mining / alloy
- **No velocity in `PickupView`**, so clients render position movement but cannot draw a directional
  *streak* when a nugget is magnetised. Adding `vx/vy` (quantised) to `PickupView` + `room::pickup_views`
  would let the renderer streak/stretch alloys along their motion (the "glinting nuggets that streak"
  idea). Fold nothing new into `state_hash` (position already covers it).
- **Asteroids show no partial-damage state** (only full vs depleted via the `depleted` set). Optionally
  surface `rock_dmg` (or a coarse crack level) so a half-mined rock looks chipped.
- **No mining feedback FX** (sparks/sound) on the client when grinding a rock.

AI
- Objective brains are solid but there is **no group/formation tactics** (focus-fire, flanking); escorts
  hold a ring slot. Target leading ignores projectile gravity in hazard sectors.
- Marauder difficulty/waves are still the old tunables — could scale with player build strength.

Physics
- The damping change is global; **per-ship handling** (e.g. a heavier ship turning slower) is only
  partially expressed (turn rate is not yet mass-scaled). Collisions translate but do not impart spin.

Cooperation / cross-cutting
- **atlas** still has open work integrating entity-anchored domains into the clients (the clients calling
  `interest_sectors`/`claim_sticky`) and `src/client.rs`/`DOMAINS.md` were being edited at push time —
  left untouched.
- **vega**'s browser HDR path is `cargo`/naga-validated but **not browser-verified** (`trunk serve` to
  eyeball).
- Repos emit `LF → CRLF` warnings on commit (a host autocrlf setting). Worth normalising with a
  `.gitattributes` (`* text=auto eol=lf`) to avoid line-ending churn, given gitsync history.
