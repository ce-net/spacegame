//! **The shader suite** — the actual WGSL the browser/native `wgpu` renderer compiles, authored as
//! hot-reloadable strings that ride the ruleset's [`Assets`](crate::ruleset::Assets) blob across the
//! mesh. Change a string here (or push a new ruleset with edited shaders), the version bumps, and a
//! million clients recompile the new pipeline mid-match without a reload — the whole point of
//! [`crate::ruleset`]'s hot-reload story, now with real shaders behind it.
//!
//! The backend never reads these — they are opaque to the sim. This module exists so the *default*
//! ruleset ships a complete, good-looking renderer out of the box instead of an empty `Assets {}`.
//! The frontend pulls [`builtin_assets`] (or a hot-pushed override) and builds one pipeline per named
//! shader, feeding them the packed [`crate::shapedef::GpuMesh`] vertex/material buffers and the
//! per-instance data the [`crate::render`] feed produces.
//!
//! The look, in one paragraph: ships are drawn with a 2D pseudo-PBR pass (a single key light, a
//! fresnel rim that rings the silhouette, a metallic sheen streak, faint plate banding from the UVs,
//! and additive emissive for engine glow and trim). Everything bright is captured by a threshold pass
//! and Gaussian-blurred into **bloom** so emissive trim, beams, thruster plumes and explosions all
//! *glow*. A parallax **starfield** and an fbm **nebula** fill the void behind. **Beams** (railgun,
//! laser, arc), **shields** (a fresnel bubble that ripples where it is hit), **particles** (soft
//! additive sprites) and a final **post** pass (filmic tonemap, vignette, chromatic aberration, a
//! whisper of scanlines) complete it. Every pass is small, self-contained, and individually
//! hot-swappable.
//!
//! Shared conventions across the suite:
//! - Clip space is wgpu's (`y` up, `z` in `0..1`); the vertex passes take a `view` 2×3 affine
//!   (the camera's `world -> clip` transform, e.g. `spacegame-render`'s camera) and a per-instance
//!   `model` affine ([`crate::shapedef::Xform::affine`]) so the CPU never multiplies a matrix per
//!   vertex.
//! - Group 0 = per-frame globals (time, viewport, camera), group 1 = per-pass resources (the material
//!   palette SSBO, source textures), pushed/bound by the renderer; the bindings are documented in each
//!   shader's header comment so the `wgpu` side is mechanical.
//! - Colour is authored linear and tonemapped once at the very end (`post`), so emissive can exceed 1.

use crate::ruleset::Assets;

/// Globals every pass shares (group 0, binding 0) — kept in one struct so adding a global is a
/// one-line change here mirrored on the GPU side. `viewport` is in physical pixels; `camera` is
/// `(x, y, zoom, rotation)`; `time` seconds; `tick` the authoritative sim tick (for deterministic
/// animation that lines up with replicated state).
pub const GLOBALS_WGSL: &str = r#"
// ---- shared globals (group 0) -------------------------------------------------
struct Globals {
    view      : mat3x3<f32>,  // world -> clip (from Camera2D::view_affine, padded to mat3)
    camera    : vec4<f32>,    // x, y, zoom, rotation
    viewport  : vec2<f32>,    // physical pixels
    time      : f32,          // seconds since load
    tick      : f32,          // authoritative sim tick (for replicated animation)
    fx        : vec4<f32>,    // x=bloom, y=chromatic, z=shake_amp, w=grade_warmth
};
@group(0) @binding(0) var<uniform> G : Globals;

const PI : f32 = 3.14159265359;
const TAU: f32 = 6.28318530718;

