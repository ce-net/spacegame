//! **Environmental hazards** — the galaxy as terrain, not an empty void.
//!
//! Every sector grows its own gravitational and atmospheric features the same way it grows its
//! asteroid field: **deterministically from the sector coordinate**, with zero stored state. A planet,
//! a star or a black hole ([`Well`]) bends the trajectory of every ship and every projectile that
//! passes near it; a [`Nebula`] cloud drags ships down and hides them from sensors. Because the field
//! is a pure function of the sector id, the authoritative host, every high-precision replica, and the
//! browser renderer all agree on exactly where the wells and clouds are without exchanging a single
//! byte — the field is part of the rules, like the rocks.
//!
//! This is what makes the LOD rigid-body physics and the recursive-AABB broad-phase *matter*: a fight
//! happening in a planet's gravity well plays differently from one in open space, missiles curve as
//! they fall inward, and a wreck can be swallowed by a black hole. It is also a horizontal-scale story
//! — each sector is its own little solar system, simulated on whichever mesh node hosts it.
//!
//! The **home sector `(0, 0)` is deliberately calm** (no wells, no clouds): it is the safe spawn
//! system every new pilot starts in, and keeping it empty also means the classic single-sector arena
//! behaves exactly as before. Hazards appear in every *other* sector.
//!
//! Pure and deterministic (no clock, no mesh); unit-tested. The sim reads [`Hazards`] each tick to
//! perturb motion ([`Hazards::accel_at`], [`Hazards::drag_at`], [`Hazards::lethal_at`]).

use serde::{Deserialize, Serialize};

use crate::physics::Vec2;
use crate::shard::SectorId;
use crate::sim::{fnv1a, SECTOR_SIZE};

/// What a gravity well is, physically — purely a visual/lore distinction plus a lethality flag; the
/// pull is governed by [`Well::mass`] and [`Well::radius`] regardless of kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WellKind {
    /// A planet: a moderate, benign gravity well you can orbit and slingshot around.
    Planet,
    /// A star: a strong well with a hot core — its [`Well::core_radius`] singes ships that fall in.
    Star,
    /// A black hole: a fierce well whose [`Well::core_radius`] is a lethal event horizon — cross it
    /// and the ship (or wreck) is gone.
    BlackHole,
}

impl WellKind {
    /// Does falling inside the core destroy a ship outright (event horizon)?
    pub fn core_is_lethal(self) -> bool {
        matches!(self, WellKind::BlackHole)
    }

    pub fn code(self) -> u8 {
        match self {
            WellKind::Planet => 0,
            WellKind::Star => 1,
            WellKind::BlackHole => 2,
        }
    }
}

/// A point gravity source in a sector (sector-local coordinates). Pulls ships and projectiles inward
/// with a softened inverse-square law, capped so nothing is flung to infinity in one tick.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Well {
    pub x: f32,
    pub y: f32,
    /// Influence radius — beyond this the pull is treated as zero (so the broad-phase stays cheap and
    /// distant sectors don't reach in).
    pub radius: f32,
    /// Inner radius: the planet/star/hole body itself. Inside it the pull is clamped and (for a star
    /// or hole) something happens to the ship.
    pub core_radius: f32,
    /// Gravitational strength (acceleration scale at the core edge, world-units/tick^2).
    pub mass: f32,
    pub kind: WellKind,
}

impl Well {
    /// Acceleration this well imparts to a body at `(x, y)` — toward the well, softened near the core,
    /// zero beyond `radius`. Deterministic and bounded.
    pub fn accel_at(&self, x: f32, y: f32) -> Vec2 {
        let dx = self.x - x;
        let dy = self.y - y;
        let d2 = dx * dx + dy * dy;
        let d = d2.sqrt();
        if d >= self.radius || d <= 1e-3 {
            return Vec2::zero();
        }
        // Softened inverse-square: clamp the denominator at the core so the pull peaks rather than
        // diverging. Falls to zero at `radius` via a linear taper for a clean cutoff.
        let soft = self.core_radius.max(8.0);
        let inv = soft * soft / (d2.max(soft * soft));
        let taper = (1.0 - d / self.radius).clamp(0.0, 1.0);
        let mag = self.mass * inv * taper;
        Vec2::new(dx / d * mag, dy / d * mag)
    }

