//! Zero-dependency benchmark suite for the spacegame backend — "benchmarks for everything". Uses only
//! `std::time`, so it adds no dependencies and runs on stable. Build against the pure SDK to skip the mesh
//! git dependencies:
//!
//! ```text
//! cargo bench --no-default-features
//! ```
//!
//! Each line reports ns/op and ops/s for a hot path that the 1:1-galaxy work leans on: anchored-coordinate
//! canonicalisation, the origin-invariant state hash, the floating-origin physics step + recenter, the
//! recursive-AABB broad phase, and the distributed map aggregation/query.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use spacegame::aabb::{Aabb, AabbTree};
use spacegame::coords::{Anchor, GalaxyPos, ANCHOR_SPAN};
use spacegame::mapview::{CellReport, MapAggregator, ViewQuery};
use spacegame::physics::{RigidBody, Shape, Vec2, World};
use spacegame::ruleset::Ruleset;
use spacegame::shard::SectorId;
use spacegame::sim::Sim;

fn bench(name: &str, iters: u64, mut f: impl FnMut()) {
    for _ in 0..(iters / 10).max(1) {
        f(); // warmup
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = t.elapsed().as_nanos() as f64 / iters as f64;
    let ops = if ns > 0.0 { 1e9 / ns } else { f64::INFINITY };
    println!("{name:<44} {ns:>12.1} ns/op  {ops:>16.0} ops/s  ({iters} iters)");
}

fn section(title: &str) {
    println!("\n== {title} ==");
}

fn main() {
    section("coords — anchored / floating-origin coordinates");
    let anchor = Anchor::new(123_456_789_012, -987_654_321);
    bench("coords::fixed8 (galactic anchor)", 5_000_000, || {
        black_box(GalaxyPos::new(anchor, 1234.5, 6789.25).fixed8());
    });
    bench("coords::normalize", 5_000_000, || {
        black_box(GalaxyPos::new(anchor, 91234.5, -6789.25).normalize());
    });
    bench("coords::delta (cross-frame)", 5_000_000, || {
        black_box(GalaxyPos::new(anchor, 10.0, 20.0).delta(GalaxyPos::new(anchor.offset(1, 0), 5.0, 8.0)));
    });

    section("sim — origin-invariant state hash + tick");
    let mut s = Sim::for_sector(SectorId::new(3, 1), Arc::new(Ruleset::builtin()));
    for i in 0..500u32 {
        let id = format!("s{i}");
        s.join(&id, &id, i.wrapping_mul(7));
        if let Some(sh) = s.ships.get_mut(&id) {
            sh.pos.x = (i.wrapping_mul(37) % 8800 + 100) as f32;
            sh.pos.y = (i.wrapping_mul(53) % 8800 + 100) as f32;
        }
    }
    bench("sim::state_hash (500 ships)", 3000, || {
        black_box(s.state_hash());
    });
    let mut st = Sim::for_sector(SectorId::new(2, 2), Arc::new(Ruleset::builtin()));
    st.seamless = false;
    for i in 0..200u32 {
        let id = format!("t{i}");
        st.join(&id, &id, i);
        if let Some(sh) = st.ships.get_mut(&id) {
            sh.pos.x = (i.wrapping_mul(37) % 8800 + 100) as f32;
            sh.pos.y = (i.wrapping_mul(53) % 8800 + 100) as f32;
        }
    }
    bench("sim::tick (200 ships, full)", 500, || {
        st.tick(1.0);
    });

    section("physics — floating-origin step + recenter");
    let mut w = World::new();
    for i in 0..1000i32 {
        let b = RigidBody::dynamic(Vec2::new((i % 100 * 80) as f32, (i / 100 * 80) as f32), 1.0, Shape::Circle { r: 6.0 });
        w.add(b);
    }
    bench("physics::step (1000 bodies)", 300, || {
        w.step(1.0 / 60.0, Vec2::zero());
    });
    bench("physics::recenter (1000 bodies)", 50_000, || {
        black_box(w.recenter());
    });

    section("aabb — recursive broad phase + node introspection");
    let bounds = Aabb::new(0.0, 0.0, ANCHOR_SPAN, ANCHOR_SPAN);
    let items: Vec<(Aabb, usize)> =
        (0..2000usize).map(|i| (Aabb::around((i % 50 * 180) as f32, (i / 50 * 180) as f32, 10.0), i)).collect();
    bench("aabb::build (2000 boxes)", 3000, || {
        black_box(AabbTree::build(bounds, items.clone()));
    });
    let tree = AabbTree::build(bounds, items.clone());
    bench("aabb::query (2000 boxes)", 500_000, || {
        black_box(tree.query(&Aabb::around(4000.0, 4000.0, 300.0)));
    });
    bench("aabb::node_boxes (2000 boxes)", 30_000, || {
        black_box(tree.node_boxes());
    });

    section("mapview — distributed map aggregation + reach query");
    let mut agg = MapAggregator::new();
    for ax in -30..30i64 {
        for ay in -30..30i64 {
            agg.ingest(CellReport {
                anchor: Anchor::new(ax, ay),
                host: "h".into(),
                players: ((ax + ay) & 7) as u32,
                entities: 5,
                tick_us: 1000,
                aabb_nodes: Vec::new(),
                dots: Vec::new(),
            });
        }
    }
    let center = GalaxyPos::at(Anchor::ORIGIN);
    let near = ViewQuery::new(center, ANCHOR_SPAN as f64 * 4.0, 256);
    let wide = ViewQuery::new(center, ANCHOR_SPAN as f64 * 40.0, 256);
    println!("(aggregator holds {} cells)", agg.known_cells());
    bench("mapview::view (3600 cells, ship reach)", 20_000, || {
        black_box(agg.view(&near));
    });
    bench("mapview::view (3600 cells, galaxy reach)", 5_000, || {
        black_box(agg.view(&wide));
    });
    bench("mapview::CellReport::from_sim (200 ships)", 5_000, || {
        black_box(CellReport::from_sim(&st, "h", 1000));
    });

    println!("\ndone.");
}