// 2x3 affine (packed [a,b,c,d,tx,ty]) applied to a 2D point.
fn affine(m: array<f32,6>, p: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(m[0]*p.x + m[2]*p.y + m[4], m[1]*p.x + m[3]*p.y + m[5]);
}
fn hash21(p: vec2<f32>) -> f32 {
    var h = dot(p, vec2<f32>(127.1, 311.7));
    return fract(sin(h) * 43758.5453123);
}
fn hash22(p: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(hash21(p), hash21(p + vec2<f32>(19.19, 7.31)));
}
fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p); let f = fract(p);
    let u = f*f*(3.0 - 2.0*f);
    let a = hash21(i + vec2<f32>(0.0,0.0));
    let b = hash21(i + vec2<f32>(1.0,0.0));
    let c = hash21(i + vec2<f32>(0.0,1.0));
    let d = hash21(i + vec2<f32>(1.0,1.0));
    return mix(mix(a,b,u.x), mix(c,d,u.x), u.y);
}
fn fbm(p0: vec2<f32>) -> f32 {
    var p = p0; var s = 0.0; var a = 0.5;
    for (var i = 0; i < 5; i = i + 1) {
        s = s + a * vnoise(p);
        p = p * 2.02 + vec2<f32>(13.1, 7.7);
        a = a * 0.5;
    }
    return s;
}
"#;

/// The **ship** pass — pseudo-PBR for the flattened ship/shape meshes. One key light, a fresnel rim
/// in the material's emissive tint, a metallic sheen streak that slides with the camera, faint plate
/// banding read from UVs, and additive emissive. Reads the packed [`crate::shapedef::Material`]
/// palette (12 floats/material) as an SSBO indexed by the per-vertex material id.
pub const SHIP_WGSL: &str = r#"
// ---- ship / shape pass --------------------------------------------------------
// vertex in : pos(vec2) uv(vec2) material(u32)   [GpuVertex, 5 floats]
// instance  : model affine(6) tint(vec4) glow(f32) hit(f32)
// material  : color(vec4) emissive_metallic(vec4) roughness_flags(vec4)
struct Material { color: vec4<f32>, em: vec4<f32>, rf: vec4<f32>, };
@group(1) @binding(0) var<storage, read> MATS : array<Material>;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv     : vec2<f32>,
    @location(1) world  : vec2<f32>,
    @location(2) tint   : vec4<f32>,
    @location(3) @interpolate(flat) mat : u32,
    @location(4) glow   : f32,
    @location(5) hit    : f32,
};

@vertex
fn vs_ship(
    @location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) mat: u32,
    @location(3) m0: vec4<f32>, @location(4) m1: vec2<f32>,   // model affine [a,b,c,d,tx,ty]
    @location(5) tint: vec4<f32>, @location(6) gh: vec2<f32>, // glow, hit-flash
) -> VOut {
    let model = array<f32,6>(m0.x, m0.y, m0.z, m0.w, m1.x, m1.y);
    let world = affine(model, pos);
    let clip3 = G.view * vec3<f32>(world, 1.0);
    var o: VOut;
    o.clip  = vec4<f32>(clip3.xy, 0.0, 1.0);
    o.uv = uv; o.world = world; o.tint = tint; o.mat = mat; o.glow = gh.x; o.hit = gh.y;
    return o;
}

@fragment
fn fs_ship(i: VOut) -> @location(0) vec4<f32> {
    let M = MATS[i.mat];
    var base = M.color.rgb * i.tint.rgb;
    let metallic  = M.em.w;
    let rough     = clamp(M.rf.x, 0.04, 1.0);
    let em_str    = M.rf.y;

    // Single key light from the upper-left; a soft 2D "normal" from the UV gradient fakes bevel.
    let n = normalize(vec2<f32>(i.uv.x - 0.5, i.uv.y - 0.5) + vec2<f32>(0.0001));
    let l = normalize(vec2<f32>(-0.6, 0.8));
    let diff = clamp(dot(n, l) * 0.5 + 0.6, 0.0, 1.4);
    var col = base * diff;

    // Plate banding: faint darker seams from the UVs, denser on rough hull, none on glass.
    let band = smoothstep(0.46, 0.5, abs(fract(i.uv.x*6.0) - 0.5)) * rough * 0.12;
    col = col * (1.0 - band);

    // Metallic sheen — a moving specular streak so hulls catch the light as they turn.
    let sweep = sin((i.uv.x + i.uv.y)*3.0 + G.time*0.6 + i.world.x*0.002);
    let spec  = pow(clamp(sweep, 0.0, 1.0), mix(8.0, 64.0, 1.0 - rough)) * metallic;
    col = col + spec * mix(vec3<f32>(1.0), base, 0.4);

    // Fresnel rim in the emissive tint — rings the silhouette, sells the "ship in the dark".
    let rim = pow(1.0 - clamp(length(i.uv - vec2<f32>(0.5)) * 2.0, 0.0, 1.0), 2.0);
    let emissive = M.em.rgb * em_str + rim * M.em.rgb * 0.6;
    col = col + emissive + emissive * i.glow;

    // Damage flash: punch the whole part toward white when freshly hit.
    col = mix(col, vec3<f32>(1.4, 1.2, 1.0), clamp(i.hit, 0.0, 0.85));

    return vec4<f32>(col, M.color.a * i.tint.a);
}
"#;

