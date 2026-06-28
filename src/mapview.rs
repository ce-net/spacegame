//! **Backend galaxy map** — aggregate the views every host gossips into one debuggable picture of the whole
//! galaxy, zoomable from ship scale to galaxy scale, with the broad-phase AABB nodes drawn in.
//!
//! The galaxy is fully distributed: no node holds it all, so the map can't fetch it all. Instead a
//! [`MapAggregator`] folds in a [`CellReport`] from each host (the sector/region it runs, its load, its
//! entities, and its broad-phase [`AabbNodeInfo`] boxes), and a [`ViewQuery`] pulls only a **bounded
//! window** around a camera: `reach` — *the slider* — says how far out (in world units) to gather, and
//! `max_cells` hard-caps the fetch so a galaxy-wide zoom can never pull unbounded data. What falls outside
//! is reported as `dropped`, never silently hidden.
//!
//! Everything in a [`GalaxyView`] is expressed **relative to the camera centre** as small `f32` offsets —
//! the view is itself a floating-origin frame (see [`crate::coords`]), so it stays precise whether the
//! camera is at the origin or a hundred-thousand light-years out. Level of detail follows `reach`: a
//! ship-scale view carries every entity dot and every AABB node; a galaxy-scale view carries only cell
//! summaries, so the payload is bounded at every zoom.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::aabb::{Aabb, AabbNodeInfo, AabbTree};
use crate::coords::{Anchor, GalaxyPos, ANCHOR_SPAN};
use crate::sim::Sim;

/// The single galaxy-wide pubsub topic every host publishes its [`CellReport`] on. The `/map` viewer
/// subscribes here and assembles the whole galaxy live from every friend's server at once — no central
/// aggregator, matching the rest of the mesh.
pub const MAP_TOPIC: &str = "spacegame/map";

/// What an entity dot represents on the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DotKind {
    Player,
    Npc,
    Rock,
}

/// One entity to draw, at its galaxy position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityDot {
    pub id: String,
    pub pos: GalaxyPos,
    pub kind: DotKind,
}

/// One host's report for one cell (the sector/region it authoritatively runs). Gossiped on the map topic
/// and folded into the [`MapAggregator`]. AABB node boxes are **local to the cell's anchor** (small `f32`,
/// galaxy-safe); entity dots carry their own anchored [`GalaxyPos`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellReport {
    pub anchor: Anchor,
    pub host: String,
    pub players: u32,
    pub entities: u32,
    pub tick_us: u32,
    pub aabb_nodes: Vec<AabbNodeInfo>,
    pub dots: Vec<EntityDot>,
}

impl CellReport {
    /// Turn a live [`Sim`] into the report its host gossips: the cell it runs, its load, its entities, and
    /// the broad-phase AABB nodes over its ships (the same recursive AABB the sim uses, here for the map).
    /// This is how a node "connects itself to the map" — one call per host per refresh.
    pub fn from_sim(sim: &Sim, host: impl Into<String>, tick_us: u32) -> Self {
        let anchor = sim.galaxy_frame();
        let mut dots = Vec::new();
        let mut items = Vec::new();
        for (id, s) in sim.ships.iter() {
            dots.push(EntityDot {
                id: id.clone(),
                pos: sim.galaxy_pos(s.pos.x, s.pos.y),
                kind: if s.owner.is_none() { DotKind::Player } else { DotKind::Npc },
            });
            items.push((Aabb::around(s.pos.x, s.pos.y, 18.0), id.clone()));
        }
        let bounds = Aabb::new(0.0, 0.0, ANCHOR_SPAN, ANCHOR_SPAN);
        let tree = AabbTree::build(bounds, items);
        CellReport {
            anchor,
            host: host.into(),
            players: sim.player_count() as u32,
            entities: sim.ships.len() as u32,
            tick_us,
            aabb_nodes: tree.node_boxes(),
            dots,
        }
    }
}

/// A host's full report — every cell it runs this refresh.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostReport {
    pub cells: Vec<CellReport>,
}

/// The zoom tier a view is rendered at, derived from `reach`. Controls level of detail so the payload stays
/// bounded as you zoom out: galaxy-scale carries no per-entity data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewScale {
    /// Close in: every entity dot and every broad-phase AABB node.
    Ship,
    /// Mid: entity dots, but not the fine AABB nodes.
    System,
    /// Far out: cell summaries and cell boxes only.
    Galaxy,
}

