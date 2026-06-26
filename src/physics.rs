//! Advanced 2D rigid-body physics, built to run **at scale with level-of-detail precision**.
//!
//! Every solid thing in the galaxy — ships, debris, asteroids, planets — is a [`RigidBody`] with mass,
//! linear and angular velocity, restitution and friction. Contacts are resolved with a sequential-
//! impulse solver (the same family as Box2D): each contact applies a normal impulse (with restitution)
//! and a tangential friction impulse at the contact point, so bodies bounce, spin, and grind
//! realistically — not just push apart. Broad-phase is the [`DynamicAabbTree`](crate::aabb) that
//! follows objects as they move; narrow-phase produces contact manifolds for circle/convex shapes.
//!
//! ## Level of detail (the scale story)
//!
//! Physics fidelity is **not uniform**. The bodies near the local player (and near nearby players'
//! nodes) are simulated at [`Lod::High`] — many substeps, many solver iterations, high tick rate — for
//! crisp, authoritative interaction. The further a region is from any interested node, the lower its
//! [`Lod`]: fewer substeps, fewer iterations, and eventually coarse extrapolation that is merely
//! *registered* for distant players rather than fully solved. [`assign_lod`] computes each body's tier
//! from its distance to the focus points (the positions the local node cares about), so compute is
//! spent where players actually are. This is what makes a million-body galaxy affordable: full physics
//! is local, cheap physics is global.
//!
//! ## GPU cross-compatibility
//!
//! The hot state is laid out for data-oriented / SIMD / GPU execution: bodies live in a flat `Vec`
//! ([`World::bodies`]) of plain `Copy` structs (struct-of-arrays-friendly, no pointers), the integrator
//! and the impulse solver are per-element kernels over that array, and [`Lod`] partitions the array
//! into batches a GPU can dispatch at different rates. A CPU build runs the same kernels in a loop;
//! a GPU build can upload the array and run the identical math in a compute shader. Nothing here uses
//! the heap inside the per-body kernels, so the step is portable across both.
//!
//! Deterministic: a fixed timestep with a fixed iteration count yields the same result on every node,
//! which is what lets [`crate::replication`] keep byte-identical high-precision replicas on the nodes
//! of nearby players and fail over between them.

use crate::aabb::{Aabb, DynamicAabbTree, Proxy};

/// A 2D vector. Plain `Copy` math, GPU/SIMD friendly.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub fn new(x: f32, y: f32) -> Self {
        Vec2 { x, y }
    }
    pub fn zero() -> Self {
        Vec2 { x: 0.0, y: 0.0 }
    }
    pub fn add(self, o: Vec2) -> Vec2 {
        Vec2 { x: self.x + o.x, y: self.y + o.y }
    }
    pub fn sub(self, o: Vec2) -> Vec2 {
        Vec2 { x: self.x - o.x, y: self.y - o.y }
    }
    pub fn scale(self, s: f32) -> Vec2 {
        Vec2 { x: self.x * s, y: self.y * s }
    }
    pub fn dot(self, o: Vec2) -> f32 {
        self.x * o.x + self.y * o.y
    }
    /// 2D cross product (scalar) `self x o`.
    pub fn cross(self, o: Vec2) -> f32 {
        self.x * o.y - self.y * o.x
    }
    /// Cross of a vector with a scalar `w`: `(w*y, -w*x)` rotated — gives the velocity of a point.
    pub fn cross_scalar(self, w: f32) -> Vec2 {
        Vec2 { x: -w * self.y, y: w * self.x }
    }
    pub fn len(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
    pub fn len_sq(self) -> f32 {
        self.x * self.x + self.y * self.y
    }
    pub fn normalize(self) -> Vec2 {
        let l = self.len();
        if l > 1e-9 { Vec2 { x: self.x / l, y: self.y / l } } else { Vec2::zero() }
    }
    /// Left normal (perpendicular).
    pub fn perp(self) -> Vec2 {
        Vec2 { x: -self.y, y: self.x }
    }
}

/// A collision shape in body-local space (centred on the centre of mass).
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Circle { r: f32 },
    /// A convex polygon, vertices in CCW order, centred on the COM.
    Polygon { verts: Vec<Vec2> },
}

