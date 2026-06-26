//! **Shape blueprints** — a recursive system for defining, saving and composing shapes, with
//! **materials**, an **auto-built root AABB** for collision/physics, and a **GPU-ready flattening**.
//!
//! [`crate::shape::Shape2D`] is the primitive kernel (rect/triangle/disc/…). This module is the layer
//! above it: a [`ShapeBlueprint`] is a *tree* of placed parts, where each part is either a primitive
//! or a **reference to another shape blueprint** — so a shape is recursive and can carry as much detail
//! as you like (a hull plate made of plates made of rivets). Every part can have a **material**.
//!
//! Three things fall out, exactly as required:
//!
//! 1. **Define / save / make new shapes** — shape blueprints and a [`MaterialDef`] palette are data in
//!    the hot-reloadable [`Ruleset`](crate::ruleset::Ruleset); add or edit one and it is live.
//! 2. **Auto AABB on the root** — [`resolve_shape`] flattens the recursion into world-placed primitives
//!    and [`root_aabb`] unions them, giving the one box the broad-phase collision/physics uses; the
//!    flattened primitives are the narrow-phase ([`FlatPrim::to_physics`]).
//! 3. **Easily sent to the GPU** — [`flatten_gpu`] triangulates the whole composite into a packed,
//!    `#[repr(C)]`, std140-friendly buffer ([`GpuMesh`]): a vertex array (position + uv + material
//!    index), an index array, a material palette, and the root AABB — plus [`GpuMesh::to_raw`] for a
//!    pointer-free `Vec<f32>`/`Vec<u32>` upload (no `unsafe`).
//!
//! Pure and unit-tested; recursion is guarded against cycles and runaway depth.

use serde::{Deserialize, Serialize};

use crate::shape::{NamedShape, Shape2D};

/// A 2D transform with uniform scale — places a part (and its whole subtree) in its parent's frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Xform {
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub rot: f32,
    #[serde(default = "one_f32")]
    pub scale: f32,
}

fn one_f32() -> f32 {
    1.0
}

impl Default for Xform {
    fn default() -> Self {
        Xform { x: 0.0, y: 0.0, rot: 0.0, scale: 1.0 }
    }
}

impl Xform {
    pub fn new(x: f32, y: f32, rot: f32, scale: f32) -> Self {
        Xform { x, y, rot, scale }
    }
    /// Map a local point into the parent frame (scale, then rotate, then translate).
    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        let (c, s) = (self.rot.cos(), self.rot.sin());
        let (sx, sy) = (p[0] * self.scale, p[1] * self.scale);
        [self.x + c * sx - s * sy, self.y + s * sx + c * sy]
    }
    /// `self ∘ child`: compose so `child`'s subtree lands correctly inside `self`.
    pub fn compose(&self, child: &Xform) -> Xform {
        let t = self.apply([child.x, child.y]);
        Xform { x: t[0], y: t[1], rot: self.rot + child.rot, scale: self.scale * child.scale }
    }
    /// The 2×3 affine matrix `[a, b, c, d, tx, ty]` (column layout `[[a,b],[c,d],[tx,ty]]`) for the GPU.
    pub fn affine(&self) -> [f32; 6] {
        let (co, si) = (self.rot.cos(), self.rot.sin());
        [co * self.scale, si * self.scale, -si * self.scale, co * self.scale, self.x, self.y]
    }
}

/// A material, **as authored** (friendly, hot-reloadable). Converted to the packed GPU [`Material`] at
/// flatten time. Every field defaults, so a JSON material lists only what it sets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialDef {
    pub id: String,
    /// Base colour RGBA, 0..1.
    #[serde(default = "white")]
    pub color: [f32; 4],
    /// Emissive RGB (glow), 0..1.
    #[serde(default)]
    pub emissive: [f32; 3],
    #[serde(default)]
    pub emissive_strength: f32,
    #[serde(default)]
    pub metallic: f32,
    #[serde(default = "half")]
    pub roughness: f32,
}

fn white() -> [f32; 4] {
    [1.0, 1.0, 1.0, 1.0]
}
fn half() -> f32 {
    0.5
}

impl MaterialDef {
    pub fn to_gpu(&self) -> Material {
        Material {
            color: self.color,
            emissive_metallic: [self.emissive[0], self.emissive[1], self.emissive[2], self.metallic],
            roughness_flags: [self.roughness, self.emissive_strength, 0.0, 0.0],
        }
    }
}

