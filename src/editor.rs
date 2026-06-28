//! The **ship editor** — compose parts on a grid into a flyable design, see its stats live, and
//! hot-reload it.
//!
//! This is the authoring half of the build system. [`crate::build`] is the kernel (parts + recursive
//! blueprints → a `ResolvedCraft`) and [`crate::shipyard`] turns a craft into the gameplay
//! [`Loadout`](crate::shipyard::Loadout) a ship flies with. A player does not hand-write a `Blueprint`;
//! they place parts in an editor. [`ShipEditor`] is that editor's model:
//!
//! - a flat grid of [`PlacedPart`]s (a part id at an integer cell, with a quarter-turn rotation);
//! - [`ShipEditor::to_blueprint`] lowers the grid into a [`Blueprint`](crate::build::Blueprint) — one
//!   placement per part at `cell * grid`, so the design flows straight through the existing
//!   `resolve_design → loadout_from_craft` pipeline the authoritative fit uses;
//! - [`ShipEditor::preview`] resolves that design against a parts catalogue and returns the live
//!   [`Loadout`] (mass, top speed, agility, hull, guns, cargo, and any problems) so the UI can show the
//!   ship's stats updating **as you build**;
//! - the whole thing is plain serde JSON ([`ShipEditor::to_json`] / [`ShipEditor::from_json`]), so a
//!   design is a file you can save, share, and **hot-reload**: a client watches the file and re-fits the
//!   ship the instant it changes (see the native client).
//!
//! It is pure, deterministic and wasm-clean — no UI, no I/O — so the same model drives a browser canvas,
//! a native window, or a hand-edited JSON file, and is fully unit-tested.

use serde::{Deserialize, Serialize};

use crate::build::{Blueprint, Catalog, Placement, Transform2D};
use crate::shipyard::{loadout_from_craft, Loadout};

/// World units per editor grid cell. A part placed at cell `(gx, gy)` sits at `(gx*GRID, gy*GRID)` in
/// the craft frame. Chosen to match the ~2-unit footprint of the builtin structure blocks.
pub const GRID: f32 = 2.0;

/// A hard cap on parts in one design — bounds the work the authoritative fit does when a client sends a
/// design over the wire, and keeps a ship a ship (not a battleship).
pub const MAX_PARTS: usize = 256;

/// One part placed in the editor: a catalogue object id at an integer grid cell, rotated by `rot`
/// quarter-turns (0..=3). Flat by design — the editor is a 2-D layout, and the loadout sums part stats
/// regardless of nesting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedPart {
    /// Object id into the ruleset parts catalogue (e.g. `"struct-block"`, `"thruster"`, `"gun"`).
    pub def: String,
    pub gx: i32,
    pub gy: i32,
    /// Rotation in quarter-turns, `0..=3` (wrapped on set).
    #[serde(default)]
    pub rot: u8,
}

impl PlacedPart {
    pub fn new(def: &str, gx: i32, gy: i32) -> Self {
        PlacedPart { def: def.to_string(), gx, gy, rot: 0 }
    }
    /// Rotation in radians for the blueprint transform.
    pub fn angle(&self) -> f32 {
        (self.rot & 3) as f32 * std::f32::consts::FRAC_PI_2
    }
}

/// An in-progress ship design: a named, grid-placed set of parts. Serializes to JSON for save / load /
/// hot-reload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShipEditor {
    pub name: String,
    /// World units per cell. Defaults to [`GRID`]; kept in the file so a design is self-describing.
    #[serde(default = "default_grid")]
    pub grid: f32,
    pub parts: Vec<PlacedPart>,
}

fn default_grid() -> f32 {
    GRID
}

impl Default for ShipEditor {
    fn default() -> Self {
        ShipEditor { name: "Untitled".into(), grid: GRID, parts: Vec::new() }
    }
}

impl ShipEditor {
    pub fn new(name: &str) -> Self {
        ShipEditor { name: name.to_string(), grid: GRID, parts: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// Place a part at cell `(gx, gy)`. Multiple parts may share a cell (e.g. a command centre sitting on
    /// a hull block). Capped at [`MAX_PARTS`]; the call is a no-op once full.
    pub fn place(&mut self, def: &str, gx: i32, gy: i32, rot: u8) {
        if self.parts.len() >= MAX_PARTS {
            return;
        }
        self.parts.push(PlacedPart { def: def.to_string(), gx, gy, rot: rot & 3 });
    }

    /// Remove the most-recently-placed part at cell `(gx, gy)`. Returns whether something was removed.
    pub fn remove_at(&mut self, gx: i32, gy: i32) -> bool {
        if let Some(i) = self.parts.iter().rposition(|p| p.gx == gx && p.gy == gy) {
            self.parts.remove(i);
            true
        } else {
            false
        }
    }

    /// Rotate the topmost part at cell `(gx, gy)` a quarter-turn clockwise. Returns whether one was found.
    pub fn rotate_at(&mut self, gx: i32, gy: i32) -> bool {
        if let Some(p) = self.parts.iter_mut().rev().find(|p| p.gx == gx && p.gy == gy) {
            p.rot = (p.rot + 1) & 3;
            true
        } else {
            false
        }
    }

    /// The parts occupying a cell, bottom-to-top.
    pub fn parts_at(&self, gx: i32, gy: i32) -> impl Iterator<Item = &PlacedPart> {
        self.parts.iter().filter(move |p| p.gx == gx && p.gy == gy)
    }

    /// Lower the grid into a [`Blueprint`]: one top-level placement per part at `cell * grid`, rotated by
    /// the part's quarter-turns. This is exactly what both [`Self::preview`] and the authoritative
    /// [`crate::sim::Sim::fit_design`] resolve, so the previewed stats are the stats you fly.
    pub fn to_blueprint(&self) -> Blueprint {
        let g = if self.grid.is_finite() && self.grid > 0.0 { self.grid } else { GRID };
        let root = self
            .parts
            .iter()
            .map(|p| {
                Placement::object(&p.def, Transform2D::new(p.gx as f32 * g, p.gy as f32 * g, p.angle()))
            })
            .collect();
        Blueprint { id: self.slug(), name: self.name.clone(), params: vec![], root }
    }

    /// Resolve this design against a parts catalogue and return its live [`Loadout`] — the stats the UI
    /// shows while building, identical to what fitting it yields. `Err` only if a part id is unknown.
    pub fn preview(&self, catalog: &Catalog) -> Result<Loadout, String> {
        let bp = self.to_blueprint();
        let craft = crate::build::resolve_design(catalog, &bp, &std::collections::BTreeMap::new())?;
        Ok(loadout_from_craft(&craft))
    }

    /// A filesystem/blueprint-safe slug of the design name.
    pub fn slug(&self) -> String {
        let mut s: String = self
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect();
        while s.contains("--") {
            s = s.replace("--", "-");
        }
        let s = s.trim_matches('-').to_string();
        if s.is_empty() { "design".to_string() } else { s }
    }

    /// Serialize to pretty JSON (what a client writes to the hot-reloadable design file).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Parse from JSON (what a client reads when the design file changes).
    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| e.to_string())
    }