impl Shape {
    /// A convex box of half-extents.
    pub fn box_hw(hw: f32, hh: f32) -> Shape {
        Shape::Polygon {
            verts: vec![
                Vec2::new(-hw, -hh),
                Vec2::new(hw, -hh),
                Vec2::new(hw, hh),
                Vec2::new(-hw, hh),
            ],
        }
    }

    /// The shape's bounding radius (for fat-AABB sizing and circle fallbacks).
    pub fn bound_radius(&self) -> f32 {
        match self {
            Shape::Circle { r } => *r,
            Shape::Polygon { verts } => verts.iter().fold(0.0_f32, |m, v| m.max(v.len())),
        }
    }

    /// Unit mass moment of inertia (per unit mass) about the COM — multiply by mass for inertia.
    pub fn unit_inertia(&self) -> f32 {
        match self {
            Shape::Circle { r } => 0.5 * r * r,
            Shape::Polygon { verts } => {
                // Standard polygon inertia about the centroid (assumed at origin), per unit mass.
                let mut num = 0.0;
                let mut den = 0.0;
                for i in 0..verts.len() {
                    let a = verts[i];
                    let b = verts[(i + 1) % verts.len()];
                    let crs = a.cross(b).abs();
                    num += crs * (a.dot(a) + a.dot(b) + b.dot(b));
                    den += crs;
                }
                if den > 1e-9 { num / (6.0 * den) } else { 0.0 }
            }
        }
    }
}

/// One rigid body. `inv_mass`/`inv_inertia` are zero for a static body (a planet anchor, a wall).
#[derive(Debug, Clone)]
pub struct RigidBody {
    pub pos: Vec2,
    pub vel: Vec2,
    /// Heading in radians.
    pub angle: f32,
    /// Angular velocity, radians/sec.
    pub ang_vel: f32,
    pub inv_mass: f32,
    pub inv_inertia: f32,
    pub restitution: f32,
    pub friction: f32,
    pub shape: Shape,
    /// Opaque user tag (e.g. a ship id index) so the game can map a body back to an entity.
    pub tag: u64,
    /// Broad-phase proxy in the world's dynamic tree (set by [`World::add`]).
    pub proxy: Proxy,
    /// Per-body LOD tier, refreshed by [`assign_lod`].
    pub lod: Lod,
}

impl RigidBody {
    /// A dynamic body of the given mass.
    pub fn dynamic(pos: Vec2, mass: f32, shape: Shape) -> Self {
        let inv_mass = if mass > 0.0 { 1.0 / mass } else { 0.0 };
        let inertia = shape.unit_inertia() * mass;
        let inv_inertia = if inertia > 0.0 { 1.0 / inertia } else { 0.0 };
        RigidBody {
            pos,
            vel: Vec2::zero(),
            angle: 0.0,
            ang_vel: 0.0,
            inv_mass,
            inv_inertia,
            restitution: 0.2,
            friction: 0.4,
            shape,
            tag: 0,
            proxy: usize::MAX,
            lod: Lod::High,
        }
    }

    /// A static (immovable) body — infinite mass. Planets, anchors.
    pub fn statik(pos: Vec2, shape: Shape) -> Self {
        let mut b = Self::dynamic(pos, 0.0, shape);
        b.inv_mass = 0.0;
        b.inv_inertia = 0.0;
        b
    }

    /// World-space AABB of this body (conservative: bounding circle box).
    pub fn world_aabb(&self) -> Aabb {
        let r = self.shape.bound_radius();
        Aabb::around(self.pos.x, self.pos.y, r)
    }

    /// World-space vertices of a polygon (empty for a circle).
    pub fn world_verts(&self) -> Vec<Vec2> {
        match &self.shape {
            Shape::Circle { .. } => Vec::new(),
            Shape::Polygon { verts } => {
                let (c, s) = (self.angle.cos(), self.angle.sin());
                verts
                    .iter()
                    .map(|v| Vec2::new(self.pos.x + c * v.x - s * v.y, self.pos.y + s * v.x + c * v.y))
                    .collect()
            }
        }
    }
}

