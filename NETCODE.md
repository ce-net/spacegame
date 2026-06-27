# Spacegame netcode — replicated authority (the target, replacing client prediction)

Leif's directive (verbatim in `STATE-MODEL.md`, 2026-06-27):

> Why do we even "predict" at all? the architecture should be we run the full backend server on our local
> mac AND relay AND on each other player for instant feedback and then merge everything - but for this we
> need to first properly sync and make sure the server ALWAYS gets all our inputs and stays in sync and rely
> on local mac backend servers for player inputs and movement with proper auto cheat merging of server
> states properly. This is very advanced. I hate sector clamping - hte sectors should ADAPT to players and
> other servers should automatically take over.

This file is the plan that direction defines. It supersedes "client-side prediction" as the goal.

---

## Why we predict today (and why Leif is right that we shouldn't have to)

Today there is ONE authoritative host per sector (often the relay). The browser is a thin client: it sends
inputs and renders snapshots that are a full round-trip old. To not feel laggy, the client re-runs the
movement math locally and *guesses* ahead of the authority — `spacegame_render::Game::predict` — then
`reconcile()` snaps back to the server when the guess diverges past `SNAP_DIST`.

Prediction is a **patch over the wrong topology**: the authority is far away, so we guess. Every failure
mode flows from that guess being wrong:

- **The teleport-back bug.** Solo, with no neighbour host, the server pins you at the sector edge
  (`readmit_coords`) while your local guess flies on. The gap exceeds `SNAP_DIST` and you get yanked
  backward toward where the server still thinks you are. The server "doesn't trust that you moved" because
  **it is the authority and you are not** — your inputs/movement aren't authoritative anywhere near you.
- **Rubber-banding / desync** generally: any time the guess and the remote authority disagree.

If the authority for you runs **on your own machine**, your latency to it is ~0, there is nothing to guess,
and there is nothing to snap. That is the fix. Prediction goes away because it is no longer needed.

## The target: replicated authority + merge (no single far-away server, no prediction, no clamping)

1. **The full authoritative sim runs in multiple places at once** for a region: your Mac, the relay, and
   each nearby player's node. Not a thin client + one server — *N authoritative replicas*.
2. **Every replica ingests every input.** Your inputs are authoritative on your own replica immediately
   (zero delay, real movement — not a guess), and are reliably broadcast to all other replicas.
3. **Replicas merge.** Because the sim is deterministic, replicas fed the same inputs converge. Where they
   diverge (latency, drops, or a cheating replica), an **anti-cheat merge** reconciles them into one agreed
   state — quorum/authority rules decide whose version of a contested entity wins.
4. **Sectors ADAPT to players; hosting auto-migrates.** No fixed grid you can clamp against. The region of
   authority follows where players are; as a player moves, the nearest capable node **automatically takes
   over** hosting its slice and hands off cleanly. Empty space is hosted by nobody (population-driven
   persistence, already in `STATE-MODEL.md`).

The result Leif wants: instant feedback (local authority), always-in-sync (input broadcast + merge),
self-cheat-resistant (merge rules), and borderless (adaptive hosting + handoff). No prediction. No clamp.

## Why it's hard (the honest list)

- **Input sync is the foundation and must be solid first.** Every input must reach every replica, in a
  deterministic order, with no silent drops, or the replicas diverge unboundedly. Needs sequence numbers,
  acks/retransmit or an ordered log, and a tie-broken total order across inputs from different players.
- **Deterministic convergence.** The sim is already deterministic given identical ordered inputs; we must
  keep it that way (no wall-clock, no map iteration order, no float nondeterminism across architectures —
  x86 relay vs arm Mac vs wasm browser is a real risk and must be tested).
- **Merge / anti-cheat is the genuinely advanced part.** When two replicas disagree about a contested
  entity (a hit, a pickup, a position), we need agreed rules for whose state is authoritative — without a
  central server. Likely: per-entity ownership (your ship is authored by your replica) + quorum hash
  checkpoints (the existing `replication.rs` / state-proof machinery) to detect and exclude a lying replica.
- **Adaptive hosting + handoff** across moving players without a fixed sector grid, while keeping the
  minimum-K replication guarantee so a node leaving never loses state.

## Migration path (staged — each stage is shippable and testable, no half-states)

- **Stage 0 — input sync (PREREQUISITE, do first).** Make every input reach the authority (and, later,
  every replica) reliably and in order: sequence-numbered inputs, server acks the last applied seq, client
  resends gaps. Add a live test that asserts a burst of inputs all register server-side. This alone makes
  the current single-authority model stop "losing" your actions — the root of the teleport.
- **Stage 1 — your node is authoritative for your ship.** Your local node hosts the cell covering you;
  your ship's movement is authored locally (zero-RTT), broadcast to others. Removes prediction for the
  local player entirely — there is no remote authority to snap to.
- **Stage 2 — multi-replica + merge.** Relay + nearby players also run the cell; replicas exchange inputs
  and periodically agree state via quorum hash (extend `replication.rs`). Anti-cheat exclusion of a
  divergent replica.
- **Stage 3 — adaptive hosting + handoff.** Authority region follows players; nearest node auto-takes-over
  and hands off as players move. Delete the sector clamp / edge-readmit path entirely.

Until Stage 1 lands, the teleport is a symptom of the missing local authority — NOT to be papered over with
client-side sector clamping (Leif: "I hate sector clamping").
