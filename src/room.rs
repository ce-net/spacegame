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
use std::collections::HashMap;

use crate::wire::{
    BeamView, BulletView, ClientMsg, ClientPacket, DebrisView, ExplosionView, FactionView, KillView,
    ShipView, Snapshot, SnapshotTag,
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
        ClientMsg::Join { name, .. } => {
            // The optional vouch `cap` is verified at the mesh ingest boundary (see `crate::auth`),
            // not here: this apply runs on every replica and must stay deterministic, and identity is
            // already trusted from the authenticated `from`.
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

/// Per-player input-reliability state for one sector host. Sits BETWEEN the lossy mesh and the [`Sim`]:
/// it turns the best-effort `/in` stream into an exactly-once, in-order application of every client
/// action, and reports an ack the client uses to stop resending. This is the substrate the whole
/// "your inputs always reach the authority" guarantee rests on (see `NETCODE.md`).
///
/// Two lanes, mirroring [`ClientPacket`]:
/// * continuous `input` — latest-wins; a frame with a stale `input_seq` is dropped.
/// * `reliable` actions — applied only when contiguous, so nothing is skipped and nothing double-applies.
#[derive(Default)]
pub struct InputSync {
    /// player -> highest contiguous reliable seq APPLIED (also the value acked to that player).
    applied: HashMap<String, u64>,
    /// player -> highest continuous `input_seq` applied, to discard reordered/stale flight frames.
    last_input: HashMap<String, u64>,
}

impl InputSync {
    /// The reliable-input ack for `player`: the highest contiguous action seq applied. `0` = none yet.
    pub fn ack(&self, player: &str) -> u64 {
        self.applied.get(player).copied().unwrap_or(0)
    }

    /// Forget a player's reliability state (call when they leave / their ship is gone), so a rejoin
    /// starts its sequence fresh instead of being rejected as "already applied".
    pub fn forget(&mut self, player: &str) {
        self.applied.remove(player);
        self.last_input.remove(player);
    }

    /// Apply one packet from `from` (the authenticated sender). Returns `true` if it contained a `Bye`
    /// (so the host can drop the player). Continuous input is latest-wins; reliable actions apply in
    /// contiguous `seq` order, exactly once — a gap stops application until the client resends the hole.
    pub fn apply(&mut self, sim: &mut Sim, from: &str, pkt: ClientPacket) -> bool {
        // Lane 1: continuous flight input — newest wins, stale/reordered frames discarded.
        if let Some(msg) = pkt.input {
            let last = self.last_input.get(from).copied().unwrap_or(0);
            // seq 0 = unsequenced (legacy / flush-only) -> always apply.
            if pkt.input_seq == 0 || pkt.input_seq > last {
                if pkt.input_seq != 0 {
                    self.last_input.insert(from.to_string(), pkt.input_seq);
                }
                apply_client_msg(sim, from, msg);
            }
        }
        // Lane 2: reliable actions — contiguous, exactly-once. Defensive sort (the client sends ascending).
        let mut reliable = pkt.reliable;
        reliable.sort_by_key(|m| m.seq);
        let mut said_bye = false;
        for sm in reliable {
            let expected = self.applied.get(from).copied().unwrap_or(0) + 1;
            if sm.seq == expected {
                if apply_client_msg(sim, from, sm.msg) {
                    said_bye = true;
                }
                self.applied.insert(from.to_string(), sm.seq);
            } else if sm.seq < expected {
                // A resend of something already applied — ignore (idempotent).
            } else {
                // A gap: an earlier seq is still missing. Stop; the client keeps resending it.
                break;
            }
        }
        said_bye
    }
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
        shield: s.shield,
        max_shield: s.max_shield,
        energy: s.energy.round() as i32,
        max_energy: s.max_energy.round() as i32,
        effects: s.effects.effects.iter().map(|e| e.kind.code()).collect(),
        minerals: s.minerals,
        kills: s.kills,
        guns: s.guns,
        weapon: s.weapon.clone(),
        weapons: s.weapons.clone(),
        owner: s.owner.clone(),
        role: role_str(s.role).to_string(),
        input_ack: 0, // stamped by the host loop from its InputSync after the snapshot is built
        alive: s.alive,
    }
}

/// Mine views for a snapshot (optionally filtered to a viewport).
fn mine_views(sim: &crate::sim::Sim, filter: Option<&Aabb>) -> Vec<crate::wire::MineView> {
    sim.mines
        .iter()
        .filter(|m| filter.map(|f| f.contains_point(m.x, m.y)).unwrap_or(true))
        .map(|m| crate::wire::MineView {
            x: m.x.round() as i32,
            y: m.y.round() as i32,
            hue: m.hue,
            r: m.trigger.round() as i32,
            armed: sim.tick >= m.arm_at,
        })
        .collect()
}

/// Pickup views for a snapshot (optionally filtered to a viewport).
fn pickup_views(sim: &crate::sim::Sim, filter: Option<&Aabb>) -> Vec<crate::wire::PickupView> {
    sim.pickups
        .iter()
        .filter(|p| filter.map(|f| f.contains_point(p.x, p.y)).unwrap_or(true))
        .map(|p| crate::wire::PickupView {
            x: p.x.round() as i32,
            y: p.y.round() as i32,
            hue: p.hue,
            kind: p.kind.code(),
        })
        .collect()
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

    let mines = mine_views(sim, None);
    let pickups = pickup_views(sim, None);

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
        mines,
        pickups,
        debris,
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

    let mines = mine_views(sim, Some(&pad));
    let pickups = pickup_views(sim, Some(&pad));

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
        mines,
        pickups,
        debris,
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
    use crate::wire::SeqMsg;

    #[test]
    fn hue_is_stable_and_bounded() {
        let a = hue_for("deadbeef");
        assert_eq!(a, hue_for("deadbeef"));
        assert!(a < 360);
        assert_ne!(hue_for("aaaa"), hue_for("bbbb"));
    }

    fn reliable(seq: u64, msg: ClientMsg) -> ClientPacket {
        ClientPacket { input: None, input_seq: 0, reliable: vec![SeqMsg { seq, msg }] }
    }

    #[test]
    fn input_sync_applies_reliable_actions_exactly_once_and_in_order() {
        let mut sim = Sim::new();
        let mut sync = InputSync::default();
        let p = "p";
        sim.join(p, "P", hue_for(p));
        sim.ships.get_mut(p).unwrap().minerals = 100_000;

        // seq 1 builds twin-guns; applied, acked.
        sync.apply(&mut sim, p, reliable(1, ClientMsg::Build { kind: "gun".into() }));
        assert_eq!(sync.ack(p), 1);

        // A DUPLICATE seq 1 (a resend that crossed an in-flight ack) is ignored — ack stays put.
        sync.apply(&mut sim, p, reliable(1, ClientMsg::Build { kind: "gun".into() }));
        assert_eq!(sync.ack(p), 1, "ack unchanged on a duplicate (no re-apply)");

        // seq 3 arrives BEFORE seq 2 (a drop): it must be HELD, not applied, ack stays at 1.
        sync.apply(&mut sim, p, reliable(3, ClientMsg::Weapon { id: "blaster".into() }));
        assert_eq!(sync.ack(p), 1, "a gap blocks later seqs — nothing skipped");

        // The client resends both 2 and 3 in one packet: now both apply, ack advances to 3.
        sync.apply(
            &mut sim,
            p,
            ClientPacket {
                input: None,
                input_seq: 0,
                reliable: vec![
                    SeqMsg { seq: 2, msg: ClientMsg::Build { kind: "gun".into() } },
                    SeqMsg { seq: 3, msg: ClientMsg::Weapon { id: "blaster".into() } },
                ],
            },
        );
        assert_eq!(sync.ack(p), 3, "the hole filled, the stream caught up — no input lost");
    }

    #[test]
    fn input_sync_continuous_input_is_latest_wins() {
        let mut sim = Sim::new();
        let mut sync = InputSync::default();
        let p = "p";
        sim.join(p, "P", hue_for(p));

        let thrust = |seq: u64| ClientPacket {
            input: Some(ClientMsg::Input { thrust: true, turn: 0, fire: false, aim: None, name: None }),
            input_seq: seq,
            reliable: vec![],
        };
        sync.apply(&mut sim, p, thrust(5));
        assert!(sim.ships[p].want_thrust);
        // A LATER frame turns thrust off.
        sync.apply(
            &mut sim,
            p,
            ClientPacket {
                input: Some(ClientMsg::Input { thrust: false, turn: 0, fire: false, aim: None, name: None }),
                input_seq: 6,
                reliable: vec![],
            },
        );
        assert!(!sim.ships[p].want_thrust);
        // A STALE frame (seq 4, arriving reordered after 6) must be ignored — not re-enable thrust.
        sync.apply(&mut sim, p, thrust(4));
        assert!(!sim.ships[p].want_thrust, "a stale reordered frame is dropped");
    }

    #[test]
    fn input_sync_survives_a_lossy_channel_losing_every_other_packet() {
        // The directive: the server ALWAYS gets all our inputs. Model a 50%-loss channel and assert that
        // resend makes every reliable action land exactly once, in order.
        let mut sim = Sim::new();
        let mut sync = InputSync::default();
        let p = "p";
        sim.join(p, "P", hue_for(p));
        sim.ships.get_mut(p).unwrap().minerals = 1_000_000;

        // Client queues 5 distinct reliable actions (seq 1..=5) and resends its whole unacked outbox.
        let actions: Vec<ClientMsg> = vec![
            ClientMsg::Weapon { id: "blaster".into() },
            ClientMsg::Build { kind: "gun".into() },
            ClientMsg::Command { order: "hold".into(), x: None, y: None },
            ClientMsg::Build { kind: "hull".into() },
            ClientMsg::Command { order: "defend".into(), x: None, y: None },
        ];
        let mut acked = 0u64;
        let mut delivered = 0;
        // The channel delivers only even-numbered ticks; the client keeps resending until acked.
        for tick in 0..50u64 {
            if acked as usize >= actions.len() { break; }
            // Outbox = all not-yet-acked actions, resent in order.
            let outbox: Vec<SeqMsg> = actions
                .iter()
                .enumerate()
                .map(|(i, m)| SeqMsg { seq: i as u64 + 1, msg: m.clone() })
                .filter(|sm| sm.seq > acked)
                .collect();
            let pkt = ClientPacket { input: None, input_seq: 0, reliable: outbox };
            if tick % 2 == 0 {
                sync.apply(&mut sim, p, pkt); // delivered
                delivered += 1;
            }
            acked = sync.ack(p); // client learns the ack from the next snapshot
        }
        assert_eq!(sync.ack(p), 5, "all 5 actions landed exactly once and in order despite 50% loss");
        assert!(delivered < 50);
    }

    #[test]
    fn build_then_select_weapon_via_wire() {
        let mut sim = Sim::new();
        apply_client_msg(&mut sim, "playerA", ClientMsg::Join { name: "Ace".into(), cap: None });
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
        apply_client_msg(&mut sim, "near", ClientMsg::Join { name: "N".into(), cap: None });
        apply_client_msg(&mut sim, "far", ClientMsg::Join { name: "F".into(), cap: None });
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