/// The **starfield** — a full-screen parallax star pass. Three depth layers of stars hashed from a
/// scrolled grid (so they are stable as the camera pans), each twinkling on its own phase, plus a few
/// coloured giants. Cheap: no texture, all procedural.
pub const STARFIELD_WGSL: &str = r#"
// ---- starfield (fullscreen) ---------------------------------------------------
struct FOut { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex
fn vs_fs(@builtin(vertex_index) vi: u32) -> FOut {
    // A single oversized triangle covering the screen.
    var p = array<vec2<f32>,3>(vec2(-1.0,-1.0), vec2(3.0,-1.0), vec2(-1.0,3.0));
    var o: FOut; o.clip = vec4<f32>(p[vi], 0.0, 1.0);
    o.uv = (p[vi]*0.5 + 0.5); return o;
}

fn star_layer(uv: vec2<f32>, density: f32, depth: f32, twinkle: f32) -> vec3<f32> {
    // Parallax: deeper layers scroll less with the camera.
    let cam = G.camera.xy * depth * 0.0006;
    let g = uv * density + cam;
    let cell = floor(g);
    let r = hash22(cell);
    let pos = fract(g) - r;
    let d = length(pos);
    let bright = smoothstep(0.06, 0.0, d) * (0.4 + 0.6*hash21(cell+3.0));
    let tw = 0.6 + 0.4*sin(G.time*twinkle + hash21(cell)*TAU);
    // Faint blue-white, occasional warm giant.
    let warm = step(0.985, hash21(cell+9.0));
    let tint = mix(vec3<f32>(0.8,0.9,1.0), vec3<f32>(1.0,0.7,0.5), warm);
    return tint * bright * tw;
}

@fragment
fn fs_starfield(i: FOut) -> @location(0) vec4<f32> {
    let uv = i.uv * vec2<f32>(G.viewport.x / G.viewport.y, 1.0);
    var c = vec3<f32>(0.0);
    c = c + star_layer(uv, 60.0,  1.0, 3.0) * 1.0;
    c = c + star_layer(uv, 120.0, 2.2, 5.0) * 0.6;
    c = c + star_layer(uv, 240.0, 4.0, 8.0) * 0.35;
    // A deep base gradient so the void is not pure black.
    let base = mix(vec3<f32>(0.01,0.012,0.03), vec3<f32>(0.02,0.01,0.04), i.uv.y);
    return vec4<f32>(c + base, 1.0);
}
"#;

/// The **nebula** — drifting fbm clouds tinted per sector. Drawn over the starfield, additively, so
/// stars shine through. The sector seed (passed in `fx`/a uniform by the renderer) shifts the hue so
/// neighbouring sectors feel like different regions of space.
pub const NEBULA_WGSL: &str = r#"
// ---- nebula (fullscreen, additive over starfield) -----------------------------
@fragment
fn fs_nebula(i: FOut) -> @location(0) vec4<f32> {
    let aspect = G.viewport.x / G.viewport.y;
    var p = (i.uv - 0.5) * vec2<f32>(aspect, 1.0) * 3.0;
    p = p + G.camera.xy * 0.00015;             // slow parallax drift
    let t = G.time * 0.02;
    // Domain-warped fbm for billowing cloud structure.
    let warp = vec2<f32>(fbm(p + vec2<f32>(t, 0.0)), fbm(p + vec2<f32>(0.0, t) + 5.2));
    let n = fbm(p + warp*1.5);
    let dense = smoothstep(0.45, 0.95, n);
    // Two-tone tint that drifts with the sector warmth global.
    let warmth = G.fx.w;
    let cold = vec3<f32>(0.15, 0.25, 0.55);
    let hot  = vec3<f32>(0.55, 0.18, 0.40);
    let col = mix(cold, hot, warmth) * dense;
    // A few embedded bright filaments.
    let fil = pow(smoothstep(0.6, 0.85, fbm(p*2.0 + warp)), 3.0);
    return vec4<f32>(col + fil*vec3<f32>(0.6,0.5,0.8), dense * 0.5);
}
"#;

/// The **beam** pass — railgun lances, laser sweeps and chain-lightning arcs. The renderer expands a
/// `(x0,y0)-(x1,y1)` segment into a quad; this shader paints a hot white core with a coloured glow
/// falloff across the quad's width, animates a travelling pulse along its length, and jitters arcs.
pub const BEAM_WGSL: &str = r#"
// ---- beam pass ----------------------------------------------------------------
// instance: p0(vec2) p1(vec2) width(f32) kind(f32) color(vec4) seed(f32)
struct BOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) along: f32, @location(1) across: f32,
    @location(2) color: vec4<f32>, @location(3) kind: f32, @location(4) seed: f32,
};
@vertex
fn vs_beam(
    @builtin(vertex_index) vi: u32,
    @location(0) p0: vec2<f32>, @location(1) p1: vec2<f32>,
    @location(2) wk: vec2<f32>, @location(3) color: vec4<f32>, @location(4) seed: f32,
) -> BOut {
    let dir = normalize(p1 - p0);
    let nor = vec2<f32>(-dir.y, dir.x) * wk.x;
    // Quad corners: (along 0/1, across -1/1)
    var quad = array<vec2<f32>,6>(
        vec2(0.0,-1.0), vec2(1.0,-1.0), vec2(0.0,1.0),
        vec2(1.0,-1.0), vec2(1.0,1.0), vec2(0.0,1.0));
    let c = quad[vi];
    let world = mix(p0, p1, c.x) + nor * c.y;
    let clip3 = G.view * vec3<f32>(world, 1.0);
    var o: BOut;
    o.clip = vec4<f32>(clip3.xy, 0.0, 1.0);
    o.along = c.x; o.across = c.y; o.color = color; o.kind = wk.y; o.seed = seed;
    return o;
}
@fragment
fn fs_beam(i: BOut) -> @location(0) vec4<f32> {
    var across = i.across;
    // kind 2 = arc: jitter the centreline so it crackles like lightning.
    if (i.kind > 1.5) {
        across = across + (vnoise(vec2<f32>(i.along*30.0 + i.seed, G.time*40.0)) - 0.5) * 0.8;
    }
    let core = smoothstep(0.18, 0.0, abs(across));        // white-hot centre
    let glow = smoothstep(1.0, 0.0, abs(across));          // coloured halo
    // Travelling pulse: a bright bead runs muzzle->target.
    let pulse = pow(fract(i.along - G.time*1.5), 8.0);
    let endfade = smoothstep(0.0, 0.06, i.along) * smoothstep(1.0, 0.94, i.along);
    let c = i.color.rgb * glow + vec3<f32>(1.0) * core + i.color.rgb * pulse * 2.0;
    return vec4<f32>(c * endfade, (glow*0.6 + core) * i.color.a * endfade);
}
"#;

