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

- **Galaxy map — LIVE at https://spa.ce-net.com/map/ (2026-06-27).** Spacegame-specific, so served under
  the spacegame app (the bare `map.ce-net.com` is reserved for a future ce-net-wide donator map).
  `galaxymap-web` (folds `galaxymap.rs`'s
  model) published via ce-serve (`deploy/deploy.sh map`): every cell, **which node hosts it** (the server
  instances contributing compute), per-cell player count + load heat, and live cell-splits, fed over the
  mesh bridge. Shows host instances + player COUNTS; individual player dots are a possible enhancement.

## Players host their region — DONE (2026-06-27)

The browser now runs the full authoritative `Sim` itself (`spacegame-wasm`: a `replica::Replica` advanced
to the shared wall-clock tick, rendered from directly), exchanging tick-tagged inputs on the sector `/in`
topic with whoever else is present. **YOUR node is one of the servers**, so your ship is zero-RTT and your
bullets fire from your real position; the quorum merge keeps you honest against the other players. There is
no relay game authority any more (see below). This is the change that makes the merge visible and kills the
teleport for good.

- **Cross-sector hand-off — DONE.** `replica::Replica::rehome_local_player` drains the sim's seamless
  transit each tick: when the local player crosses a sector edge, the replica re-homes onto the
  destination sector (a fresh `Sim::for_sector` at the same shared tick, carrying the ship in with full
  state) and the client re-points its `/in` subscription there. Without this the ship was deleted at the
  edge and the next input re-spawned it at the sector centre — that WAS the "teleport back to centre" bug,
  and with no relay there is no host-side `readmit_coords` to mask it. Tested:
  `replica::tests::local_player_rehomes_across_a_sector_edge_instead_of_teleporting_to_centre` — the
  edge-crossing test that was missing (every prior transit test exercised the `Sim` primitive or the
  relay host loop, never a `Replica` flown to an edge, which is exactly why it shipped).

## The relay: TRANSPORT + a warm genesis SEED (not an authority)

The relay is no longer a game server. Its roles:

- **Transport (essential).** libp2p relay / NAT traversal / DCUtR + the `/mesh-bridge` wss + ce-serve
  static hosting of the wasm bundle — this is how players reach each other over the global internet.
  (Future: many relays; transport itself decentralizes.)
- **Genesis seed (lightweight).** One non-authoritative `spacegame host` replica pinned to the genesis
  ring (`spacegame-seed` service) keeps the origin region warm so the first player always has a peer to
  bootstrap/merge against. It is one vote in the quorum, outvoted by the player majority. The old
  planet-scale `spacegame node` (gateway + leaderless controller + autoscale) is no longer deployed.

**Empty region ⇒ state dropped** still holds away from genesis: no players hosting it ⇒ no compute, no
storage ⇒ it's gone. Near genesis the seed keeps it warm — by deliberate choice (STATE-MODEL.md).

## What remains

- **Joining-state adoption.** A player entering a populated region currently starts a fresh `Sim` for it
  and converges via the merge; adopting the quorum's latest snapshot on entry (a `SnapshotAnnounce` the
  region already publishes) would make the first second seamless. Builds on `replication::{ReplicaSet,
  ReplicationPlan}` + `lib::adopt_latest_snapshot`.
- **Native client self-hosts.** The native desktop client still renders the seed's `/state` as a thin
  client; porting it to run its own `Replica` (like the browser) makes native players servers too.

No declared positions. No trusted server. The world is whatever the players present compute and agree on.
