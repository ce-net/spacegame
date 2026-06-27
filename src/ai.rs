//! **Objective-driven NPC brains.** An NPC ship used to recompute a fresh goal from scratch every
//! tick, so it would dither — flip targets, peck at a rock and wander off, charge a stronger enemy to
//! its death. Here each ship instead holds an [`Objective`] it *commits* to, and changes its mind only
//! when the world genuinely changes (its rock is gone, its prey escaped, its hull is failing). That
//! commitment plus a little hysteresis is what reads as "smart": a drone strips a vein before moving
//! on, a fighter presses an attack, a wounded ship breaks off and lives to fight again.
//!
//! This module is the pure decision core — given what a ship *senses* this tick ([`Senses`]) and what
//! it is *currently doing* ([`Objective`]), [`next_objective`] returns what it should do next. The sim
//! gathers the senses and turns the chosen objective into steering; keeping the policy here makes it
//! deterministic and unit-testable in isolation.

use crate::faction::FactionCommand;
use crate::sim::ShipRole;

/// Hull fraction at or below which a fighter breaks off to survive.
pub const RETREAT_HP: f32 = 0.28;
/// Hull fraction a retreating ship must regenerate back to before it will re-engage.
pub const RECOVER_HP: f32 = 0.6;
/// How long (ticks) a retreat lasts even if no shield/hull comes back — long enough to clear the fight.
pub const RETREAT_TICKS: u64 = 200;
/// Hysteresis: a committed target is kept until it leaves `engage_r * ENGAGE_KEEP` (so a fighter does
/// not drop a prey that briefly dips out of its nominal range).
pub const ENGAGE_KEEP: f32 = 1.4;

/// One thing an NPC is set on doing. Persisted (transiently) on the ship between ticks so it commits.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Objective {
    /// No standing goal — fall back to the role default (usually escorting the owner).
    #[default]
    Idle,
    /// Strip a specific asteroid cell. Held until the rock is depleted.
    Mine { cx: i32, cy: i32 },
    /// Press an attack on a specific ship. `since` is when the lock began (for "stale lock" decisions).
    Engage { target: String, since: u64 },
    /// Break off and survive until `until`, or until hull recovers past [`RECOVER_HP`].
    Retreat { until: u64 },
    /// Hold a formation slot around the owner.
    Escort,
    /// Drive to a fixed point (an attack-move order's waypoint).
    Move { x: f32, y: f32 },
}

/// A contact the ship can see this tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Contact {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    /// Distance from the deciding ship.
    pub dist: f32,
}

/// Everything the deciding ship perceives this tick. The sim fills this in from its broad-phase + the
/// deterministic asteroid field; the policy below is a pure function of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Senses {
    pub now: u64,
    /// Current hull as a fraction of max (0..1).
    pub hp_frac: f32,
    /// Nearest enemy within the engage radius, if any.
    pub enemy: Option<Contact>,
    /// Nearest mineable asteroid `(cx, cy, x, y)`, if any.
    pub rock: Option<(i32, i32, f32, f32)>,
    /// Whether the rock named by the *current* [`Objective::Mine`] is still mineable.
    pub current_rock_live: bool,
    /// Whether the ship named by the *current* [`Objective::Engage`] is still alive and in keep-range.
    pub current_target_held: bool,
    /// The effective engage radius for this ship under its current command.
    pub engage_r: f32,
}

/// Decide what the ship should do next, given its role, its faction's standing order, what it is doing
/// now, and what it senses. Pure and deterministic.
pub fn next_objective(role: ShipRole, cmd: FactionCommand, cur: &Objective, s: &Senses) -> Objective {
    // --- Survival overrides everything (fighters only; drones/haulers are expendable economy). ---
    if role == ShipRole::Fighter {
        if let Objective::Retreat { until } = cur {
            // Stay broken off until the timer elapses AND some hull has come back.
            if s.now < *until && s.hp_frac < RECOVER_HP {
                return Objective::Retreat { until: *until };
            }
        } else if s.hp_frac <= RETREAT_HP {
            return Objective::Retreat { until: s.now + RETREAT_TICKS };
        }
    }

    match role {
        ShipRole::Fighter => fighter_objective(cmd, cur, s),
        ShipRole::Drone => drone_objective(cmd, cur, s),
        ShipRole::Hauler => hauler_objective(cmd, s),
        ShipRole::Player => Objective::Idle,
    }
}

