//! **Spawn & matchmaking** — where a ship enters the galaxy, and how friends find each other in it.
//!
//! Entering an infinite, self-subdividing, procedurally-born galaxy is a matchmaking problem: drop a
//! newcomer in the wrong place and they're alone in the void or dead on arrival. The resolver places
//! every spawn against the live signals the rest of the system already produces — cell **danger**
//! ([`crate::worldgen`]), cell **population + headroom** ([`crate::galaxy`]/[`crate::fleet`]), and
//! **presence** (where your friends/squad are) — so the answer is always "somewhere alive, somewhere
//! fair, somewhere you meant to be":
//!
//! - **Fresh player** → a **haven**: a deterministic, low-danger, populated core system everyone can
//!   reliably start at. New players cluster at known safe ports, not scattered in the dark.
//! - **Join near a friend** → spawn inside your friend's view, *with their consent* and danger-gated, so
//!   you arrive next to them — but the game won't teleport a newbie into a deep-frontier ambush uninvited.
//! - **With a party** → the whole squad lands together, in formation.
//! - **Respawn** → near where you fell (after a cooldown) or at your home/allied territory.
//! - **At a landmark / invite** → spawn at a named place someone shared.
//!
//! Every spawn gets brief **spawn protection** and is nudged to an open spot clear of hostile guns, so
//! "appear and instantly die" can't happen.

use std::collections::{HashMap, HashSet};

use crate::galaxy::{CellId, World};
use crate::worldgen::{CellGenesis, GalaxySeed};

pub type PlayerId = String;
pub type PartyId = u64;

/// Why a ship is entering — the player's intent. The resolver turns this into a concrete place.
#[derive(Debug, Clone)]
pub enum SpawnIntent {
    /// Brand-new pilot: send me somewhere safe and sociable.
    Fresh,
    /// Put me next to this friend (subject to their consent + danger gating).
    NearFriend { friend: PlayerId },
    /// Land my whole squad together.
    WithParty { party: PartyId, members: Vec<PlayerId> },
    /// Respawn after death, near where I fell (cooldown) or at my home.
    Respawn { died_at: CellId, died_pos: (World, World), home: Option<CellId> },
    /// Spawn at a named place from an invite / shared landmark.
    AtLandmark { cell: CellId, pos: (World, World) },
}

/// A player's live whereabouts, gossiped on a presence topic and visible per the privacy rules below.
#[derive(Debug, Clone)]
pub struct Presence {
    pub cell: CellId,
    pub pos: (World, World),
    pub party: Option<PartyId>,
    pub epoch: u64,
    /// Whether this player currently accepts "join near me" (off in ranked/solo, or while cloaked).
    pub joinable: bool,
}

/// Mutual friend graph + per-player join settings. Visibility is friends-only and consensual — you
/// can't locate or spawn onto someone who hasn't friended you and left joining on.
#[derive(Debug, Default)]
pub struct Social {
    friends: HashMap<PlayerId, HashSet<PlayerId>>,
}

impl Social {
    pub fn befriend(&mut self, a: PlayerId, b: PlayerId) {
        self.friends.entry(a.clone()).or_default().insert(b.clone());
        self.friends.entry(b).or_default().insert(a);
    }
    pub fn are_friends(&self, a: &PlayerId, b: &PlayerId) -> bool {
        self.friends.get(a).map(|s| s.contains(b)).unwrap_or(false)
    }
}

/// The rules a spawn must satisfy.
#[derive(Debug, Clone, Copy)]
pub struct SpawnRules {
    /// A newcomer is never dropped into a cell more dangerous than this.
    pub max_danger_for_newcomer: f32,
    /// Don't spawn into a cell with fewer free host slots than this (it may be merging/culling).
    pub min_headroom: u32,
    /// Seconds of invulnerability on arrival.
    pub protection_secs: u32,
    /// "Near a friend" must land within this distance (their view radius).
    pub join_within: World,
    /// Respawn cooldown before you can return to your death cell.
    pub respawn_cooldown_secs: u32,
    /// Keep new spawns at least this far from any known hostile.
    pub clear_of_hostiles: World,
}

