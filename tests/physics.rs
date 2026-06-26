//! Integration tests for the 2D rigid-body physics + LOD, driven through the public API.

use spacegame::physics::{assign_lod, Lod, RigidBody, Shape, Vec2, World};

fn step_n(w: &mut World, focus: &[Vec2], gravity: Vec2, n: usize) {
    for _ in 0..n {
        assign_lod(&mut w.bodies, focus, 1e6, 1e6, 1e6);
        w.step(1.0 / 60.0, gravity);
    }
}

#[test]
fn elastic_circles_transfer_momentum() {
    let mut w = World::new();
    let a = w.add({
        let mut b = RigidBody::dynamic(Vec2::new(0.0, 0.0), 1.0, Shape::Circle { r: 5.0 });
        b.vel = Vec2::new(12.0, 0.0);
        b.restitution = 1.0;
        b.friction = 0.0;
        b
    });
    let bb = w.add({
        let mut b = RigidBody::dynamic(Vec2::new(9.0, 0.0), 1.0, Shape::Circle { r: 5.0 });
        b.restitution = 1.0;
        b.friction = 0.0;
        b
    });
    let p0 = w.bodies[a].vel.x + w.bodies[bb].vel.x;
    step_n(&mut w, &[Vec2::zero()], Vec2::zero(), 30);
    let p1 = w.bodies[a].vel.x + w.bodies[bb].vel.x;
    assert!((p0 - p1).abs() < 1.5, "linear momentum ~conserved: {p0} vs {p1}");
    assert!(w.bodies[bb].vel.x > 0.5, "the struck circle moves off");
}

#[test]
fn a_box_rests_on_a_static_floor() {
    let mut w = World::new();
    w.add(RigidBody::statik(Vec2::new(0.0, 0.0), Shape::box_hw(1000.0, 10.0)));
    let ball = w.add(RigidBody::dynamic(Vec2::new(0.0, 80.0), 1.0, Shape::Circle { r: 8.0 }));
    step_n(&mut w, &[Vec2::zero()], Vec2::new(0.0, -300.0), 300);
    let y = w.bodies[ball].pos.y;
    assert!(y > 10.0 && y < 32.0, "the ball settled on the floor (~18), got {y}");
}

#[test]
fn off_centre_impact_imparts_spin() {
    let mut w = World::new();
    let boxi = w.add(RigidBody::dynamic(Vec2::new(0.0, 0.0), 5.0, Shape::box_hw(20.0, 20.0)));
    w.add({
        let mut b = RigidBody::dynamic(Vec2::new(-45.0, 16.0), 1.0, Shape::Circle { r: 4.0 });
        b.vel = Vec2::new(45.0, 0.0);
        b.restitution = 0.6;
        b
    });
    step_n(&mut w, &[Vec2::zero()], Vec2::zero(), 30);
    assert!(w.bodies[boxi].ang_vel.abs() > 1e-3, "off-centre hit spun the box: {}", w.bodies[boxi].ang_vel);
}

#[test]
fn lod_far_bodies_are_registered_and_only_integrate() {
    // No focus point -> everything is Registered (no contact solve): two overlapping circles coast
    // through each other instead of bouncing, proving the cheap far-LOD path runs.
    let mut w = World::new();
    let a = w.add({
        let mut b = RigidBody::dynamic(Vec2::new(0.0, 0.0), 1.0, Shape::Circle { r: 5.0 });
        b.vel = Vec2::new(2.0, 0.0);
        b
    });
    w.add(RigidBody::dynamic(Vec2::new(6.0, 0.0), 1.0, Shape::Circle { r: 5.0 }));
    for _ in 0..10 {
        assign_lod(&mut w.bodies, &[], 100.0, 400.0, 1000.0);
        w.step(1.0 / 60.0, Vec2::zero());
    }
    assert_eq!(w.bodies[a].lod, Lod::Registered);
    assert!((w.bodies[a].vel.x - 2.0).abs() < 1e-3, "a registered body just coasts (no impulse)");
}

#[test]
fn assign_lod_tiers_by_distance() {
    let mut bodies = vec![
        RigidBody::dynamic(Vec2::new(0.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
        RigidBody::dynamic(Vec2::new(200.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
        RigidBody::dynamic(Vec2::new(700.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
        RigidBody::dynamic(Vec2::new(9000.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
    ];
    assign_lod(&mut bodies, &[Vec2::zero()], 100.0, 400.0, 1000.0);
    assert_eq!(bodies[0].lod, Lod::High);
    assert_eq!(bodies[1].lod, Lod::Medium);
    assert_eq!(bodies[2].lod, Lod::Low);
    assert_eq!(bodies[3].lod, Lod::Registered);
}

#[test]
fn retain_removes_bodies_and_keeps_the_broadphase_consistent() {
    let mut w = World::new();
    for i in 0..20 {
        w.add(RigidBody::dynamic(Vec2::new(i as f32 * 30.0, 0.0), 1.0, Shape::Circle { r: 3.0 }));
    }
    w.retain(|b| b.pos.x < 150.0); // keep ~5
    assert!(w.bodies.len() <= 6 && !w.bodies.is_empty());
    // A step still runs cleanly after the bulk removal (proxies were rebuilt).
    step_n(&mut w, &[Vec2::zero()], Vec2::zero(), 3);
}
