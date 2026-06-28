//! Fail-first TDD for the **per-body floating-origin frame** in the physics world: the active integrator
//! must run near `local ≈ 0` no matter where in the galaxy the action is, and must **re-anchor during
//! flight**, not only at sector seams. These tests are written against the intended `World` API (a `frame`
//! anchor + global positions + mid-flight recenter); they go red on the old absolute-coordinate `World` and
//! green once floating origin lands.

use spacegame::coords::{Anchor, ANCHOR_SPAN};
use spacegame::physics::{Shape, Vec2, World, RigidBody};

fn body(px: f32, py: f32, vx: f32, vy: f32) -> RigidBody {
    let mut b = RigidBody::dynamic(Vec2::new(px, py), 1.0, Shape::Circle { r: 4.0 });
    b.vel = Vec2::new(vx, vy);
    b
}

/// Integration is **frame-invariant**: the same body, in a world framed at the origin and in a world framed
/// a hundred-thousand light-years out, follows the identical *local* trajectory. The galactic anchor carries
/// the magnitude; the integrator never sees a large number, so there is no precision penalty for being far.
#[test]
fn integration_is_frame_invariant() {
    let mut near = World::new();
    let ina = near.add(body(120.0, -40.0, 2.0, -1.0));

    let mut far = World::new();
    far.set_frame(Anchor::new(100_000_000_000_000_000, -50_000_000_000_000_000));
    let ifa = far.add(body(120.0, -40.0, 2.0, -1.0));

    for _ in 0..400 {
        near.step(1.0, Vec2::zero());
        far.step(1.0, Vec2::zero());
    }
    // Identical local motion despite a galactic frame difference (the active integrator is origin-agnostic).
    assert_eq!(near.bodies[ina].pos, far.bodies[ifa].pos, "frame must not perturb integration");
}

/// A 1/8-unit motion is **resolved at galactic distance**, which a flat absolute coordinate cannot do — the
/// frame holds the magnitude and the local float holds the precision.
#[test]
fn galactic_frame_preserves_sub_unit_motion() {
    let mut w = World::new();
    w.set_frame(Anchor::new(100_000_000_000_000_000, 0)); // ~9e20 m ≈ galactic radius
    let i = w.add(body(0.0, 0.0, 0.125, 0.0)); // one quantum per step
    let g0 = w.global_pos(i).fixed8();
    w.step(1.0, Vec2::zero());
    let g1 = w.global_pos(i).fixed8();
    assert_ne!(g0, g1, "a 1/8-unit step changes the canonical galaxy position even 9e20 m out");
}

/// **Rebasing during flight.** A body cruising for thousands of steps travels far in *global* space, yet its
/// *local* coordinate stays small because the world recenters its floating origin mid-flight. This is the
/// property the old absolute `World` lacked (local would grow without bound).
#[test]
fn flight_re_anchors_to_keep_local_small() {
    let mut w = World::new();
    let i = w.add(body(0.0, 0.0, 50.0, 30.0));
    let start = w.global_pos(i).global_f64();

    let mut max_local = 0.0f32;
    for _ in 0..2000 {
        w.step(1.0, Vec2::zero());
        let p = w.bodies[i].pos;
        max_local = max_local.max(p.x.abs().max(p.y.abs()));
    }

    let end = w.global_pos(i).global_f64();
    let travelled = ((end.0 - start.0).powi(2) + (end.1 - start.1).powi(2)).sqrt();
    assert!(travelled > 50_000.0, "the body genuinely flew across many cells (travelled={travelled:.0})");
    assert!(max_local < ANCHOR_SPAN * 1.5, "but the local coordinate stayed bounded (max_local={max_local:.0})");
    assert_ne!(w.frame(), Anchor::ORIGIN, "the floating origin followed the flight");
}
