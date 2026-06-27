# Spacegame netcode — deterministic replicas + quorum state-merge (no trusted server)

Leif's directives are transcribed verbatim in `STATE-MODEL.md`. The binding ones for netcode:

> we must have proper state merging ! people will try to cheat when there are millions of players. the
> state merging and the one server per player goes hand in hand. We dont even need the relay backend
> server - the players machines can themselves be servers ... when there are no players the state is lost
> ... proves something: in ce-net apps scales on demand by having each user host a part of it.

> No authored bool please thats stupid

## The model (what we are building — and what we are NOT)

NOT this: client-side prediction reconciled against one far-away authority; and NOT a client that
**declares its own position** for a host to trust (the rejected `authored` bool / `self_state`). Declared
state is trivially cheatable — at a million players, someone WILL lie about where they are, what they hit,
what they own.

THIS: **deterministic simulation, replicated across the players present in a region, merged by quorum.**

- **No trusted authority, no required relay.** The players in a region ARE the servers. Each runs the full
  authoritative `Sim` for that region.
- **Movement is re-computed, never declared.** Every replica runs the SAME deterministic sim from the SAME
  ordered input log (Stage 0). Your own ship feels zero-RTT because YOUR node is one of the authorities —
  you are not predicting against a remote server, you are running the real sim locally and agreeing with
  the others.
- **Merge by quorum hash = anti-cheat.** Each checkpoint, every replica hashes its world state
  (`Sim::state_hash`) and publishes it. `replication::agree` finds the majority hash. A replica whose hash
  differs is out-voted and **merges** — it re-syncs to the agreed state. A cheater can't win: lying changes
  your hash, the honest majority out-votes you, and you are pulled back to the truth (or excluded). A
  single honest node is never overruled by a single liar (a 1-1 split is "no quorum", not an accusation).
- **Empty region ⇒ state dropped.** No players hosting it ⇒ no compute, no storage ⇒ it's gone. For the
  solo dev this means the world resets — which is the point: it proves ce-net apps scale on demand because
  each user brings the compute + storage for the slice they play.

## Status

- **Reliable, ordered input log — DONE + deployed (2026-06-27).** `wire::ClientPacket` (continuous input
  latest-wins + a reliable `SeqMsg` stream, acked via `ShipView.input_ack`). Every replica needs the same
  inputs in the same order; this is that substrate. (`room::InputSync`.)
- **Quorum state-merge — DONE (engine), 2026-06-27.** `Sim::state_hash` + `replication::{StateProof,
  agree, Agreement::verdict, Verdict}`. Replicas publish per-checkpoint hashes; `agree` finds the quorum;
  an out-voted replica computes `Verdict::ResyncTo(hash)` and `lib::merge_to_quorum_if_outvoted` fetches
  the snapshot whose `SnapshotAnnounce.hash` matches the agreed hash and adopts it — the actual merge,
  not a log line. Inert below 2 replicas (correctly: you can't cross-check a single host). Tested:
  `replication::tests::verdict_tells_an_outvoted_node_to_merge_to_the_quorum`.

- **Deterministic replica engine — DONE (engine), 2026-06-27.** `replica::Replica`: advances a `Sim`
  from a tick-tagged input log (`TickInput`), applying each tick's inputs in canonical `(player, seq)`
  order so packet arrival order can't change the result. Mesh-free, so the SAME engine runs in the
  browser (wasm), native, and relay — every participant is the same kind of replica. Tested:
  `replica::tests::two_replicas_from_the_same_inputs_reach_identical_state` (the load-bearing determinism
  guarantee) + rogue-input divergence + late-input rejection. This is the core that makes "everyone runs
  the server and they agree" possible.

## Live monitor

- **Galaxy map — LIVE at https://map.ce-net.com/ (2026-06-27).** `galaxymap-web` (folds `galaxymap.rs`'s
  model) published via ce-serve (`deploy/deploy.sh map`): every cell, **which node hosts it** (the server
  instances contributing compute), per-cell player count + load heat, and live cell-splits, fed over the
  mesh bridge. Shows host instances + player COUNTS; individual player dots are a possible enhancement.

## What remains (this is where the visible fixes live)

The merge engine is real but **inert in solo play**, because today only ONE node (the relay) hosts a
region — there's nothing to cross-check, and the lone host's view is still the only authority, so the
old divergence (the teleport, bullets firing from the host's stale copy of you) persists in solo. The
merge only does work once a region has ≥2 player-replicas. So the companion slice — the one Leif says goes
hand-in-hand — is next:

- **Players host their region (no relay authority).** Each player's own node runs the authoritative `Sim`
  replica for the region it's in; the relay is at most a bootstrap/rendezvous, never the source of truth.
  Then YOUR node authoritatively simulates YOUR ship from YOUR inputs (zero-RTT, bullets from your real
  position), and the merge keeps your node honest against the other players' replicas. This is the change
  that makes the merge visible and kills the teleport for good.
  - Open design fork (browser players): the authoritative replica runs in the browser's wasm `Sim` itself,
    or in the player's local native `ce` node, with replicas talking over the mesh bridge. The browser
    does not currently run the full `Sim` (only the render view-model), so this is the substantial lift.
- **Region hand-off as players move** (sectors adapt; nearest replica takes over) and **population-driven
  drop** (last player leaves ⇒ region released). Builds on `replication::{ReplicaSet, ReplicationPlan}`.

No declared positions. No trusted server. The world is whatever the players present compute and agree on.
