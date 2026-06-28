//! Client tuning — the **shared source of truth** for how a frontend connects and what it asks for,
//! across the three targets: a **native app**, a **desktop browser**, and a **mobile browser**. The
//! transport and the wire are the same everywhere (the CE mesh; see [`crate::wire`]); what differs is
//! *budget* — a phone must spend less bandwidth, CPU and battery than a workstation. This module pins
//! those budgets as data so the client and the host agree on them, and provides the interest-scoping
//! helpers both sides use.
//!
//! See `FRONTEND.md` for the full connection/fault-tolerance/scale story; this is the small,
//! unit-tested core it refers to.

use serde::{Deserialize, Serialize};

use crate::aabb::Aabb;
use crate::shard::SectorId;

/// The three frontend targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// A packaged native application running a full CE node — the most capable client.
    Native,
    /// A desktop browser (an in-page WASM node, or the same-origin `/ce` proxy).
    DesktopBrowser,
    /// A phone browser — a WASM-only peer on a tight bandwidth/CPU/battery budget.
    MobileBrowser,
}

/// The budget + behaviour a platform asks for. The host honours `view_radius`/`max_entities` when it
/// scopes a per-client snapshot ([`crate::room::build_snapshot_view`]); the client honours
/// `snapshot_divisor`/`predict` locally.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClientProfile {
    pub platform: Platform,
    /// Half-extent (world units) of the viewport box the client renders / subscribes within.
    pub view_radius: f32,
    /// Hard cap on entities the client will receive/draw (the host trims to the nearest this many).
    pub max_entities: usize,
    /// Process/render every Nth authoritative snapshot (1 = every tick; 2 = half-rate on mobile).
    pub snapshot_divisor: u32,
    /// Run the deterministic local prediction for zero-delay feel.
    pub predict: bool,
    /// Eagerly fetch shaders/assets on connect (deferred on mobile to save the first-load budget).
    pub prefetch_assets: bool,
    /// How many sectors of the 3×3 neighbourhood to subscribe to (mobile may drop to the centre + 4).
    pub interest_sectors: usize,
}

impl Platform {
    /// The default profile for this platform — tuned budgets, not hard limits.
    pub fn profile(self) -> ClientProfile {
        match self {
            Platform::Native => ClientProfile {
                platform: self,
                view_radius: 2200.0,
                max_entities: 4000,
                snapshot_divisor: 1,
                predict: true,
                prefetch_assets: true,
                interest_sectors: 9,
            },
            Platform::DesktopBrowser => ClientProfile {
                platform: self,
                view_radius: 1600.0,
                max_entities: 1500,
                snapshot_divisor: 1,
                predict: true,
                prefetch_assets: true,
                interest_sectors: 9,
            },
            Platform::MobileBrowser => ClientProfile {
                platform: self,
                view_radius: 1000.0,
                max_entities: 400,
                snapshot_divisor: 2,
                predict: true,
                prefetch_assets: false,
                interest_sectors: 5,
            },
        }
    }
}

impl ClientProfile {
    /// The viewport box around `(x, y)` to scope the client's snapshot to — the host answers this with
    /// an AABB query so per-client bandwidth is `O(visible)`, not `O(sector population)`.
    pub fn viewport(&self, x: f32, y: f32) -> Aabb {
        Aabb::new(x - self.view_radius, y - self.view_radius, x + self.view_radius, y + self.view_radius)
    }

    /// Whether to process this authoritative snapshot (mobile subsamples to halve work/bandwidth).
    pub fn should_process(&self, server_tick: u64) -> bool {
        let d = self.snapshot_divisor.max(1) as u64;
        server_tick % d == 0
    }