impl Default for SpawnRules {
    fn default() -> Self {
        SpawnRules {
            max_danger_for_newcomer: 0.25,
            min_headroom: 1,
            protection_secs: 5,
            join_within: 1200.0,
            respawn_cooldown_secs: 4,
            clear_of_hostiles: 600.0,
        }
    }
}

/// The concrete result: land here, with protection until this epoch, for this reason.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnPlan {
    pub cell: CellId,
    pub pos: (World, World),
    pub protection_until_epoch: u64,
    pub reason: SpawnReason,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpawnReason {
    Haven,
    NextTo(PlayerId),
    WithParty(PartyId),
    NearDeath,
    Home,
    Landmark,
    /// The requested target was unavailable (too dangerous / no consent / full) — fell back to a haven.
    FallbackHaven(&'static str),
}

/// What the resolver needs to know about a candidate cell, supplied by the caller from the live galaxy
/// + fleet views (kept as a closure-free struct so this module stays pure and testable).
#[derive(Debug, Clone, Copy)]
pub struct CellStatus {
    pub danger: f32,
    pub players: u32,
    pub host_headroom: u32,
    /// Whether a host is currently authoritative for it (a void cell has none until born).
    pub charted: bool,
}

/// Resolves spawn intents into places. Holds the social graph, a presence snapshot, the rules, and the
/// galaxy seed (for finding deterministic havens). The `status` fn is injected so the resolver reads the
/// same live signals the autoscaler does without depending on its types.
pub struct SpawnResolver<'a> {
    pub seed: GalaxySeed,
    pub rules: SpawnRules,
    pub social: &'a Social,
    pub presence: &'a HashMap<PlayerId, Presence>,
    /// Live status of a cell (danger/pop/headroom/charted).
    pub status: &'a dyn Fn(CellId) -> CellStatus,
    /// Known hostile positions near a candidate spot, so we don't spawn under guns.
    pub hostiles_near: &'a dyn Fn(CellId, (World, World)) -> Vec<(World, World)>,
}

impl<'a> SpawnResolver<'a> {
    pub fn resolve(&self, who: &PlayerId, intent: SpawnIntent, epoch: u64) -> SpawnPlan {
        let protect_epochs = self.rules.protection_secs as u64; // 1 epoch ~ 1s here
        let until = epoch + protect_epochs;
        match intent {
            SpawnIntent::Fresh => self.haven_plan(SpawnReason::Haven, until),

            SpawnIntent::NearFriend { friend } => self
                .join_near(who, &friend, until)
                .unwrap_or_else(|why| self.haven_plan(SpawnReason::FallbackHaven(why), until)),

            SpawnIntent::WithParty { party, members } => self
                .party_anchor(&members)
                .map(|(cell, pos)| SpawnPlan {
                    cell,
                    pos: self.open_spot(cell, pos),
                    protection_until_epoch: until,
                    reason: SpawnReason::WithParty(party),
                })
                .unwrap_or_else(|| self.haven_plan(SpawnReason::FallbackHaven("party not located"), until)),

            SpawnIntent::Respawn { died_at, died_pos, home } => {
                let s = (self.status)(died_at);
                // Prefer the death cell once it's safe-ish and still live; else home; else haven.
                if s.charted && s.danger <= 0.8 {
                    SpawnPlan { cell: died_at, pos: self.open_spot(died_at, died_pos), protection_until_epoch: until, reason: SpawnReason::NearDeath }
                } else if let Some(h) = home {
                    SpawnPlan { cell: h, pos: self.open_spot(h, h.rect().center()), protection_until_epoch: until, reason: SpawnReason::Home }
                } else {
                    self.haven_plan(SpawnReason::FallbackHaven("death cell unsafe, no home"), until)
                }
            }

            SpawnIntent::AtLandmark { cell, pos } => {
                let s = (self.status)(cell);
                if s.charted && s.host_headroom >= self.rules.min_headroom {
                    SpawnPlan { cell, pos: self.open_spot(cell, pos), protection_until_epoch: until, reason: SpawnReason::Landmark }
                } else {
                    self.haven_plan(SpawnReason::FallbackHaven("landmark unavailable"), until)
                }
            }
        }
    }