    /// Is `(x, y)` inside the lethal core (a black hole's event horizon)?
    pub fn lethal(&self, x: f32, y: f32) -> bool {
        if !self.kind.core_is_lethal() {
            return false;
        }
        let dx = self.x - x;
        let dy = self.y - y;
        dx * dx + dy * dy <= self.core_radius * self.core_radius
    }
}

/// A nebula cloud (sector-local). Inside it, ships move as though through fluid (extra drag) and are
/// hidden from long-range sensors — cover. Cheap circle test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Nebula {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    /// Extra per-tick velocity damping applied inside (0 = none, e.g. 0.04 bleeds 4%/tick on top of
    /// the ship's normal damping).
    pub drag: f32,
}

impl Nebula {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        let dx = self.x - x;
        let dy = self.y - y;
        dx * dx + dy * dy <= self.radius * self.radius
    }
}

/// The full deterministic hazard field of one sector: its gravity wells and nebula clouds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Hazards {
    pub wells: Vec<Well>,
    pub nebulae: Vec<Nebula>,
}

impl Hazards {
    /// An empty hazard field (the calm home sector / the classic closed arena).
    pub fn empty() -> Self {
        Hazards::default()
    }

    /// **Grow** the hazard field for `sector` deterministically from its coordinate. The home sector
    /// `(0, 0)` is always empty (safe spawn); every other sector seeds 0..=2 wells and 0..=3 nebulae
    /// from hashes of its id, placed away from the edges so transit hand-offs never start inside a
    /// hazard.
    pub fn for_sector(sector: SectorId) -> Self {
        if sector.sx == 0 && sector.sy == 0 {
            return Hazards::empty();
        }
        let salt = sector.token();
        let h = |tag: &str| fnv1a(&format!("{salt}:{tag}"));

        // A unit coordinate in [0.18, 0.82] of the sector, kept clear of the edges.
        let inset = |v: u32| 0.18 + (v % 1000) as f32 / 1000.0 * 0.64;

        let mut wells = Vec::new();
        // Most sectors hold one well; some hold a second; a rare few are open void.
        let well_count = match h("nwell") % 10 {
            0 | 1 => 0,
            2..=6 => 1,
            _ => 2,
        };
        for i in 0..well_count {
            let hx = h(&format!("wx{i}"));
            let hy = h(&format!("wy{i}"));
            let hk = h(&format!("wk{i}"));
            let hm = h(&format!("wm{i}"));
            let x = inset(hx) * SECTOR_SIZE;
            let y = inset(hy) * SECTOR_SIZE;
            // Roughly: planets common, stars uncommon, black holes rare.
            let kind = match hk % 12 {
                0 => WellKind::BlackHole,
                1 | 2 => WellKind::Star,
                _ => WellKind::Planet,
            };
            let (radius, core, mass) = match kind {
                WellKind::Planet => (520.0, 70.0, 1.1 + (hm % 5) as f32 * 0.2),
                WellKind::Star => (760.0, 95.0, 2.0 + (hm % 5) as f32 * 0.3),
                WellKind::BlackHole => (900.0, 46.0, 3.6 + (hm % 5) as f32 * 0.5),
            };
            wells.push(Well { x, y, radius, core_radius: core, mass, kind });
        }

        let mut nebulae = Vec::new();
        let neb_count = h("nneb") % 4; // 0..=3
        for i in 0..neb_count {
            let hx = h(&format!("nx{i}"));
            let hy = h(&format!("ny{i}"));
            let hr = h(&format!("nr{i}"));
            let x = inset(hx) * SECTOR_SIZE;
            let y = inset(hy) * SECTOR_SIZE;
            let radius = 280.0 + (hr % 320) as f32;
            nebulae.push(Nebula { x, y, radius, drag: 0.035 });
        }

        Hazards { wells, nebulae }
    }

    pub fn is_empty(&self) -> bool {
        self.wells.is_empty() && self.nebulae.is_empty()
    }

    /// Summed gravitational acceleration on a body at `(x, y)` from every well (bounded).
    pub fn accel_at(&self, x: f32, y: f32) -> Vec2 {
        let mut a = Vec2::zero();
        for w in &self.wells {
            a = a.add(w.accel_at(x, y));
        }
        a
    }

