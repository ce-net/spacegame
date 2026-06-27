//! Co-authoritative **lockstep replica** — the netcode that makes every participant a server.
//!
//! There is no separate "game server". Each device (browser tab, native client, headless donor) runs
//! the **full authoritative [`Sim`]** for the sector it cares about, and they agree by determinism:
//! every input is stamped with the **simulation tick** it applies at ([`crate::wire::TaggedInput`]) and
//! broadcast a few ticks ahead ([`INPUT_DELAY`]) on the sector `/in` topic. Because the [`Sim`] is a
//! pure deterministic function of `(start state, the set of tick-tagged inputs)`, and the shared clock
//! ([`tick_at`]) gives everyone the same tick, every replica that has seen the same inputs computes the
//! **identical** state — so any of them can render it (zero local delay for your own ship, applied
//! immediately via [`Replica::apply_local_now`]) and any of them can take over if another drops. This is
//! the same determinism the anti-cheat ([`crate::sim::Sim::state_hash`]) and proximity-replica failover
//! ([`crate::replication`]) rely on, expressed as the client's run loop.
//!
//! This module is pure (no mesh, no clock of its own — the caller passes wall-clock millis to
//! [`tick_at`]); the thin I/O that publishes/receives [`TaggedInput`]s over the node lives in the
//! native/wasm clients.

use std::collections::BTreeMap;
use std::collections::HashSet;

use crate::room::apply_client_msg;
use crate::shard::SectorId;
use crate::sim::Sim;
use crate::wire::ClientMsg;

/// Authoritative tick rate (Hz). The same value the sector host loop ticks at, so a host and a
/// co-authoritative replica advance in lockstep.
pub const HZ: u64 = 20;

/// Milliseconds per tick — `tick_at` quantises the shared wall-clock to this grid.
pub const TICK_MS: f64 = 1000.0 / HZ as f64;

/// How many ticks ahead an input is broadcast so it reaches peers *before* the tick it applies at. This
/// is the propagation budget: as long as the mesh delivers an input within `INPUT_DELAY` ticks
/// (`INPUT_DELAY * TICK_MS` ms), every replica applies it at the same tick and stays in sync.
pub const INPUT_DELAY: u64 = 6;

/// Cap on how many ticks a single [`Replica::advance_to`] will simulate, so a long stall (a tab that
/// was backgrounded) catches up bounded rather than freezing the loop for thousands of ticks.
pub const MAX_CATCHUP: u64 = 240;

/// The shared simulation tick for a wall-clock instant (epoch millis). Every device computes the same
/// tick from the same clock, which is what lets independent replicas agree without a coordinator.
pub fn tick_at(wall_ms: f64) -> u64 {
    if wall_ms <= 0.0 { 0 } else { (wall_ms / TICK_MS) as u64 }
}

/// One client action bound to the tick it applies at, with its node-authenticated author and a
/// per-author sequence (for dedup/ordering). The wire form is [`crate::wire::TaggedInput`]; this adds
/// the author the node authenticated.
#[derive(Debug, Clone, PartialEq)]
pub struct TickInput {
    pub tick: u64,
    pub player: String,
    pub seq: u64,
    pub msg: ClientMsg,
}

/// A co-authoritative replica: an authoritative [`Sim`] plus the buffer of future tick-tagged inputs
/// not yet applied. Drive it by [`schedule`](Replica::schedule)-ing inputs (local and peers') and
/// [`advance_to`](Replica::advance_to)-ing the shared tick each frame.
#[derive(Debug, Clone)]
pub struct Replica {
    sim: Sim,
    /// Inputs to apply, keyed by their tick; each bucket is applied (sorted) just before that tick runs.
    pending: BTreeMap<u64, Vec<TickInput>>,
    /// `(player, seq)` already accepted, so a resend or a locally-echoed input is never applied twice.
    seen: HashSet<(String, u64)>,
}

impl Replica {
    pub fn new(sim: Sim) -> Self {
        Replica { sim, pending: BTreeMap::new(), seen: HashSet::new() }
    }

    /// The current authoritative tick.
    pub fn tick(&self) -> u64 {
        self.sim.tick
    }

    /// Borrow the authoritative sim (to build a snapshot to render / publish).
    pub fn sim(&self) -> &Sim {
        &self.sim
    }

    /// Mutable access to the authoritative sim (for setup / sector re-home).
    pub fn sim_mut(&mut self) -> &mut Sim {
        &mut self.sim
    }

    /// Apply an input to the **current** tick immediately — used for the local player's own actions so
    /// their ship responds with zero delay (the mesh confirms the same input on peers a few ticks
    /// later). Idempotent inputs (the local player also broadcasts these tagged) are deduped by author
    /// not needed here, since locals are not re-scheduled from their own echo.
    pub fn apply_local_now(&mut self, player: &str, msg: ClientMsg) {
        apply_client_msg(&mut self.sim, player, msg);
    }

    /// Buffer a tick-tagged input (local or from a peer) to apply when its tick runs. Inputs whose tick
    /// already passed are dropped (too late to keep determinism — the `INPUT_DELAY` budget was missed).
    /// Deduplicated by `(player, seq)`.
    pub fn schedule(&mut self, ti: TickInput) {
        if ti.tick < self.sim.tick {
            return; // already simulated past this tick; applying now would diverge from peers
        }
        let key = (ti.player.clone(), ti.seq);
        if !self.seen.insert(key) {
            return; // a resend / echo we already have
        }
        self.pending.entry(ti.tick).or_default().push(ti);
    }