impl ViewScale {
    pub fn for_reach(reach: f64) -> ViewScale {
        let span = ANCHOR_SPAN as f64;
        if reach <= span * 3.0 {
            ViewScale::Ship
        } else if reach <= span * 80.0 {
            ViewScale::System
        } else {
            ViewScale::Galaxy
        }
    }
}

/// A camera query against the aggregated map. `reach` is the slider: how far out to gather. `max_cells`
/// hard-caps the fetch so a galaxy-wide query can never pull unbounded data.
#[derive(Debug, Clone)]
pub struct ViewQuery {
    pub center: GalaxyPos,
    pub reach: f64,
    pub max_cells: usize,
}

impl ViewQuery {
    pub fn new(center: GalaxyPos, reach: f64, max_cells: usize) -> Self {
        ViewQuery { center, reach, max_cells }
    }
}

/// A box relative to the view centre (small `f32`), tagged with the tree depth it came from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RelBox {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
    pub depth: u32,
    pub leaf: bool,
}

/// An entity dot relative to the view centre.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelDot {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub kind: DotKind,
}

/// One cell rendered into a view: its footprint and (LOD-permitting) its AABB nodes and entity dots, all
/// relative to the camera centre.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedCell {
    pub anchor: Anchor,
    pub host: String,
    pub players: u32,
    pub entities: u32,
    pub tick_us: u32,
    pub rect: RelBox,
    pub aabb_nodes: Vec<RelBox>,
    pub dots: Vec<RelDot>,
    /// Distance of the cell centre from the camera, world units (for ordering / fade).
    pub dist: f64,
}

/// The assembled, bounded, camera-relative map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalaxyView {
    pub center: GalaxyPos,
    pub reach: f64,
    pub scale: ViewScale,
    pub cells: Vec<RenderedCell>,
    /// Cells actually returned (== `cells.len()`).
    pub fetched: usize,
    /// Cells that were within reach but cut by `max_cells` — surfaced, not hidden.
    pub dropped: usize,
    pub total_players: u32,
}

/// Aggregates the latest [`CellReport`] per cell from every host into one queryable galaxy map.
#[derive(Debug, Default, Clone)]
pub struct MapAggregator {
    cells: BTreeMap<Anchor, CellReport>,
}

impl MapAggregator {
    pub fn new() -> Self {
        MapAggregator::default()
    }

    /// Fold one cell report in (latest-wins per cell, so a host's fresh report replaces its prior one).
    pub fn ingest(&mut self, report: CellReport) {
        self.cells.insert(report.anchor, report);
    }

    /// Fold a whole host report in.
    pub fn ingest_host(&mut self, report: HostReport) {
        for c in report.cells {
            self.ingest(c);
        }
    }

    /// Drop a cell that has gone dark (its host stopped reporting it).
    pub fn forget(&mut self, anchor: Anchor) {
        self.cells.remove(&anchor);
    }

    /// How many cells the aggregator currently knows about across the whole galaxy.
    pub fn known_cells(&self) -> usize {
        self.cells.len()
    }