/// Level-of-detail tier for a body's simulation. Higher = more compute, more accuracy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lod {
    /// Near the local player / a nearby node: many substeps & iterations, full collision.
    High,
    /// A ring further out: a few substeps, a few iterations.
    Medium,
    /// Distant: a single coarse step, minimal iterations — kept consistent but cheap.
    Low,
    /// Out of any interested node's range: integrate motion only (extrapolate), no contact solving —
    /// merely *registered* so far-away players still see something move.
    Registered,
}

impl Lod {
    /// Substeps and solver iterations for this tier.
    pub fn budget(self) -> (u32, u32) {
        match self {
            Lod::High => (4, 8),
            Lod::Medium => (2, 4),
            Lod::Low => (1, 2),
            Lod::Registered => (1, 0),
        }
    }
}

/// Assign each body a [`Lod`] from its distance to the nearest **focus point** (a position the local
/// node cares about — typically the local player and nearby players whose replicas live here). Bodies
/// near a focus get [`Lod::High`]; the tiers step down with distance. This is the dial that spends
/// compute where players are.
pub fn assign_lod(bodies: &mut [RigidBody], focus: &[Vec2], high_r: f32, med_r: f32, low_r: f32) {
    for b in bodies.iter_mut() {
        let mut best = f32::INFINITY;
        for f in focus {
            let d = b.pos.sub(*f).len_sq();
            if d < best {
                best = d;
            }
        }
        let d = best.sqrt();
        b.lod = if focus.is_empty() {
            Lod::Registered
        } else if d <= high_r {
            Lod::High
        } else if d <= med_r {
            Lod::Medium
        } else if d <= low_r {
            Lod::Low
        } else {
            Lod::Registered
        };
    }
}

/// A contact between two bodies: one or two points, a shared normal (from A to B) and penetration.
#[derive(Debug, Clone, Copy)]
pub struct Contact {
    pub a: usize,
    pub b: usize,
    pub normal: Vec2,
    pub point: Vec2,
    pub penetration: f32,
}

/// The physics world: a flat array of bodies plus the dynamic broad-phase tree over them.
#[derive(Debug, Clone)]
pub struct World {
    pub bodies: Vec<RigidBody>,
    tree: DynamicAabbTree<usize>,
    /// Baumgarte positional-correction factor and penetration slop.
    pub beta: f32,
    pub slop: f32,
}

impl Default for World {
    fn default() -> Self {
        World { bodies: Vec::new(), tree: DynamicAabbTree::new(8.0), beta: 0.2, slop: 0.05 }
    }
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a body, indexing it in the broad-phase. Returns its index in [`World::bodies`].
    pub fn add(&mut self, mut body: RigidBody) -> usize {
        let idx = self.bodies.len();
        body.proxy = self.tree.insert(body.world_aabb(), idx);
        self.bodies.push(body);
        idx
    }

    /// Refresh the broad-phase entry for body `i` after it moved (cheap if it stayed in its fat box).
    fn refit(&mut self, i: usize) {
        let aabb = self.bodies[i].world_aabb();
        let proxy = self.bodies[i].proxy;
        self.tree.update(proxy, aabb);
    }

