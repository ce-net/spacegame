//! The **dynamic shape system** — parametric, variable 2D geometry that every buildable block type
//! reuses. A structure block, a slab of armor, a storage tank, a weapon mount: they are all an object
//! definition plus a [`Shape2D`], so the *one* geometry kernel here serves the whole free-form
//! building system ([`crate::build`]).
//!
//! Shapes are **data** — serializable and therefore hot-reloadable like everything else — and
//! **parametric**: a rectangle of any width/height, a triangle of any width/height/lean (so any set of
//! angles), a trapezoid, a disc, a regular polygon, or an arbitrary convex outline. A placement can
//! resize a shape per-instance ([`Shape2D::sized`]), so one armor definition becomes a whole family of
//! plates without new data.
//!
//! Every shape can produce:
//! - its **outline** ([`Shape2D::outline`]) — vertices centred on the shape's centre of mass, ready for
//!   rendering and for physics;
//! - its **area**, **centroid**, **bounding box**, **bounding radius**, and **unit moment of inertia**
//!   (so an assembled craft's mass properties fall out of its parts);
//! - a **[`crate::physics::Shape`]** ([`Shape2D::to_physics`]) so a built part collides in the rigid-
//!   body world.
//!
//! Pure and unit-tested: no mesh, no clock.

use serde::{Deserialize, Serialize};

/// A point / vertex, kept as a plain pair so the wire form is compact (`[x, y]`).
pub type P2 = [f32; 2];

/// A named, reusable shape — the hot-reloadable **shape library** entry. Designers define shapes once
/// here and reference them by id from objects/blueprints, or edit a named shape to change every block
/// that uses it at once. Lives in the [`Ruleset`](crate::ruleset::Ruleset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedShape {
    pub id: String,
    /// The shape geometry (tagged by its `shape` discriminant).
    pub def: Shape2D,
}

/// A parametric 2D shape. Each variant is defined in a natural frame; [`outline`](Self::outline)
/// returns the vertices **re-centred on the centroid**, so a part's centre of mass is its origin
/// (which is what the physics solver assumes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum Shape2D {
    /// Axis-aligned rectangle, centred. Variable width and height.
    Rect { w: f32, h: f32 },
    /// Triangle with a horizontal base of width `w`, height `h`, and an apex shifted `skew` units
    /// sideways from centre — `skew` lets the three **angles** vary freely (0 = isosceles).
    Triangle {
        w: f32,
        h: f32,
        #[serde(default)]
        skew: f32,
    },
    /// Trapezoid: parallel top/bottom edges of `top_w`/`bottom_w`, height `h`. (A right-trapezoid or a
    /// symmetric one depending on `top_skew`.)
    Trapezoid {
        top_w: f32,
        bottom_w: f32,
        h: f32,
        #[serde(default)]
        top_skew: f32,
    },
    /// A disc of radius `r` (a true circle for physics; approximated as a polygon for the outline).
    Disc { r: f32 },
    /// A regular `sides`-gon of circumradius `r` (hexes, octagons, …).
    RegularPolygon { sides: u32, r: f32 },
    /// An arbitrary convex polygon given by explicit vertices (CCW).
    Polygon { verts: Vec<P2> },
}

/// How finely a [`Shape2D::Disc`] is tessellated for its outline (physics still uses a true circle).
const DISC_SEGMENTS: u32 = 24;

impl Shape2D {
    /// A unit square, the default block shape.
    pub fn unit() -> Shape2D {
        Shape2D::Rect { w: 1.0, h: 1.0 }
    }

    /// Raw vertices in the shape's natural frame (NOT yet centred on the centroid).
    fn raw_verts(&self) -> Vec<P2> {
        match *self {
            Shape2D::Rect { w, h } => {
                let (x, y) = (w * 0.5, h * 0.5);
                vec![[-x, -y], [x, -y], [x, y], [-x, y]]
            }
            Shape2D::Triangle { w, h, skew } => {
                let (x, y) = (w * 0.5, h * 0.5);
                vec![[-x, -y], [x, -y], [skew, y]]
            }
            Shape2D::Trapezoid { top_w, bottom_w, h, top_skew } => {
                let (bx, tx, y) = (bottom_w * 0.5, top_w * 0.5, h * 0.5);
                vec![[-bx, -y], [bx, -y], [tx + top_skew, y], [-tx + top_skew, y]]
            }
            Shape2D::Disc { r } => (0..DISC_SEGMENTS)
                .map(|i| {
                    let a = std::f32::consts::TAU * (i as f32) / (DISC_SEGMENTS as f32);
                    [a.cos() * r, a.sin() * r]
                })
                .collect(),
            Shape2D::RegularPolygon { sides, r } => {
                let n = sides.max(3);
                (0..n)
                    .map(|i| {
                        // Start at the top, go CCW.
                        let a = std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * (i as f32) / (n as f32);
                        [a.cos() * r, a.sin() * r]
                    })
                    .collect()
            }
            Shape2D::Polygon { ref verts } => verts.clone(),
        }
    }

