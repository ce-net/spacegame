//! Integration tests for the distributed/scale systems: cross-sector transit (infinite map),
//! hot reload, replica agreement (anti-cheat), state-hash determinism, snapshot failover, the build /
//! procgen / one-ship-mesh pipeline, the always-alive faction economy, and client interest scoping.

use std::collections::BTreeMap;
use std::sync::Arc;

use spacegame::client::Platform;
use spacegame::faction::FactionCommand;
use spacegame::replication::{agree, ReplicaCandidate, ReplicaSet, ReplicationConstraint, StateProof};
use spacegame::room::{apply_client_msg, build_snapshot_view};
use spacegame::ruleset::{Ruleset, TechEffect};
use spacegame::shapedef::MeshCache;
use spacegame::shard::SectorId;
use spacegame::sim::{Intent, Sim, SECTOR_SIZE};
use spacegame::snapshot::SectorSnapshot;
use spacegame::wire::ClientMsg;

// ---------------------------------------------------------------------------------------------------
// Infinite map: cross-sector transit handoff.
// ---------------------------------------------------------------------------------------------------

#[test]
fn ship_transits_from_one_sector_to_the_neighbour() {
    let mut a = Sim::for_sector(SectorId::new(0, 0), Arc::new(Ruleset::builtin()));
    a.join("p", "P", 0);
    a.factions.values_mut().for_each(|f| f.units.clear()); // no NPC fleet to collide with at the edge
    {
        let s = a.ships.get_mut("p").unwrap();
        s.x = SECTOR_SIZE - 2.0;
        s.y = 1500.0;
        s.a = 0.0;
        s.vx = 6.0;
        s.minerals = 42;
    }
    for _ in 0..4 {
        a.apply_intent("p", Intent { thrust: true, aim: Some(0.0), ..Default::default() }, 0);
        a.tick(1.0);
        if a.ships.is_empty() {
            break;
        }
    }
    let transits = a.take_transits();
    assert!(!a.ships.contains_key("p"), "the ship left sector (0,0)");
    assert_eq!(transits.len(), 1);
    assert_eq!(transits[0].to, SectorId::new(1, 0), "handed east to (1,0)");

    // The neighbour admits it with its state carried over.
    let mut b = Sim::for_sector(SectorId::new(1, 0), Arc::new(Ruleset::builtin()));
    b.accept_transit(transits[0].ship.clone());
    assert!(b.ships.contains_key("p"), "the neighbour received the ship");
    assert_eq!(b.ships["p"].minerals, 42, "its loadout/economy carried across the boundary");
}

// ---------------------------------------------------------------------------------------------------
// Hot reload.
// ---------------------------------------------------------------------------------------------------

#[test]
fn hot_reload_retunes_live_and_keeps_ships() {
    let mut s = Sim::new();
    s.join("n", "P", 0);
    s.tick(1.0);
    let mut r = Ruleset::builtin();
    r.version = 2;
    r.weapons[0].damage = 99;
    s.apply_ruleset(Arc::new(r));
    assert_eq!(s.player_count(), 1, "ships survive a hot reload");
    assert_eq!(s.rules.weapon("blaster").unwrap().damage, 99, "the new stats are live");
}

#[test]
fn hot_reload_falls_back_when_a_selected_weapon_is_removed() {
    let mut s = Sim::new();
    s.join("n", "P", 0);
    s.ships.get_mut("n").unwrap().minerals = 1000;
    apply_client_msg(&mut s, "n", ClientMsg::Build { kind: "tech-missile".into() });
    apply_client_msg(&mut s, "n", ClientMsg::Weapon { id: "missile".into() });
    assert_eq!(s.ships["n"].weapon, "missile");
    let mut r = Ruleset::builtin();
    r.version = 5;
    r.weapons.retain(|w| w.id != "missile");
    r.tech.retain(|t| !matches!(&t.effect, TechEffect::UnlockWeapon { weapon } if weapon == "missile"));
    s.apply_ruleset(Arc::new(r));
    assert_eq!(s.ships["n"].weapon, "blaster", "the ship falls back to the default weapon, still armed");
}