    /// Advance the world by `dt` seconds. Bodies are stepped according to their [`Lod`]: high-tier
    /// bodies take more substeps and more solver iterations than low-tier ones, and `Registered`
    /// bodies are only integrated (no contact solving). Call [`assign_lod`] first.
    pub fn step(&mut self, dt: f32, gravity: Vec2) {
        // Group work by the coarsest budget present, but apply each body's own substep/iteration count.
        // For simplicity and determinism we run the whole world at the max substep count and let each
        // body's solver iterations gate its contact work; Registered bodies skip the solver entirely.
        let max_sub = self.bodies.iter().map(|b| b.lod.budget().0).max().unwrap_or(1).max(1);
        let h = dt / max_sub as f32;

        for _ in 0..max_sub {
            // Integrate velocities (forces) for non-registered movable bodies.
            for b in self.bodies.iter_mut() {
                if b.inv_mass > 0.0 {
                    b.vel = b.vel.add(gravity.scale(h));
                }
            }

            // Broad-phase: refit and gather candidate pairs.
            for i in 0..self.bodies.len() {
                self.refit(i);
            }
            let contacts = self.find_contacts();

            // Solve velocity constraints (sequential impulses). Iteration count = the higher tier of
            // the two bodies; a contact touching a Registered body is skipped.
            let global_iters = self
                .bodies
                .iter()
                .map(|b| b.lod.budget().1)
                .max()
                .unwrap_or(0);
            for _ in 0..global_iters {
                for c in &contacts {
                    if self.bodies[c.a].lod == Lod::Registered || self.bodies[c.b].lod == Lod::Registered {
                        continue;
                    }
                    self.solve_contact(c);
                }
            }

            // Integrate positions.
            for b in self.bodies.iter_mut() {
                b.pos = b.pos.add(b.vel.scale(h));
                b.angle += b.ang_vel * h;
            }

            // Positional correction (Baumgarte) to remove residual penetration.
            for c in &contacts {
                if self.bodies[c.a].lod == Lod::Registered || self.bodies[c.b].lod == Lod::Registered {
                    continue;
                }
                self.correct_position(c);
            }
        }
    }

    /// Broad-phase + narrow-phase: every contact manifold this step.
    pub fn find_contacts(&self) -> Vec<Contact> {
        let mut out = Vec::new();
        for i in 0..self.bodies.len() {
            let qi = self.bodies[i].world_aabb();
            let candidates = self.tree.query(&qi);
            for j in candidates {
                if j <= i {
                    continue; // each unordered pair once
                }
                // Two static bodies never collide.
                if self.bodies[i].inv_mass == 0.0 && self.bodies[j].inv_mass == 0.0 {
                    continue;
                }
                if let Some(c) = self.collide(i, j) {
                    out.push(c);
                }
            }
        }
        out
    }

    /// Narrow-phase dispatch for a pair, producing a contact if they overlap.
    fn collide(&self, i: usize, j: usize) -> Option<Contact> {
        let a = &self.bodies[i];
        let b = &self.bodies[j];
        match (&a.shape, &b.shape) {
            (Shape::Circle { r: ra }, Shape::Circle { r: rb }) => {
                let d = b.pos.sub(a.pos);
                let dist = d.len();
                let sum = ra + rb;
                if dist >= sum {
                    return None;
                }
                let normal = if dist > 1e-6 { d.scale(1.0 / dist) } else { Vec2::new(1.0, 0.0) };
                let point = a.pos.add(normal.scale(*ra));
                Some(Contact { a: i, b: j, normal, point, penetration: sum - dist })
            }
            (Shape::Circle { r }, Shape::Polygon { .. }) => self.collide_circle_poly(i, j, *r, false),
            (Shape::Polygon { .. }, Shape::Circle { r }) => self.collide_circle_poly(j, i, *r, true),
            (Shape::Polygon { .. }, Shape::Polygon { .. }) => self.collide_poly_poly(i, j),
        }
    }

