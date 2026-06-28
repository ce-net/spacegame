//! Deterministic replicated simulation — the unit each player's node runs so that **the players present
//! in a region ARE the servers** (see `NETCODE.md`). There is no trusted host: every replica advances
//! the SAME [`Sim`] from the SAME tick-tagged input log, so honest replicas compute bit-identical state,
//! and the periodic quorum hash ([`crate::sim::Sim::state_hash`] + [`crate::replication::agree`]) catches
//! a cheater or a desynced node — the odd one out merges to the agreed state.
//!
//! The hard requirement this module exists to guarantee is **determinism**: the result must depend ONLY
//! on the set of inputs and the tick each applies at — never on the order packets happened to arrive in,
//! nor on which node is simulating. That is what makes "everyone runs the server and they agree" possible.
//!
//! ## Tick-tagged inputs (the shared clock)
//! An input is scheduled to apply at a specific simulation `tick`, the SAME tick on every replica. A
//! node's own input is scheduled a few ticks in the future ([`INPUT_DELAY`]) so it has time to propagate
//! and land on every replica before that tick is simulated; the node broadcasts the same tag so peers
//! schedule it identically. Within a tick, inputs are applied in a canonical order — sorted by
//! `(player, seq)` — so two replicas that received the same inputs in different network order still
//! produce the same state.
//!
//! This module is mesh-free (it builds only on `sim`/`room`/`wire`), so the identical engine runs in the
//! browser (wasm), the native client, and the relay — every participant is the same kind of replica.

use std::collections::BTreeMap;

use crate::shard::SectorId;
use crate::sim::Sim;
use crate::wire::ClientMsg;

/// Ticks of input delay: a locally-generated input applies at `current_tick + INPUT_DELAY` on every
/// replica, giving the network time to deliver it everywhere before that tick is simulated. ~100 ms at
/// 60 Hz — the budget for an input to reach the other replicas in a region. (Your own ship can still feel
/// instant via a separate local-echo/prediction layer above this; the canonical scheduled input is what
/// every replica — including yours — agrees to simulate, so they never diverge.)
pub const INPUT_DELAY: u64 = 6;

/// Milliseconds per simulation tick (60 Hz) — the rate every replica advances at.
pub const TICK_MS: f64 = 1000.0 / 60.0;

/// A fixed epoch (ms since the Unix epoch) the shared tick clock counts from. Arbitrary, but it MUST be
/// the same constant on every replica so they all derive the same tick number from wall-clock time. This
/// is the "shared clock": with roughly NTP-synced wall clocks, every node agrees what tick it is now, so
/// a tick-tagged input lands on the same tick everywhere.
pub const TICK_EPOCH_MS: f64 = 1_700_000_000_000.0;

/// The canonical simulation tick for a wall-clock time (ms since the Unix epoch). Saturates at 0.
pub fn tick_at(now_ms: f64) -> u64 {
    let t = (now_ms - TICK_EPOCH_MS) / TICK_MS;
    if t < 0.0 { 0 } else { t as u64 }
}

/// One tick-tagged input: apply `msg` from `player` at simulation `tick` on every replica. `seq` is the
/// author's monotonic sequence (from [`crate::wire::ClientPacket`]), used only to order multiple inputs
/// from the same player within one tick — deterministically and identically on every replica.
#[derive(Debug, Clone, PartialEq)]
pub struct TickInput {
    pub tick: u64,
    pub player: String,
    pub seq: u64,
    pub msg: ClientMsg,
}

/// A deterministic replica of one region's simulation, driven by a tick-tagged input log.
pub struct Replica {
    sim: Sim,
    /// Inputs scheduled for a future tick: tick -> [(player, seq, msg)]. Drained as the sim reaches each
    /// tick. A `BTreeMap` so we can cheaply find/drop the entry for the exact tick being simulated.
    scheduled: BTreeMap<u64, Vec<(String, u64, ClientMsg)>>,
}

impl Replica {
    /// Wrap an initial `sim` (a fresh world, or one restored from a replicated snapshot). All replicas of
    /// a region MUST start from the same initial state for their inputs to converge.
    pub fn new(sim: Sim) -> Self {
        Replica { sim, scheduled: BTreeMap::new() }
    }

    /// The next tick this replica will simulate.
    pub fn tick(&self) -> u64 {
        self.sim.tick
    }

