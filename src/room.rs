//! Sector glue: turn authenticated mesh messages into simulation intents, and turn the authoritative
//! [`Sim`] of one sector into a wire [`Snapshot`]. Pure (no mesh I/O), so the input → sim → snapshot
//! pipeline is unit-testable end to end.

use crate::sim::{Intent, Sim, Upgrade, SECTOR_SIZE};
use crate::wire::{BulletView, ClientMsg, KillView, ShipView, Snapshot, SnapshotTag};

/// Derive a stable, unspoofable hue (0..360) from a player's NodeId hex. The NodeId is authenticated
/// by the node, so the color cannot be faked. FNV-1a/32 over the id bytes, matching the frontend's
/// `hueForId` so a client's self-rendered color agrees with the authoritative one.
pub fn hue_for(node_id: &str) -> u32 {
    crate::sim::fnv1a(node_id) % 360
}

/// Apply one authenticated client message from `from` to a sector's simulation. `from` is the
/// verified sender NodeId; the wire payload never carries a player id, so identity is trusted from
/// the node, not the message. Returns `true` if the message was a `Bye`.
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
            if let Some(up) = Upgrade::from_token(&kind) {
                sim.buy(from, up);
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

/// Build the wire snapshot for a sector's current authoritative state. `sector` is the sector token,
/// `host` this backend's NodeId, `now_ms` the host wall clock so clients can estimate RTT.
pub fn build_snapshot(sim: &Sim, sector: &str, host: &str, now_ms: u64) -> Snapshot {
    let mut ships: Vec<ShipView> = sim
        .ships
        .iter()
        .map(|(id, s)| ShipView {
            id: id.clone(),
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
            alive: s.alive,
        })
        .collect();
    // Deterministic ordering keeps snapshots stable across hosts and tests reliable.
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
        depleted,
        kills,
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
    fn join_input_build_flow_produces_snapshot() {
        let mut sim = Sim::new();
        apply_client_msg(&mut sim, "playerA", ClientMsg::Join { name: "Ace".into() });
        apply_client_msg(
            &mut sim,
            "playerA",
            ClientMsg::Input { thrust: true, turn: 1, fire: false, aim: Some(0.5), name: None },
        );
        // Give them minerals and buy a gun through the wire path.
        sim.ships.get_mut("playerA").unwrap().minerals = 100;
        apply_client_msg(&mut sim, "playerA", ClientMsg::Build { kind: "gun".into() });
        sim.tick(1.0);

        let snap = build_snapshot(&sim, "0_0", "hostNode", 123_456);
        assert_eq!(snap.host, "hostNode");
        assert_eq!(snap.sector, "0_0");
        assert_eq!(snap.tick, 1);
        assert_eq!(snap.ships.len(), 1);
        let sv = &snap.ships[0];
        assert_eq!(sv.id, "playerA");
        assert_eq!(sv.name, "Ace");
        assert_eq!(sv.guns, 2, "the build was applied");
        // Hue is the authoritative one, not anything the client supplied.
        assert_eq!(sv.hue, hue_for("playerA"));
        assert_eq!(snap.ts, 123_456);
    }

    #[test]
    fn bye_removes_ship_and_is_signalled() {
        let mut sim = Sim::new();
        apply_client_msg(&mut sim, "p", ClientMsg::Join { name: "P".into() });
        assert_eq!(sim.player_count(), 1);
        let was_bye = apply_client_msg(&mut sim, "p", ClientMsg::Bye);
        assert!(was_bye);
        assert_eq!(sim.player_count(), 0);
    }

    #[test]
    fn snapshot_ship_order_is_deterministic() {
        let mut sim = Sim::new();
        apply_client_msg(&mut sim, "zeta", ClientMsg::Join { name: "Z".into() });
        apply_client_msg(&mut sim, "alpha", ClientMsg::Join { name: "A".into() });
        sim.tick(1.0);
        let snap = build_snapshot(&sim, "0_0", "h", 0);
        let ids: Vec<&str> = snap.ships.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
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
}