    /// Build a bounded, camera-relative view. Selects every known cell whose footprint reaches within
    /// `reach` of the centre, sorts by distance, keeps the nearest `max_cells` (reporting the rest as
    /// `dropped`), and attaches AABB nodes / entity dots per the zoom tier.
    pub fn view(&self, q: &ViewQuery) -> GalaxyView {
        let scale = ViewScale::for_reach(q.reach);
        let (cgx, cgy) = q.center.global_f64();
        let half = ANCHOR_SPAN as f64 * 0.5;
        let half_diag = half * std::f64::consts::SQRT_2;

        // Candidate cells: footprint within reach of the camera.
        let mut hits: Vec<(f64, &CellReport)> = Vec::new();
        for cr in self.cells.values() {
            let (ox, oy) = cr.anchor.origin_f64();
            let (ccx, ccy) = (ox + half, oy + half);
            let d = ((ccx - cgx).powi(2) + (ccy - cgy).powi(2)).sqrt();
            if d - half_diag <= q.reach {
                hits.push((d, cr));
            }
        }
        hits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.anchor.cmp(&b.1.anchor)));
        let dropped = hits.len().saturating_sub(q.max_cells);
        hits.truncate(q.max_cells);

        let mut cells = Vec::with_capacity(hits.len());
        let mut total_players = 0u32;
        for (d, cr) in hits {
            total_players += cr.players;
            let (ox, oy) = cr.anchor.origin_f64();
            let rel = |gx: f64, gy: f64| ((gx - cgx) as f32, (gy - cgy) as f32);
            let (minx, miny) = rel(ox, oy);
            let (maxx, maxy) = rel(ox + ANCHOR_SPAN as f64, oy + ANCHOR_SPAN as f64);
            let rect = RelBox { min_x: minx, min_y: miny, max_x: maxx, max_y: maxy, depth: 0, leaf: true };

            let aabb_nodes = if scale == ViewScale::Ship {
                cr.aabb_nodes
                    .iter()
                    .map(|n| {
                        let (nx0, ny0) = rel(ox + n.region.min_x as f64, oy + n.region.min_y as f64);
                        let (nx1, ny1) = rel(ox + n.region.max_x as f64, oy + n.region.max_y as f64);
                        RelBox { min_x: nx0, min_y: ny0, max_x: nx1, max_y: ny1, depth: n.depth, leaf: n.leaf }
                    })
                    .collect()
            } else {
                Vec::new()
            };

            let dots = if scale != ViewScale::Galaxy {
                cr.dots
                    .iter()
                    .map(|dot| {
                        let (dx, dy) = dot.pos.delta(q.center);
                        RelDot { id: dot.id.clone(), x: dx, y: dy, kind: dot.kind }
                    })
                    .collect()
            } else {
                Vec::new()
            };

            cells.push(RenderedCell {
                anchor: cr.anchor,
                host: cr.host.clone(),
                players: cr.players,
                entities: cr.entities,
                tick_us: cr.tick_us,
                rect,
                aabb_nodes,
                dots,
                dist: d,
            });
        }

        GalaxyView { center: q.center, reach: q.reach, scale, fetched: cells.len(), cells, dropped, total_players }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(ax: i64, ay: i64, host: &str, players: u32) -> CellReport {
        CellReport {
            anchor: Anchor::new(ax, ay),
            host: host.into(),
            players,
            entities: players,
            tick_us: 1000,
            aabb_nodes: Vec::new(),
            dots: Vec::new(),
        }
    }

    /// The reach slider bounds what is fetched: a tight reach pulls only nearby cells; widening it pulls more.
    #[test]
    fn reach_slider_bounds_the_fetch() {
        let mut m = MapAggregator::new();
        for ax in [0i64, 1, 5, 50, 500] {
            m.ingest(report(ax, 0, "h", 1));
        }
        let center = GalaxyPos::at(Anchor::ORIGIN);
        let span = ANCHOR_SPAN as f64;

        let tight = m.view(&ViewQuery::new(center, span * 2.0, 999));
        let cells: Vec<i64> = tight.cells.iter().map(|c| c.anchor.ax).collect();
        assert_eq!(cells, vec![0, 1], "a tight reach pulls only the adjacent cells");

        let wide = m.view(&ViewQuery::new(center, span * 600.0, 999));
        assert_eq!(wide.fetched, 5, "a wide reach pulls the whole arm");
        assert!(wide.cells.windows(2).all(|w| w[0].dist <= w[1].dist), "cells come back nearest-first");
    }

    /// `max_cells` hard-caps the fetch and the overflow is reported, never silently dropped.
    #[test]
    fn max_cells_caps_and_reports_the_overflow() {
        let mut m = MapAggregator::new();
        for ax in 0..20 {
            m.ingest(report(ax, 0, "h", 1));
        }
        let v = m.view(&ViewQuery::new(GalaxyPos::at(Anchor::ORIGIN), ANCHOR_SPAN as f64 * 1000.0, 5));
        assert_eq!(v.fetched, 5, "only max_cells are returned");
        assert_eq!(v.dropped, 15, "the rest are surfaced as dropped");
        assert!(v.cells.iter().all(|c| c.anchor.ax < 5), "and they are the nearest five");
    }

    /// Zoom tier follows reach: ship-scale carries AABB nodes + dots; galaxy-scale carries neither.
    #[test]
    fn scale_controls_level_of_detail() {
        let mut m = MapAggregator::new();
        let mut r = report(0, 0, "h", 2);
        r.aabb_nodes = vec![AabbNodeInfo { region: Aabb::new(10.0, 10.0, 50.0, 50.0), depth: 0, leaf: true, count: 2 }];
        r.dots = vec![EntityDot { id: "p".into(), pos: GalaxyPos::new(Anchor::ORIGIN, 100.0, 200.0), kind: DotKind::Player }];
        m.ingest(r);
        let center = GalaxyPos::new(Anchor::ORIGIN, 100.0, 200.0);

        let ship = m.view(&ViewQuery::new(center, ANCHOR_SPAN as f64 * 1.0, 99));
        assert_eq!(ship.scale, ViewScale::Ship);
        assert_eq!(ship.cells[0].aabb_nodes.len(), 1, "ship scale draws AABB nodes");
        assert_eq!(ship.cells[0].dots.len(), 1, "ship scale draws dots");
        // The dot sits at the camera centre, so its relative position is ~0.
        assert!(ship.cells[0].dots[0].x.abs() < 0.01 && ship.cells[0].dots[0].y.abs() < 0.01);

        let galaxy = m.view(&ViewQuery::new(center, ANCHOR_SPAN as f64 * 500.0, 99));
        assert_eq!(galaxy.scale, ViewScale::Galaxy);
        assert!(galaxy.cells[0].aabb_nodes.is_empty(), "galaxy scale drops fine AABB nodes");
        assert!(galaxy.cells[0].dots.is_empty(), "galaxy scale drops per-entity dots");
    }

    /// Many friends, each running their own server, fold into one galaxy map; the served view serialises to
    /// JSON and back unchanged. This is the multi-server picture the frontend renders.
    #[test]
    fn many_servers_merge_into_one_map_and_serialise() {
        let mut m = MapAggregator::new();
        // Three friends, each hosting a different region of the shared galaxy.
        for (host, ax) in [("alice", 0i64), ("bob", 1), ("cara", 2)] {
            let mut r = report(ax, 0, host, 4);
            r.dots = vec![EntityDot { id: format!("{host}-ship"), pos: GalaxyPos::new(Anchor::new(ax, 0), 4500.0, 4500.0), kind: DotKind::Player }];
            m.ingest(r);
        }
        assert_eq!(m.known_cells(), 3, "one merged map across three servers");
        let view = m.view(&ViewQuery::new(GalaxyPos::at(Anchor::ORIGIN), ANCHOR_SPAN as f64 * 5.0, 64));
        assert_eq!(view.fetched, 3);
        let hosts: std::collections::BTreeSet<&str> = view.cells.iter().map(|c| c.host.as_str()).collect();
        assert_eq!(hosts.len(), 3, "every friend's server shows on the map");

        // The served view round-trips through JSON unchanged (the wire to the web frontend).
        let json = serde_json::to_string(&view).unwrap();
        let back: GalaxyView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.fetched, view.fetched);
        assert_eq!(back.cells.len(), view.cells.len());
        assert_eq!(back.total_players, view.total_players);
    }

    /// The view stays precise at galactic distance: a camera and a cell a hundred-thousand light-years out
    /// still produce small, correct camera-relative coordinates (the view is its own floating origin).
    #[test]
    fn view_is_precise_at_galactic_distance() {
        let far = Anchor::new(100_000_000_000_000_000, 0);
        let mut m = MapAggregator::new();
        let mut r = report(far.ax, far.ay, "h", 1);
        r.dots = vec![EntityDot { id: "p".into(), pos: GalaxyPos::new(far, 4500.0, 4500.0), kind: DotKind::Player }];
        m.ingest(r);
        // Camera at the cell centre.
        let center = GalaxyPos::new(far, 4500.0, 4500.0);
        let v = m.view(&ViewQuery::new(center, ANCHOR_SPAN as f64 * 1.0, 9));
        assert_eq!(v.fetched, 1);
        assert!(v.cells[0].dots[0].x.abs() < 1.0, "dot is at the camera, sub-unit precise 9e20 m out");
    }
}