    /// The authoritative simulation (read-only) — render from this; it is the same on every honest replica.
    pub fn sim(&self) -> &Sim {
        &self.sim
    }

    /// Which sector this replica is currently simulating. Changes when the local player crosses a sector
    /// edge (see [`rehome_local_player`](Self::rehome_local_player)).
    pub fn sector(&self) -> SectorId {
        self.sim.sector
    }

    /// **Players ARE the server — including across sector edges.** Each player's node runs the full sim
    /// for the region it is in; when the local player crosses an edge, that node must take over the
    /// region it moved INTO rather than letting the ship fall off the world. Call this once per tick,
    /// right after [`advance_to`](Self::advance_to), with the local player's id.
    ///
    /// The sim's seamless transit (see [`Sim::tick`]) removes any ship that crossed the edge and queues
    /// it in `transit_out` (its `x/y` already wrapped into the neighbour's local coordinates). Here we
    /// drain that queue:
    /// * if the LOCAL player crossed, we re-home this replica onto the destination sector — a fresh
    ///   [`Sim::for_sector`] (its own hazards / NPC factions) at the SAME shared tick, carrying the
    ///   player's ship in with its full state — and return the new [`SectorId`] so the caller can move
    ///   its `/in` subscription to the new region (where its peers, if any, already publish);
    /// * any OTHER ship that crossed is simply released — the replicas hosting its destination own it now.
    ///
    /// Returns `Some(new_sector)` iff the local player moved (so the caller re-points its subscriptions),
    /// `None` otherwise. With NO relay authority this is the ONLY place a cross-edge ship is preserved —
    /// without it the ship vanishes and the next input auto-rejoins it at the sector centre (the
    /// "teleport back to centre" bug).
    pub fn rehome_local_player(&mut self, me: &str) -> Option<SectorId> {
        let transits = self.sim.take_transits();
        let mut moved = None;
        for t in transits {
            if t.ship.id == me {
                let tick = self.sim.tick;
                let mut next = Sim::for_sector(t.to, self.sim.rules.clone());
                next.tick = tick;
                next.accept_transit(t.ship);
                self.sim = next;
                // The old region's queued inputs do not apply in the new one; start its log clean.
                self.scheduled.clear();
                moved = Some(t.to);
            }
            // Non-local ships that left are not re-admitted here: their destination's replicas host them.
        }
        moved
    }

    /// This replica's deterministic state hash — its claim about the world, for the quorum merge.
    pub fn state_hash(&self) -> u64 {
        self.sim.state_hash()
    }

    /// Drain the ships that crossed this replica's sector edge this tick (the other end of
    /// [`accept_transit`](Self::accept_transit)). The [`SectorHost`] routes each to the replica that
    /// hosts the destination sector, so a ship slides between two warm sectors with no `Sim` rebuild.
    pub fn drain_transits(&mut self) -> Vec<crate::sim::Transit> {
        self.sim.take_transits()
    }

    /// Admit a ship handed off from a neighbouring sector's replica, carrying its full persistent state
    /// (already in this sector's local coordinates). Used by [`SectorHost::step_transits`].
    pub fn accept_transit(&mut self, ship: crate::snapshot::ShipSnap) {
        self.sim.accept_transit(ship);
    }

    /// The **entity-anchored authority bubbles** for everything this replica simulates — the local
    /// player, their fleet, and any NPC factions present — in the absolute world frame, each grown by
    /// `view_radius`. See [`crate::domain`]: a domain follows its owner, so the partition moves with the
    /// players instead of being a grid they cross.
    pub fn domain_field(&self, view_radius: f64) -> crate::domain::DomainField {
        crate::domain::DomainField::from_sim(&self.sim, view_radius)
    }

    /// The set of sectors the local player's interest bubble currently overlaps — the **seamless
    /// interest set** that replaces [`SectorId`]'s fixed 8-neighbour ring. It is **one** sector when the
    /// player is mid-sector, **two** on an edge, **four** on a corner, and it grows with the player's
    /// fleet; the caller subscribes to exactly these regions' `/in` + `/state` topics. Because the bubble
    /// slides, crossing a seam *adds* the neighbour before you reach it and *drops* the old one once your
    /// bubble clears it — there is no discrete transition to feel. Falls back to just the current sector
    /// if the player isn't present yet.
    pub fn interest_sectors(&self, me: &str, view_radius: f64) -> std::collections::BTreeSet<SectorId> {
        let field = self.domain_field(view_radius);
        match field.get(me) {
            Some(d) => d.interest.sectors().into_iter().collect(),
            None => std::iter::once(self.sim.sector).collect(),
        }
    }