// ---------------------------------------------------------------------------------------------------
// Anti-cheat / redundancy: state hashes + replica agreement.
// ---------------------------------------------------------------------------------------------------

#[test]
fn honest_replicas_agree_on_the_state_hash_and_a_cheat_diverges() {
    let mut a = Sim::new();
    let mut b = Sim::new();
    a.seamless = false;
    b.seamless = false;
    for r in [&mut a, &mut b] {
        r.join("x", "X", 1);
        r.join("y", "Y", 200);
    }
    for _ in 0..25 {
        for r in [&mut a, &mut b] {
            r.apply_intent("x", Intent { thrust: true, fire: true, aim: Some(0.4), ..Default::default() }, 1);
            r.tick(1.0);
        }
    }
    assert_eq!(a.state_hash(), b.state_hash(), "honest replicas agree");
    a.ships.get_mut("x").unwrap().x += 75.0; // a cheating host teleports a ship
    assert_ne!(a.state_hash(), b.state_hash(), "the tampered replica's hash diverges");
}

#[test]
fn the_quorum_outvotes_a_cheater() {
    let p = |node: &str, hash: u64| StateProof { region: "0_0".into(), node: node.into(), tick: 100, hash };
    let v = agree(&[p("a", 7), p("b", 7), p("c", 7), p("cheat", 9)]);
    assert_eq!(v.quorum_hash, Some(7));
    assert!(v.has_quorum);
    assert_eq!(v.dissent, vec!["cheat"]);
    // A 1-1 split has no quorum (one liar cannot frame one honest node).
    assert!(!agree(&[p("a", 1), p("b", 2)]).has_quorum);
}

#[test]
fn replica_set_promotes_and_re_replicates_on_failure() {
    let mut set = ReplicaSet::new("0_0", "host", ReplicationConstraint { k: 3 }, 0);
    set.admit("near", 0);
    set.observe("near", 18);
    set.expire(20, 5); // host went silent; the backup (t=18) stays healthy
    let cands = vec![
        ReplicaCandidate { node_id: "near".into(), rtt_ms: Some(20.0), in_game_dist: 100.0, free_cores: 4, alive: true },
        ReplicaCandidate { node_id: "fresh".into(), rtt_ms: Some(30.0), in_game_dist: 400.0, free_cores: 8, alive: true },
    ];
    let plan = set.plan(&cands);
    assert_eq!(plan.promote.as_deref(), Some("near"), "the surviving replica is promoted");
    assert!(plan.drop.contains(&"host".to_string()), "the dead host is retired");
    assert!(plan.copy_to.contains(&"fresh".to_string()), "the map is re-replicated to restore k");
}

// ---------------------------------------------------------------------------------------------------
// Snapshot failover determinism.
// ---------------------------------------------------------------------------------------------------

fn busy_sim() -> Sim {
    let mut s = Sim::new();
    s.seamless = false;
    apply_client_msg(&mut s, "a", ClientMsg::Join { name: "Ace".into(), cap: None });
    apply_client_msg(&mut s, "b", ClientMsg::Join { name: "Bee".into(), cap: None });
    s.ships.get_mut("a").unwrap().minerals = 500;
    apply_client_msg(&mut s, "a", ClientMsg::Build { kind: "tech-missile".into() });
    apply_client_msg(&mut s, "a", ClientMsg::Weapon { id: "missile".into() });
    for _ in 0..3 {
        apply_client_msg(&mut s, "a", ClientMsg::Input { thrust: true, turn: 0, fire: true, aim: Some(0.0), name: None });
        s.tick(1.0);
    }
    s
}

#[test]
fn a_restored_snapshot_evolves_identically_to_the_original() {
    let mut original = busy_sim();
    let snap = SectorSnapshot::capture(&original);
    let mut restored = snap.restore();
    for _ in 0..30 {
        original.tick(1.0);
        restored.tick(1.0);
    }
    assert_eq!(
        SectorSnapshot::capture(&original),
        SectorSnapshot::capture(&restored),
        "the failover host continues the same future as the one it replaced"
    );
}

// ---------------------------------------------------------------------------------------------------
// Build / procgen / one-ship-mesh pipeline.
// ---------------------------------------------------------------------------------------------------