    /// The sectors this client subscribes to, given its **world** position `(x, y)`. Domain-driven (see
    /// [`crate::domain`]): the set of sectors the client's viewport bubble — the box `(x, y) ±
    /// view_radius` — actually overlaps. That is **one** sector when the player is mid-sector, **two** on
    /// an edge, **four** on a corner, and it slides as the player moves, so interest *follows the player*
    /// instead of snapping a fixed 3×3 ring at the seam. Replaces the old ring/plus heuristic; the
    /// `interest_sectors` budget is now expressed as the `view_radius` itself (mobile's smaller radius
    /// keeps it on one sector longer).
    pub fn interest_set(&self, x: f32, y: f32) -> Vec<SectorId> {
        crate::domain::Bounds::around(x as f64, y as f64, self.view_radius as f64).sectors()
    }
}

/// Exponential reconnect backoff (ms) for a dropped mesh stream — capped, so a flapping link does not
/// hammer the relay. Same policy on every platform; the host loop uses the matching server-side value.
pub fn reconnect_backoff_ms(attempt: u32) -> u64 {
    let base = 250u64;
    base.saturating_mul(1u64 << attempt.min(5)).min(8000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_budget_is_tighter_than_desktop_and_native() {
        let m = Platform::MobileBrowser.profile();
        let d = Platform::DesktopBrowser.profile();
        let n = Platform::Native.profile();
        assert!(m.view_radius < d.view_radius && d.view_radius < n.view_radius);
        assert!(m.max_entities < d.max_entities && d.max_entities < n.max_entities);
        assert!(m.snapshot_divisor >= d.snapshot_divisor, "mobile subsamples");
        assert!(!m.prefetch_assets && d.prefetch_assets, "mobile defers asset prefetch");
    }

    #[test]
    fn all_platforms_predict_for_zero_delay() {
        for p in [Platform::Native, Platform::DesktopBrowser, Platform::MobileBrowser] {
            assert!(p.profile().predict, "local-first prediction is on everywhere");
        }
    }

    #[test]
    fn viewport_is_centred_and_sized() {
        let p = Platform::DesktopBrowser.profile();
        let vp = p.viewport(1000.0, 500.0);
        assert!((vp.min_x - (1000.0 - p.view_radius)).abs() < 1e-3);
        assert!((vp.max_y - (500.0 + p.view_radius)).abs() < 1e-3);
        assert!(vp.contains_point(1000.0, 500.0));
    }

    #[test]
    fn mobile_subsamples_snapshots() {
        let m = Platform::MobileBrowser.profile();
        assert!(m.should_process(0) && !m.should_process(1) && m.should_process(2));
        let n = Platform::Native.profile();
        assert!(n.should_process(0) && n.should_process(1), "native processes every tick");
    }

    #[test]
    fn interest_follows_the_player_and_slides_across_seams() {
        use crate::sim::SECTOR_SIZE;
        let p = Platform::DesktopBrowser.profile();
        // Mid-sector: interest is exactly the one home sector — no fixed ring.
        let mid = p.interest_set(SECTOR_SIZE * 0.5, SECTOR_SIZE * 0.5);
        assert_eq!(mid, vec![SectorId::new(0, 0)]);
        // Near the east seam (within view_radius of it): the eastern neighbour joins, nothing else.
        let edge = p.interest_set(SECTOR_SIZE - 50.0, SECTOR_SIZE * 0.5);
        assert_eq!(edge.len(), 2);
        assert!(edge.contains(&SectorId::new(0, 0)) && edge.contains(&SectorId::new(1, 0)));
        // On a corner: four.
        assert_eq!(p.interest_set(SECTOR_SIZE - 50.0, SECTOR_SIZE - 50.0).len(), 4);
        // Mobile's tighter view_radius keeps it on one sector closer to the seam than desktop.
        let m = Platform::MobileBrowser.profile();
        assert_eq!(m.interest_set(SECTOR_SIZE - 1500.0, SECTOR_SIZE * 0.5), vec![SectorId::new(0, 0)]);
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(reconnect_backoff_ms(0), 250);
        assert_eq!(reconnect_backoff_ms(1), 500);
        assert!(reconnect_backoff_ms(20) <= 8000, "backoff is capped");
        assert!(reconnect_backoff_ms(3) > reconnect_backoff_ms(1));
    }
}
