//! **Status effects** — the transient combat-state layer that turns flat damage into tactics.
//!
//! A weapon can do more than subtract hull: an EMP torpedo *disables* a ship, an incendiary round
//! sets it *burning* over time, a tractor beam *pins* it in place, a disruptor *slows* it, and a
//! salvaged overcharge cell *buffs* the ship that grabs it. Each of those is a [`StatusEffect`] — a
//! `(kind, expiry-tick, magnitude)` triple — held in a per-ship [`StatusStack`] the authoritative
//! [`crate::sim::Sim`] reads every tick.
//!
//! Everything here is **pure and deterministic**: effects are keyed by [`StatusKind`] (at most one
//! entry per kind), application takes the *stronger and longer* of the two so the order in which two
//! replicas apply the same hits never matters, and expiry is by tick number, not wall clock. That is
//! what keeps the replicated, anti-cheat simulation in agreement — two honest hosts that apply the
//! same EMP on the same tick fold to the same [`StatusStack::hash`].
//!
//! The effects themselves are interpreted by the sim ([`crate::sim`]): EMP zeroes a ship's thrust and
//! trigger, Slow scales its top speed, Stasis bleeds its velocity, Burn ticks hull damage credited to
//! the source, and Overcharge sharpens its rate of fire and damage. New effects are additive: add a
//! variant, give a weapon an [`crate::ruleset::OnHitEffect`], and teach the sim to read it.

use serde::{Deserialize, Serialize};

/// A kind of status effect. The sim dispatches behaviour on this; the renderer dispatches a visual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    /// **Electromagnetic shock.** While active, the ship cannot thrust or fire (its drive and
    /// triggers are fried) and its shield does not regenerate. The signature effect of EMP ordnance.
    Emp,
    /// **Burn / incendiary.** Deals `magnitude` hull damage every tick directly (bypassing shields —
    /// it is a fire on the hull), credited to the ship that lit it. Damage-over-time pressure.
    Burn,
    /// **Disruptor slow.** Scales the ship's effective top speed and acceleration by `(1 - magnitude)`
    /// (clamped), so `magnitude = 0.5` halves a ship's mobility. Kiting / zoning.
    Slow,
    /// **Stasis / tractor lock.** Bleeds the ship's velocity toward zero at rate `magnitude` each tick
    /// (`1.0` = fully pinned), holding a target in a kill box. What a tractor beam projects.
    Stasis,
    /// **Overcharge buff** (a *friendly* effect, from a salvaged power cell). Raises rate of fire and
    /// damage by `magnitude` while active. The one effect a ship *wants*.
    Overcharge,
}

impl StatusKind {
    /// Is this a buff the ship benefits from (rather than a debuff inflicted on it)?
    pub fn is_buff(self) -> bool {
        matches!(self, StatusKind::Overcharge)
    }

    /// A stable small integer for the wire / hashing, independent of enum layout.
    pub fn code(self) -> u8 {
        match self {
            StatusKind::Emp => 0,
            StatusKind::Burn => 1,
            StatusKind::Slow => 2,
            StatusKind::Stasis => 3,
            StatusKind::Overcharge => 4,
        }
    }
}

/// One active status effect on a ship.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusEffect {
    pub kind: StatusKind,
    /// Tick number at (and after) which this effect has expired.
    pub until: u64,
    /// Effect-specific strength (burn dmg/tick, slow fraction, stasis bleed, overcharge bonus).
    pub magnitude: f32,
    /// Who inflicted it — so a Burn kill is credited to the right ship in the feed. Empty for an
    /// environmental or self-applied effect.
    #[serde(default)]
    pub source: String,
}

/// The set of status effects on one ship — at most one entry per [`StatusKind`]. Persistent (carried
/// through snapshots and cross-sector transit) so a debuff survives a host failover or a jump.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusStack {
    pub effects: Vec<StatusEffect>,
}

impl StatusStack {
    pub fn new() -> Self {
        StatusStack::default()
    }

    /// Apply (or refresh) an effect. If one of the same kind is already present, take the **stronger
    /// magnitude and the later expiry** — so applying the same hit on two replicas converges to one
    /// state regardless of order or duplication. Returns whether anything changed.
    pub fn apply(&mut self, kind: StatusKind, until: u64, magnitude: f32, source: &str) -> bool {
        if let Some(e) = self.effects.iter_mut().find(|e| e.kind == kind) {
            let mut changed = false;
            if until > e.until {
                e.until = until;
                changed = true;
            }
            if magnitude > e.magnitude {
                e.magnitude = magnitude;
                // The strongest application owns the credit (matters for Burn kills).
                e.source = source.to_string();
                changed = true;
            }
            changed
        } else {
            self.effects.push(StatusEffect { kind, until, magnitude, source: source.to_string() });
            // Keep a deterministic order (by kind code) so the Vec never depends on application order.
            self.effects.sort_by_key(|e| e.kind.code());
            true
        }
    }

    /// Drop every effect that has expired at `now`. Cheap; call once per ship per tick.
    pub fn expire(&mut self, now: u64) {
        self.effects.retain(|e| now < e.until);
    }

    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// Is an effect of `kind` currently present (assumes [`expire`](Self::expire) already ran)?
    pub fn has(&self, kind: StatusKind) -> bool {
        self.effects.iter().any(|e| e.kind == kind)
    }