/// The **shield** pass — a fresnel energy bubble around a ship. Faint while idle (just a rim), it
/// flares and ripples outward from the most recent impact point (fed per-instance), and tiles a
/// hex pattern that brightens where hit so a hit reads as a shimmer of cells.
pub const SHIELD_WGSL: &str = r#"
// ---- shield bubble ------------------------------------------------------------
// instance: model affine(6) color(vec4) charge(f32) hit_dir(vec2) hit_age(f32)
struct SOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>, @location(1) color: vec4<f32>,
    @location(2) charge: f32, @location(3) hitdir: vec2<f32>, @location(4) hitage: f32,
};
@vertex
fn vs_shield(
    @builtin(vertex_index) vi: u32,
    @location(0) m0: vec4<f32>, @location(1) m1: vec2<f32>,
    @location(2) color: vec4<f32>, @location(3) ch: f32,
    @location(4) hit: vec3<f32>,
) -> SOut {
    var quad = array<vec2<f32>,6>(
        vec2(-1.0,-1.0), vec2(1.0,-1.0), vec2(-1.0,1.0),
        vec2(1.0,-1.0), vec2(1.0,1.0), vec2(-1.0,1.0));
    let local = quad[vi];
    let model = array<f32,6>(m0.x,m0.y,m0.z,m0.w,m1.x,m1.y);
    let world = affine(model, local);
    let clip3 = G.view * vec3<f32>(world, 1.0);
    var o: SOut; o.clip = vec4<f32>(clip3.xy, 0.0, 1.0);
    o.local = local; o.color = color; o.charge = ch; o.hitdir = hit.xy; o.hitage = hit.z;
    return o;
}
@fragment
fn fs_shield(i: SOut) -> @location(0) vec4<f32> {
    let r = length(i.local);
    if (r > 1.0) { discard; }
    let fres = pow(r, 3.0);                                 // bright at the rim
    // Hex cells.
    let hex = abs(sin(i.local.x*18.0)) * abs(sin(i.local.y*18.0));
    let cells = smoothstep(0.85, 1.0, hex) * 0.25;
    // Impact ripple: a ring expanding from the hit direction, fading with age.
    let toward = dot(normalize(i.local + vec2<f32>(0.0001)), normalize(i.hitdir + vec2<f32>(0.0001)));
    let ripple = smoothstep(0.0, 1.0, toward) * pow(1.0 - i.hitage, 2.0)
               * exp(-pow((r - i.hitage*1.2)*6.0, 2.0)) * 3.0;
    let a = (fres*0.5 + cells + ripple) * i.charge * i.color.a;
    return vec4<f32>(i.color.rgb * (1.0 + ripple), a);
}
"#;

