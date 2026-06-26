//! **Deterministic world genesis** — conjure a cell's entire contents from nothing but its coordinates.
//!
//! The galaxy is never pre-generated and never stored. What lives in a patch of space — its biome, star
//! systems, asteroid fields, nebulae, derelicts, the pirates lurking in them — is a **pure function of
//! `(galaxy_seed, CellId)`**. Every node computes the same universe for the same cell, so there is
//! nothing to coordinate, nothing to ship, nothing to persist: an unvisited cell costs zero bytes and
//! zero compute until a ship arrives and it springs into being, identically, on whoever hosts it.
//!
//! The seed is the chain's genesis hash, so a galaxy is unique and tamper-evident: anyone can regenerate
//! any cell and check its `content_hash`, and no host can secretly salt a richer asteroid field for
//! itself — the dice are fixed by the chain.
//!
//! Two gradients give the galaxy its shape and its pull:
//! - **biome** — low-frequency noise over cell coordinates, so neighbours share character: a cluster of
//!   dense star systems here, an empty void there, a nebula reach beyond.
//! - **frontier gradient** — danger and reward both rise with distance from the origin. The core is
//!   safe and poor; the deep frontier is lethal and gilded. Players are pulled outward by greed and
//!   pushed back by death — the loop that makes a galaxy worth exploring.

use crate::galaxy::{CellId, World};

/// The root seed for a galaxy instance — the chain genesis hash. Shared by every node; makes all
/// generation deterministic *and* unique to this galaxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GalaxySeed(pub [u8; 32]);

impl GalaxySeed {
    /// Fold the seed into a single u64 base for the per-cell streams.
    fn base(&self) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        for b in self.0 {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        h
    }
}

/// A tiny, fast, deterministic PRNG (splitmix64). Seeded from `(galaxy, cell, channel)` so independent
/// aspects of a cell (biome vs. asteroids vs. names) draw from independent, reproducible streams.
pub struct Rng(u64);

impl Rng {
    pub fn for_cell(seed: &GalaxySeed, cell: CellId, channel: u64) -> Self {
        let mut s = seed.base();
        s ^= (cell.morton() as u64).wrapping_mul(0x9e3779b97f4a7c15);
        s ^= channel.wrapping_mul(0xff51afd7ed558ccd);
        Rng(s | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    /// Uniform in `[0, 1)`.
    pub fn f01(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Inclusive integer range.
    pub fn range(&mut self, lo: u32, hi: u32) -> u32 {
        if hi <= lo {
            lo
        } else {
            lo + (self.next_u64() % (hi - lo + 1) as u64) as u32
        }
    }
    pub fn chance(&mut self, p: f32) -> bool {
        self.f01() < p
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next_u64() as usize) % xs.len()]
    }
}

/// The character of a region. Determined by coarse noise so it's contiguous — you cross *into* a nebula
/// reach, you don't flicker in and out of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Biome {
    /// Dense, bright, safe-ish: lots of systems, modest loot. Where new players thrive.
    CoreCluster,
    /// Empty deep space: few systems, long hauls, the occasional lonely derelict.
    OpenVoid,
    /// Glowing gas: sensor-occluding cover, ion hazards, hidden riches.
    NebulaReach,
    /// Belts on belts: the mining heartland — minerals and alloys, contested by pirates.
    AsteroidExpanse,
    /// Ancient dead civilisation: ruins, tech caches, and the Ancients that still guard them.
    RuinField,
    /// The edge of the known: lethal, gilded, exotic resources found nowhere else.
    FrontierDark,
}

impl Biome {
    /// Pick a biome from low-frequency noise over the cell's normalised position, weighted outward so
    /// the deep frontier trends toward `FrontierDark`/`RuinField` and the core toward `CoreCluster`.
    pub fn of(seed: &GalaxySeed, cell: CellId, ring: u32) -> Biome {
        // Quantise to a coarse grid so neighbouring leaves usually share a biome (contiguous regions).
        let coarse = CellId { depth: cell.depth.min(6), x: cell.x >> cell.depth.saturating_sub(6), y: cell.y >> cell.depth.saturating_sub(6) };
        let mut r = Rng::for_cell(seed, coarse, 0xB10E);
        let roll = r.f01();
        let edge = (ring as f32 / 32.0).clamp(0.0, 1.0); // 0 = core, 1 = deep frontier
        // Blend two distributions by `edge`.
        let core = [
            (Biome::CoreCluster, 0.40),
            (Biome::AsteroidExpanse, 0.25),
            (Biome::OpenVoid, 0.20),
            (Biome::NebulaReach, 0.10),
            (Biome::RuinField, 0.04),
            (Biome::FrontierDark, 0.01),
        ];
        let frontier = [
            (Biome::FrontierDark, 0.35),
            (Biome::RuinField, 0.25),
            (Biome::NebulaReach, 0.20),
            (Biome::AsteroidExpanse, 0.12),
            (Biome::OpenVoid, 0.06),
            (Biome::CoreCluster, 0.02),
        ];
        let mut acc = 0.0;
        for i in 0..6 {
            let w = core[i].1 * (1.0 - edge) + frontier[i].1 * edge;
            acc += w;
            if roll <= acc {
                return core[i].0;
            }
        }
        Biome::OpenVoid
    }

