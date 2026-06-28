//! End-to-end tests for **anchored / floating-origin coordinates** ([`spacegame::coords`]) wired through
//! the live [`Sim`]. The theme is *disagreeing servers*: two authoritative hosts may anchor the same patch
//! of the galaxy at **different floating origins** (the two sides of a seamless transit, neighbouring
//! sectors, a re-based domain). The contract these tests pin down:
//!
//! 1. Different origins, **same physical point ⇒ same canonical galaxy position** (`fixed8`).
//! 2. A ship crossing a seam is **globally continuous** — re-anchoring does not teleport it.
//! 3. A server that **mishandles the origin** lands its ship at a different galaxy position and is caught
//!    by the origin-invariant `state_hash` — the anti-cheat/quorum signal.
//! 4. The canonical `state_hash` of a whole world is **identical across hosts anchored differently**.
//! 5. Precision **survives at galaxy scale**, where a flat coordinate cannot.
//!
//! These drive the real `Sim` (transit path, physics, hashing), not the `coords` unit tests in isolation.

use std::sync::Arc;

use spacegame::coords::{Anchor, GalaxyPos, ANCHOR_SPAN};
use spacegame::ruleset::Ruleset;
use spacegame::shard::SectorId;
use spacegame::sim::{Intent, Sim, SECTOR_SIZE};

fn rules() -> Arc<Ruleset> {
    Arc::new(Ruleset::builtin())
}

/// Re-express a local offset in sector `(0,0)`'s frame as the local offset for sector `(sx,sy)`'s frame, so
/// the *physical* point is unchanged. This is what an honest neighbour host does when it adopts a position.
fn reframe_local(x: f32, y: f32, sx: i32, sy: i32) -> (f32, f32) {
    (x - sx as f32 * SECTOR_SIZE, y - sy as f32 * SECTOR_SIZE)
}

// ---------------------------------------------------------------------------------------------------
// 1. Two servers at different origins agree on a ship's galaxy position.
// ---------------------------------------------------------------------------------------------------

#[test]
fn two_servers_at_different_origins_agree_on_the_same_ship() {
    // Server A hosts sector (0,0); server B hosts sector (1,0). The SAME physical ship sits near their
    // shared seam: in A's frame it is local x≈8990; in B's frame that identical point is local x≈-10.
    let mut a = Sim::for_sector(SectorId::new(0, 0), rules());
    let mut b = Sim::for_sector(SectorId::new(1, 0), rules());
    a.join("n", "N", 0);
    b.join("n", "N", 0);
    {
        let s = a.ships.get_mut("n").unwrap();
        s.pos.x = 8990.0;
        s.pos.y = 1500.0;
    }
    let (bx, by) = reframe_local(8990.0, 1500.0, 1, 0);
    {
        let s = b.ships.get_mut("n").unwrap();
        s.pos.x = bx; // ≈ -10.0
        s.pos.y = by;
    }

    // Their *local* coordinates disagree wildly (8990 vs -10)...
    assert_ne!(a.ships["n"].pos.x, b.ships["n"].pos.x, "the two hosts genuinely hold different local coordinates");
    // ...but the canonical galaxy position is identical. This is the whole point of floating origin.
    assert_eq!(
        a.ship_galaxy_pos("n").unwrap().fixed8(),
        b.ship_galaxy_pos("n").unwrap().fixed8(),
        "different origins, same physical point => one canonical galaxy position",
    );
}

// ---------------------------------------------------------------------------------------------------
// 2. A real cross-seam transit is globally continuous (no teleport when the origin re-anchors).
// ---------------------------------------------------------------------------------------------------