    /// Schedule a tick-tagged input received from the region's input stream (the network). An input for a
    /// tick already simulated is dropped — it is too late to apply deterministically; the quorum merge is
    /// what repairs a replica that fell behind, not a late-applied input that would itself cause a divergence.
    /// Returns `false` if the input was too late.
    pub fn schedule(&mut self, input: TickInput) -> bool {
        if input.tick < self.sim.tick {
            return false;
        }
        self.scheduled.entry(input.tick).or_default().push((input.player, input.seq, input.msg));
        true
    }

    /// Schedule one of THIS node's inputs at the canonical future tick (`current + INPUT_DELAY`) and return
    /// that tick, so the caller broadcasts the SAME `(tick, player, seq, msg)` to the other replicas and
    /// everyone — including us — applies it at exactly that tick.
    pub fn schedule_local(&mut self, player: &str, seq: u64, msg: ClientMsg) -> u64 {
        let at = self.sim.tick + INPUT_DELAY;
        self.scheduled.entry(at).or_default().push((player.to_string(), seq, msg));
        at
    }

    /// Apply one of THIS node's own inputs to the local sim IMMEDIATELY (zero input delay for your own
    /// ship). Pair it with broadcasting the same input tick-tagged at `tick + INPUT_DELAY` so the OTHER
    /// replicas apply it deterministically; your screen shows your ship "now", everyone else sees it a few
    /// ticks behind — the standard, correct trade. No-op concerns: `apply_client_msg` auto-joins an unknown
    /// player, so this also creates your ship on first input.
    pub fn apply_local_now(&mut self, player: &str, msg: ClientMsg) {
        crate::room::apply_client_msg(&mut self.sim, player, msg);
    }

    /// Set this replica's tick directly (e.g. to the shared wall-clock tick at creation, or after adopting
    /// a snapshot), so `advance_to(tick_at(now))` only steps the elapsed ticks rather than from zero.
    pub fn set_tick(&mut self, tick: u64) {
        self.sim.tick = tick;
    }

