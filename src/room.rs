//! Sector glue: turn authenticated mesh messages into simulation intents, and turn the authoritative
//! [`Sim`] of one sector into a wire [`Snapshot`]. Pure (no mesh I/O), so the input → sim → snapshot
//! pipeline is unit-testable end to end.
//!
//! [`build_snapshot_view`] is the **interest-management** path that keeps the game playable at scale:
//! instead of shipping every entity in a crowded sector to every client, it scopes the snapshot to the
//! entities inside the client's viewport using the per-tick recursive [`AabbTree`](crate::aabb), so
//! per-client bandwidth is `O(visible)` rather than `O(sector population)`.

use crate::aabb::{Aabb, AabbTree};
use crate::faction::FactionCommand;
use crate::sim::{Intent, ShipRole, Sim, SECTOR_SIZE};
use crate::wire::{
    BeamView, BulletView, ClientMsg, DebrisView, ExplosionView, FactionView, KillView, LootView, ShipView,
    Snapshot, SnapshotTag,
};

/// Derive a stable, unspoofable hue (0..360) from a player's NodeId hex.
pub fn hue_for(node_id: &str) -> u32 {
    crate::sim::fnv1a(node_id) % 360
}

/// Map a legacy build token to its tech-tree node id, so the existing frontend's `"hull"/"speed"/"gun"`
/// buttons keep working while new content is addressed by node id directly.
fn tech_node_for(token: &str) -> &str {
    match token {
        "hull" => "hull-1",
        "speed" | "thruster" => "thruster-1",
        "gun" => "twin-guns",
        other => other,
    }
}

/// Parse a fleet command from the wire `order` token + optional coordinates.
fn command_for(order: &str, x: Option<f32>, y: Option<f32>) -> Option<FactionCommand> {
    Some(match order {
        "defend" => FactionCommand::Defend,
        "follow" => FactionCommand::Follow,
        "mine" => FactionCommand::Mine,
        "hold" => FactionCommand::Hold,
        "attack" | "attacknearest" | "attack_nearest" => FactionCommand::AttackNearest,
        "attackmove" | "attack_move" => FactionCommand::AttackMove { x: x?, y: y? },
        _ => return None,
    })
}

fn role_str(r: ShipRole) -> &'static str {
    match r {
        ShipRole::Player => "player",
        ShipRole::Drone => "drone",
        ShipRole::Fighter => "fighter",
        ShipRole::Hauler => "hauler",
    }
}

fn command_str(c: FactionCommand) -> &'static str {
    match c {
        FactionCommand::Defend => "defend",
        FactionCommand::Follow => "follow",
        FactionCommand::Mine => "mine",
        FactionCommand::Hold => "hold",
        FactionCommand::AttackNearest => "attack_nearest",
        FactionCommand::AttackMove { .. } => "attack_move",
    }
}