#[test]
fn transit_across_the_seam_is_globally_continuous() {
    let mut a = Sim::for_sector(SectorId::new(0, 0), rules());
    a.join("p", "P", 0);
    a.factions.values_mut().for_each(|f| f.units.clear()); // no NPC fleet at the edge
    {
        let s = a.ships.get_mut("p").unwrap();
        s.pos.x = SECTOR_SIZE - 2.0;
        s.pos.y = 1500.0;
        s.a = 0.0;
        s.vx = 6.0;
    }

    // Drive east until the ship is handed off, recording its galaxy position right up to the seam.
    let mut last_global_in_a = a.ship_galaxy_pos("p").unwrap().global_f64();
    let mut transits = Vec::new();
    for _ in 0..6 {
        a.apply_intent("p", Intent { thrust: true, aim: Some(0.0), ..Default::default() }, 0);
        a.tick(1.0);
        if let Some(g) = a.ship_galaxy_pos("p") {
            last_global_in_a = g.global_f64();
        }
        transits = a.take_transits();
        if !transits.is_empty() {
            break;
        }
    }
    assert_eq!(transits.len(), 1, "the ship transited east");
    assert_eq!(transits[0].to, SectorId::new(1, 0));

    // The neighbour (a different origin) admits it and reports its galaxy position.
    let mut b = Sim::for_sector(SectorId::new(1, 0), rules());
    b.accept_transit(transits[0].ship.clone());
    let global_in_b = b.ship_galaxy_pos("p").unwrap().global_f64();

    // The handoff crosses a 9000-unit anchor boundary, yet the galaxy position barely moves: continuity is
    // a few ticks of velocity, NOT a span-sized jump. A broken re-anchor would show up as a ~9000 leap.
    let jump = ((global_in_b.0 - last_global_in_a.0).powi(2) + (global_in_b.1 - last_global_in_a.1).powi(2)).sqrt();
    assert!(jump < 30.0, "global position is continuous across the seam (jump={jump:.2})");
    assert!(jump < ANCHOR_SPAN as f64 * 0.1, "and is nothing like a span-sized teleport");
}

// ---------------------------------------------------------------------------------------------------
// 3. A server that mishandles the origin diverges and is caught by the canonical hash.
// ---------------------------------------------------------------------------------------------------

#[test]
fn a_server_that_botches_the_re_anchor_is_caught() {
    // Server A hands a ship east across the seam.
    let mut a = Sim::for_sector(SectorId::new(0, 0), rules());
    a.join("p", "P", 0);
    a.factions.values_mut().for_each(|f| f.units.clear());
    {
        let s = a.ships.get_mut("p").unwrap();
        s.pos.x = SECTOR_SIZE - 2.0;
        s.pos.y = 1500.0;
        s.vx = 6.0;
    }
    let mut transit = None;
    for _ in 0..6 {
        a.apply_intent("p", Intent { thrust: true, aim: Some(0.0), ..Default::default() }, 0);
        a.tick(1.0);
        if let Some(t) = a.take_transits().into_iter().next() {
            transit = Some(t);
            break;
        }
    }
    let snap = transit.expect("ship transited").ship;

    // Two neighbour servers receive the SAME handoff. The honest one adopts the wrapped local as given. The
    // buggy one "re-anchors" wrong — it adds the span back, double-counting the seam it just crossed (a real
    // floating-origin bug class). Both think they host sector (1,0).
    let mut honest = Sim::for_sector(SectorId::new(1, 0), rules());
    honest.accept_transit(snap.clone());

    let mut buggy = Sim::for_sector(SectorId::new(1, 0), rules());
    let mut bad = snap.clone();
    bad.pos.x += SECTOR_SIZE; // the mishandled re-anchor
    buggy.accept_transit(bad);

    // The honest server's galaxy position matches A's; the buggy one is a whole span away.
    let g_honest = honest.ship_galaxy_pos("p").unwrap();
    let g_buggy = buggy.ship_galaxy_pos("p").unwrap();
    let (dx, dy) = g_buggy.delta(g_honest);
    assert!((dx.abs() - ANCHOR_SPAN).abs() < 1.0 && dy.abs() < 1.0, "the buggy host is one span off (dx={dx})");

    // ...so their canonical positions differ, and therefore the origin-invariant state_hash differs: the
    // quorum/anti-cheat layer ([`spacegame::replication`]) outvotes the host that got the origin wrong.
    assert_ne!(g_honest.fixed8(), g_buggy.fixed8(), "mishandled origin => different canonical position");
    assert_ne!(honest.state_hash(), buggy.state_hash(), "and the divergence shows up in the authoritative hash");
}