    /// Advance the simulation up to (but not including) `target`, applying each tick's scheduled inputs in
    /// canonical `(player, seq)` order before stepping that tick. Deterministic: given the same scheduled
    /// inputs, every replica ends in the same state regardless of arrival order or host. No-op if already
    /// at or past `target`.
    pub fn advance_to(&mut self, target: u64) {
        // A long stall (backgrounded tab, or first frame against the wall-clock tick) must not spin
        // through millions of ticks. Cap the catch-up; if we're further behind than that, jump the clock
        // forward (the gap is a desync the quorum merge / a fresh snapshot repairs) and simulate the cap.
        const MAX_CATCHUP: u64 = 12;
        if target.saturating_sub(self.sim.tick) > MAX_CATCHUP {
            self.scheduled.retain(|&t, _| t >= target.saturating_sub(MAX_CATCHUP));
            self.sim.tick = target.saturating_sub(MAX_CATCHUP);
        }
        while self.sim.tick < target {
            let t = self.sim.tick;
            if let Some(mut inputs) = self.scheduled.remove(&t) {
                // Canonical order so packet arrival order can't change the result.
                inputs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                for (player, _seq, msg) in inputs {
                    crate::room::apply_client_msg(&mut self.sim, &player, msg);
                }
            }
            self.sim.tick(1.0);
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// SectorHost — simulate (and therefore render) the WHOLE interest bubble, not one sector.
// ---------------------------------------------------------------------------------------------------

/// The set of sector cells a single player's node simulates at once — its **interest bubble** (the
/// sectors the entity-anchored recursive AABB overlaps: one mid-sector, two on an edge, four on a
/// corner, growing with the fleet). This is what makes crossing a seam **seamless**.
///
/// The old single-`Replica` client ran exactly one sector and, on crossing an edge, threw the whole
/// surrounding world away and built a fresh [`Sim::for_sector`] carrying only the player's ship
/// ([`Replica::rehome_local_player`]). Every other ship, asteroid, NPC and bullet popped out and a
/// freshly-generated set popped in — the "horrible switch". A `SectorHost` instead keeps a deterministic
/// replica **warm for every sector the bubble overlaps**, so the neighbour you are about to cross into is
/// already simulated and already on screen. Crossing then only changes *which* warm replica carries your
/// ship ([`step_transits`](Self::step_transits)); nothing is rebuilt, so nothing pops.
///
/// The renderer is already multi-sector and world-framed (`spacegame_render::Game` composes every
/// snapshot in `sectors` at its absolute `sector * SECTOR_SIZE` offset), so [`snapshots`](Self::snapshots)
/// just hands it one snapshot per warm sector and the seam disappears in the view too.
///
/// Each warm replica is an ordinary deterministic [`Replica`]: a sector I don't pilot is driven by the
/// real inputs of whoever *is* there (routed by [`schedule`](Self::schedule) from that sector's `/in`),
/// and an empty sector runs its deterministic environment — so every node that holds the same sector
/// computes the same state, exactly as the quorum-merge model requires.
pub struct SectorHost {
    /// The sector that carries the local player's ship — the one we publish our own inputs to and are
    /// the authority for. Follows the player across edges (see [`step_transits`](Self::step_transits)).
    home: SectorId,
    /// One deterministic replica per warm sector. A `BTreeMap` so iteration (snapshots, transit drain) is
    /// deterministic regardless of insertion order.
    reps: BTreeMap<SectorId, Replica>,
    /// The ruleset every warm sector is spawned with — cloned from the home sim so all sectors agree.
    rules: crate::ruleset::RulesetHandle,
}

impl SectorHost {
    /// Start a host from the local player's initial home sim (a fresh world, or one restored from a
    /// snapshot) — mirrors [`Replica::new`]. `home_sim.tick` should already be the shared wall-clock tick
    /// so every warm sector and every peer agree on the clock.
    pub fn from_home(home_sim: Sim) -> Self {
        let home = home_sim.sector;
        let rules = home_sim.rules.clone();
        let mut reps = BTreeMap::new();
        reps.insert(home, Replica::new(home_sim));
        SectorHost { home, reps, rules }
    }

    /// The sector currently carrying the local player.
    pub fn home(&self) -> SectorId {
        self.home
    }

    /// The next tick the home replica will simulate — the reference clock for scheduling our own input
    /// and warming a new sector.
    pub fn home_tick(&self) -> u64 {
        self.reps.get(&self.home).map(|r| r.tick()).unwrap_or(0)
    }

    /// The home replica's authoritative sim (read-only) — for the local ship's position / interest math.
    pub fn home_sim(&self) -> &Sim {
        // `home` is always present (seeded in `from_home`, kept by `retain`), so this never panics; the
        // `expect` documents the invariant.
        &self.reps.get(&self.home).expect("home replica is always present").sim
    }

    /// A warm sector's sim, if hosted.
    pub fn sim(&self, sector: SectorId) -> Option<&Sim> {
        self.reps.get(&sector).map(|r| &r.sim)
    }

    /// Warm a sector if it is not already hosted: a fresh deterministic [`Sim::for_sector`] (its own
    /// hazards / NPC factions) started at the home clock so [`advance_all`](Self::advance_all) steps it in
    /// lockstep with every other warm sector. Idempotent — an already-warm sector keeps its live state.
    pub fn ensure(&mut self, sector: SectorId) {
        if self.reps.contains_key(&sector) {
            return;
        }
        let tick = self.home_tick();
        let mut sim = Sim::for_sector(sector, self.rules.clone());
        sim.tick = tick;
        self.reps.insert(sector, Replica::new(sim));
    }

    /// Apply one of THIS node's own inputs to the **home** sector immediately (zero input delay for your
    /// own ship); pair with a tick-tagged broadcast so peers apply it deterministically. Used for the
    /// initial join and any local-echo action.
    pub fn apply_local_now(&mut self, player: &str, msg: ClientMsg) {
        if let Some(r) = self.reps.get_mut(&self.home) {
            r.apply_local_now(player, msg);
        }
    }

    /// Schedule one of THIS node's own tick-tagged inputs into the **home** sector (where our ship lives).
    pub fn schedule_home(&mut self, input: TickInput) -> bool {
        match self.reps.get_mut(&self.home) {
            Some(r) => r.schedule(input),
            None => false,
        }
    }

    /// Route an input received from a sector's `/in` stream to the replica that hosts **that** sector —
    /// not blindly into home. This is what replaces the old "filter out every neighbour input" guard: a
    /// neighbour sector is now driven by the real players there, so it is correct to render, and when you
    /// cross into it its world is already live. Dropped if that sector is not warm (we don't host it).
    pub fn schedule(&mut self, sector: SectorId, input: TickInput) -> bool {
        match self.reps.get_mut(&sector) {
            Some(r) => r.schedule(input),
            None => false,
        }
    }

    /// Advance every warm sector to the shared wall-clock `target` tick. Deterministic per sector.
    pub fn advance_all(&mut self, target: u64) {
        for r in self.reps.values_mut() {
            r.advance_to(target);
        }
    }

    /// Resolve cross-edge ships across all warm sectors. A ship that left sector A this tick is handed to
    /// the replica hosting its destination B (already warm if B is in the bubble), so it **slides** between
    /// sectors with its full state and no rebuild:
    /// * if the LOCAL player (`me`) crossed, we warm the destination if needed, admit the ship, and move
    ///   `home` onto it — the player's node now hosts the region it moved into. Returns `Some(new_home)`.
    /// * any other ship crossing between two sectors we host is admitted to its destination (kept seamless
    ///   for the players who can see it); a ship leaving for a sector we do NOT host is dropped — its real
    ///   owner's node hosts that region.
    ///
    /// Because the destination is normally already warm and at the same tick, the admitted ship enters a
    /// live world at the correct coordinate — never the sector-centre teleport the single-replica re-home
    /// risked, and never a full-world pop.
    pub fn step_transits(&mut self, me: &str) -> Option<SectorId> {
        let mut pending: Vec<crate::sim::Transit> = Vec::new();
        for r in self.reps.values_mut() {
            pending.extend(r.drain_transits());
        }
        let mut new_home = None;
        for t in pending {
            if t.ship.id == me {
                self.ensure(t.to); // normally a no-op: the bubble pre-warmed it
                if let Some(dst) = self.reps.get_mut(&t.to) {
                    dst.accept_transit(t.ship);
                }
                self.home = t.to;
                new_home = Some(t.to);
            } else if let Some(dst) = self.reps.get_mut(&t.to) {
                dst.accept_transit(t.ship);
            }
            // else: destination not hosted here — its owner's node has it.
        }
        new_home
    }

    /// Drop warm sectors that have left the interest bubble (their `/state` no longer needs simulating or
    /// rendering), returning the dropped [`SectorId`]s so the caller can also forget their stale snapshot
    /// in the renderer's `sectors` map. `home` is always kept, even if a momentary interest glitch omits
    /// it. Call once per frame with the current interest set.
    pub fn retain(&mut self, interest: &std::collections::BTreeSet<SectorId>) -> Vec<SectorId> {
        let home = self.home;
        let dropped: Vec<SectorId> =
            self.reps.keys().copied().filter(|s| *s != home && !interest.contains(s)).collect();
        for s in &dropped {
            self.reps.remove(s);
        }
        dropped
    }

    /// One authoritative snapshot per warm sector, keyed by its `/state` topic, ready to feed straight to
    /// `Game::ingest`. The world-framed renderer composes them into one seamless view (each offset by its
    /// sector origin), so a player standing on a seam sees both sides at once and a crossing shows no
    /// transition.
    pub fn snapshots(&self, me: &str, now_ms: u64) -> Vec<(String, crate::wire::Snapshot)> {
        self.reps
            .iter()
            .map(|(sector, r)| {
                let tok = sector.token();
                let snap = crate::room::build_snapshot(&r.sim, &tok, me, now_ms);
                (crate::wire::topics::state(&tok), snap)
            })
            .collect()
    }

    /// The sectors currently warm — for tests and diagnostics.
    pub fn warm_sectors(&self) -> Vec<SectorId> {
        self.reps.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tick: u64, player: &str, seq: u64, msg: ClientMsg) -> TickInput {
        TickInput { tick, player: player.into(), seq, msg }
    }

    fn thrust(tick: u64, player: &str, seq: u64) -> TickInput {
        TickInput {
            tick,
            player: player.into(),
            seq,
            msg: ClientMsg::Input { thrust: true, turn: 1, fire: true, aim: Some(0.7), name: None },
        }
    }

    #[test]
    fn two_replicas_from_the_same_inputs_reach_identical_state() {
        // The premise of "everyone runs the server and they agree": two replicas, the SAME tick-tagged
        // inputs scheduled in DIFFERENT order, must end bit-identical. If this ever fails, the quorum
        // merge would falsely flag honest players — so this is the load-bearing test for the whole model.
        let mut a = Replica::new(Sim::new());
        let mut b = Replica::new(Sim::new());
        assert_eq!(a.state_hash(), b.state_hash(), "identical fresh worlds start equal");

        let inputs = vec![
            thrust(3, "p1", 1),
            thrust(3, "p2", 1),
            thrust(5, "p1", 2),
            input(4, "p2", 2, ClientMsg::Build { kind: "gun".into() }),
            input(6, "p1", 3, ClientMsg::Weapon { id: "blaster".into() }),
        ];
        // a: scheduled in given order; b: scheduled in REVERSED order — arrival order must not matter.
        for ti in inputs.iter().cloned() {
            a.schedule(ti);
        }
        for ti in inputs.iter().rev().cloned() {
            b.schedule(ti);
        }
        // Advance within one catch-up window so every scheduled input is applied (not jumped past).
        a.advance_to(11);
        b.advance_to(11);
        assert_eq!(a.tick(), 11);
        assert_eq!(a.state_hash(), b.state_hash(), "same inputs => same state, independent of arrival order");
    }

    #[test]
    fn a_rogue_input_diverges_the_hash_so_the_merge_can_catch_it() {
        // A replica that injects an input no one else has (a cheat) computes a different hash and is the
        // odd one out — exactly what replication::agree turns into a ResyncTo verdict.
        let mut honest = Replica::new(Sim::new());
        let mut cheat = Replica::new(Sim::new());
        let shared = thrust(2, "p1", 1);
        honest.schedule(shared.clone());
        cheat.schedule(shared);
        // The cheat injects an input the rest of the region never saw — here a phantom player that only
        // its sim knows about. Any input with a real effect works; a new ship is an unmistakable one.
        cheat.schedule(thrust(3, "ghost", 1));
        honest.advance_to(10);
        cheat.advance_to(10);
        assert_ne!(honest.state_hash(), cheat.state_hash(), "a forged input shows up as a divergent hash");
    }

    #[test]
    fn an_input_for_an_already_simulated_tick_is_rejected() {
        let mut r = Replica::new(Sim::new());
        r.advance_to(10);
        assert!(!r.schedule(thrust(4, "p1", 1)), "a tick already simulated can't accept a late input");
        assert!(r.schedule(thrust(12, "p1", 1)), "a future tick still accepts inputs");
    }

    #[test]
    fn the_browser_server_fires_bullets_from_the_player() {
        // Headless proof of the fix that started this whole thread: a player who fires gets bullets
        // spawned AT their own ship. The `Replica` IS the server the browser (or a headless donor node)
        // runs, so this verifies "shoot from the player" with NO browser and NO frontend — exactly the
        // testability Leif asked for, and the same code path a donation node would run.
        let mut r = Replica::new(Sim::new());
        let p = "p1";
        let t = r.tick();
        r.schedule(TickInput { tick: t, player: p.into(), seq: 1, msg: ClientMsg::Join { name: "P".into(), cap: None } });
        r.schedule(TickInput {
            tick: t + 1,
            player: p.into(),
            seq: 2,
            msg: ClientMsg::Input { thrust: false, turn: 0, fire: true, aim: Some(0.0), name: None },
        });
        // Check right after the shot is fired, before it has flown far (bullets travel ~30/tick).
        r.advance_to(t + 3);
        let sim = r.sim();
        let ship = sim.ships.get(p).expect("the server spawned the player's ship");
        let mine: Vec<_> = sim.bullets.iter().filter(|b| b.owner == p).collect();
        assert!(!mine.is_empty(), "firing produced a bullet owned by the player");
        // The just-fired bullet originates at the player (not at some divergent host-side position).
        let nearest = mine
            .iter()
            .map(|b| ((b.pos.x - ship.pos.x).powi(2) + (b.pos.y - ship.pos.y).powi(2)).sqrt())
            .fold(f32::INFINITY, f32::min);
        assert!(nearest < 150.0, "a bullet spawned from the player's position (nearest {nearest})");
    }

    #[test]
    fn local_player_rehomes_across_a_sector_edge_instead_of_teleporting_to_centre() {
        // The bug this guards: a player crossing a sector edge had their ship deleted (seamless transit
        // with nothing draining the hand-off), and the next input auto-rejoined them at the sector CENTRE.
        // With the players as the servers there is no relay to re-admit the ship, so the replica must
        // re-home itself onto the region it moved into. No test flew a replica to an edge before — which
        // is exactly why this shipped. This one does.
        use crate::sim::{SECTOR_SIZE, SHIP_R};
        let mut sim = Sim::new();
        sim.join("me", "Me", 0);
        sim.factions.values_mut().for_each(|f| f.units.clear()); // no NPC fleet to collide with at the edge
        {
            let s = sim.ships.get_mut("me").unwrap();
            s.pos.x = SECTOR_SIZE - 2.0;
            s.pos.y = 1500.0;
            s.a = 0.0;
            s.vx = 6.0; // carried velocity pushes it across the east edge on the next tick
            s.minerals = 7;
        }
        let mut r = Replica::new(sim);
        let start = r.tick();
        r.advance_to(start + 3);
        // The ship left this sector's sim — the exact moment the old code dropped it and re-spawned at centre.
        assert!(!r.sim().ships.contains_key("me"), "the ship transited out of (0,0)");
        let moved = r.rehome_local_player("me");
        assert_eq!(moved, Some(SectorId::new(1, 0)), "re-homed east into (1,0)");
        assert_eq!(r.sector(), SectorId::new(1, 0), "the replica now hosts the neighbour sector");
        let s = r.sim().ships.get("me").expect("the ship was carried into the new sector, not lost");
        assert_eq!(s.minerals, 7, "its full state crossed the boundary");
        // The decisive assertions: entered at the WRAPPED edge coordinate, NOT teleported to the centre.
        assert!(s.pos.x < SHIP_R + 50.0, "entered at the west edge of the new sector (x={}), not mid-sector", s.pos.x);
        assert!((s.pos.x - SECTOR_SIZE / 2.0).abs() > 100.0, "NOT at the sector centre (the teleport bug)");
    }

    #[test]
    fn interest_grows_to_the_neighbour_as_the_bubble_nears_a_seam() {
        // The seamless-interest property: a player mid-sector subscribes to one sector; as they near the
        // east edge their bubble overlaps the neighbour, so the interest set picks it up BEFORE they
        // cross — no transition, the bubble just slides. Replaces the fixed 8-neighbour ring.
        use crate::sim::SECTOR_SIZE;
        let view = 1500.0; // ~a screen
        let mut sim = Sim::new();
        sim.join("me", "Me", 0);
        sim.factions.values_mut().for_each(|f| f.units.clear()); // isolate the player's own bubble
        {
            let s = sim.ships.get_mut("me").unwrap();
            s.pos.x = SECTOR_SIZE * 0.5; // dead centre
            s.pos.y = SECTOR_SIZE * 0.5;
        }
        let r = Replica::new(sim);
        let mid = r.interest_sectors("me", view);
        assert_eq!(mid.len(), 1, "mid-sector: interest is exactly the home sector");
        assert!(mid.contains(&SectorId::new(0, 0)));

        // Now sit near the east edge (within `view` of it): the neighbour (1,0) joins the interest set.
        let mut sim2 = Sim::new();
        sim2.join("me", "Me", 0);
        sim2.factions.values_mut().for_each(|f| f.units.clear());
        {
            let s = sim2.ships.get_mut("me").unwrap();
            s.pos.x = SECTOR_SIZE - 100.0; // 100 units from the east seam, inside a 1500 view
            s.pos.y = SECTOR_SIZE * 0.5;
        }
        let edge = Replica::new(sim2).interest_sectors("me", view);
        assert!(edge.contains(&SectorId::new(0, 0)) && edge.contains(&SectorId::new(1, 0)));
        assert_eq!(edge.len(), 2, "near the east edge: home + the eastern neighbour, nothing else");
    }

    #[test]
    fn schedule_local_targets_the_input_delay_tick() {
        let mut r = Replica::new(Sim::new());
        r.advance_to(100);
        let at = r.schedule_local("me", 1, ClientMsg::Respawn);
        assert_eq!(at, 100 + INPUT_DELAY, "local input is scheduled INPUT_DELAY ticks ahead for propagation");
    }

    #[test]
    fn crossing_between_warm_sectors_slides_the_ship_without_a_rebuild() {
        // The seamless property at the host level: with the eastern neighbour already warm (as the
        // interest bubble keeps it), a player crossing the seam is HANDED to that live replica with full
        // state — not dropped into a freshly rebuilt sector — and the sector left behind stays warm, so
        // nothing pops in front of OR behind the player.
        use crate::sim::{SECTOR_SIZE, SHIP_R};
        let mut sim = Sim::new(); // home sector (0,0)
        sim.join("me", "Me", 0);
        sim.factions.values_mut().for_each(|f| f.units.clear()); // no NPC fleet to collide with at the edge
        {
            let s = sim.ships.get_mut("me").unwrap();
            s.pos.x = SECTOR_SIZE - 2.0;
            s.pos.y = 1500.0;
            s.a = 0.0;
            s.vx = 6.0; // carried velocity pushes it across the east edge
            s.minerals = 7;
        }
        let mut host = SectorHost::from_home(sim);
        host.ensure(SectorId::new(1, 0)); // pre-warm the neighbour, as the bubble does every frame
        let start = host.home_tick();
        host.advance_all(start + 3);
        let moved = host.step_transits("me");
        assert_eq!(moved, Some(SectorId::new(1, 0)), "home follows the player east");
        assert_eq!(host.home(), SectorId::new(1, 0), "the node now hosts the neighbour as home");
        let s = host.home_sim().ships.get("me").expect("ship carried into the neighbour, not lost");
        assert_eq!(s.minerals, 7, "full state crossed the boundary");
        assert!(s.pos.x < SHIP_R + 50.0, "entered at the west edge (x={}), not mid-sector", s.pos.x);
        assert!((s.pos.x - SECTOR_SIZE / 2.0).abs() > 100.0, "NOT teleported to the sector centre");
        assert!(host.sim(SectorId::new(0, 0)).is_some(), "the sector we left stays warm — no pop behind us");
    }

    #[test]
    fn retain_drops_far_sectors_but_never_home() {
        let mut sim = Sim::new();
        sim.join("me", "Me", 0);
        let mut host = SectorHost::from_home(sim);
        host.ensure(SectorId::new(1, 0));
        host.ensure(SectorId::new(5, 5)); // a sector far outside the bubble
        let interest: std::collections::BTreeSet<SectorId> =
            [SectorId::new(0, 0), SectorId::new(1, 0)].into_iter().collect();
        let dropped = host.retain(&interest);
        assert_eq!(dropped, vec![SectorId::new(5, 5)], "the far sector is released");
        let warm = host.warm_sectors();
        assert!(warm.contains(&SectorId::new(0, 0)) && warm.contains(&SectorId::new(1, 0)));
        assert!(!warm.contains(&SectorId::new(5, 5)));
        // Home is kept even if an interest glitch omits it.
        host.retain(&std::collections::BTreeSet::new());
        assert!(host.warm_sectors().contains(&SectorId::new(0, 0)), "home is never dropped");
    }

    #[test]
    fn an_input_routes_to_its_own_sector_not_home() {
        // A neighbour player's input arrives on the NEIGHBOUR's /in. It must drive the neighbour's warm
        // replica (so we render it correctly across the seam), never auto-join a ghost at our home centre.
        let mut sim = Sim::new();
        sim.join("me", "Me", 0);
        sim.factions.values_mut().for_each(|f| f.units.clear());
        let mut host = SectorHost::from_home(sim);
        host.ensure(SectorId::new(1, 0));
        let t = host.home_tick();
        host.schedule(
            SectorId::new(1, 0),
            TickInput { tick: t, player: "neighbour".into(), seq: 1, msg: ClientMsg::Join { name: "N".into(), cap: None } },
        );
        host.advance_all(t + 2);
        assert!(
            host.sim(SectorId::new(1, 0)).unwrap().ships.contains_key("neighbour"),
            "the neighbour spawned in its own sector"
        );
        assert!(!host.home_sim().ships.contains_key("neighbour"), "and did NOT ghost-join home");
    }
}