/// The **particle** pass — soft additive point sprites for thruster plumes, explosion sparks, smoke,
/// debris glints and warp streaks. The renderer uploads an instance buffer the [`crate::render`]
/// particle system fills; each is a billboarded quad with a radial soft falloff and a per-particle
/// colour/alpha already faded by its life.
pub const PARTICLE_WGSL: &str = r#"
// ---- particle (additive soft sprites) -----------------------------------------
// instance: center(vec2) size(f32) rot(f32) color(vec4) shape(f32)
struct POut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>, @location(1) color: vec4<f32>, @location(2) shape: f32,
};
@vertex
fn vs_particle(
    @builtin(vertex_index) vi: u32,
    @location(0) center: vec2<f32>, @location(1) sr: vec2<f32>,
    @location(2) color: vec4<f32>, @location(3) shape: f32,
) -> POut {
    var quad = array<vec2<f32>,6>(
        vec2(-1.0,-1.0), vec2(1.0,-1.0), vec2(-1.0,1.0),
        vec2(1.0,-1.0), vec2(1.0,1.0), vec2(-1.0,1.0));
    let q = quad[vi];
    let cr = cos(sr.y); let sn = sin(sr.y);
    let r = vec2<f32>(q.x*cr - q.y*sn, q.x*sn + q.y*cr) * sr.x;
    let world = center + r;
    let clip3 = G.view * vec3<f32>(world, 1.0);
    var o: POut; o.clip = vec4<f32>(clip3.xy, 0.0, 1.0);
    o.local = q; o.color = color; o.shape = shape; return o;
}
@fragment
fn fs_particle(i: POut) -> @location(0) vec4<f32> {
    let d = length(i.local);
    var a: f32;
    if (i.shape < 0.5) {
        a = smoothstep(1.0, 0.0, d);                 // soft round
    } else if (i.shape < 1.5) {
        a = smoothstep(1.0, 0.0, d) * smoothstep(0.0, 0.3, d); // ring/spark
    } else {
        a = smoothstep(1.0, 0.2, abs(i.local.y)) * smoothstep(1.0, 0.0, abs(i.local.x)); // streak
    }
    return vec4<f32>(i.color.rgb, i.color.a * a);
}
"#;