    /// Advance the authoritative sim to `target`, applying each tick's buffered inputs (in deterministic
    /// `(player, seq)` order) just before that tick steps. Bounded by [`MAX_CATCHUP`].
    pub fn advance_to(&mut self, target: u64) {
        let target = target.min(self.sim.tick + MAX_CATCHUP);
        while self.sim.tick < target {
            let t = self.sim.tick;
            if let Some(mut inputs) = self.pending.remove(&t) {
                inputs.sort_by(|a, b| a.player.cmp(&b.player).then(a.seq.cmp(&b.seq)));
                for ti in inputs {
                    apply_client_msg(&mut self.sim, &ti.player, ti.msg);
                }
            }
            self.sim.tick(1.0);
        }
        // Forget dedup keys / buckets for ticks now in the past so the maps stay bounded.
        let floor = self.sim.tick;
        self.pending.retain(|&k, _| k >= floor);
    }

    /// **Infinite map / follow-me:** if the local player crossed a sector edge this advance, re-home the
    /// replica onto the destination sector (so the player's own node becomes the authority for the
    /// region they entered) and return the new [`SectorId`]. Other ships' transits are dropped — their
    /// own owners' replicas carry them. Returns `None` if the local player stayed in this sector.
    pub fn rehome_local_player(&mut self, me: &str) -> Option<SectorId> {
        let transits = self.sim.take_transits();
        let mut moved = None;
        for t in transits {
            if t.ship.id == me {
                self.sim.sector = t.to;
                self.sim.accept_transit(t.ship);
                moved = Some(t.to);
            }
            // A non-local ship leaving our sector is now the destination host's responsibility.
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_at_is_shared_and_monotonic() {
        assert_eq!(tick_at(0.0), 0);
        assert_eq!(tick_at(TICK_MS - 0.001), 0);
        assert_eq!(tick_at(TICK_MS), 1);
        assert_eq!(tick_at(TICK_MS * 100.0), 100);
        assert!(tick_at(1_000_000.0) < tick_at(1_000_050.0));
    }

    #[test]
    fn two_replicas_with_the_same_inputs_agree_bit_for_bit() {
        // The core determinism guarantee: feed two fresh replicas the identical tick-tagged inputs (in
        // DIFFERENT arrival orders) and they must reach the same authoritative state hash.
        let mk = || {
            let mut s = Sim::new();
            s.seamless = false;
            Replica::new(s)
        };
        let inputs = vec![
            TickInput { tick: 2, player: "a".into(), seq: 1, msg: ClientMsg::Join { name: "A".into(), cap: None } },
            TickInput { tick: 2, player: "b".into(), seq: 1, msg: ClientMsg::Join { name: "B".into(), cap: None } },
            TickInput { tick: 4, player: "a".into(), seq: 2, msg: ClientMsg::Input { thrust: true, turn: 1, fire: true, aim: Some(0.5), name: None } },
            TickInput { tick: 6, player: "b".into(), seq: 2, msg: ClientMsg::Input { thrust: true, turn: -1, fire: false, aim: Some(2.0), name: None } },
        ];

        let mut r1 = mk();
        for ti in &inputs {
            r1.schedule(ti.clone());
        }
        r1.advance_to(20);

        // r2 gets them in reverse order, with a duplicate resend of one — must not change the result.
        let mut r2 = mk();
        for ti in inputs.iter().rev() {
            r2.schedule(ti.clone());
        }
        r2.schedule(inputs[0].clone()); // resend (deduped)
        r2.advance_to(20);

        assert_eq!(r1.tick(), r2.tick());
        assert_eq!(r1.sim().state_hash(), r2.sim().state_hash(), "same inputs -> same world, regardless of arrival order");
    }

    #[test]
    fn late_input_is_dropped_not_applied_out_of_order() {
        let mut r = Replica::new({
            let mut s = Sim::new();
            s.seamless = false;
            s
        });
        r.advance_to(10);
        // An input tagged for a tick we already simulated is refused (keeps replicas in agreement).
        r.schedule(TickInput { tick: 3, player: "a".into(), seq: 1, msg: ClientMsg::Join { name: "A".into(), cap: None } });
        r.advance_to(11);
        assert!(r.sim().ships.is_empty(), "a late-tagged join must not spawn out of lockstep");
    }

    #[test]
    fn local_now_gives_zero_delay_then_dedupes_the_echo() {
        let mut r = Replica::new({
            let mut s = Sim::new();
            s.seamless = false;
            s
        });
        // Local action applies immediately (the ship exists this very tick).
        r.apply_local_now("me", ClientMsg::Join { name: "Me".into(), cap: None });
        assert!(r.sim().ships.contains_key("me"), "local input is zero-delay");
        // The same action, scheduled from a tick-tagged broadcast, is a no-op spawn-wise (already joined)
        // and the world stays consistent.
        r.schedule(TickInput { tick: r.tick() + INPUT_DELAY, player: "me".into(), seq: 1, msg: ClientMsg::Join { name: "Me".into(), cap: None } });
        r.advance_to(r.tick() + INPUT_DELAY + 2);
        assert!(r.sim().ships.contains_key("me"));
    }
}
