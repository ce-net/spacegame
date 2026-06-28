//! Fail-first TDD for **global-cell content keying**. The asteroid field was hashed on *sector-local* cells
//! (`0..30`), so every sector in the galaxy held the byte-identical field — a 1:1 galaxy of copy-paste rocks.
//! Re-keying the hash on the **global** cell makes each galactic region unique while leaving the home sector
//! `(0,0)` unchanged (its global cells equal its local cells, so the calm spawn arena and the browser
//! renderer stay in sync). These tests are written against the sector-aware API and go red on the old code.

use std::sync::Arc;

use spacegame::ruleset::Ruleset;
use spacegame::shard::SectorId;
use spacegame::sim::{rock_in_cell, rock_in_cell_at, rock_world, Sim, ROCKS_PER_SECTOR, SECTOR_SIZE};

/// Snapshot a whole sector's asteroid field as a comparable fingerprint.
fn field(sector: SectorId) -> Vec<(i32, i32, u32, u32)> {
    let mut v = Vec::new();
    for cx in 0..ROCKS_PER_SECTOR {
        for cy in 0..ROCKS_PER_SECTOR {
            if let Some(r) = rock_in_cell_at(sector, cx, cy) {
                v.push((r.cx, r.cy, r.val, r.hp));
            }
        }
    }
    v
}

/// Distinct galactic regions must hold distinct asteroid fields — the bug was that they didn't.
#[test]
fn asteroid_field_is_unique_per_galactic_region() {
    let home = field(SectorId::new(0, 0));
    let a = field(SectorId::new(3, 1));
    let b = field(SectorId::new(-7, 4));
    assert!(!home.is_empty(), "the home sector still has rocks");
    assert_ne!(home, a, "a different region must not be a copy of home");
    assert_ne!(a, b, "two different regions must differ from each other");
    // ...and it is still deterministic (replica-safe): same region, same field.
    assert_eq!(a, field(SectorId::new(3, 1)));
}

/// The home sector `(0,0)` field is unchanged by the re-keying — global cells equal local cells there, so the
/// legacy `rock_in_cell` and the new `rock_in_cell_at((0,0), ..)` agree bit-for-bit (no renderer desync).
#[test]
fn home_sector_field_is_backward_compatible() {
    for cx in 0..ROCKS_PER_SECTOR {
        for cy in 0..ROCKS_PER_SECTOR {
            assert_eq!(
                rock_in_cell(cx, cy),
                rock_in_cell_at(SectorId::new(0, 0), cx, cy),
                "home sector must match the legacy field at cell ({cx},{cy})",
            );
        }
    }
}

/// The renderer's world-cell accessor (`rock_world`) draws **exactly** the field the authoritative sim
/// simulates (`rock_in_cell_at`): same existence, value, hp, and the same position modulo the sector origin.
/// This is the client/server agreement that was broken when only the backend moved to global cells.
#[test]
fn rock_world_matches_authoritative_field() {
    for (sx, sy) in [(0, 0), (5, -2), (-7, 4)] {
        let sector = SectorId::new(sx, sy);
        for cx in 0..ROCKS_PER_SECTOR {
            for cy in 0..ROCKS_PER_SECTOR {
                let gcx = sx * ROCKS_PER_SECTOR + cx;
                let gcy = sy * ROCKS_PER_SECTOR + cy;
                match (rock_in_cell_at(sector, cx, cy), rock_world(gcx, gcy)) {
                    (None, None) => {}
                    (Some(local), Some(world)) => {
                        assert_eq!((local.val, local.hp), (world.val, world.hp), "val/hp must match at ({sx},{sy}) cell ({cx},{cy})");
                        // World position is the sector origin plus the local position, to sub-unit precision.
                        assert!((world.x - (local.x + sx as f32 * SECTOR_SIZE)).abs() < 0.01);
                        assert!((world.y - (local.y + sy as f32 * SECTOR_SIZE)).abs() < 0.01);
                    }
                    (a, b) => panic!("existence disagreement at ({sx},{sy}) cell ({cx},{cy}): {:?} vs {:?}", a.is_some(), b.is_some()),
                }
            }
        }
    }
}

/// End-to-end through the sim: two sectors' sims expose different asteroid fields, because the sim now keys
/// content on its global region, not on repeating local cells.
#[test]
fn sims_in_different_sectors_have_different_asteroids() {
    let home = Sim::for_sector(SectorId::new(0, 0), Arc::new(Ruleset::builtin()));
    let away = Sim::for_sector(SectorId::new(5, -2), Arc::new(Ruleset::builtin()));
    // Scan the first row of cells via each sim's authoritative accessor.
    let row = |s: &Sim| (0..ROCKS_PER_SECTOR).filter_map(|cx| s.rock(cx, 3).map(|r| (r.val, r.hp))).collect::<Vec<_>>();
    assert_ne!(row(&home), row(&away), "the two regions are not the same asteroid field");
}