/// A material in the **packed GPU layout** — three 16-byte `vec4`s (std140-friendly, 48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Material {
    /// `rgba`.
    pub color: [f32; 4],
    /// `xyz` = emissive, `w` = metallic.
    pub emissive_metallic: [f32; 4],
    /// `x` = roughness, `y` = emissive_strength, `z`,`w` reserved.
    pub roughness_flags: [f32; 4],
}

/// What a shape-tree part draws: a primitive, or a reference to another shape blueprint (recursion).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum ShapeKind {
    /// A primitive leaf shape.
    Prim { shape: Shape2D },
    /// A named primitive from the [`crate::shape`] library (reuse a saved primitive by id).
    PrimRef { shape: String },
    /// Another shape blueprint, expanded in place — shapes within shapes.
    Ref { blueprint: String },
}

/// One placed part of a shape tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShapePart {
    #[serde(default)]
    pub at: Xform,
    pub kind: ShapeKind,
    /// Material id for this part (and inherited by a referenced subtree unless it overrides).
    #[serde(default)]
    pub material: Option<String>,
}

/// A named, recursive, savable shape composed of parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShapeBlueprint {
    pub id: String,
    pub name: String,
    /// Default material for parts that don't specify one.
    #[serde(default)]
    pub material: Option<String>,
    pub root: Vec<ShapePart>,
}

/// Read-only view over the shape-blueprint, primitive-shape and material libraries (all in the ruleset).
#[derive(Debug, Clone, Copy)]
pub struct ShapeLibrary<'a> {
    pub blueprints: &'a [ShapeBlueprint],
    pub prims: &'a [NamedShape],
    pub materials: &'a [MaterialDef],
}

impl<'a> ShapeLibrary<'a> {
    pub fn blueprint(&self, id: &str) -> Option<&'a ShapeBlueprint> {
        self.blueprints.iter().find(|b| b.id == id)
    }
    pub fn prim(&self, id: &str) -> Option<&'a Shape2D> {
        self.prims.iter().find(|s| s.id == id).map(|s| &s.def)
    }
    pub fn material_index(&self, id: &str) -> Option<usize> {
        self.materials.iter().position(|m| m.id == id)
    }
}

/// One flattened primitive of a resolved shape: a primitive placed by a world [`Xform`] with a material
/// index into the library's material list (`usize::MAX` = the default material).
#[derive(Debug, Clone, PartialEq)]
pub struct FlatPrim {
    pub shape: Shape2D,
    pub xform: Xform,
    pub material: usize,
}

impl FlatPrim {
    /// The primitive's collision shape (for narrow-phase). The placement transform is separate.
    pub fn to_physics(&self) -> crate::physics::Shape {
        self.shape.scaled(self.xform.scale).to_physics()
    }
    /// World-space outline of this primitive.
    pub fn world_outline(&self) -> Vec<[f32; 2]> {
        self.shape.outline().into_iter().map(|p| self.xform.apply(p)).collect()
    }
}

/// Recursion safety for shape trees.
pub const MAX_SHAPE_DEPTH: usize = 24;

/// Flatten a shape blueprint into its world-placed primitives, expanding every nested reference.
/// `material` indices point into `library.materials`. Errors on unknown ids, cycles, or excess depth.
pub fn resolve_shape(library: &ShapeLibrary, id: &str) -> Result<Vec<FlatPrim>, String> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    resolve_into(library, id, &Xform::default(), usize::MAX, 0, &mut path, &mut out)?;
    Ok(out)
}

/// Resolve a shape blueprint **value** (e.g. a whole-ship blueprint synthesized from a built craft,
/// which is not in the library) against the library it references.
pub fn resolve_shape_design(library: &ShapeLibrary, bp: &ShapeBlueprint) -> Result<Vec<FlatPrim>, String> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    resolve_value(library, bp, &Xform::default(), usize::MAX, 0, &mut path, &mut out)?;
    Ok(out)
}

fn resolve_into(
    library: &ShapeLibrary,
    id: &str,
    base: &Xform,
    inherited_material: usize,
    depth: usize,
    path: &mut Vec<String>,
    out: &mut Vec<FlatPrim>,
) -> Result<(), String> {
    let bp = library.blueprint(id).ok_or_else(|| format!("unknown shape blueprint {id}"))?;
    resolve_value(library, bp, base, inherited_material, depth, path, out)
}