    /// Extra velocity damping at `(x, y)` from any nebula covering it (0 if in open space). If clouds
    /// overlap, the strongest drag wins (no compounding to a dead stop).
    pub fn drag_at(&self, x: f32, y: f32) -> f32 {
        let mut d = 0.0f32;
        for n in &self.nebulae {
            if n.contains(x, y) {
                d = d.max(n.drag);
            }
        }
        d
    }

    /// Is `(x, y)` hidden from long-range sensors (inside any nebula)? Cover for ambushes.
    pub fn hidden_at(&self, x: f32, y: f32) -> bool {
        self.nebulae.iter().any(|n| n.contains(x, y))
    }

    /// Is `(x, y)` inside a lethal core (a black hole event horizon)? If so the body is destroyed.
    pub fn lethal_at(&self, x: f32, y: f32) -> bool {
        self.wells.iter().any(|w| w.lethal(x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_sector_is_calm() {
        let h = Hazards::for_sector(SectorId::new(0, 0));
        assert!(h.is_empty(), "the spawn sector has no hazards");
    }

    #[test]
    fn field_is_deterministic_per_sector() {
        for (sx, sy) in [(1, 0), (-3, 2), (7, 7), (0, 5)] {
            let a = Hazards::for_sector(SectorId::new(sx, sy));
            let b = Hazards::for_sector(SectorId::new(sx, sy));
            assert_eq!(a, b, "same sector -> identical hazard field (replica-safe)");
        }
    }

    #[test]
    fn wells_stay_inside_and_off_the_edges() {
        // Scan many sectors; every well/nebula must sit comfortably inside the sector bounds so a ship
        // transiting in at an edge never spawns inside a hazard.
        for sx in -6..6 {
            for sy in -6..6 {
                let h = Hazards::for_sector(SectorId::new(sx, sy));
                for w in &h.wells {
                    assert!(w.x > 100.0 && w.x < SECTOR_SIZE - 100.0, "well x inset");
                    assert!(w.y > 100.0 && w.y < SECTOR_SIZE - 100.0, "well y inset");
                    assert!(w.core_radius < w.radius);
                }
                for n in &h.nebulae {
                    assert!(n.contains(n.x, n.y));
                }
            }
        }
    }

    #[test]
    fn gravity_pulls_toward_the_well_and_vanishes_far_away() {
        let w = Well { x: 1000.0, y: 1000.0, radius: 500.0, core_radius: 60.0, mass: 2.0, kind: WellKind::Planet };
        // A body to the left of the well is pulled to the right (+x), and downward (+y) toward it.
        let a = w.accel_at(600.0, 1000.0);
        assert!(a.x > 0.0, "pulled toward the well in +x");
        assert!(a.y.abs() < 1e-4, "no transverse pull on-axis");
        // Outside the influence radius there is no pull.
        let far = w.accel_at(2000.0, 2000.0);
        assert_eq!(far.len(), 0.0);
    }

    #[test]
    fn black_hole_core_is_lethal_but_a_planet_core_is_not() {
        let hole = Well { x: 0.0, y: 0.0, radius: 800.0, core_radius: 50.0, mass: 4.0, kind: WellKind::BlackHole };
        assert!(hole.lethal(10.0, 10.0), "inside the event horizon is lethal");
        assert!(!hole.lethal(400.0, 0.0), "outside the horizon is survivable");
        let planet = Well { x: 0.0, y: 0.0, radius: 800.0, core_radius: 50.0, mass: 4.0, kind: WellKind::Planet };
        assert!(!planet.lethal(10.0, 10.0), "a planet never instakills");
    }

    #[test]
    fn nebula_drags_and_hides_inside_only() {
        let h = Hazards {
            wells: vec![],
            nebulae: vec![Nebula { x: 500.0, y: 500.0, radius: 200.0, drag: 0.05 }],
        };
        assert!(h.drag_at(500.0, 500.0) > 0.0 && h.hidden_at(510.0, 500.0));
        assert_eq!(h.drag_at(2000.0, 2000.0), 0.0);
        assert!(!h.hidden_at(2000.0, 2000.0));
    }
}
