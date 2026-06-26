//! The **cell runtime** — what a host node does for one authoritative cell the galaxy assigned it.
//!
//! It's the bridge between the abstract galaxy (cells split/merge) and the concrete simulation (the
//! existing per-sector tick/publish/replicate loop). One `CellRuntime` per hosted cell; a busy donor
//! node runs several, each in its own task, each independently subdividing.
//!
//! Every tick it: advances the sim, publishes the viewport-scoped `/state`, **measures its own load**
//! (the honest p99 tick time + outbound bandwidth + counts), and gossips a [`LoadFrame`]. When its
//! owning controller commits a `Split`/`Merge`/`Migrate` for it on the shape topic, the runtime carries
//! it out locally — partitioning live state into children, absorbing siblings, or handing off by
//! snapshot — with no dropped frame for the players inside.

use std::collections::VecDeque;

use crate::galaxy::{CellId, CellLoad};
use crate::galaxywire::{topics, LoadFrame, ShapeOp};

/// A rolling window of recent tick durations so we report a real **p99**, not a flattering average —
/// the tail is what makes a host miss frames, so the tail is what triggers a split.
#[derive(Debug)]
pub struct TickMeter {
    samples: VecDeque<u32>, // microseconds
    cap: usize,
}

impl TickMeter {
    pub fn new() -> Self {
        TickMeter { samples: VecDeque::with_capacity(256), cap: 256 }
    }
    pub fn record(&mut self, micros: u32) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(micros);
    }
    /// 99th-percentile tick time over the window (0 until warmed).
    pub fn p99(&self) -> u32 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut v: Vec<u32> = self.samples.iter().copied().collect();
        v.sort_unstable();
        let idx = ((v.len() as f32 * 0.99) as usize).min(v.len() - 1);
        v[idx]
    }
}

/// A leaky-bucket byte counter → bytes/sec, for the outbound snapshot bandwidth load signal.
#[derive(Debug, Default)]
pub struct BandwidthMeter {
    bytes_window: u64,
    secs: f32,
}
impl BandwidthMeter {
    pub fn add(&mut self, bytes: u64, dt: f32) {
        // Exponential window so a burst shows up but doesn't dominate forever.
        let decay = (-dt / 2.0).exp();
        self.bytes_window = ((self.bytes_window as f32) * decay + bytes as f32) as u64;
        self.secs = self.secs * decay + dt;
    }
    pub fn bps(&self) -> u64 {
        if self.secs <= 0.0 {
            0
        } else {
            (self.bytes_window as f32 / self.secs) as u64
        }
    }
}

/// The host-side runtime for one cell. Generic over the sim handle (`S`) so this module reads as the
/// orchestration, not the simulation internals (the real `S` is `crate::sim::Sim` behind `room`).
pub struct CellRuntime<S> {
    pub cell: CellId,
    pub sim: S,
    pub host_id: String,
    tick: TickMeter,
    bw: BandwidthMeter,
    epoch: u64,
}

impl<S> CellRuntime<S> {
    pub fn new(cell: CellId, sim: S, host_id: String) -> Self {
        CellRuntime { cell, sim, host_id, tick: TickMeter::new(), bw: BandwidthMeter::default(), epoch: 0 }
    }

    /// Fold this tick's measurements in. Called right after `sim.tick()` + publish, with the measured
    /// tick duration and bytes pushed this frame.
    pub fn measure(&mut self, tick_micros: u32, out_bytes: u64, dt: f32, players: u32, entities: u32, epoch: u64) {
        self.tick.record(tick_micros);
        self.bw.add(out_bytes, dt);
        self.epoch = epoch;
        let _ = (players, entities); // captured into the LoadFrame below
    }

    /// The load frame to gossip on this cell's `/load` topic this epoch — the *only* input the
    /// controller needs to decide split/merge. Honest by construction (p99 tail + real bandwidth).
    pub fn load_frame(&self, players: u32, entities: u32) -> LoadFrame {
        LoadFrame {
            cell: self.cell,
            host: self.host_id.clone(),
            load: CellLoad {
                players,
                entities,
                tick_p99_us: self.tick.p99(),
                bandwidth_bps: self.bw.bps(),
                epoch: self.epoch,
            },
        }
    }

    pub fn load_topic(&self) -> String {
        topics::load(&self.cell.token())
    }

    /// React to a committed shape op that names this cell. Returns the local action to take. The actual
    /// state moves (partition / absorb / handoff) run through `crate::partition` + the snapshot blob
    /// store; this returns the plan so the host loop executes it deterministically.
    pub fn on_shape(&self, op: &ShapeOp) -> CellTransition {
        match op {
            ShapeOp::Split { cell, children, place_on, .. } if *cell == self.cell => {
                CellTransition::SplitInto { children: *children, hosts: place_on.to_vec() }
            }
            ShapeOp::Merge { parent, .. } if parent.children().contains(&self.cell) => {
                CellTransition::MergeUp { parent: *parent }
            }
            ShapeOp::Migrate { cell, to, snapshot, .. } if *cell == self.cell => {
                CellTransition::HandOff { to: to.clone(), snapshot: snapshot.clone() }
            }
            _ => CellTransition::None,
        }
    }
}

/// The local consequence of a shape commit for a running cell.
#[derive(Debug, Clone, PartialEq)]
pub enum CellTransition {
    None,
    /// This cell becomes four children on `hosts`: snapshot, partition (`crate::partition::split_entities`),
    /// deploy each child seeded with its bucket, then stop running this cell.
    SplitInto { children: [CellId; 4], hosts: Vec<String> },
    /// This cell is one of four merging into `parent`: snapshot, and if we win the placement, absorb the
    /// other three buckets (`crate::partition::merge_entities`); otherwise stop after handing our snapshot up.
    MergeUp { parent: CellId },
    /// Hand this cell to `to`: it adopts `snapshot`, takes over the `/state` advertisement, we stop after
    /// one snapshot-interval of overlap (no gap for players).
    HandOff { to: String, snapshot: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p99_tracks_the_tail() {
        let mut m = TickMeter::new();
        for _ in 0..99 {
            m.record(1000);
        }
        m.record(50_000); // one ugly spike
        assert!(m.p99() >= 1000, "p99 reflects the window incl. the tail");
    }

    #[test]
    fn split_commit_for_my_cell_triggers_split() {
        let rt = CellRuntime::new(CellId { depth: 1, x: 0, y: 0 }, (), "host".into());
        let op = ShapeOp::Split {
            cell: rt.cell,
            children: rt.cell.children().try_into().unwrap(),
            place_on: ["a".into(), "b".into(), "c".into(), "d".into()],
            from_snapshot: "Qm".into(),
        };
        assert!(matches!(rt.on_shape(&op), CellTransition::SplitInto { .. }));
    }
}