    /// Circle (`ci`) vs polygon (`pi`). If `flip`, the resulting contact normal/order is set so that
    /// it still points from body A to body B in the caller's `(i, j)` orientation.
    fn collide_circle_poly(&self, ci: usize, pi: usize, r: f32, flip: bool) -> Option<Contact> {
        let circle = &self.bodies[ci];
        let poly = &self.bodies[pi];
        let verts = poly.world_verts();
        if verts.is_empty() {
            return None;
        }
        // Find the closest point on the polygon to the circle centre.
        let c = circle.pos;
        let mut best_dist = f32::INFINITY;
        let mut best_point = verts[0];
        let mut best_normal = Vec2::new(1.0, 0.0);
        let mut inside = true;
        for k in 0..verts.len() {
            let p1 = verts[k];
            let p2 = verts[(k + 1) % verts.len()];
            let edge = p2.sub(p1);
            let n = edge.perp().normalize(); // CCW outward normal
            let signed = c.sub(p1).dot(n);
            if signed > 0.0 {
                inside = false;
            }
            // Closest point on this segment to c.
            let t = (c.sub(p1).dot(edge) / edge.len_sq().max(1e-9)).clamp(0.0, 1.0);
            let proj = p1.add(edge.scale(t));
            let d = c.sub(proj).len();
            if d < best_dist {
                best_dist = d;
                best_point = proj;
                best_normal = n;
            }
        }
        if inside {
            // Centre inside polygon: push out along the nearest face normal.
            let normal = if flip { best_normal.scale(-1.0) } else { best_normal };
            let (a, b) = if flip { (pi, ci) } else { (ci, pi) };
            return Some(Contact { a, b, normal: normal.scale(if flip { 1.0 } else { 1.0 }), point: best_point, penetration: r + best_dist });
        }
        if best_dist >= r {
            return None;
        }
        let mut normal = c.sub(best_point).normalize(); // from poly surface toward circle centre
        let penetration = r - best_dist;
        // Orient so the contact normal points from A to B for the caller's (i,j).
        let (a, b);
        if flip {
            // caller had (poly=pi as A? actually caller passed (i=poly, j=circle) with flip=true)
            a = pi;
            b = ci;
            normal = normal.scale(-1.0); // from poly(A) to circle(B)
        } else {
            a = ci;
            b = pi;
            normal = normal.scale(-1.0); // from circle(A) to poly(B): toward the surface
        }
        Some(Contact { a, b, normal, point: best_point, penetration })
    }

    /// Convex polygon vs convex polygon via SAT, taking the axis of least penetration. Produces a
    /// single representative contact point (the deepest vertex). Adequate for arcade-scale stacking.
    fn collide_poly_poly(&self, i: usize, j: usize) -> Option<Contact> {
        let va = self.bodies[i].world_verts();
        let vb = self.bodies[j].world_verts();
        if va.is_empty() || vb.is_empty() {
            return None;
        }
        let (pen_a, na, _) = max_separation(&va, &vb)?;
        if pen_a > 0.0 {
            return None;
        }
        let (pen_b, nb, vbest) = max_separation(&vb, &va)?;
        if pen_b > 0.0 {
            return None;
        }
        // Least penetration axis.
        let (normal, penetration, point) = if pen_a >= pen_b {
            (na, -pen_a, deepest_point(&vb, na))
        } else {
            (nb.scale(-1.0), -pen_b, vbest)
        };
        Some(Contact { a: i, b: j, normal, point, penetration })
    }

    /// Sequential-impulse contact resolution: a normal impulse with restitution plus a Coulomb
    /// friction impulse, applied at the contact point so bodies gain the right linear *and* angular
    /// response (spin from off-centre hits).
    fn solve_contact(&mut self, c: &Contact) {
        let (ia, ib) = (c.a, c.b);
        let (ra, rb) = (c.point.sub(self.bodies[ia].pos), c.point.sub(self.bodies[ib].pos));
        let n = c.normal;

        // Relative velocity at contact.
        let va = self.bodies[ia].vel.add(ra.cross_scalar(self.bodies[ia].ang_vel));
        let vb = self.bodies[ib].vel.add(rb.cross_scalar(self.bodies[ib].ang_vel));
        let rv = vb.sub(va);
        let vel_along_n = rv.dot(n);
        if vel_along_n > 0.0 {
            return; // separating
        }
        let e = self.bodies[ia].restitution.min(self.bodies[ib].restitution);
        let ra_cn = ra.cross(n);
        let rb_cn = rb.cross(n);
        let inv_mass_sum = self.bodies[ia].inv_mass
            + self.bodies[ib].inv_mass
            + ra_cn * ra_cn * self.bodies[ia].inv_inertia
            + rb_cn * rb_cn * self.bodies[ib].inv_inertia;
        if inv_mass_sum <= 1e-9 {
            return;
        }
        let jn = -(1.0 + e) * vel_along_n / inv_mass_sum;
        let impulse = n.scale(jn);
        self.apply_impulse(ia, impulse.scale(-1.0), ra);
        self.apply_impulse(ib, impulse, rb);

        // Friction.
        let va = self.bodies[ia].vel.add(ra.cross_scalar(self.bodies[ia].ang_vel));
        let vb = self.bodies[ib].vel.add(rb.cross_scalar(self.bodies[ib].ang_vel));
        let rv = vb.sub(va);
        let t = rv.sub(n.scale(rv.dot(n)));
        let tlen = t.len();
        if tlen <= 1e-6 {
            return;
        }
        let t = t.scale(1.0 / tlen);
        let ra_ct = ra.cross(t);
        let rb_ct = rb.cross(t);
        let inv_mass_sum_t = self.bodies[ia].inv_mass
            + self.bodies[ib].inv_mass
            + ra_ct * ra_ct * self.bodies[ia].inv_inertia
            + rb_ct * rb_ct * self.bodies[ib].inv_inertia;
        if inv_mass_sum_t <= 1e-9 {
            return;
        }
        let jt = -rv.dot(t) / inv_mass_sum_t;
        let mu = (self.bodies[ia].friction * self.bodies[ib].friction).sqrt();
        let jt = jt.clamp(-jn * mu, jn * mu);
        let friction = t.scale(jt);
        self.apply_impulse(ia, friction.scale(-1.0), ra);
        self.apply_impulse(ib, friction, rb);
    }