    /// The shape's outline, vertices re-centred on the centroid (so the centre of mass is the origin).
    pub fn outline(&self) -> Vec<P2> {
        let mut v = self.raw_verts();
        let c = poly_centroid(&v);
        for p in &mut v {
            p[0] -= c[0];
            p[1] -= c[1];
        }
        v
    }

    /// Surface area (exact for the disc).
    pub fn area(&self) -> f32 {
        match *self {
            Shape2D::Disc { r } => std::f32::consts::PI * r * r,
            _ => poly_area(&self.raw_verts()).abs(),
        }
    }

    /// Centroid in the shape's natural frame.
    pub fn centroid(&self) -> P2 {
        poly_centroid(&self.raw_verts())
    }

    /// Axis-aligned bounding box of the centred outline, as `(min, max)`.
    pub fn aabb(&self) -> (P2, P2) {
        let v = self.outline();
        let mut mn = [f32::INFINITY, f32::INFINITY];
        let mut mx = [f32::NEG_INFINITY, f32::NEG_INFINITY];
        for p in &v {
            mn[0] = mn[0].min(p[0]);
            mn[1] = mn[1].min(p[1]);
            mx[0] = mx[0].max(p[0]);
            mx[1] = mx[1].max(p[1]);
        }
        (mn, mx)
    }

    /// Radius of the smallest origin-centred circle containing the centred outline.
    pub fn bounding_radius(&self) -> f32 {
        match *self {
            Shape2D::Disc { r } => r,
            _ => self.outline().iter().fold(0.0_f32, |m, p| m.max((p[0] * p[0] + p[1] * p[1]).sqrt())),
        }
    }

    /// Moment of inertia per unit mass about the centroid — multiply by a part's mass for its inertia.
    pub fn unit_inertia(&self) -> f32 {
        match *self {
            Shape2D::Disc { r } => 0.5 * r * r,
            _ => poly_unit_inertia(&self.outline()),
        }
    }

    /// Return a copy resized by overriding whichever size parameters are supplied — the per-placement
    /// **customization** hook. `w`/`h`/`r` map to the obvious fields; unknown shapes ignore them.
    pub fn sized(&self, w: Option<f32>, h: Option<f32>, r: Option<f32>) -> Shape2D {
        match self.clone() {
            Shape2D::Rect { w: ow, h: oh } => Shape2D::Rect { w: w.unwrap_or(ow), h: h.unwrap_or(oh) },
            Shape2D::Triangle { w: ow, h: oh, skew } => {
                Shape2D::Triangle { w: w.unwrap_or(ow), h: h.unwrap_or(oh), skew }
            }
            Shape2D::Trapezoid { top_w, bottom_w, h: oh, top_skew } => Shape2D::Trapezoid {
                top_w: w.unwrap_or(top_w),
                bottom_w: w.map(|x| x * (bottom_w / top_w.max(1e-3))).unwrap_or(bottom_w),
                h: h.unwrap_or(oh),
                top_skew,
            },
            Shape2D::Disc { r: or } => Shape2D::Disc { r: r.or(w.map(|x| x * 0.5)).unwrap_or(or) },
            Shape2D::RegularPolygon { sides, r: or } => {
                Shape2D::RegularPolygon { sides, r: r.or(w.map(|x| x * 0.5)).unwrap_or(or) }
            }
            other => other,
        }
    }