fn resolve_value(
    library: &ShapeLibrary,
    bp: &ShapeBlueprint,
    base: &Xform,
    inherited_material: usize,
    depth: usize,
    path: &mut Vec<String>,
    out: &mut Vec<FlatPrim>,
) -> Result<(), String> {
    if depth > MAX_SHAPE_DEPTH {
        return Err(format!("shape nesting exceeded {MAX_SHAPE_DEPTH} at {}", bp.id));
    }
    if path.iter().any(|p| p == &bp.id) {
        return Err(format!("shape cycle: {} -> {}", path.join(" -> "), bp.id));
    }
    let bp_material = bp.material.as_deref().and_then(|m| library.material_index(m)).unwrap_or(inherited_material);

    path.push(bp.id.clone());
    for part in &bp.root {
        let world = base.compose(&part.at);
        let mat = part.material.as_deref().and_then(|m| library.material_index(m)).unwrap_or(bp_material);
        match &part.kind {
            ShapeKind::Prim { shape } => out.push(FlatPrim { shape: shape.clone(), xform: world, material: mat }),
            ShapeKind::PrimRef { shape } => {
                let s = library.prim(shape).ok_or_else(|| format!("unknown primitive shape {shape}"))?;
                out.push(FlatPrim { shape: s.clone(), xform: world, material: mat });
            }
            ShapeKind::Ref { blueprint } => {
                resolve_into(library, blueprint, &world, mat, depth + 1, path, out)?;
            }
        }
    }
    path.pop();
    Ok(())
}

/// The **auto root AABB** of resolved primitives, `[min_x, min_y, max_x, max_y]` — the one box the
/// broad-phase uses for collision and physics.
pub fn root_aabb(prims: &[FlatPrim]) -> [f32; 4] {
    let mut mn = [f32::INFINITY, f32::INFINITY];
    let mut mx = [f32::NEG_INFINITY, f32::NEG_INFINITY];
    for fp in prims {
        for v in fp.world_outline() {
            mn[0] = mn[0].min(v[0]);
            mn[1] = mn[1].min(v[1]);
            mx[0] = mx[0].max(v[0]);
            mx[1] = mx[1].max(v[1]);
        }
    }
    if prims.is_empty() {
        return [0.0, 0.0, 0.0, 0.0];
    }
    [mn[0], mn[1], mx[0], mx[1]]
}

/// A GPU vertex — `#[repr(C)]`, tightly packed (24 bytes): position, uv, material index.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub material: u32,
    /// Padding so the struct is a round 24 bytes and the next vertex is 4-byte aligned.
    pub _pad: u32,
}

/// A fully flattened, GPU-uploadable mesh for a shape: triangulated geometry, a material palette, and
/// the root AABB. The vertex/index/material arrays are contiguous `#[repr(C)]` and pointer-free.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuMesh {
    /// Root AABB `[min_x, min_y, max_x, max_y]` (collision/physics + GPU culling).
    pub aabb: [f32; 4],
    pub vertices: Vec<GpuVertex>,
    pub indices: Vec<u32>,
    /// The material palette; `GpuVertex::material` indexes into this.
    pub materials: Vec<Material>,
}

impl GpuMesh {
    /// A pointer-free raw form for direct upload (no `unsafe` byte-casting needed by the caller):
    /// interleaved vertex floats `[x, y, u, v, material_as_f32]`, the index list, the material floats
    /// (12 per material), and the AABB.
    pub fn to_raw(&self) -> RawMesh {
        let mut verts = Vec::with_capacity(self.vertices.len() * 5);
        for v in &self.vertices {
            verts.extend_from_slice(&[v.pos[0], v.pos[1], v.uv[0], v.uv[1], v.material as f32]);
        }
        let mut mats = Vec::with_capacity(self.materials.len() * 12);
        for m in &self.materials {
            mats.extend_from_slice(&m.color);
            mats.extend_from_slice(&m.emissive_metallic);
            mats.extend_from_slice(&m.roughness_flags);
        }
        RawMesh { vertices: verts, indices: self.indices.clone(), materials: mats, aabb: self.aabb }
    }
}

/// Pointer-free flat arrays, ready to memcpy into GPU buffers.
#[derive(Debug, Clone, PartialEq)]
pub struct RawMesh {
    /// 5 floats per vertex: `x, y, u, v, material_index_as_f32`.
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    /// 12 floats per material (3 × vec4).
    pub materials: Vec<f32>,
    pub aabb: [f32; 4],
}