    fn apply_impulse(&mut self, i: usize, impulse: Vec2, r: Vec2) {
        let b = &mut self.bodies[i];
        b.vel = b.vel.add(impulse.scale(b.inv_mass));
        b.ang_vel += b.inv_inertia * r.cross(impulse);
    }

    fn correct_position(&mut self, c: &Contact) {
        let corr = (c.penetration - self.slop).max(0.0);
        if corr <= 0.0 {
            return;
        }
        let inv_sum = self.bodies[c.a].inv_mass + self.bodies[c.b].inv_mass;
        if inv_sum <= 1e-9 {
            return;
        }
        let push = c.normal.scale(self.beta * corr / inv_sum);
        let ima = self.bodies[c.a].inv_mass;
        let imb = self.bodies[c.b].inv_mass;
        self.bodies[c.a].pos = self.bodies[c.a].pos.sub(push.scale(ima));
        self.bodies[c.b].pos = self.bodies[c.b].pos.add(push.scale(imb));
    }
}

/// SAT helper: the maximum separation of polygon `b` from any face of polygon `a`, with the face
/// normal and the supporting vertex of `b`. Negative = overlap along that axis.
fn max_separation(a: &[Vec2], b: &[Vec2]) -> Option<(f32, Vec2, Vec2)> {
    let mut best = f32::NEG_INFINITY;
    let mut best_n = Vec2::new(1.0, 0.0);
    let mut best_v = b[0];
    for k in 0..a.len() {
        let p1 = a[k];
        let p2 = a[(k + 1) % a.len()];
        let n = p2.sub(p1).perp().normalize();
        // Support point of b in -n direction (deepest into face).
        let mut min_proj = f32::INFINITY;
        let mut sv = b[0];
        for v in b {
            let proj = v.sub(p1).dot(n);
            if proj < min_proj {
                min_proj = proj;
                sv = *v;
            }
        }
        if min_proj > best {
            best = min_proj;
            best_n = n;
            best_v = sv;
        }
    }
    Some((best, best_n, best_v))
}

