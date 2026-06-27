//! The **live galaxy map** — fold the shape + load gossip into one renderable picture of the whole
//! galaxy, so you can *watch it breathe*: cells subdividing under a battle, merging back as it clears,
//! burst nodes lighting up across the planet, all in real time.
//!
//! It is renderer-agnostic on purpose — the same [`MapModel`] drives the CLI quadtree dump
//! (`spacegame galaxy`), a web heatmap, and the in-game minimap. It holds no game state, just the
//! galaxy's *shape* (which cells are leaves), each cell's *heat* (latest load), each cell's *host*, and
//! a short trail of recent reshape *events* so the view can pulse a split where it happened.

use std::collections::HashMap;

use crate::galaxy::{CellId, CellLoad, Galaxy, WorldRect};
use crate::galaxywire::{DomainFrame, DomainView, LoadFrame, ShapeOp};

/// A cell rendered on the map: where it is, who hosts it, how hot it is.
#[derive(Debug, Clone)]
pub struct MapCell {
    pub cell: CellId,
    pub rect: WorldRect,
    pub host: Option<String>,
    pub load: Option<CellLoad>,
}

impl MapCell {
    /// Heat 0.0..1.0 from the load, blending the three pins (players / tick time / bandwidth) so the
    /// colour reflects whichever resource is closest to forcing a split.
    pub fn heat(&self, policy_players: u32, policy_tick_us: u32, policy_bps: u64) -> f32 {
        let l = match &self.load {
            Some(l) => l,
            None => return 0.0,
        };
        let p = l.players as f32 / policy_players.max(1) as f32;
        let t = l.tick_p99_us as f32 / policy_tick_us.max(1) as f32;
        let b = l.bandwidth_bps as f32 / policy_bps.max(1) as f32;
        p.max(t).max(b).clamp(0.0, 1.5) / 1.5
    }
}

/// A recent reshape, kept briefly so the map can pulse it (a split flashes the four children, a merge
/// implodes the four into one).
#[derive(Debug, Clone)]
pub struct MapEvent {
    pub epoch: u64,
    pub kind: MapEventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapEventKind {
    Split { cell: CellId },
    Merge { parent: CellId },
    Migrate { cell: CellId, to: String },
    BurstLit { region: String, nodes: usize },
}

/// The whole map: shape + per-cell heat/host + an event trail. Cheap to keep current — every gossip
/// frame folds in with one update.
#[derive(Debug, Default)]
pub struct MapModel {
    pub galaxy: Galaxy,
    hosts: HashMap<CellId, String>,
    loads: HashMap<CellId, CellLoad>,
    /// Latest player bubbles per host (from the `/domains` topic). Keyed by host, each value the full
    /// snapshot that host last gossiped — so a player leaving drops out when its host's next frame omits
    /// it. This is the moving "who is where" layer the grid `cells()` can't express.
    domains: HashMap<String, Vec<DomainView>>,
    pub events: std::collections::VecDeque<MapEvent>,
    event_cap: usize,
}

impl MapModel {
    pub fn new() -> Self {
        MapModel { galaxy: Galaxy::genesis(), event_cap: 64, ..Default::default() }
    }

    /// Fold a per-cell load frame in (from a `/load` topic).
    pub fn on_load(&mut self, f: LoadFrame) {
        self.hosts.insert(f.cell, f.host);
        self.loads.insert(f.cell, f.load);
    }

    /// Fold a host's player-bubble snapshot in (from the `/domains` topic). Replaces that host's previous
    /// set wholesale, so departures are handled implicitly.
    pub fn on_domains(&mut self, f: DomainFrame) {
        if f.domains.is_empty() {
            self.domains.remove(&f.host);
        } else {
            self.domains.insert(f.host, f.domains);
        }
    }

    /// Every player bubble currently on the map, across all hosts — the moving "who is where" dots. Sorted
    /// by owner so the render order is stable.
    pub fn player_domains(&self) -> Vec<DomainView> {
        let mut out: Vec<DomainView> = self.domains.values().flatten().cloned().collect();
        out.sort_by(|a, b| a.owner.cmp(&b.owner));
        out
    }

    /// Apply a committed reshape (from the `/galaxy` topic): mutate the shape, retarget hosts, and push
    /// a pulse event.
    pub fn on_shape(&mut self, epoch: u64, op: &ShapeOp) {
        match op {
            ShapeOp::Split { cell, children, place_on, .. } => {
                self.galaxy.split(*cell);
                self.loads.remove(cell);
                self.hosts.remove(cell);
                for (c, h) in children.iter().zip(place_on.iter()) {
                    self.hosts.insert(*c, h.clone());
                }
                self.push(epoch, MapEventKind::Split { cell: *cell });
            }
            ShapeOp::Merge { parent, place_on, .. } => {
                self.galaxy.merge(*parent);
                for c in parent.children() {
                    self.loads.remove(&c);
                    self.hosts.remove(&c);
                }
                self.hosts.insert(*parent, place_on.clone());
                self.push(epoch, MapEventKind::Merge { parent: *parent });
            }
            ShapeOp::Place { cell, node, .. } => {
                self.hosts.insert(*cell, node.clone());
            }
            ShapeOp::Migrate { cell, to, .. } => {
                self.hosts.insert(*cell, to.clone());
                self.push(epoch, MapEventKind::Migrate { cell: *cell, to: to.clone() });
            }
        }
    }