/// Flatten a shape blueprint into a GPU mesh: resolve the recursion, triangulate each primitive
/// (convex fan) into world space, build the material palette, and compute the root AABB.
pub fn flatten_gpu(library: &ShapeLibrary, id: &str) -> Result<GpuMesh, String> {
    let prims = resolve_shape(library, id)?;
    Ok(flatten_prims(library, &prims))
}

/// Flatten a shape blueprint **value** (e.g. a whole ship) into one GPU mesh + root AABB.
pub fn flatten_gpu_design(library: &ShapeLibrary, bp: &ShapeBlueprint) -> Result<GpuMesh, String> {
    let prims = resolve_shape_design(library, bp)?;
    Ok(flatten_prims(library, &prims))
}

/// Triangulate already-resolved primitives into one GPU mesh: vertices (pos+uv+material), indices, a
/// compacted material palette, and the root AABB.
fn flatten_prims(library: &ShapeLibrary, prims: &[FlatPrim]) -> GpuMesh {
    let aabb = root_aabb(prims);

    // Material palette: a default at index 0, then every library material actually used.
    let default = MaterialDef {
        id: "__default".into(),
        color: [0.7, 0.75, 0.82, 1.0],
        emissive: [0.0, 0.0, 0.0],
        emissive_strength: 0.0,
        metallic: 0.1,
        roughness: 0.6,
    };
    let mut materials: Vec<Material> = vec![default.to_gpu()];
    // Remap library material index -> palette index (compacting only the used ones).
    let mut remap: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();

    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for fp in prims {
        let mat_idx = if fp.material == usize::MAX {
            0u32
        } else {
            *remap.entry(fp.material).or_insert_with(|| {
                let m = &library.materials[fp.material];
                materials.push(m.to_gpu());
                (materials.len() - 1) as u32
            })
        };
        // Triangulate the primitive's world outline as a fan (all shapes here are convex).
        let outline = fp.world_outline();
        if outline.len() < 3 {
            continue;
        }
        let (lmn, lmx) = fp.shape.aabb();
        let span = [(lmx[0] - lmn[0]).max(1e-3), (lmx[1] - lmn[1]).max(1e-3)];
        let base = vertices.len() as u32;
        let local = fp.shape.outline();
        for (k, w) in outline.iter().enumerate() {
            // UV from the primitive-local position, normalised to 0..1 over the shape's box.
            let u = (local[k][0] - lmn[0]) / span[0];
            let vv = (local[k][1] - lmn[1]) / span[1];
            vertices.push(GpuVertex { pos: *w, uv: [u, vv], material: mat_idx, _pad: 0 });
        }
        for k in 1..(outline.len() as u32 - 1) {
            indices.extend_from_slice(&[base, base + k, base + k + 1]);
        }
    }

    GpuMesh { aabb, vertices, indices, materials }
}

/// A cache of flattened ship meshes (and their root AABBs), keyed by a design key, that **invalidates
/// itself on a hot reload**. Flattening a whole ship every frame would be wasteful; with this the mesh
/// + AABB are computed once and reused until either the design or the ruleset changes. Cheap to clone
/// out (each entry is an `Arc<GpuMesh>`).
#[derive(Debug, Default)]
pub struct MeshCache {
    ruleset_version: u64,
    entries: std::collections::HashMap<String, std::sync::Arc<GpuMesh>>,
}

impl MeshCache {
    pub fn new() -> Self {
        MeshCache { ruleset_version: 0, entries: std::collections::HashMap::new() }
    }

    /// Number of cached meshes (for diagnostics/tests).
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get a cached mesh for `key`, or build + store it. If `ruleset_version` differs from the cached
    /// one (a hot reload happened), the whole cache is dropped first so stale geometry is never served.
    pub fn get_or_build(
        &mut self,
        ruleset_version: u64,
        key: &str,
        build: impl FnOnce() -> Result<GpuMesh, String>,
    ) -> Result<std::sync::Arc<GpuMesh>, String> {
        if ruleset_version != self.ruleset_version {
            self.entries.clear();
            self.ruleset_version = ruleset_version;
        }
        if let Some(m) = self.entries.get(key) {
            return Ok(m.clone());
        }
        let mesh = std::sync::Arc::new(build()?);
        self.entries.insert(key.to_string(), mesh.clone());
        Ok(mesh)
    }
}