    /// A uniform scale of the shape (everything ×`s`).
    pub fn scaled(&self, s: f32) -> Shape2D {
        match self.clone() {
            Shape2D::Rect { w, h } => Shape2D::Rect { w: w * s, h: h * s },
            Shape2D::Triangle { w, h, skew } => Shape2D::Triangle { w: w * s, h: h * s, skew: skew * s },
            Shape2D::Trapezoid { top_w, bottom_w, h, top_skew } => {
                Shape2D::Trapezoid { top_w: top_w * s, bottom_w: bottom_w * s, h: h * s, top_skew: top_skew * s }
            }
            Shape2D::Disc { r } => Shape2D::Disc { r: r * s },
            Shape2D::RegularPolygon { sides, r } => Shape2D::RegularPolygon { sides, r: r * s },
            Shape2D::Polygon { verts } => {
                Shape2D::Polygon { verts: verts.into_iter().map(|p| [p[0] * s, p[1] * s]).collect() }
            }
        }
    }

    /// Convert to a [`crate::physics::Shape`] so a built part collides in the rigid-body world: a disc
    /// becomes a true circle, everything else a convex polygon of the centred outline.
    pub fn to_physics(&self) -> crate::physics::Shape {
        match *self {
            Shape2D::Disc { r } => crate::physics::Shape::Circle { r },
            _ => crate::physics::Shape::Polygon {
                verts: self.outline().into_iter().map(|p| crate::physics::Vec2::new(p[0], p[1])).collect(),
            },
        }
    }

    /// Reject nonsensical geometry (non-positive sizes, degenerate polygons) so a hand-edited shape
    /// can't slip into a live build.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Shape2D::Rect { w, h } => pos2(*w, *h, "rect"),
            Shape2D::Triangle { w, h, .. } => pos2(*w, *h, "triangle"),
            Shape2D::Trapezoid { top_w, bottom_w, h, .. } => {
                if *top_w < 0.0 || *bottom_w <= 0.0 || *h <= 0.0 {
                    return Err("trapezoid needs positive bottom_w/h and non-negative top_w".into());
                }
                Ok(())
            }
            Shape2D::Disc { r } => {
                if *r <= 0.0 {
                    Err("disc radius must be positive".into())
                } else {
                    Ok(())
                }
            }
            Shape2D::RegularPolygon { sides, r } => {
                if *sides < 3 {
                    Err("regular polygon needs >= 3 sides".into())
                } else if *r <= 0.0 {
                    Err("regular polygon radius must be positive".into())
                } else {
                    Ok(())
                }
            }
            Shape2D::Polygon { verts } => {
                if verts.len() < 3 {
                    Err("polygon needs >= 3 vertices".into())
                } else {
                    Ok(())
                }
            }
        }
    }
}

fn pos2(w: f32, h: f32, what: &str) -> Result<(), String> {
    if w > 0.0 && h > 0.0 {
        Ok(())
    } else {
        Err(format!("{what} needs positive width and height"))
    }
}

/// Signed area of a polygon (shoelace); positive for CCW.
pub fn poly_area(v: &[P2]) -> f32 {
    if v.len() < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..v.len() {
        let p = v[i];
        let q = v[(i + 1) % v.len()];
        a += p[0] * q[1] - q[0] * p[1];
    }
    a * 0.5
}

/// Area-weighted centroid of a polygon.
pub fn poly_centroid(v: &[P2]) -> P2 {
    if v.len() < 3 {
        // Average of points (handles degenerate input gracefully).
        let n = v.len().max(1) as f32;
        let (mut sx, mut sy) = (0.0, 0.0);
        for p in v {
            sx += p[0];
            sy += p[1];
        }
        return [sx / n, sy / n];
    }
    let a = poly_area(v);
    if a.abs() < 1e-9 {
        return [0.0, 0.0];
    }
    let (mut cx, mut cy) = (0.0, 0.0);
    for i in 0..v.len() {
        let p = v[i];
        let q = v[(i + 1) % v.len()];
        let cross = p[0] * q[1] - q[0] * p[1];
        cx += (p[0] + q[0]) * cross;
        cy += (p[1] + q[1]) * cross;
    }
    [cx / (6.0 * a), cy / (6.0 * a)]
}