    fn push(&mut self, epoch: u64, kind: MapEventKind) {
        if self.events.len() == self.event_cap {
            self.events.pop_front();
        }
        self.events.push_back(MapEvent { epoch, kind });
    }

    /// The full set of map cells, ready to draw.
    pub fn cells(&self) -> Vec<MapCell> {
        self.galaxy
            .leaves()
            .map(|c| MapCell {
                cell: *c,
                rect: c.rect(),
                host: self.hosts.get(c).cloned(),
                load: self.loads.get(c).copied(),
            })
            .collect()
    }

    /// Headline numbers for a status line / HUD: how big the galaxy is and how busy.
    pub fn summary(&self) -> MapSummary {
        let cells = self.cells();
        let players: u32 = cells.iter().filter_map(|c| c.load.map(|l| l.players)).sum();
        let hosts: std::collections::BTreeSet<&String> = cells.iter().filter_map(|c| c.host.as_ref()).collect();
        let max_depth = cells.iter().map(|c| c.cell.depth).max().unwrap_or(0);
        let live_players = self.domains.values().map(|v| v.len()).sum();
        MapSummary { leaf_cells: cells.len(), players, host_nodes: hosts.len(), max_depth, live_players }
    }

    /// A compact ASCII quadtree dump for `spacegame galaxy` — the galaxy at a glance from a terminal.
    /// Each line is a leaf: its address, host, and a heat bar.
    pub fn ascii(&self, policy_players: u32, policy_tick_us: u32, policy_bps: u64) -> String {
        let mut cells = self.cells();
        cells.sort_by_key(|c| c.cell.morton());
        let mut out = String::new();
        let s = self.summary();
        out.push_str(&format!(
            "galaxy: {} leaf cells, {} players ({} bubbles), {} host nodes, depth {}\n",
            s.leaf_cells, s.players, s.live_players, s.host_nodes, s.max_depth
        ));
        for c in cells {
            let heat = c.heat(policy_players, policy_tick_us, policy_bps);
            let bars = (heat * 10.0) as usize;
            let bar: String = "#".repeat(bars) + &"-".repeat(10 - bars.min(10));
            let host = c.host.as_deref().map(|h| &h[..h.len().min(8)]).unwrap_or("unhosted");
            let pl = c.load.map(|l| l.players).unwrap_or(0);
            out.push_str(&format!("  {:<10} [{}] {:>4}p  {}\n", c.cell.token(), bar, pl, host));
        }
        out
    }
}

/// One-glance galaxy stats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapSummary {
    pub leaf_cells: usize,
    /// Players summed from per-cell host load reports (the coarse, grid view).
    pub players: u32,
    pub host_nodes: usize,
    pub max_depth: u8,
    /// Individual player bubbles tracked on the `/domains` layer (the precise, moving "who is where").
    pub live_players: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_event_reshapes_map_and_pulses() {
        let mut m = MapModel::new();
        let children: [CellId; 4] = CellId::ROOT.children().try_into().unwrap();
        m.on_shape(
            7,
            &ShapeOp::Split {
                cell: CellId::ROOT,
                children,
                place_on: ["a".into(), "b".into(), "c".into(), "d".into()],
                from_snapshot: "Qm".into(),
            },
        );
        assert_eq!(m.summary().leaf_cells, 4);
        assert!(matches!(m.events.back().unwrap().kind, MapEventKind::Split { .. }));
        // The ascii dump renders without panicking and names the children.
        let s = m.ascii(150, 35_000, 12_000_000);
        assert!(s.contains("1.0.0") || s.contains("1.1.1"));
    }

    #[test]
    fn domain_frames_track_moving_player_bubbles() {
        let mut m = MapModel::new();
        // Two hosts each report their players' bubbles.
        m.on_domains(DomainFrame {
            host: "h1".into(),
            domains: vec![
                DomainView { owner: "alice".into(), x: 10.0, y: 0.0, span: 200.0, entities: 3 },
                DomainView { owner: "bob".into(), x: 9000.0, y: 0.0, span: 100.0, entities: 1 },
            ],
        });
        m.on_domains(DomainFrame {
            host: "h2".into(),
            domains: vec![DomainView { owner: "cara".into(), x: -5000.0, y: 1.0, span: 50.0, entities: 1 }],
        });
        assert_eq!(m.summary().live_players, 3, "three player bubbles across two hosts");
        let dots = m.player_domains();
        assert_eq!(dots.iter().map(|d| d.owner.as_str()).collect::<Vec<_>>(), vec!["alice", "bob", "cara"]);

        // bob leaves: h1's next frame omits him. The bubble disappears without an explicit removal.
        m.on_domains(DomainFrame {
            host: "h1".into(),
            domains: vec![DomainView { owner: "alice".into(), x: 12.0, y: 0.0, span: 220.0, entities: 4 }],
        });
        assert_eq!(m.summary().live_players, 2, "bob dropped out when his host stopped reporting him");
        assert!(m.player_domains().iter().all(|d| d.owner != "bob"));

        // A host that empties out entirely is forgotten.
        m.on_domains(DomainFrame { host: "h2".into(), domains: vec![] });
        assert_eq!(m.summary().live_players, 1, "only alice remains");
    }
}