    /// The magnitude of `kind`, or `0.0` if absent.
    pub fn magnitude(&self, kind: StatusKind) -> f32 {
        self.effects.iter().find(|e| e.kind == kind).map(|e| e.magnitude).unwrap_or(0.0)
    }

    /// The active effect of `kind`, if any.
    pub fn get(&self, kind: StatusKind) -> Option<&StatusEffect> {
        self.effects.iter().find(|e| e.kind == kind)
    }

    /// **Movement gate:** can the ship thrust this tick? (EMP fries the drive.)
    pub fn can_thrust(&self) -> bool {
        !self.has(StatusKind::Emp)
    }

    /// **Weapon gate:** can the ship fire this tick? (EMP fries the triggers.)
    pub fn can_fire(&self) -> bool {
        !self.has(StatusKind::Emp)
    }

    /// **Shield gate:** may the shield regenerate this tick? (EMP suppresses the regulator.)
    pub fn shield_regenerates(&self) -> bool {
        !self.has(StatusKind::Emp)
    }

    /// Multiplier on top speed / acceleration from Slow (1.0 = unaffected, 0.0 = pinned).
    pub fn mobility_mult(&self) -> f32 {
        (1.0 - self.magnitude(StatusKind::Slow)).clamp(0.05, 1.0)
    }

    /// Velocity-retention factor from Stasis (1.0 = free, lower = dragged toward zero).
    pub fn stasis_retain(&self) -> f32 {
        (1.0 - self.magnitude(StatusKind::Stasis)).clamp(0.0, 1.0)
    }

    /// Fire-rate / damage multiplier from Overcharge (>= 1.0).
    pub fn overcharge_mult(&self) -> f32 {
        1.0 + self.magnitude(StatusKind::Overcharge)
    }

    /// An order-independent digest of the stack for the deterministic state hash. Folds each effect
    /// with XOR so the (already sorted) Vec order can never cause a false replica disagreement.
    pub fn hash(&self) -> u64 {
        const PRIME: u64 = 0x100000001b3;
        let mut acc: u64 = 0;
        for e in &self.effects {
            let mut h: u64 = 0x9e3779b97f4a7c15;
            h ^= e.kind.code() as u64;
            h = h.wrapping_mul(PRIME);
            h ^= e.until;
            h = h.wrapping_mul(PRIME);
            h ^= (e.magnitude * 64.0).round() as i64 as u64;
            h = h.wrapping_mul(PRIME);
            acc ^= h;
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_takes_stronger_and_longer() {
        let mut s = StatusStack::new();
        assert!(s.apply(StatusKind::Slow, 100, 0.3, "a"));
        // A weaker, shorter re-application changes nothing.
        assert!(!s.apply(StatusKind::Slow, 50, 0.1, "b"));
        assert_eq!(s.magnitude(StatusKind::Slow), 0.3);
        assert_eq!(s.get(StatusKind::Slow).unwrap().until, 100);
        // A stronger, longer one upgrades both and takes the credit.
        assert!(s.apply(StatusKind::Slow, 200, 0.6, "c"));
        assert_eq!(s.magnitude(StatusKind::Slow), 0.6);
        assert_eq!(s.get(StatusKind::Slow).unwrap().until, 200);
        assert_eq!(s.get(StatusKind::Slow).unwrap().source, "c");
        // Still exactly one Slow entry.
        assert_eq!(s.effects.iter().filter(|e| e.kind == StatusKind::Slow).count(), 1);
    }

    #[test]
    fn expiry_is_by_tick() {
        let mut s = StatusStack::new();
        s.apply(StatusKind::Burn, 10, 5.0, "x");
        s.expire(9);
        assert!(s.has(StatusKind::Burn));
        s.expire(10);
        assert!(!s.has(StatusKind::Burn), "expired exactly at `until`");
    }

    #[test]
    fn emp_gates_thrust_fire_and_shield() {
        let mut s = StatusStack::new();
        s.apply(StatusKind::Emp, 50, 1.0, "x");
        assert!(!s.can_thrust());
        assert!(!s.can_fire());
        assert!(!s.shield_regenerates());
    }

    #[test]
    fn gameplay_multipliers_are_sane() {
        let mut s = StatusStack::new();
        s.apply(StatusKind::Slow, 50, 0.5, "x");
        assert!((s.mobility_mult() - 0.5).abs() < 1e-6);
        s.apply(StatusKind::Stasis, 50, 1.0, "x");
        assert_eq!(s.stasis_retain(), 0.0, "full stasis pins velocity");
        s.apply(StatusKind::Overcharge, 50, 0.4, "self");
        assert!((s.overcharge_mult() - 1.4).abs() < 1e-6);
    }

    #[test]
    fn hash_is_order_independent_and_reacts_to_change() {
        let mut a = StatusStack::new();
        let mut b = StatusStack::new();
        a.apply(StatusKind::Burn, 30, 4.0, "s");
        a.apply(StatusKind::Slow, 40, 0.3, "s");
        b.apply(StatusKind::Slow, 40, 0.3, "s");
        b.apply(StatusKind::Burn, 30, 4.0, "s");
        assert_eq!(a.hash(), b.hash(), "same effects, applied in any order, hash the same");
        b.apply(StatusKind::Emp, 60, 1.0, "s");
        assert_ne!(a.hash(), b.hash(), "an extra effect changes the digest");
    }
}