// ---------------------------------------------------------------------------------------------------
// 4. The whole-world canonical hash is identical across hosts anchored at different origins.
// ---------------------------------------------------------------------------------------------------

#[test]
fn state_hash_is_identical_across_hosts_with_different_origins() {
    // The same multi-entity world, hosted by two servers whose frames differ by (sx,sy) = (4,-2). Each
    // server holds every entity at the local offset that places it at the SAME galaxy position. No tick:
    // this isolates the coordinate system (deterministic *content* — hazards/asteroids — is still keyed to
    // the sector today, so it is deliberately left out of this invariance check; folding content onto
    // global coordinates is the documented next step in LIVING-GALAXY.md).
    let build = |sx: i32, sy: i32| -> Sim {
        let mut s = Sim::for_sector(SectorId::new(sx, sy), rules());
        s.factions.clear(); // keep the comparison about positions, not spawned faction economies
        // Two ships placed by their galaxy position, expressed in this server's frame.
        for (id, gx, gy, vx, hp, min) in [("a", 1234.0_f32, 5678.0_f32, 3.5_f32, 88, 12u32), ("b", 8000.0, 200.0, -2.0, 41, 99)] {
            s.join(id, id, 0);
            let (lx, ly) = reframe_local(gx, gy, sx, sy);
            let sh = s.ships.get_mut(id).unwrap();
            sh.pos.x = lx;
            sh.pos.y = ly;
            sh.vx = vx;
            sh.hp = hp;
            sh.minerals = min;
        }
        s.factions.clear(); // join may re-add a player faction; drop it again so both servers match exactly
        s
    };

    let a = build(0, 0);
    let b = build(4, -2);

    // Sanity: the servers really are anchored differently and hold different local coordinates.
    assert_ne!(a.galaxy_frame(), b.galaxy_frame());
    assert_ne!(a.ships["a"].pos.x, b.ships["a"].pos.x);
    // Yet every entity is at the same galaxy position, so the canonical world hash is identical.
    assert_eq!(a.ship_galaxy_pos("a").unwrap().fixed8(), b.ship_galaxy_pos("a").unwrap().fixed8());
    assert_eq!(a.ship_galaxy_pos("b").unwrap().fixed8(), b.ship_galaxy_pos("b").unwrap().fixed8());
    assert_eq!(a.state_hash(), b.state_hash(), "origin-invariant: same world, different origins, one hash");
}

// ---------------------------------------------------------------------------------------------------
// 5. Precision survives at galaxy scale (where flat coordinates collapse).
// ---------------------------------------------------------------------------------------------------

#[test]
fn galaxy_scale_position_keeps_precision_through_the_sim() {
    // A server hosting a sector near the far edge of today's i32 sector grid (~1.9e13 m out — already far
    // past where a flat f32 world position dies, ~1e5 m). The anchor carries the magnitude; the local f32
    // keeps full sub-unit precision.
    let far = SectorId::new(2_000_000_000, -1_500_000_000);
    let mut s = Sim::for_sector(far, rules());
    s.join("n", "N", 0);
    {
        let sh = s.ships.get_mut("n").unwrap();
        sh.pos.x = 100.000;
        sh.pos.y = 4000.0;
    }
    let p1 = s.ship_galaxy_pos("n").unwrap();
    s.ships.get_mut("n").unwrap().pos.x = 100.125; // nudge by exactly one 1/8-unit quantum
    let p2 = s.ship_galaxy_pos("n").unwrap();
    assert_ne!(p1.fixed8(), p2.fixed8(), "1/8-unit precision is preserved even billions of cells out");

    // The full galaxy-scale anchor range lives in `coords` (i64), beyond the sim's current i32 sector — a
    // ship a hundred-thousand light-years out still resolves a quantum, which a flat f64 cannot.
    let galactic = Anchor::new(100_000_000_000_000_000, 0); // ~9e20 m ≈ galactic radius
    let q1 = GalaxyPos::new(galactic, 50.0, 0.0).fixed8();
    let q2 = GalaxyPos::new(galactic, 50.125, 0.0).fixed8();
    assert_ne!(q1, q2);
}