/// Build the per-player faction summaries for the snapshot (economy + the live NPC fleet count).
pub fn faction_views(sim: &Sim) -> Vec<FactionView> {
    use crate::faction::UnitKind;
    let mut out: Vec<FactionView> = sim
        .factions
        .values()
        .map(|f| {
            let fleet_alive = sim
                .ships
                .values()
                .filter(|s| s.owner.as_deref() == Some(f.owner.as_str()) && s.alive)
                .count() as u32;
            FactionView {
                owner: f.owner.clone(),
                minerals: f.resources.minerals,
                energy: f.resources.energy,
                alloys: f.resources.alloys,
                buildings: f.buildings.len() as u32,
                drones: f.unit_count(UnitKind::Drone) as u32,
                fighters: f.unit_count(UnitKind::Fighter) as u32,
                haulers: f.unit_count(UnitKind::Hauler) as u32,
                fleet_alive,
                power: f.power(),
                command: command_str(f.command).to_string(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.owner.cmp(&b.owner));
    out
}

/// Apply one authenticated client message from `from` to a sector's simulation. `from` is the verified
/// sender NodeId; identity is trusted from the node, not the message. Returns `true` if it was a `Bye`.
pub fn apply_client_msg(sim: &mut Sim, from: &str, msg: ClientMsg) -> bool {
    let hue = hue_for(from);
    match msg {
        ClientMsg::Join { name } => {
            sim.join(from, &name, hue);
        }
        ClientMsg::Input { thrust, turn, fire, aim, name } => {
            sim.apply_intent(from, Intent { thrust, turn, fire, aim, name }, hue);
        }
        ClientMsg::Build { kind } => {
            sim.buy_tech(from, tech_node_for(&kind));
        }
        ClientMsg::Weapon { id } => {
            sim.select_weapon(from, &id);
        }
        ClientMsg::Command { order, x, y } => {
            if let Some(cmd) = command_for(&order, x, y) {
                sim.command_faction(from, cmd);
            }
        }
        ClientMsg::Respawn => {
            sim.respawn(from);
        }
        ClientMsg::Bye => {
            sim.leave(from);
            return true;
        }
    }
    false
}

fn ship_view(id: &str, s: &crate::sim::Ship) -> ShipView {
    ShipView {
        id: id.to_string(),
        name: s.name.clone(),
        hue: s.hue,
        x: s.x.round().clamp(0.0, SECTOR_SIZE) as i32,
        y: s.y.round().clamp(0.0, SECTOR_SIZE) as i32,
        a: (s.a * 100.0).round() as i32,
        hp: s.hp,
        max_hp: s.max_hp,
        minerals: s.minerals,
        kills: s.kills,
        guns: s.guns,
        weapon: s.weapon.clone(),
        weapons: s.weapons.clone(),
        owner: s.owner.clone(),
        role: role_str(s.role).to_string(),
        alive: s.alive,
    }
}

/// Build the full wire snapshot for a sector's current authoritative state (every entity). Used for
/// small sectors and tests; large sectors should prefer [`build_snapshot_view`].
pub fn build_snapshot(sim: &Sim, sector: &str, host: &str, now_ms: u64) -> Snapshot {
    let mut ships: Vec<ShipView> = sim.ships.iter().map(|(id, s)| ship_view(id, s)).collect();
    ships.sort_by(|a, b| a.id.cmp(&b.id));

    let bullets: Vec<BulletView> = sim
        .bullets
        .iter()
        .map(|b| BulletView {
            x: b.x.round() as i32,
            y: b.y.round() as i32,
            vx: b.vx.round() as i32,
            vy: b.vy.round() as i32,
            hue: b.hue,
            homing: b.homing > 0.0,
        })
        .collect();

    let beams: Vec<BeamView> = sim
        .beams
        .iter()
        .map(|b| BeamView {
            x0: b.x0.round() as i32,
            y0: b.y0.round() as i32,
            x1: b.x1.round() as i32,
            y1: b.y1.round() as i32,
            hue: b.hue,
            kind: b.kind,
        })
        .collect();

    let explosions: Vec<ExplosionView> = sim
        .explosions
        .iter()
        .map(|e| ExplosionView { x: e.x.round() as i32, y: e.y.round() as i32, r: e.r.round() as i32, hue: e.hue })
        .collect();

    let debris: Vec<DebrisView> = sim
        .debris
        .bodies
        .iter()
        .map(|b| DebrisView {
            x: b.pos.x.round() as i32,
            y: b.pos.y.round() as i32,
            a: (b.angle * 100.0).round() as i32,
            r: b.shape.bound_radius().round() as i32,
        })
        .collect();

    let loot: Vec<LootView> = sim
        .loot
        .iter()
        .map(|l| LootView {
            x: l.x.round() as i32,
            y: l.y.round() as i32,
            vx: l.vx.round() as i32,
            vy: l.vy.round() as i32,
            amount: l.amount,
        })
        .collect();

    let depleted = sim.depleted_cells().into_iter().map(|(cx, cy, _)| [cx, cy]).collect();

    let kills = sim
        .kill_feed
        .iter()
        .map(|k| KillView { killer: k.killer.clone(), victim: k.victim.clone() })
        .collect();

    Snapshot {
        t: SnapshotTag::St,
        sector: sector.to_string(),
        host: host.to_string(),
        tick: sim.tick,
        ships,
        bullets,
        beams,
        explosions,
        debris,
        loot,
        factions: faction_views(sim),
        depleted,
        kills,
        ruleset: sim.rules.version,
        ts: now_ms,
    }
}

/// **Interest management:** build a snapshot scoped to a client's `viewport` (sector-local world
/// rectangle). Only ships/bullets/beams whose position falls inside the (slightly padded) viewport are
/// included, found via a recursive AABB query rather than scanning every entity — so a client in a
/// 5000-ship sector still receives only what is on its screen, and per-client bandwidth stays bounded
/// no matter how full the sector is. Kill feed and ruleset version are global and always included.
pub fn build_snapshot_view(sim: &Sim, sector: &str, host: &str, now_ms: u64, viewport: Aabb) -> Snapshot {
    let pad = viewport.expanded(64.0);

    // Ships: index with the AABB tree, query the viewport.
    let ship_tree: AabbTree<String> = AabbTree::build(
        Aabb::new(0.0, 0.0, SECTOR_SIZE, SECTOR_SIZE),
        sim.ships.iter().map(|(id, s)| (Aabb::around(s.x, s.y, crate::sim::SHIP_R), id.clone())),
    );
    let mut visible_ids = ship_tree.query(&pad);
    visible_ids.sort();
    let ships: Vec<ShipView> = visible_ids
        .iter()
        .filter_map(|id| sim.ships.get(id).map(|s| ship_view(id, s)))
        .collect();

    let bullets: Vec<BulletView> = sim
        .bullets
        .iter()
        .filter(|b| pad.contains_point(b.x, b.y))
        .map(|b| BulletView {
            x: b.x.round() as i32,
            y: b.y.round() as i32,
            vx: b.vx.round() as i32,
            vy: b.vy.round() as i32,
            hue: b.hue,
            homing: b.homing > 0.0,
        })
        .collect();

    let beams: Vec<BeamView> = sim
        .beams
        .iter()
        .filter(|b| pad.intersects(&Aabb::new(b.x0, b.y0, b.x1, b.y1)))
        .map(|b| BeamView {
            x0: b.x0.round() as i32,
            y0: b.y0.round() as i32,
            x1: b.x1.round() as i32,
            y1: b.y1.round() as i32,
            hue: b.hue,
            kind: b.kind,
        })
        .collect();

    let explosions: Vec<ExplosionView> = sim
        .explosions
        .iter()
        .filter(|e| pad.contains_point(e.x, e.y))
        .map(|e| ExplosionView { x: e.x.round() as i32, y: e.y.round() as i32, r: e.r.round() as i32, hue: e.hue })
        .collect();

    let debris: Vec<DebrisView> = sim
        .debris
        .bodies
        .iter()
        .filter(|b| pad.contains_point(b.pos.x, b.pos.y))
        .map(|b| DebrisView {
            x: b.pos.x.round() as i32,
            y: b.pos.y.round() as i32,
            a: (b.angle * 100.0).round() as i32,
            r: b.shape.bound_radius().round() as i32,
        })
        .collect();

    let loot: Vec<LootView> = sim
        .loot
        .iter()
        .filter(|l| pad.contains_point(l.x, l.y))
        .map(|l| LootView {
            x: l.x.round() as i32,
            y: l.y.round() as i32,
            vx: l.vx.round() as i32,
            vy: l.vy.round() as i32,
            amount: l.amount,
        })
        .collect();

    let depleted = sim
        .depleted_cells()
        .into_iter()
        .filter(|(cx, cy, _)| {
            let x = *cx as f32 * crate::sim::ROCK_CELL;
            let y = *cy as f32 * crate::sim::ROCK_CELL;
            pad.intersects(&Aabb::new(x, y, x + crate::sim::ROCK_CELL, y + crate::sim::ROCK_CELL))
        })
        .map(|(cx, cy, _)| [cx, cy])
        .collect();

    let kills = sim
        .kill_feed
        .iter()
        .map(|k| KillView { killer: k.killer.clone(), victim: k.victim.clone() })
        .collect();

    Snapshot {
        t: SnapshotTag::St,
        sector: sector.to_string(),
        host: host.to_string(),
        tick: sim.tick,
        ships,
        bullets,
        beams,
        explosions,
        debris,
        loot,
        factions: faction_views(sim),
        depleted,
        kills,
        ruleset: sim.rules.version,
        ts: now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_is_stable_and_bounded() {
        let a = hue_for("deadbeef");
        assert_eq!(a, hue_for("deadbeef"));
        assert!(a < 360);
        assert_ne!(hue_for("aaaa"), hue_for("bbbb"));
    }

    #[test]
    fn build_then_select_weapon_via_wire() {
        let mut sim = Sim::new();
        apply_client_msg(&mut sim, "playerA", ClientMsg::Join { name: "Ace".into() });
        sim.factions.values_mut().for_each(|f| f.units.clear()); // no NPC fleet ships in the snapshot
        sim.ships.get_mut("playerA").unwrap().minerals = 1000;
        // Legacy "gun" token maps onto the twin-guns tech node.
        apply_client_msg(&mut sim, "playerA", ClientMsg::Build { kind: "gun".into() });
        assert_eq!(sim.ships["playerA"].guns, 2, "legacy gun token bought twin-guns");
        // Unlock + select the railgun by node/weapon id.
        apply_client_msg(&mut sim, "playerA", ClientMsg::Build { kind: "tech-railgun".into() });
        apply_client_msg(&mut sim, "playerA", ClientMsg::Weapon { id: "railgun".into() });
        sim.tick(1.0);
        let snap = build_snapshot(&sim, "0_0", "hostNode", 123_456);
        let sv = &snap.ships[0];
        assert_eq!(sv.weapon, "railgun");
        assert!(sv.weapons.contains(&"railgun".to_string()));
        assert_eq!(snap.ruleset, sim.rules.version);
    }

    #[test]
    fn client_cannot_spoof_hue() {
        let mut sim = Sim::new();
        apply_client_msg(
            &mut sim,
            "p",
            ClientMsg::Input { thrust: false, turn: 0, fire: false, aim: None, name: Some("nm".into()) },
        );
        sim.tick(1.0);
        let snap = build_snapshot(&sim, "0_0", "h", 0);
        assert_eq!(snap.ships[0].hue, hue_for("p"));
    }

    #[test]
    fn view_scoped_snapshot_only_includes_visible_ships() {
        let mut sim = Sim::new();
        sim.seamless = false;
        // One ship near the origin, one far away.
        apply_client_msg(&mut sim, "near", ClientMsg::Join { name: "N".into() });
        apply_client_msg(&mut sim, "far", ClientMsg::Join { name: "F".into() });
        sim.factions.values_mut().for_each(|f| f.units.clear()); // count only the two player ships
        sim.ships.get_mut("near").unwrap().x = 200.0;
        sim.ships.get_mut("near").unwrap().y = 200.0;
        sim.ships.get_mut("far").unwrap().x = 2800.0;
        sim.ships.get_mut("far").unwrap().y = 2800.0;
        sim.tick(1.0);

        let viewport = Aabb::new(0.0, 0.0, 600.0, 600.0);
        let snap = build_snapshot_view(&sim, "0_0", "h", 0, viewport);
        let ids: Vec<&str> = snap.ships.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"near"), "the near ship is visible");
        assert!(!ids.contains(&"far"), "the far ship is culled — bounded per-client bandwidth");

        // The full snapshot still has both (used for small sectors).
        let full = build_snapshot(&sim, "0_0", "h", 0);
        assert_eq!(full.ships.len(), 2);
    }
}
