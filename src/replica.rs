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

use crate::sim::Sim;
use crate::wire::ClientMsg;

/// Ticks of input delay: a locally-generated input applies at `current_tick + INPUT_DELAY` on every
/// replica, giving the network time to deliver it everywhere before that tick is simulated. ~100 ms at
/// 60 Hz — the budget for an input to reach the other replicas in a region. (Your own ship can still feel
/// instant via a separate local-echo/prediction layer above this; the canonical scheduled input is what
/// every replica — including yours — agrees to simulate, so they never diverge.)
pub const INPUT_DELAY: u64 = 6;

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

    /// This replica's deterministic state hash — its claim about the world, for the quorum merge.
    pub fn state_hash(&self) -> u64 {
        self.sim.state_hash()
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

    /// Advance the simulation up to (but not including) `target`, applying each tick's scheduled inputs in
    /// canonical `(player, seq)` order before stepping that tick. Deterministic: given the same scheduled
    /// inputs, every replica ends in the same state regardless of arrival order or host. No-op if already
    /// at or past `target`.
    pub fn advance_to(&mut self, target: u64) {
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
        a.advance_to(20);
        b.advance_to(20);
        assert_eq!(a.tick(), 20);
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
    fn schedule_local_targets_the_input_delay_tick() {
        let mut r = Replica::new(Sim::new());
        r.advance_to(100);
        let at = r.schedule_local("me", 1, ClientMsg::Respawn);
        assert_eq!(at, 100 + INPUT_DELAY, "local input is scheduled INPUT_DELAY ticks ahead for propagation");
    }
}
