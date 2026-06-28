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

### 1.6 Ship editor (compose parts → fit → hot reload)

The authoring half of the build system, so designing a ship is hands-on, not hand-written JSON.

- **`src/editor.rs`** — `ShipEditor`, a pure, tested model: place/remove/rotate parts on an integer
  grid (`PlacedPart`), `to_blueprint()` lowers the grid into a `Blueprint` (one placement per part at
  `cell * grid`), and `preview(catalog)` returns the live `Loadout` so a UI shows the ship's stats
  (mass, top speed, agility, hull, guns, cargo, problems) **as you build**. It is plain serde JSON
  (`to_json`/`from_json`) and ships a flyable `starter()` design.
- **Custom fit** — `ClientMsg::FitDesign { design }` carries a whole composed `Blueprint` (not just a
  named ruleset id). `Sim::fit_design` resolves it against the live parts catalogue (`resolve_design`),
  bounds it (`editor::MAX_PARTS`, also after expansion), and refits the ship only if it is flyable — so
  a malformed or brick design can never strand the player. Deterministic across replicas.
- **Hot reload (native)** — a design is a file (`SPACEGAME_SHIP`, default
  `<data-dir>/spacegame-ship.json`). The native client seeds a starter on first run, then watches the
  file's mtime every frame (B forces a reload): edit-and-save and it parses the `ShipEditor`, prints the
  previewed loadout as build feedback, and broadcasts `FitDesign` so the ship rebuilds live. The clean,
  literal "easy to hot reload" loop: save the file, watch your ship change.

### 1.7 Tests

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

Build system / editor
- **Ship editor: model + native hot-reload shipped; richer UIs remain.** `src/editor.rs` + the native
  file-watch deliver compose-parts → preview → fit-live. Still to do: an **interactive in-game editor**
  (drag/click parts on a canvas) — native could render the `ShipEditor` grid and bind mouse placement;
  and a **browser editor** for `spacegame-wasm` (it has no filesystem, so drive `ShipEditor` from a
  canvas/localStorage/textarea and send `FitDesign`). The wasm/render fixed presets (`ship_slots()`)
  still work alongside.
- **Custom hull is not rendered as its design.** A `FitDesign` ship flies correctly (its `Loadout`
  fields persist) but draws with the default/procgen shape, because only the loadout — not the part
  layout — reaches the renderer. To draw the actual design, the host would need to expose the resolved
  craft shape (e.g. `craft_to_shape_blueprint`) to clients, or the client keeps its own local design.
- **Fitting is free.** Consider gating `fit_blueprint`/`fit_design` on a cost in faction `alloys` (a
  real sink for the mining loop) and/or only at a station/when stationary.
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