/// Validate the shape libraries before they go live: every blueprint's primitives are sane, every
/// reference (to a blueprint, a named primitive, or a material) resolves, and there are no cycles.
pub fn validate_shape_library(library: &ShapeLibrary) -> Result<(), String> {
    for bp in library.blueprints {
        let mut path = Vec::new();
        check_shape(library, &bp.id, &mut path)?;
    }
    Ok(())
}

fn check_shape(library: &ShapeLibrary, id: &str, path: &mut Vec<String>) -> Result<(), String> {
    if path.iter().any(|p| p == id) {
        return Err(format!("shape cycle: {} -> {id}", path.join(" -> ")));
    }
    if path.len() > MAX_SHAPE_DEPTH {
        return Err(format!("shape too deep at {id}"));
    }
    let bp = library.blueprint(id).ok_or_else(|| format!("unknown shape blueprint {id}"))?;
    if let Some(m) = &bp.material
        && library.material_index(m).is_none()
    {
        return Err(format!("shape {id} uses unknown material {m}"));
    }
    path.push(id.to_string());
    for part in &bp.root {
        if let Some(m) = &part.material
            && library.material_index(m).is_none()
        {
            return Err(format!("shape {id} part uses unknown material {m}"));
        }
        match &part.kind {
            ShapeKind::Prim { shape } => shape.validate().map_err(|e| format!("shape {id}: {e}"))?,
            ShapeKind::PrimRef { shape } => {
                if library.prim(shape).is_none() {
                    return Err(format!("shape {id} references unknown primitive {shape}"));
                }
            }
            ShapeKind::Ref { blueprint } => check_shape(library, blueprint, path)?,
        }
    }
    path.pop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> (Vec<MaterialDef>, Vec<NamedShape>, Vec<ShapeBlueprint>) {
        let materials = vec![
            MaterialDef { id: "steel".into(), color: [0.6, 0.6, 0.7, 1.0], emissive: [0.0; 3], emissive_strength: 0.0, metallic: 0.9, roughness: 0.4 },
            MaterialDef { id: "glow".into(), color: [0.2, 0.8, 1.0, 1.0], emissive: [0.2, 0.8, 1.0], emissive_strength: 3.0, metallic: 0.0, roughness: 0.3 },
        ];
        let prims = vec![NamedShape { id: "rivet".into(), def: Shape2D::Disc { r: 0.15 } }];
        // A "plate" shape: a rectangle of steel with a glowing strip and a referenced rivet.
        let plate = ShapeBlueprint {
            id: "plate".into(),
            name: "Plate".into(),
            material: Some("steel".into()),
            root: vec![
                ShapePart { at: Xform::default(), kind: ShapeKind::Prim { shape: Shape2D::Rect { w: 4.0, h: 1.0 } }, material: None },
                ShapePart { at: Xform::new(0.0, 0.0, 0.0, 1.0), kind: ShapeKind::Prim { shape: Shape2D::Rect { w: 4.0, h: 0.2 } }, material: Some("glow".into()) },
                ShapePart { at: Xform::new(1.6, 0.0, 0.0, 1.0), kind: ShapeKind::PrimRef { shape: "rivet".into() }, material: None },
            ],
        };
        // A "panel" shape that RECURSIVELY references two plates (shapes within shapes).
        let panel = ShapeBlueprint {
            id: "panel".into(),
            name: "Panel".into(),
            material: None,
            root: vec![
                ShapePart { at: Xform::new(0.0, 1.0, 0.0, 1.0), kind: ShapeKind::Ref { blueprint: "plate".into() }, material: None },
                ShapePart { at: Xform::new(0.0, -1.0, 0.0, 1.0), kind: ShapeKind::Ref { blueprint: "plate".into() }, material: None },
            ],
        };
        (materials, prims, vec![plate, panel])
    }

    fn library<'a>(m: &'a [MaterialDef], p: &'a [NamedShape], b: &'a [ShapeBlueprint]) -> ShapeLibrary<'a> {
        ShapeLibrary { blueprints: b, prims: p, materials: m }
    }

    #[test]
    fn recursive_shape_resolves_and_validates() {
        let (m, p, b) = lib();
        let lib = library(&m, &p, &b);
        assert!(validate_shape_library(&lib).is_ok());
        let prims = resolve_shape(&lib, "panel").unwrap();
        // panel -> 2× plate, each plate = rect + strip + rivet = 3 prims -> 6 prims.
        assert_eq!(prims.len(), 6, "recursion flattened the whole tree");
    }

    #[test]
    fn root_aabb_covers_the_whole_composite() {
        let (m, p, b) = lib();
        let lib = library(&m, &p, &b);
        let prims = resolve_shape(&lib, "panel").unwrap();
        let aabb = root_aabb(&prims);
        // Plates are 4 wide; the panel stacks them at y=±1 -> spans ~ x:[-2,2], y:[-1.5,1.5].
        assert!(aabb[0] <= -1.9 && aabb[2] >= 1.9, "AABB spans the width: {aabb:?}");
        assert!(aabb[1] <= -1.4 && aabb[3] >= 1.4, "AABB spans the stacked height: {aabb:?}");
    }

    #[test]
    fn materials_are_inherited_and_overridden() {
        let (m, p, b) = lib();
        let lib = library(&m, &p, &b);
        let prims = resolve_shape(&lib, "plate").unwrap();
        let steel = lib.material_index("steel").unwrap();
        let glow = lib.material_index("glow").unwrap();
        // The rect + rivet inherit steel (the blueprint default); the strip overrides to glow.
        assert_eq!(prims[0].material, steel, "rect inherits the default material");
        assert_eq!(prims[1].material, glow, "strip overrides to glow");
        assert_eq!(prims[2].material, steel, "rivet inherits the default material");
    }

    #[test]
    fn gpu_flatten_produces_triangulated_buffers_with_aabb_and_materials() {
        let (m, p, b) = lib();
        let lib = library(&m, &p, &b);
        let mesh = flatten_gpu(&lib, "panel").unwrap();
        assert!(!mesh.vertices.is_empty() && !mesh.indices.is_empty(), "triangulated geometry");
        assert!(mesh.indices.len() % 3 == 0, "indices are whole triangles");
        // Default (index 0) + the two used materials (steel, glow) present.
        assert!(mesh.materials.len() >= 3, "material palette built");
        // Every index is in range.
        assert!(mesh.indices.iter().all(|&i| (i as usize) < mesh.vertices.len()));
        // Every vertex material indexes a real palette slot.
        assert!(mesh.vertices.iter().all(|v| (v.material as usize) < mesh.materials.len()));
        // AABB matches the resolved one.
        let prims = resolve_shape(&lib, "panel").unwrap();
        assert_eq!(mesh.aabb, root_aabb(&prims));
    }

    #[test]
    fn raw_form_is_flat_and_sized_right() {
        let (m, p, b) = lib();
        let lib = library(&m, &p, &b);
        let mesh = flatten_gpu(&lib, "plate").unwrap();
        let raw = mesh.to_raw();
        assert_eq!(raw.vertices.len(), mesh.vertices.len() * 5, "5 floats per vertex");
        assert_eq!(raw.materials.len(), mesh.materials.len() * 12, "12 floats per material");
        assert_eq!(raw.aabb, mesh.aabb);
    }

    #[test]
    fn cycles_and_unknown_refs_are_caught() {
        let materials = vec![];
        let prims = vec![];
        // a -> b -> a
        let a = ShapeBlueprint { id: "a".into(), name: "A".into(), material: None, root: vec![ShapePart { at: Xform::default(), kind: ShapeKind::Ref { blueprint: "b".into() }, material: None }] };
        let b = ShapeBlueprint { id: "b".into(), name: "B".into(), material: None, root: vec![ShapePart { at: Xform::default(), kind: ShapeKind::Ref { blueprint: "a".into() }, material: None }] };
        let bps = vec![a, b];
        let lib = ShapeLibrary { blueprints: &bps, prims: &prims, materials: &materials };
        assert!(validate_shape_library(&lib).is_err(), "cycle caught at validation");
        assert!(resolve_shape(&lib, "a").is_err(), "and at resolution");
    }

    #[test]
    fn xform_compose_and_affine() {
        let parent = Xform::new(10.0, 0.0, std::f32::consts::FRAC_PI_2, 2.0);
        let child = Xform::new(1.0, 0.0, 0.0, 1.0);
        let w = parent.compose(&child);
        // child local +x, parent rotated 90° and scaled 2 -> moves +y by 2.
        assert!((w.x - 10.0).abs() < 1e-4 && (w.y - 2.0).abs() < 1e-4, "composed: {w:?}");
        assert!((w.scale - 2.0).abs() < 1e-5);
        let a = parent.affine();
        assert_eq!(a.len(), 6);
    }
}