fn fighter_objective(cmd: FactionCommand, cur: &Objective, s: &Senses) -> Objective {
    // Pure-economy / passive orders never pick a fight.
    if matches!(cmd, FactionCommand::Mine | FactionCommand::Hold) {
        return passive_objective(cmd, s);
    }
    // Keep pressing a still-valid locked target (hysteresis) before looking for a new one.
    if let Objective::Engage { target, since } = cur
        && s.current_target_held
    {
        return Objective::Engage { target: target.clone(), since: *since };
    }
    // Acquire the nearest enemy in range.
    if let Some(e) = &s.enemy
        && e.dist <= s.engage_r
    {
        return Objective::Engage { target: e.id.clone(), since: s.now };
    }
    passive_objective(cmd, s)
}

fn drone_objective(cmd: FactionCommand, cur: &Objective, s: &Senses) -> Objective {
    if matches!(cmd, FactionCommand::Hold) {
        return Objective::Idle;
    }
    // Strip the current vein before wandering off.
    if let Objective::Mine { cx, cy } = cur
        && s.current_rock_live
    {
        return Objective::Mine { cx: *cx, cy: *cy };
    }
    if let Some((cx, cy, _, _)) = s.rock {
        return Objective::Mine { cx, cy };
    }
    passive_objective(cmd, s)
}

fn hauler_objective(cmd: FactionCommand, s: &Senses) -> Objective {
    passive_objective(cmd, s)
}

/// The non-combat default for a command: hold for `Hold`, drive to the waypoint for `AttackMove`, else
/// fall in around the owner.
fn passive_objective(cmd: FactionCommand, _s: &Senses) -> Objective {
    match cmd {
        FactionCommand::Hold => Objective::Idle,
        FactionCommand::AttackMove { x, y } => Objective::Move { x, y },
        _ => Objective::Escort,
    }
}