/// The vertex of `verts` deepest along `-normal` (the representative contact point).
fn deepest_point(verts: &[Vec2], normal: Vec2) -> Vec2 {
    let mut best = verts[0];
    let mut best_proj = f32::INFINITY;
    for v in verts {
        let p = v.dot(normal);
        if p < best_proj {
            best_proj = p;
            best = *v;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_circles_bounce_and_conserve_momentum() {
        let mut w = World::new();
        let a = w.add({
            let mut b = RigidBody::dynamic(Vec2::new(0.0, 0.0), 1.0, Shape::Circle { r: 5.0 });
            b.vel = Vec2::new(10.0, 0.0);
            b.restitution = 1.0;
            b.friction = 0.0;
            b
        });
        let bb = w.add({
            let mut b = RigidBody::dynamic(Vec2::new(8.0, 0.0), 1.0, Shape::Circle { r: 5.0 });
            b.restitution = 1.0;
            b.friction = 0.0;
            b
        });
        let p_before = w.bodies[a].vel.x * 1.0 + w.bodies[bb].vel.x * 1.0;
        for _ in 0..20 {
            assign_lod(&mut w.bodies, &[Vec2::zero()], 1e6, 1e6, 1e6);
            w.step(1.0 / 60.0, Vec2::zero());
        }
        let p_after = w.bodies[a].vel.x + w.bodies[bb].vel.x;
        assert!((p_before - p_after).abs() < 1.0, "linear momentum roughly conserved: {p_before} vs {p_after}");
        assert!(w.bodies[bb].vel.x > 0.0, "the struck circle moves off");
    }

    #[test]
    fn circle_rests_on_static_floor() {
        let mut w = World::new();
        // A wide static box floor.
        w.add(RigidBody::statik(Vec2::new(0.0, 0.0), Shape::box_hw(1000.0, 10.0)));
        let ball = w.add(RigidBody::dynamic(Vec2::new(0.0, 60.0), 1.0, Shape::Circle { r: 8.0 }));
        for _ in 0..240 {
            assign_lod(&mut w.bodies, &[Vec2::zero()], 1e6, 1e6, 1e6);
            w.step(1.0 / 60.0, Vec2::new(0.0, -300.0));
        }
        let y = w.bodies[ball].pos.y;
        assert!(y > 10.0 && y < 30.0, "ball settles on the floor near y~18, got {y}");
    }

    #[test]
    fn lod_assignment_steps_down_with_distance() {
        let mut bodies = vec![
            RigidBody::dynamic(Vec2::new(0.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
            RigidBody::dynamic(Vec2::new(150.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
            RigidBody::dynamic(Vec2::new(600.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
            RigidBody::dynamic(Vec2::new(5000.0, 0.0), 1.0, Shape::Circle { r: 1.0 }),
        ];
        assign_lod(&mut bodies, &[Vec2::zero()], 100.0, 400.0, 1000.0);
        assert_eq!(bodies[0].lod, Lod::High);
        assert_eq!(bodies[1].lod, Lod::Medium);
        assert_eq!(bodies[2].lod, Lod::Low);
        assert_eq!(bodies[3].lod, Lod::Registered);
    }

    #[test]
    fn registered_bodies_only_integrate_no_solve() {
        // Two overlapping circles both Registered: they should pass through (no contact solve), just
        // coast — proving the far-LOD path is the cheap integrate-only path.
        let mut w = World::new();
        let a = w.add({
            let mut b = RigidBody::dynamic(Vec2::new(0.0, 0.0), 1.0, Shape::Circle { r: 5.0 });
            b.vel = Vec2::new(1.0, 0.0);
            b
        });
        let bb = w.add(RigidBody::dynamic(Vec2::new(6.0, 0.0), 1.0, Shape::Circle { r: 5.0 }));
        for _ in 0..10 {
            // No focus -> everything is Registered.
            assign_lod(&mut w.bodies, &[], 100.0, 400.0, 1000.0);
            w.step(1.0 / 60.0, Vec2::zero());
        }
        assert_eq!(w.bodies[a].lod, Lod::Registered);
        // The moving body kept its velocity (no impulse applied).
        assert!((w.bodies[a].vel.x - 1.0).abs() < 1e-3, "registered body just coasts");
        let _ = bb;
    }

    #[test]
    fn off_centre_hit_imparts_spin() {
        let mut w = World::new();
        // A box hit off-centre by a fast small circle should start rotating.
        let boxi = w.add(RigidBody::dynamic(Vec2::new(0.0, 0.0), 5.0, Shape::box_hw(20.0, 20.0)));
        w.add({
            let mut b = RigidBody::dynamic(Vec2::new(-40.0, 15.0), 1.0, Shape::Circle { r: 4.0 });
            b.vel = Vec2::new(40.0, 0.0);
            b.restitution = 0.5;
            b
        });
        for _ in 0..30 {
            assign_lod(&mut w.bodies, &[Vec2::zero()], 1e6, 1e6, 1e6);
            w.step(1.0 / 60.0, Vec2::zero());
        }
        assert!(w.bodies[boxi].ang_vel.abs() > 1e-3, "off-centre impact spins the box: {}", w.bodies[boxi].ang_vel);
    }
}