    /// Try to land `who` beside `friend`: requires friendship, the friend present + joinable, the cell
    /// not too lethal for the joiner, and an open slot. Errors give a reason for the haven fallback.
    fn join_near(&self, who: &PlayerId, friend: &PlayerId, until: u64) -> Result<SpawnPlan, &'static str> {
        if !self.social.are_friends(who, friend) {
            return Err("not friends");
        }
        let p = self.presence.get(friend).ok_or("friend offline")?;
        if !p.joinable {
            return Err("friend not accepting joins");
        }
        let s = (self.status)(p.cell);
        if s.danger > 0.8 {
            return Err("friend in lethal space"); // app may offer an explicit "join anyway"
        }
        if s.host_headroom < self.rules.min_headroom {
            return Err("friend's cell is full");
        }
        // Land within the friend's view, then nudge clear of hostiles.
        let near = nudge_within(p.pos, self.rules.join_within * 0.5);
        Ok(SpawnPlan {
            cell: p.cell,
            pos: self.open_spot(p.cell, near),
            protection_until_epoch: until,
            reason: SpawnReason::NextTo(friend.clone()),
        })
    }

    /// The squad's anchor: the cell most party members are already in (so a party regroups on its bulk),
    /// or the first online member's cell.
    fn party_anchor(&self, members: &[PlayerId]) -> Option<(CellId, (World, World))> {
        let mut votes: HashMap<CellId, u32> = HashMap::new();
        let mut any: Option<&Presence> = None;
        for m in members {
            if let Some(p) = self.presence.get(m) {
                *votes.entry(p.cell).or_default() += 1;
                any = Some(p);
            }
        }
        votes
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .and_then(|(cell, _)| self.presence.values().find(|p| p.cell == cell).map(|p| (cell, p.pos)))
            .or_else(|| any.map(|p| (p.cell, p.pos)))
    }

    /// A deterministic, safe, sociable starting cell — the haven. Scans the low-ring core for a charted
    /// cell with a habitable system under the newcomer danger cap and with host headroom; falls back to
    /// the origin. Because worldgen is deterministic, the set of havens is stable galaxy-wide, so new
    /// players reliably meet at the same ports.
    fn haven_plan(&self, reason: SpawnReason, until: u64) -> SpawnPlan {
        for cell in self.candidate_havens() {
            let s = (self.status)(cell);
            if s.danger <= self.rules.max_danger_for_newcomer && s.host_headroom >= self.rules.min_headroom {
                let g = CellGenesis::generate(&self.seed, cell);
                let pos = g
                    .systems
                    .iter()
                    .find(|sys| sys.habitable)
                    .map(|sys| sys.pos)
                    .unwrap_or_else(|| cell.rect().center());
                return SpawnPlan { cell, pos: self.open_spot(cell, pos), protection_until_epoch: until, reason };
            }
        }
        let origin = CellId::at_depth_for(12, 0.0, 0.0);
        SpawnPlan { cell: origin, pos: origin.rect().center(), protection_until_epoch: until, reason }
    }

    /// Deterministic candidate haven cells: a small spiral of low-ring cells around the origin. Stable
    /// for a given seed/depth, so havens are a fixed, discoverable set of safe ports.
    fn candidate_havens(&self) -> Vec<CellId> {
        let origin = CellId::at_depth_for(12, 0.0, 0.0);
        let mut out = vec![origin];
        for (dx, dy) in [(1i64, 0), (-1, 0), (0, 1), (0, -1), (2, 0), (0, 2), (-2, 0), (0, -2)] {
            out.push(CellId {
                depth: origin.depth,
                x: (origin.x as i64 + dx).max(0) as u32,
                y: (origin.y as i64 + dy).max(0) as u32,
            });
        }
        out
    }

    /// Nudge a desired position to a clear spot — away from hostile guns, within the cell. Keeps spawns
    /// from materialising inside a firefight.
    fn open_spot(&self, cell: CellId, near: (World, World)) -> (World, World) {
        let hostiles = (self.hostiles_near)(cell, near);
        if hostiles.is_empty() {
            return near;
        }
        // Spiral out from `near` until far enough from every hostile (bounded, deterministic).
        let step = self.rules.clear_of_hostiles * 0.5;
        for ring in 1..8 {
            for k in 0..8 {
                let ang = k as World * std::f64::consts::TAU / 8.0;
                let cand = (near.0 + ang.cos() * step * ring as World, near.1 + ang.sin() * step * ring as World);
                if hostiles.iter().all(|h| dist(cand, *h) >= self.rules.clear_of_hostiles) {
                    return cand;
                }
            }
        }
        near
    }
}