    /// Expected star-system density multiplier.
    pub fn density(&self) -> f32 {
        match self {
            Biome::CoreCluster => 1.6,
            Biome::AsteroidExpanse => 0.9,
            Biome::NebulaReach => 0.7,
            Biome::RuinField => 0.5,
            Biome::OpenVoid => 0.25,
            Biome::FrontierDark => 0.4,
        }
    }
    pub fn danger_mod(&self) -> f32 {
        match self {
            Biome::CoreCluster => -0.2,
            Biome::OpenVoid => -0.1,
            Biome::AsteroidExpanse => 0.1,
            Biome::NebulaReach => 0.15,
            Biome::RuinField => 0.3,
            Biome::FrontierDark => 0.5,
        }
    }
    pub fn richness_mod(&self) -> f32 {
        match self {
            Biome::CoreCluster => 0.0,
            Biome::OpenVoid => -0.1,
            Biome::AsteroidExpanse => 0.4,
            Biome::NebulaReach => 0.25,
            Biome::RuinField => 0.5,
            Biome::FrontierDark => 0.7,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Biome::CoreCluster => "Core Cluster",
            Biome::OpenVoid => "Open Void",
            Biome::NebulaReach => "Nebula Reach",
            Biome::AsteroidExpanse => "Asteroid Expanse",
            Biome::RuinField => "Ruin Field",
            Biome::FrontierDark => "Frontier Dark",
        }
    }
}