#[test]
fn authored_and_generated_ships_resolve_and_collapse_to_one_cached_mesh() {
    let r = Ruleset::builtin();
    // Authored ship resolves to its nested parts.
    let craft = r.resolve_craft("scout", &BTreeMap::new()).unwrap();
    assert_eq!(craft.parts.len(), 13);
    assert!(craft.total_thrust > 0.0 && !craft.weapon_mounts.is_empty());

    // The whole ship collapses to ONE mesh + root AABB, and the cache reuses it.
    let mut cache = MeshCache::new();
    let m1 = r.ship_mesh_cached(&mut cache, "scout", &BTreeMap::new()).unwrap();
    let m2 = r.ship_mesh_cached(&mut cache, "scout", &BTreeMap::new()).unwrap();
    assert!(Arc::ptr_eq(&m1, &m2), "the cached ship mesh is reused");
    assert!(m1.aabb[2] > m1.aabb[0] && m1.indices.len() % 3 == 0, "one triangulated whole-ship mesh");

    // Procedural generation is deterministic and resolves to a flyable craft.
    let ship = r.generate_ship("warship", 0xBEEF).unwrap();
    assert_eq!(ship.blueprint, r.generate_ship("warship", 0xBEEF).unwrap().blueprint, "seed-deterministic");
    let gcraft = r.resolve_generated(&ship).unwrap();
    assert!(!gcraft.parts.is_empty() && gcraft.total_thrust > 0.0);
    assert!(!r.generated_ship_mesh(&ship).unwrap().vertices.is_empty(), "generated ship -> one mesh");
}

// ---------------------------------------------------------------------------------------------------
// Always-alive faction economy.
// ---------------------------------------------------------------------------------------------------

#[test]
fn a_faction_builds_in_the_background_while_simulated() {
    let mut s = Sim::new();
    s.seamless = false;
    s.join("p", "P", 0);
    // Stock the faction and let the world tick — the autonomy spends resources for the player.
    {
        let f = s.factions.get_mut("p").unwrap();
        f.resources = spacegame::faction::Resources::new(4000, 4000, 4000);
    }
    let b0 = s.factions["p"].buildings.len();
    let p0 = s.factions["p"].power();
    for _ in 0..2500 {
        s.tick(1.0);
    }
    assert!(s.factions["p"].buildings.len() > b0, "the faction expanded its base autonomously");
    assert!(s.factions["p"].power() > p0, "and grew stronger while simulated");
    // The roster became real NPC fleet ships in the world.
    assert!(s.ships.values().any(|sh| sh.owner.as_deref() == Some("p")), "fielded NPC fleet ships");
}

#[test]
fn fleet_obeys_a_command() {
    let mut s = Sim::new();
    s.join("p", "P", 0);
    apply_client_msg(&mut s, "p", ClientMsg::Command { order: "attack_nearest".into(), x: None, y: None });
    assert_eq!(s.factions["p"].command, FactionCommand::AttackNearest);
}

// ---------------------------------------------------------------------------------------------------
// Client interest scoping (the frontend-bandwidth path).
// ---------------------------------------------------------------------------------------------------

#[test]
fn a_mobile_viewport_only_receives_nearby_entities() {
    let mut s = Sim::new();
    s.seamless = false;
    apply_client_msg(&mut s, "near", ClientMsg::Join { name: "N".into(), cap: None });
    apply_client_msg(&mut s, "far", ClientMsg::Join { name: "F".into(), cap: None });
    s.ships.get_mut("near").unwrap().x = 200.0;
    s.ships.get_mut("near").unwrap().y = 200.0;
    s.ships.get_mut("far").unwrap().x = 2800.0;
    s.ships.get_mut("far").unwrap().y = 2800.0;
    s.tick(1.0);

    let profile = Platform::MobileBrowser.profile();
    let viewport = profile.viewport(200.0, 200.0);
    let snap = build_snapshot_view(&s, "0_0", "host", 0, viewport);
    let ids: Vec<&str> = snap.ships.iter().map(|sh| sh.id.as_str()).collect();
    assert!(ids.contains(&"near"), "the on-screen ship is sent");
    assert!(!ids.contains(&"far"), "the off-screen ship is culled — bounded mobile bandwidth");
}