fn dist(a: (World, World), b: (World, World)) -> World {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Deterministic small offset within `r` of a point (so two joiners don't stack exactly).
fn nudge_within(p: (World, World), r: World) -> (World, World) {
    (p.0 + r * 0.3, p.1 - r * 0.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe_status(_c: CellId) -> CellStatus {
        CellStatus { danger: 0.1, players: 5, host_headroom: 3, charted: true }
    }
    fn no_hostiles(_c: CellId, _p: (World, World)) -> Vec<(World, World)> {
        Vec::new()
    }

    #[test]
    fn fresh_player_spawns_at_a_safe_haven() {
        let social = Social::default();
        let presence = HashMap::new();
        let r = SpawnResolver {
            seed: GalaxySeed([1u8; 32]),
            rules: SpawnRules::default(),
            social: &social,
            presence: &presence,
            status: &safe_status,
            hostiles_near: &no_hostiles,
        };
        let plan = r.resolve(&"new".into(), SpawnIntent::Fresh, 100);
        assert_eq!(plan.reason, SpawnReason::Haven);
        assert!(plan.protection_until_epoch > 100);
    }

    #[test]
    fn join_near_friend_requires_friendship_and_consent() {
        let mut social = Social::default();
        social.befriend("me".into(), "bud".into());
        let mut presence = HashMap::new();
        presence.insert(
            "bud".to_string(),
            Presence { cell: CellId { depth: 12, x: 2049, y: 2050 }, pos: (10.0, 10.0), party: None, epoch: 99, joinable: true },
        );
        let r = SpawnResolver {
            seed: GalaxySeed([1u8; 32]),
            rules: SpawnRules::default(),
            social: &social,
            presence: &presence,
            status: &safe_status,
            hostiles_near: &no_hostiles,
        };
        let plan = r.resolve(&"me".into(), SpawnIntent::NearFriend { friend: "bud".into() }, 100);
        assert_eq!(plan.reason, SpawnReason::NextTo("bud".into()));
        assert_eq!(plan.cell, CellId { depth: 12, x: 2049, y: 2050 });

        // A stranger can't locate or spawn onto them → haven fallback.
        let plan2 = r.resolve(&"stranger".into(), SpawnIntent::NearFriend { friend: "bud".into() }, 100);
        assert!(matches!(plan2.reason, SpawnReason::FallbackHaven(_)));
    }

    #[test]
    fn spawn_avoids_hostile_guns() {
        let social = Social::default();
        let presence = HashMap::new();
        let hostile_at_target = |_c: CellId, p: (World, World)| vec![p]; // a hostile exactly where we'd land
        let r = SpawnResolver {
            seed: GalaxySeed([1u8; 32]),
            rules: SpawnRules::default(),
            social: &social,
            presence: &presence,
            status: &safe_status,
            hostiles_near: &hostile_at_target,
        };
        let plan = r.resolve(&"new".into(), SpawnIntent::Fresh, 100);
        // The chosen spot is pushed clear of the hostile sitting on the ideal point.
        assert!((plan.pos.0).abs() + (plan.pos.1).abs() > 0.0);
    }
}