/// The chebyshev ring distance of a cell from the origin, normalised to a reference depth so it means
/// the same thing at any zoom — the "how deep into the frontier am I" coordinate.
pub fn ring_distance(cell: CellId) -> u32 {
    let ref_depth = 12u8;
    let (x, y) = if cell.depth <= ref_depth {
        let s = ref_depth - cell.depth;
        (cell.x << s, cell.y << s)
    } else {
        let s = cell.depth - ref_depth;
        (cell.x >> s, cell.y >> s)
    };
    let half = 1u32 << (ref_depth - 1);
    let dx = (x as i64 - half as i64).unsigned_abs() as u32;
    let dy = (y as i64 - half as i64).unsigned_abs() as u32;
    dx.max(dy)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StarType {
    YellowDwarf,
    RedDwarf,
    BlueGiant,
    Neutron,
    BlackHole,
}

#[derive(Debug, Clone)]
pub struct StarSystem {
    pub name: String,
    pub pos: (World, World),
    pub star: StarType,
    pub planets: u8,
    /// True if this system has a habitable world — a natural settlement/respawn anchor.
    pub habitable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Minerals,
    Energy,
    Alloys,
    /// Only out past the frontier gradient — the reason to risk the deep dark.
    Exotics,
    /// Rarer still; powers the highest tech. Practically a treasure map's X.
    Antimatter,
}

#[derive(Debug, Clone)]
pub struct ResourceField {
    pub kind: Resource,
    pub pos: (World, World),
    pub radius: World,
    /// 0..1, scaled by the frontier gradient + biome.
    pub richness: f32,
}

#[derive(Debug, Clone)]
pub enum Phenomenon {
    /// Sensor-occluding cover; ships inside are hidden from the map beyond a short range.
    Nebula { pos: (World, World), radius: World },
    /// Periodic ion damage / control scramble — risk you route around or dash through.
    IonStorm { pos: (World, World), radius: World, intensity: f32 },
    /// Bends trajectories; slingshot for the bold, grave for the careless.
    GravityWell { pos: (World, World), mass: f32 },
    /// A latent wormhole mouth — pairs with a far cell's anchor to stitch the galaxy (see `frontier`).
    WormholeAnchor { pos: (World, World), pair_hint: u128 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerelictKind {
    Wreck,
    AncientVault,
    DerelictStation,
    DistressBeacon,
}

#[derive(Debug, Clone)]
pub struct Derelict {
    pub kind: DerelictKind,
    pub pos: (World, World),
    /// 0..4 loot tier, scaled by danger — the deeper the dark, the better the salvage.
    pub loot_tier: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpcFaction {
    Pirates,
    Ancients,
    /// A spreading hostile swarm — denser at the frontier.
    Swarm,
}

#[derive(Debug, Clone)]
pub struct NpcPresence {
    pub faction: NpcFaction,
    /// 0..1; multiplies fleet size + ship tier the cell spawns.
    pub strength: f32,
}

/// The full, deterministic contents of one cell — everything that materialises when it is first
/// entered. Reproducible anywhere from `(seed, cell)`; `content_hash` lets anyone verify a host didn't
/// cheat the dice.
#[derive(Debug, Clone)]
pub struct CellGenesis {
    pub cell: CellId,
    pub biome: Biome,
    pub ring: u32,
    /// 0..1 outward gradient + biome — drives NPC strength, hazard frequency, loot tiers.
    pub danger: f32,
    /// 0..1 — drives resource richness and rare-resource odds.
    pub richness: f32,
    pub systems: Vec<StarSystem>,
    pub fields: Vec<ResourceField>,
    pub phenomena: Vec<Phenomenon>,
    pub derelicts: Vec<Derelict>,
    pub npcs: Option<NpcPresence>,
    pub content_hash: u64,
}

impl CellGenesis {
    /// Generate a cell from first principles. PURE — same `(seed, cell)` yields byte-identical content
    /// on every node, forever. This is the whole "infinite, coordinator-free, zero-storage universe".
    pub fn generate(seed: &GalaxySeed, cell: CellId) -> CellGenesis {
        let ring = ring_distance(cell);
        let biome = Biome::of(seed, cell, ring);
        let edge = (ring as f32 / 64.0).clamp(0.0, 1.0);
        let danger = (edge * 0.8 + biome.danger_mod()).clamp(0.0, 1.0);
        let richness = (edge * 0.6 + biome.richness_mod()).clamp(0.0, 1.0);

        let r = cell.rect();
        let mut rng = Rng::for_cell(seed, cell, 0x5A1D);

        // Star systems — count from biome density, positions scattered in the cell.
        let n_systems = (rng.range(0, 4) as f32 * biome.density()) as u32;
        let mut systems = Vec::new();
        for _ in 0..n_systems {
            let star = *rng.pick(&[StarType::YellowDwarf, StarType::RedDwarf, StarType::BlueGiant, StarType::Neutron, StarType::BlackHole]);
            systems.push(StarSystem {
                name: gen_name(&mut rng),
                pos: (r.x + rng.f01() as World * r.span, r.y + rng.f01() as World * r.span),
                star,
                planets: rng.range(0, 8) as u8,
                habitable: rng.chance(0.18 - danger * 0.12), // safe space is more livable
            });
        }

        // Resource fields — richer outward; exotics/antimatter only past the gradient.
        let mut fields = Vec::new();
        let n_fields = rng.range(0, 3 + (richness * 4.0) as u32);
        for _ in 0..n_fields {
            let kind = if richness > 0.85 && rng.chance(0.15) {
                Resource::Antimatter
            } else if richness > 0.6 && rng.chance(0.3) {
                Resource::Exotics
            } else {
                *rng.pick(&[Resource::Minerals, Resource::Energy, Resource::Alloys])
            };
            fields.push(ResourceField {
                kind,
                pos: (r.x + rng.f01() as World * r.span, r.y + rng.f01() as World * r.span),
                radius: 200.0 + rng.f01() as World * 800.0,
                richness: (richness + rng.f01() * 0.3).clamp(0.0, 1.0),
            });
        }

        // Phenomena — biome-flavoured.
        let mut phenomena = Vec::new();
        let pos = |rng: &mut Rng| (r.x + rng.f01() as World * r.span, r.y + rng.f01() as World * r.span);
        match biome {
            Biome::NebulaReach => phenomena.push(Phenomenon::Nebula { pos: pos(&mut rng), radius: r.span * 0.4 }),
            Biome::FrontierDark if rng.chance(0.3) => {
                phenomena.push(Phenomenon::IonStorm { pos: pos(&mut rng), radius: r.span * 0.3, intensity: danger })
            }
            _ => {}
        }
        if rng.chance(0.05 + danger * 0.1) {
            phenomena.push(Phenomenon::GravityWell { pos: pos(&mut rng), mass: 0.5 + rng.f01() });
        }
        if rng.chance(0.02 + edge * 0.06) {
            // A wormhole mouth: pairs deterministically with a distant cell (the frontier stitches them).
            phenomena.push(Phenomenon::WormholeAnchor { pos: pos(&mut rng), pair_hint: rng.next_u64() as u128 });
        }

        // Derelicts — better salvage in deeper, deadlier space.
        let mut derelicts = Vec::new();
        if rng.chance(0.1 + danger * 0.25) {
            let kind = if biome == Biome::RuinField {
                *rng.pick(&[DerelictKind::AncientVault, DerelictKind::DerelictStation])
            } else {
                *rng.pick(&[DerelictKind::Wreck, DerelictKind::DistressBeacon])
            };
            derelicts.push(Derelict { kind, pos: pos(&mut rng), loot_tier: (danger * 4.0) as u8 });
        }

        // NPC presence — pirates contest the rich, Ancients guard the ruins, the Swarm festers at the edge.
        let npcs = if rng.chance(0.15 + danger * 0.5) {
            let faction = match biome {
                Biome::RuinField => NpcFaction::Ancients,
                Biome::FrontierDark => *rng.pick(&[NpcFaction::Swarm, NpcFaction::Pirates]),
                _ => NpcFaction::Pirates,
            };
            Some(NpcPresence { faction, strength: (danger + rng.f01() * 0.3).clamp(0.0, 1.0) })
        } else {
            None
        };

        let mut hash = Rng::for_cell(seed, cell, 0xC0DE);
        let content_hash = hash.next_u64()
            ^ (systems.len() as u64).wrapping_mul(0x100000001b3)
            ^ (fields.len() as u64).wrapping_mul(0x9e3779b97f4a7c15)
            ^ ((danger * 1000.0) as u64);

        CellGenesis { cell, biome, ring, danger, richness, systems, fields, phenomena, derelicts, npcs, content_hash }
    }

    /// A one-line "what's out here" the client shows on entering a fresh cell.
    pub fn headline(&self) -> String {
        format!(
            "{} — danger {:.0}% · {} systems · {} fields{}",
            self.biome.label(),
            self.danger * 100.0,
            self.systems.len(),
            self.fields.len(),
            self.npcs.as_ref().map(|n| format!(" · {:?} present", n.faction)).unwrap_or_default(),
        )
    }
}

/// A pronounceable procedural designation for a system/landmark, e.g. "Veyra-7", "Tharn Expanse".
fn gen_name(rng: &mut Rng) -> String {
    const A: [&str; 12] = ["Vey", "Tharn", "Sol", "Kep", "Ori", "Lyr", "Cass", "Drav", "Eos", "Nyx", "Zar", "Hel"];
    const B: [&str; 10] = ["ra", "is", "or", "en", "ax", "une", "ix", "eth", "ara", "us"];
    if rng.chance(0.5) {
        format!("{}{}-{}", rng.pick(&A), rng.pick(&B), rng.range(1, 999))
    } else {
        format!("{}{}", rng.pick(&A), rng.pick(&B))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> GalaxySeed {
        GalaxySeed([7u8; 32])
    }

    #[test]
    fn generation_is_deterministic() {
        let cell = CellId { depth: 8, x: 130, y: 77 };
        let a = CellGenesis::generate(&seed(), cell);
        let b = CellGenesis::generate(&seed(), cell);
        assert_eq!(a.content_hash, b.content_hash);
        assert_eq!(a.systems.len(), b.systems.len());
    }

    #[test]
    fn different_seeds_yield_different_galaxies() {
        let cell = CellId { depth: 8, x: 130, y: 77 };
        let a = CellGenesis::generate(&GalaxySeed([1u8; 32]), cell);
        let b = CellGenesis::generate(&GalaxySeed([2u8; 32]), cell);
        assert_ne!(a.content_hash, b.content_hash);
    }

    #[test]
    fn danger_and_richness_rise_outward() {
        let core = CellGenesis::generate(&seed(), CellId::at_depth_for(12, 0.0, 0.0));
        let edge = CellGenesis::generate(&seed(), CellId { depth: 12, x: 0, y: 0 }); // a corner, far from centre
        assert!(edge.ring >= core.ring);
    }

    #[test]
    fn neighbours_usually_share_a_biome() {
        // Contiguity: a run of adjacent cells should not change biome every step.
        let mut changes = 0;
        let mut prev = None;
        for x in 100..140u32 {
            let b = Biome::of(&seed(), CellId { depth: 8, x, y: 100 }, 50);
            if let Some(p) = prev {
                if p != b {
                    changes += 1;
                }
            }
            prev = Some(b);
        }
        assert!(changes < 20, "biomes should form contiguous regions, not flicker ({changes} changes)");
    }
}
