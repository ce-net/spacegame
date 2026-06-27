//! Integration tests for weapons, combat and destruction, driven through the public sim API.

use spacegame::faction::{FactionCommand, Unit, UnitKind};
use spacegame::sim::{Intent, ShipRole, Sim, SHIP_R};

/// A closed-arena sim (ships bounce off edges) for deterministic combat setups.
fn arena() -> Sim {
    let mut s = Sim::new();
    s.seamless = false;
    s
}

fn give_weapon(s: &mut Sim, id: &str, weapon: &str) {
    let sh = s.ships.get_mut(id).unwrap();
    sh.weapons.push(weapon.to_string());
    sh.weapon = weapon.to_string();
}

/// Clear every faction's roster + disable its autonomy, so no stray NPC ships appear to perturb a
/// precise 1-on-1 combat assertion.
fn no_npcs(s: &mut Sim) {
    for f in s.factions.values_mut() {
        f.units.clear();
        f.policy.enabled = false;
    }
}

#[test]
fn blaster_kills_and_scatters_debris() {
    let mut s = arena();
    s.join("k", "K", 10);
    s.join("v", "V", 200);
    no_npcs(&mut s);
    {
        let k = s.ships.get_mut("k").unwrap();
        k.x = 1000.0;
        k.y = 1000.0;
        k.a = 0.0;
        k.vx = 0.0;
        k.vy = 0.0;
    }
    {
        let v = s.ships.get_mut("v").unwrap();
        v.x = 1000.0 + SHIP_R + 8.0;
        v.y = 1000.0;
        v.hp = 5;
    }
    let mut killed = false;
    for _ in 0..50 {
        {
            let k = s.ships.get_mut("k").unwrap();
            k.x = 1000.0;
            k.y = 1000.0;
            k.a = 0.0;
        }
        if let Some(v) = s.ships.get_mut("v") {
            v.x = 1000.0 + SHIP_R + 8.0;
            v.y = 1000.0;
        }
        s.apply_intent("k", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
        s.tick(1.0);
        if !s.kill_feed.is_empty() {
            killed = true;
            break;
        }
    }
    assert!(killed, "sustained blaster fire kills the weak target");
    assert_eq!(s.ships["k"].kills, 1, "the killer is credited");
    assert!(!s.debris.bodies.is_empty(), "destruction scattered rigid-body debris");
}

#[test]
fn railgun_is_instant_hitscan_with_a_beam() {
    let mut s = arena();
    s.join("g", "G", 10);
    s.join("t", "T", 200);
    no_npcs(&mut s);
    give_weapon(&mut s, "g", "railgun");
    {
        let g = s.ships.get_mut("g").unwrap();
        g.x = 500.0;
        g.y = 500.0;
        g.a = 0.0;
    }
    {
        let t = s.ships.get_mut("t").unwrap();
        t.x = 900.0;
        t.y = 500.0;
        t.hp = 5;
    }
    s.apply_intent("g", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
    s.tick(1.0);
    assert!(s.bullets.is_empty(), "railgun spawns no projectile");
    assert_eq!(s.beams.len(), 1, "it emits one beam");
    assert!(!s.kill_feed.is_empty(), "and one-shots the weak target");
}

#[test]
fn laser_chips_hp_over_time() {
    let mut s = arena();
    s.join("g", "G", 10);
    s.join("t", "T", 200);
    no_npcs(&mut s);
    give_weapon(&mut s, "g", "laser");
    {
        let t = s.ships.get_mut("t").unwrap();
        t.x = 650.0;
        t.y = 500.0;
        t.hp = 300;
        t.max_hp = 300;
    }
    let start = s.ships["t"].hp;
    for _ in 0..15 {
        {
            let g = s.ships.get_mut("g").unwrap();
            g.x = 500.0;
            g.y = 500.0;
            g.a = 0.0;
        }
        if let Some(t) = s.ships.get_mut("t") {
            t.x = 650.0;
            t.y = 500.0;
        }
        s.apply_intent("g", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
        s.tick(1.0);
    }
    assert!(s.ships["t"].hp < start, "the laser ground the target down over ticks");
}

#[test]
fn homing_missile_flies_in_and_explodes_with_area_damage() {
    let mut s = arena();
    s.join("g", "G", 10);
    s.join("e1", "E1", 100);
    s.join("e2", "E2", 110);
    no_npcs(&mut s);
    give_weapon(&mut s, "g", "missile");
    {
        let g = s.ships.get_mut("g").unwrap();
        g.x = 500.0;
        g.y = 500.0;
        g.a = 0.0;
        g.vx = 0.0;
        g.vy = 0.0;
    }
    for (id, x, y) in [("e1", 900.0, 500.0), ("e2", 930.0, 522.0)] {
        let e = s.ships.get_mut(id).unwrap();
        e.x = x;
        e.y = y;
        e.hp = 400;
        e.max_hp = 400;
    }
    s.apply_intent("g", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
    s.tick(1.0);
    assert!(s.bullets.iter().any(|b| b.explode_radius > 0.0), "a missile is in flight");

    let mut exploded = false;
    for _ in 0..60 {
        for (id, x, y) in [("e1", 900.0, 500.0), ("e2", 930.0, 522.0)] {
            if let Some(e) = s.ships.get_mut(id) {
                e.x = x;
                e.y = y;
            }
        }
        s.tick(1.0);
        if !s.explosions.is_empty() {
            exploded = true;
            break;
        }
    }
    assert!(exploded, "the missile detonated");
    // Damage may land on shields before hull, so count both: "hurt" = total of shield + hull dropped.
    let hurt = |e: &spacegame::sim::Ship| !e.alive || (e.hp + e.shield) < (400 + e.max_shield);
    let e1_hurt = s.ships.get("e1").map(hurt).unwrap_or(true);
    let e2_hurt = s.ships.get("e2").map(hurt).unwrap_or(true);
    assert!(e1_hurt && e2_hurt, "the blast damaged BOTH clustered enemies (area of effect)");
    assert!(s.ships["g"].hp == s.ships["g"].max_hp, "the firer is unharmed by its own blast");
}

#[test]
fn destroying_a_faction_npc_strikes_its_roster() {
    // K (its own faction) destroys an NPC fighter belonging to B's faction; the fighter is removed
    // from the world AND from B's roster (NPCs don't respawn), and debris is left behind.
    let mut s = arena();
    s.join("k", "K", 10);
    s.join("b", "B", 200);
    // Clear founding drones so the only NPC in play is the fighter we add (no friendly-fire noise),
    // and disable autonomy so neither faction builds more.
    s.factions.get_mut("k").unwrap().units.clear();
    s.factions.get_mut("k").unwrap().policy.enabled = false;
    {
        let f = s.factions.get_mut("b").unwrap();
        f.units.clear();
        f.policy.enabled = false;
        f.units.push(Unit { kind: UnitKind::Fighter, hp: 90 });
    }
    s.command_faction("b", FactionCommand::Hold); // keep the fighter still
    s.tick(1.0); // reconcile spawns the NPC fighter
    let fid = s
        .ships
        .iter()
        .find(|(_, sh)| sh.role == ShipRole::Fighter && sh.owner.as_deref() == Some("b"))
        .map(|(id, _)| id.clone())
        .expect("the faction fielded a fighter");
    let before = s.factions["b"].units.iter().filter(|u| u.kind == UnitKind::Fighter).count();
    // Wound the fighter once, then keep K firing point-blank at it.
    if let Some(n) = s.ships.get_mut(&fid) {
        n.hp = 6;
    }
    let mut gone = false;
    for _ in 0..60 {
        {
            let k = s.ships.get_mut("k").unwrap();
            k.x = 1000.0;
            k.y = 1000.0;
            k.a = 0.0;
            k.vx = 0.0;
            k.vy = 0.0;
        }
        if let Some(n) = s.ships.get_mut(&fid) {
            n.x = 1000.0 + SHIP_R + 8.0;
            n.y = 1000.0;
            n.vx = 0.0;
            n.vy = 0.0;
        }
        s.apply_intent("k", Intent { fire: true, aim: Some(0.0), ..Default::default() }, 10);
        s.tick(1.0);
        if !s.ships.contains_key(&fid) {
            gone = true;
            break;
        }
    }
    assert!(gone, "the enemy fighter was destroyed");
    let after = s.factions["b"].units.iter().filter(|u| u.kind == UnitKind::Fighter).count();
    assert!(after < before, "the loss was struck from the faction roster ({before} -> {after})");
    assert!(!s.debris.bodies.is_empty(), "destruction scattered debris");
}

#[test]
fn a_ship_never_hits_itself() {
    let mut s = arena();
    s.join("n", "P", 0);
    no_npcs(&mut s);
    s.ships.get_mut("n").unwrap().hp = 5;
    for _ in 0..40 {
        s.apply_intent("n", Intent { fire: true, ..Default::default() }, 0);
        s.tick(1.0);
    }
    assert!(s.ships["n"].alive, "own fire never harms the firer");
}