    /// A complete, flyable starter design (a small interceptor) for seeding the editor / the example
    /// hot-reload file: a hull spine with the command centre + reactor inside, twin engines, and a gun.
    pub fn starter() -> Self {
        let mut e = ShipEditor::new("My Ship");
        e.place("struct-block", 0, 0, 0);
        e.place("command-center", 0, 0, 0); // sits on the hull cell
        e.place("reactor", 0, 1, 0);
        e.place("struct-block", 0, 1, 0);
        e.place("thruster", -1, -1, 0);
        e.place("thruster", 1, -1, 0);
        e.place("gun", 0, 2, 0);
        e.place("armor-wedge", 0, 3, 0);
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ruleset::Ruleset;

    fn cat(rules: &Ruleset) -> Catalog<'_> {
        rules.catalog()
    }

    #[test]
    fn placing_parts_lowers_to_a_blueprint_with_world_transforms() {
        let mut e = ShipEditor::new("Test");
        e.place("struct-block", 0, 0, 0);
        e.place("thruster", 0, -1, 2); // half-turn
        let bp = e.to_blueprint();
        assert_eq!(bp.root.len(), 2);
        // Cell (0,-1) lowers to y = -1 * GRID, with a half-turn rotation.
        let thr = &bp.root[1];
        assert!((thr.at.y + GRID).abs() < 1e-4, "y placed at cell*grid");
        assert!((thr.at.rot - std::f32::consts::PI).abs() < 1e-4, "rot is two quarter-turns");
    }

    #[test]
    fn preview_reports_a_flyable_starter_and_its_stats() {
        let rules = Ruleset::builtin();
        let e = ShipEditor::starter();
        let lo = e.preview(&cat(&rules)).expect("parts resolve");
        assert!(lo.is_flyable(), "the starter design flies: {:?}", lo.issues);
        assert!(lo.max_hp > 0 && lo.mass > 0.0);
        assert!(lo.speed_mult > 0.0 && lo.thrust_mult > 0.0);
        assert_eq!(lo.weapons, vec!["blaster".to_string()], "its gun mounts the blaster");
    }

    #[test]
    fn a_design_with_no_engine_previews_as_unflyable() {
        let rules = Ruleset::builtin();
        let mut e = ShipEditor::new("Brick");
        e.place("struct-block", 0, 0, 0);
        e.place("command-center", 0, 0, 0);
        let lo = e.preview(&cat(&rules)).unwrap();
        assert!(!lo.is_flyable(), "no thruster -> unflyable");
    }

    #[test]
    fn remove_and_rotate_target_the_top_part_of_a_cell() {
        let mut e = ShipEditor::new("T");
        e.place("struct-block", 1, 1, 0);
        e.place("gun", 1, 1, 0);
        assert!(e.rotate_at(1, 1), "rotates the gun (topmost)");
        assert_eq!(e.parts.last().unwrap().rot, 1);
        assert!(e.remove_at(1, 1), "removes the gun");
        assert_eq!(e.len(), 1);
        assert_eq!(e.parts[0].def, "struct-block");
        assert!(!e.remove_at(5, 5), "nothing at an empty cell");
    }

    #[test]
    fn json_round_trips() {
        let e = ShipEditor::starter();
        let json = e.to_json();
        let back = ShipEditor::from_json(&json).expect("parse");
        assert_eq!(e, back);
    }

    #[test]
    fn parts_are_capped() {
        let mut e = ShipEditor::new("Big");
        for i in 0..(MAX_PARTS as i32 + 50) {
            e.place("struct-block", i, 0, 0);
        }
        assert_eq!(e.len(), MAX_PARTS, "placement is capped at MAX_PARTS");
    }

    #[test]
    fn slug_is_safe() {
        assert_eq!(ShipEditor::new("My Cool Ship!!").slug(), "my-cool-ship");
        assert_eq!(ShipEditor::new("").slug(), "design");
    }
}