/// **Bloom** — three small passes the renderer chains: a bright-pass threshold, a separable Gaussian
/// blur (run H then V into a half-res target), and an additive composite back over the scene. This is
/// what makes every emissive material, beam, plume and explosion actually glow.
pub const BLOOM_WGSL: &str = r#"
// ---- bloom (threshold -> blur(H/V) -> composite) ------------------------------
@group(1) @binding(0) var SRC : texture_2d<f32>;
@group(1) @binding(1) var SMP : sampler;

@fragment
fn fs_threshold(i: FOut) -> @location(0) vec4<f32> {
    let c = textureSample(SRC, SMP, i.uv).rgb;
    let b = max(dot(c, vec3<f32>(0.2126,0.7152,0.0722)) - 1.0, 0.0); // anything over 1.0 blooms
    return vec4<f32>(c * b, 1.0);
}

// `G.fx.x` carries the blur direction packed by the renderer per dispatch (1,0)|(0,1)*radius.
@fragment
fn fs_blur(i: FOut) -> @location(0) vec4<f32> {
    let texel = 1.0 / G.viewport;
    let dir = vec2<f32>(G.fx.x, G.fx.y) * texel;
    let w = array<f32,5>(0.227027, 0.194595, 0.121622, 0.054054, 0.016216);
    var c = textureSample(SRC, SMP, i.uv).rgb * w[0];
    for (var k = 1; k < 5; k = k + 1) {
        let o = dir * f32(k);
        c = c + textureSample(SRC, SMP, i.uv + o).rgb * w[k];
        c = c + textureSample(SRC, SMP, i.uv - o).rgb * w[k];
    }
    return vec4<f32>(c, 1.0);
}

@fragment
fn fs_composite(i: FOut) -> @location(0) vec4<f32> {
    let scene = textureSample(SRC, SMP, i.uv).rgb;
    return vec4<f32>(scene, 1.0); // the blurred bloom target is blended additively by the pipeline
}
"#;

/// The final **post** pass — filmic tonemap (so emissive rolls off instead of clipping to flat white),
/// camera-shake-driven **chromatic aberration**, a soft **vignette**, a warmth/teal colour grade, and
/// the faintest scanline so it reads as a ship's viewscreen. Run once, last, into the swapchain.
pub const POST_WGSL: &str = r#"
// ---- post (tonemap + grade + vignette + chromatic + scanline) -----------------
@group(1) @binding(0) var SCENE : texture_2d<f32>;
@group(1) @binding(1) var SMP2  : sampler;

fn aces(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x*(a*x+b))/(x*(c*x+d)+e), vec3<f32>(0.0), vec3<f32>(1.0));
}
@fragment
fn fs_post(i: FOut) -> @location(0) vec4<f32> {
    let uv = i.uv;
    let center = uv - 0.5;
    // Chromatic aberration grows toward the edges and with screen shake.
    let ca = (0.0015 + G.fx.z*0.004) * length(center);
    let off = normalize(center + vec2<f32>(0.0001)) * ca;
    var col: vec3<f32>;
    col.r = textureSample(SCENE, SMP2, uv + off).r;
    col.g = textureSample(SCENE, SMP2, uv).g;
    col.b = textureSample(SCENE, SMP2, uv - off).b;

    col = aces(col * 1.1);

    // Colour grade: lift shadows toward teal, push highlights warm.
    let warmth = G.fx.w;
    let grade = mix(vec3<f32>(0.9,1.0,1.05), vec3<f32>(1.06,1.0,0.92), warmth);
    col = col * grade;

    // Vignette + scanline.
    let vig = smoothstep(0.95, 0.35, length(center));
    let scan = 0.97 + 0.03*sin(uv.y * G.viewport.y * 1.4);
    col = col * vig * scan;
    return vec4<f32>(col, 1.0);
}
"#;