/// Lead a moving target: where should a shooter at `(sx, sy)` aim so a projectile of speed
/// `proj_speed` meets a target now at `(tx, ty)` moving at `(tvx, tvy)`? Solves the intercept quadratic;
/// if there is no forward solution (target faster than the shot, or `proj_speed <= 0`), aim at the
/// target's present position. Pure.
pub fn lead_target(
    sx: f32,
    sy: f32,
    tx: f32,
    ty: f32,
    tvx: f32,
    tvy: f32,
    proj_speed: f32,
) -> (f32, f32) {
    if proj_speed <= 1e-3 {
        return (tx, ty);
    }
    let rx = tx - sx;
    let ry = ty - sy;
    // |R + V t| = proj_speed t  ->  (V·V - s²) t² + 2 (R·V) t + R·R = 0
    let a = tvx * tvx + tvy * tvy - proj_speed * proj_speed;
    let b = 2.0 * (rx * tvx + ry * tvy);
    let c = rx * rx + ry * ry;
    let t = if a.abs() < 1e-4 {
        // Linear case: target speed ≈ projectile speed.
        if b.abs() < 1e-4 { return (tx, ty) }
        -c / b
    } else {
        let disc = b * b - 4.0 * a * c;
        if disc < 0.0 {
            return (tx, ty);
        }
        let sq = disc.sqrt();
        let t1 = (-b + sq) / (2.0 * a);
        let t2 = (-b - sq) / (2.0 * a);
        // Smallest positive time.
        match (t1 > 0.0, t2 > 0.0) {
            (true, true) => t1.min(t2),
            (true, false) => t1,
            (false, true) => t2,
            (false, false) => return (tx, ty),
        }
    };
    if t <= 0.0 || !t.is_finite() {
        return (tx, ty);
    }
    (tx + tvx * t, ty + tvy * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn senses() -> Senses {
        Senses {
            now: 1000,
            hp_frac: 1.0,
            enemy: None,
            rock: None,
            current_rock_live: false,
            current_target_held: false,
            engage_r: 1000.0,
        }
    }

    #[test]
    fn a_fighter_engages_an_enemy_in_range() {
        let mut s = senses();
        s.enemy = Some(Contact { id: "x".into(), x: 100.0, y: 0.0, vx: 0.0, vy: 0.0, dist: 500.0 });
        let o = next_objective(ShipRole::Fighter, FactionCommand::Defend, &Objective::Idle, &s);
        assert_eq!(o, Objective::Engage { target: "x".into(), since: 1000 });
    }

    #[test]
    fn a_fighter_keeps_its_lock_even_as_the_target_drifts_out_of_nominal_range() {
        let mut s = senses();
        s.current_target_held = true; // sim says the locked target is still in keep-range
        s.enemy = None; // no fresh acquisition this tick
        let cur = Objective::Engage { target: "x".into(), since: 900 };
        let o = next_objective(ShipRole::Fighter, FactionCommand::AttackNearest, &cur, &s);
        assert_eq!(o, Objective::Engage { target: "x".into(), since: 900 }, "lock and since are preserved");
    }

    #[test]
    fn a_wounded_fighter_retreats_and_stays_broken_off_until_recovered() {
        let mut s = senses();
        s.hp_frac = 0.2;
        s.enemy = Some(Contact { id: "x".into(), x: 10.0, y: 0.0, vx: 0.0, vy: 0.0, dist: 50.0 });
        let o = next_objective(ShipRole::Fighter, FactionCommand::AttackNearest, &Objective::Idle, &s);
        let Objective::Retreat { until } = o else { panic!("should retreat, got {o:?}") };
        assert_eq!(until, 1000 + RETREAT_TICKS);

        // Still hurt next tick -> keeps retreating even though the enemy is right there.
        let mut s2 = senses();
        s2.now = 1100;
        s2.hp_frac = 0.4; // below RECOVER_HP
        s2.enemy = s.enemy.clone();
        let o2 = next_objective(ShipRole::Fighter, FactionCommand::AttackNearest, &Objective::Retreat { until }, &s2);
        assert_eq!(o2, Objective::Retreat { until });

        // Recovered -> back to the fight.
        let mut s3 = senses();
        s3.now = 1200;
        s3.hp_frac = 0.7; // above RECOVER_HP
        s3.enemy = s.enemy.clone();
        let o3 = next_objective(ShipRole::Fighter, FactionCommand::AttackNearest, &Objective::Retreat { until }, &s3);
        assert!(matches!(o3, Objective::Engage { .. }), "recovered fighter re-engages, got {o3:?}");
    }

    #[test]
    fn a_drone_commits_to_a_vein_until_it_is_dry() {
        let mut s = senses();
        s.current_rock_live = true;
        // Even with another rock visible, it sticks to its current cell.
        s.rock = Some((9, 9, 100.0, 100.0));
        let cur = Objective::Mine { cx: 1, cy: 2 };
        let o = next_objective(ShipRole::Drone, FactionCommand::Defend, &cur, &s);
        assert_eq!(o, Objective::Mine { cx: 1, cy: 2 });

        // Vein dry -> move to the next visible rock.
        s.current_rock_live = false;
        let o2 = next_objective(ShipRole::Drone, FactionCommand::Defend, &cur, &s);
        assert_eq!(o2, Objective::Mine { cx: 9, cy: 9 });
    }

    #[test]
    fn hold_order_parks_everyone() {
        let s = senses();
        assert_eq!(next_objective(ShipRole::Fighter, FactionCommand::Hold, &Objective::Idle, &s), Objective::Idle);
        assert_eq!(next_objective(ShipRole::Drone, FactionCommand::Hold, &Objective::Idle, &s), Objective::Idle);
    }

    #[test]
    fn attack_move_sends_ships_to_the_waypoint_when_no_enemy() {
        let mut s = senses();
        s.enemy = None;
        let o = next_objective(ShipRole::Fighter, FactionCommand::AttackMove { x: 500.0, y: 600.0 }, &Objective::Idle, &s);
        assert_eq!(o, Objective::Move { x: 500.0, y: 600.0 });
    }

    #[test]
    fn lead_aims_ahead_of_a_crossing_target() {
        // Target at (1000,0) moving +y at 10; shot speed 40. Aim point should be ahead in +y.
        let (ax, ay) = lead_target(0.0, 0.0, 1000.0, 0.0, 0.0, 10.0, 40.0);
        assert!(ay > 0.0, "aim leads the target along its velocity: {ay}");
        assert!(ax > 0.0);
        // Sanity: time-to-target consistent (distance / proj_speed ≈ ay / tvy).
        let d = (ax * ax + ay * ay).sqrt();
        let t = d / 40.0;
        assert!((ay - 10.0 * t).abs() < 5.0, "intercept is self-consistent");
    }

    #[test]
    fn lead_falls_back_to_present_position_for_a_stationary_target() {
        let (ax, ay) = lead_target(0.0, 0.0, 300.0, 400.0, 0.0, 0.0, 30.0);
        assert!((ax - 300.0).abs() < 1e-3 && (ay - 400.0).abs() < 1e-3);
    }
}