/// Per-unit-mass moment of inertia of a polygon about its centroid (vertices assumed centred).
pub fn poly_unit_inertia(v: &[P2]) -> f32 {
    if v.len() < 3 {
        return 0.0;
    }
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..v.len() {
        let a = v[i];
        let b = v[(i + 1) % v.len()];
        let cross = (a[0] * b[1] - b[0] * a[1]).abs();
        num += cross * (a[0] * a[0] + a[0] * b[0] + b[0] * b[0] + a[1] * a[1] + a[1] * b[1] + b[1] * b[1]);
        den += cross;
    }
    if den > 1e-9 { num / (6.0 * den) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_area_centroid_and_outline() {
        let s = Shape2D::Rect { w: 4.0, h: 2.0 };
        assert!((s.area() - 8.0).abs() < 1e-4);
        let c = s.centroid();
        assert!(c[0].abs() < 1e-5 && c[1].abs() < 1e-5, "rect centroid at origin");
        assert_eq!(s.outline().len(), 4);
        let (mn, mx) = s.aabb();
        assert!((mx[0] - mn[0] - 4.0).abs() < 1e-4 && (mx[1] - mn[1] - 2.0).abs() < 1e-4);
    }

    #[test]
    fn triangle_area_and_variable_angles() {
        let s = Shape2D::Triangle { w: 6.0, h: 4.0, skew: 0.0 };
        assert!((s.area() - 12.0).abs() < 1e-3, "tri area = 1/2 b h");
        // A leaning triangle has the same area but a shifted apex -> different angles.
        let leaned = Shape2D::Triangle { w: 6.0, h: 4.0, skew: 3.0 };
        assert!((leaned.area() - 12.0).abs() < 1e-3);
        assert_ne!(s.outline(), leaned.outline(), "skew changes the geometry (angles)");
    }

    #[test]
    fn outline_is_centred_on_centroid() {
        for s in [
            Shape2D::Triangle { w: 5.0, h: 9.0, skew: 2.0 },
            Shape2D::Trapezoid { top_w: 2.0, bottom_w: 6.0, h: 4.0, top_skew: 1.0 },
            Shape2D::RegularPolygon { sides: 6, r: 3.0 },
        ] {
            let c = poly_centroid(&s.outline());
            assert!(c[0].abs() < 1e-3 && c[1].abs() < 1e-3, "centred outline has centroid ~origin: {c:?}");
        }
    }

    #[test]
    fn disc_area_and_physics_is_a_circle() {
        let s = Shape2D::Disc { r: 2.0 };
        assert!((s.area() - std::f32::consts::PI * 4.0).abs() < 1e-3);
        match s.to_physics() {
            crate::physics::Shape::Circle { r } => assert!((r - 2.0).abs() < 1e-5),
            _ => panic!("disc must map to a physics circle"),
        }
        assert!((s.bounding_radius() - 2.0).abs() < 1e-5);
    }

    #[test]
    fn polygon_physics_is_a_polygon() {
        let s = Shape2D::Rect { w: 2.0, h: 2.0 };
        match s.to_physics() {
            crate::physics::Shape::Polygon { verts } => assert_eq!(verts.len(), 4),
            _ => panic!("rect must map to a physics polygon"),
        }
    }

    #[test]
    fn sized_customizes_per_placement() {
        let base = Shape2D::Rect { w: 1.0, h: 1.0 };
        let big = base.sized(Some(10.0), Some(3.0), None);
        assert_eq!(big, Shape2D::Rect { w: 10.0, h: 3.0 });
        assert!((big.area() - 30.0).abs() < 1e-3);
    }

    #[test]
    fn scaled_scales_everything() {
        let s = Shape2D::Disc { r: 2.0 }.scaled(3.0);
        assert_eq!(s, Shape2D::Disc { r: 6.0 });
    }

    #[test]
    fn unit_inertia_is_positive_and_grows_with_size() {
        let small = Shape2D::Rect { w: 1.0, h: 1.0 }.unit_inertia();
        let big = Shape2D::Rect { w: 4.0, h: 4.0 }.unit_inertia();
        assert!(small > 0.0 && big > small);
    }

    #[test]
    fn validate_rejects_degenerate_shapes() {
        assert!(Shape2D::Rect { w: 0.0, h: 2.0 }.validate().is_err());
        assert!(Shape2D::RegularPolygon { sides: 2, r: 1.0 }.validate().is_err());
        assert!(Shape2D::Polygon { verts: vec![[0.0, 0.0], [1.0, 0.0]] }.validate().is_err());
        assert!(Shape2D::Disc { r: 1.0 }.validate().is_ok());
    }

    #[test]
    fn roundtrips_through_json_with_tag() {
        let s = Shape2D::Triangle { w: 3.0, h: 4.0, skew: 1.0 };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"shape\":\"triangle\""), "tagged serialization: {j}");
        assert_eq!(serde_json::from_str::<Shape2D>(&j).unwrap(), s);
    }
}