/// Assemble the full default shader map: each named pass is `GLOBALS_WGSL` (the shared prelude)
/// concatenated with that pass's source, so every shader compiles standalone. The renderer looks
/// these up by name and builds one pipeline each; a hot-pushed ruleset can override any single entry.
pub fn builtin_shaders() -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    let with_prelude = |src: &str| format!("{GLOBALS_WGSL}\n{src}");
    m.insert("ship".into(), with_prelude(SHIP_WGSL));
    m.insert("starfield".into(), with_prelude(STARFIELD_WGSL));
    m.insert("nebula".into(), with_prelude(NEBULA_WGSL));
    m.insert("beam".into(), with_prelude(BEAM_WGSL));
    m.insert("shield".into(), with_prelude(SHIELD_WGSL));
    m.insert("particle".into(), with_prelude(PARTICLE_WGSL));
    m.insert("bloom".into(), with_prelude(BLOOM_WGSL));
    m.insert("post".into(), with_prelude(POST_WGSL));
    m
}

/// Free-form render parameters the frontend reads alongside the shaders (also hot-reloadable): the
/// default post-FX tuning, the layer draw order, and the per-platform quality tiers. Kept as JSON so
/// the renderer can grow new knobs without a backend type change (exactly the `Assets::params`
/// contract). Mirrors [`crate::render::PostFx::cinematic`] / [`crate::render::QualityTier`].
pub fn builtin_visual_params() -> serde_json::Value {
    serde_json::json!({
        "postfx": {
            "bloom_threshold": 1.0,
            "bloom_intensity": 0.9,
            "bloom_radius": 4.0,
            "vignette": 0.6,
            "chromatic": 0.0015,
            "scanline": 0.03,
            "grade_warmth": 0.5,
            "tonemap": "aces"
        },
        // Draw order, back to front — matches render::Layer::order().
        "layers": ["starfield", "nebula", "mines", "pickups", "trails", "ships",
                   "beams", "particles", "shields", "overlay"],
        "quality": {
            "native":         { "bloom": true,  "particles": 4000, "nebula": true,  "shadows": true,  "msaa": 4 },
            "desktop_browser":{ "bloom": true,  "particles": 1500, "nebula": true,  "shadows": false, "msaa": 4 },
            "mobile_browser": { "bloom": false, "particles": 350,  "nebula": false, "shadows": false, "msaa": 1 }
        },
        "starfield": { "layers": 3, "base_density": 60.0, "parallax": 0.0006 }
    })
}

/// The complete default [`Assets`] blob the builtin ruleset ships: every shader plus the visual
/// params. This is what makes a fresh node render a finished-looking game with zero extra fetches.
pub fn builtin_assets() -> Assets {
    Assets { shaders: builtin_shaders(), params: builtin_visual_params() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_shader_is_present_and_nonempty() {
        let s = builtin_shaders();
        for name in ["ship", "starfield", "nebula", "beam", "shield", "particle", "bloom", "post"] {
            let src = s.get(name).unwrap_or_else(|| panic!("missing shader {name}"));
            assert!(src.contains("@fragment"), "{name} has a fragment stage");
            // The shared prelude is prepended to every pass.
            assert!(src.contains("struct Globals"), "{name} carries the globals prelude");
        }
    }

    #[test]
    fn builtin_assets_carry_shaders_and_params() {
        let a = builtin_assets();
        assert!(a.shaders.len() >= 8, "all passes present");
        assert!(a.params.get("postfx").is_some(), "post-fx tuning present");
        assert!(a.params.get("layers").is_some(), "layer order present");
        // The layer list lines up with the render module's ordering count.
        let layers = a.params.get("layers").and_then(|l| l.as_array()).unwrap();
        assert_eq!(layers.len(), 10, "ten draw layers");
    }
}
