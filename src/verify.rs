//! **The verification dial** — anti-cheat that scales with the stakes.
//!
//! Verifying every cell to the hilt would waste the planet's compute; verifying nothing would let a
//! cheating host fake a sealed leaderboard or mint an antimatter haul. So scrutiny is a *dial*, turned
//! by what a cell is worth: an empty void cell is simply trusted (one host, no checks); a battle over an
//! antimatter field with high-value pilots and a record on the line is simulated by a quorum of
//! independent high-precision replicas that must agree on the state hash every single tick, post bond,
//! attest their execution, and seal the result to the chain.
//!
//! The elegant part — the same one the treasury has: **stakes fund their own scrutiny.** The cells worth
//! cheating are exactly the cells generating the revenue that pays for the extra replicas verifying them.
//! Honesty is cheap where nothing's at stake and bought where everything is.
//!
//! Builds directly on what exists: the replicas + majority-hash agreement of [`crate::replication`], the
//! per-region war chests of [`crate::treasury`], and the bond/slashing of the Sybil-security design. The
//! dial just decides *how much* of all that to apply, per cell, per tick.

use crate::treasury::Credits;

/// Everything that makes a cell worth cheating — the inputs to the dial. Computed each epoch from the
/// cell's sim + treasury + the host's trust.
#[derive(Debug, Clone, Copy)]
pub struct Stakes {
    pub players: u32,
    /// Sum of at-risk value carried by the players in the cell (minerals + credits + loadout worth).
    pub carried_value: u128,
    /// A rare resource (exotics/antimatter) is present — high incentive to fake a haul.
    pub rare_resource: bool,
    /// Credits/minute flowing through the cell (treasury rent rate) — proxy for economic weight.
    pub credits_per_min: u128,
    /// A leaderboard seal / record is imminent here — a result that becomes permanent and public.
    pub near_sealed_event: bool,
    /// The host's earned trust, 0..1. Low trust *raises* effective stakes (watch the suspicious closely).
    pub host_trust: f32,
}

impl Stakes {
    /// The stake score, 0..1 — monotonic in every risk factor. This single number turns the dial.
    pub fn score(&self) -> f32 {
        let players = (self.players as f32 / 200.0).min(1.0);
        let value = (self.carried_value as f64 / 1e21_f64) as f32; // ~1000 credits worth saturates
        let value = value.min(1.0);
        let econ = (self.credits_per_min as f64 / 1e18_f64) as f32; // ~1 credit/min saturates
        let econ = econ.min(1.0);
        let rare = if self.rare_resource { 0.3 } else { 0.0 };
        let sealed = if self.near_sealed_event { 0.4 } else { 0.0 };
        let suspicion = (1.0 - self.host_trust.clamp(0.0, 1.0)) * 0.5;
        // Saturating combine: any single big factor can demand scrutiny; they reinforce.
        (0.30 * players + 0.25 * value + 0.20 * econ + rare + sealed + suspicion).min(1.0)
    }
}

/// Named rungs of the dial, for logs/telemetry and the per-cell HUD ("VERIFIED ×5").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Trust the host outright — empty/cheap space.
    Trust,
    Light,
    Standard,
    High,
    /// The works: dense replication every tick, attestation, on-chain seal, full bond.
    Maximum,
}

/// The concrete verification regime for a cell this epoch — the dial's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationLevel {
    pub tier: Tier,
    /// Independent high-precision replicas that simulate the cell in parallel (incl. the host).
    pub replicas: u8,
    /// How many of those must agree on the state hash for a result to be accepted.
    pub quorum: u8,
    /// State-hash checkpoint cadence (1 = every tick).
    pub hash_every_ticks: u32,
    /// Require hosts to attest their execution (TEE / proof) — for the top tiers.
    pub require_attestation: bool,
    /// Anchor the agreed state hash to the PoW chain (tamper-evident, publicly checkable).
    pub seal_on_chain: bool,
    /// Bond each replica must post; slashed if it diverges from the quorum. Scales with stakes so the
    /// expected cost of lying always exceeds the prize.
    pub bond_required: Credits,
}

/// Maps stakes → level. The thresholds are the only policy; everything downstream is mechanical.
#[derive(Debug, Clone, Copy)]
pub struct VerificationPolicy {
    pub light_above: f32,
    pub standard_above: f32,
    pub high_above: f32,
    pub maximum_above: f32,
    /// Bond per unit stake (credits) at the top of the dial.
    pub max_bond: Credits,
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        VerificationPolicy {
            light_above: 0.15,
            standard_above: 0.35,
            high_above: 0.60,
            maximum_above: 0.85,
            max_bond: 1_000_000_000_000_000_000, // 1 credit
        }
    }
}

impl VerificationPolicy {
    /// Turn the dial for these stakes. Monotonic: more at stake never yields *less* scrutiny.
    pub fn level_for(&self, stakes: &Stakes) -> VerificationLevel {
        let s = stakes.score();
        let bond = (self.max_bond as f64 * s as f64) as u128;
        if s >= self.maximum_above {
            VerificationLevel { tier: Tier::Maximum, replicas: 7, quorum: 4, hash_every_ticks: 1, require_attestation: true, seal_on_chain: true, bond_required: bond }
        } else if s >= self.high_above {
            VerificationLevel { tier: Tier::High, replicas: 5, quorum: 3, hash_every_ticks: 4, require_attestation: true, seal_on_chain: true, bond_required: bond }
        } else if s >= self.standard_above {
            VerificationLevel { tier: Tier::Standard, replicas: 3, quorum: 2, hash_every_ticks: 20, require_attestation: false, seal_on_chain: false, bond_required: bond }
        } else if s >= self.light_above {
            VerificationLevel { tier: Tier::Light, replicas: 2, quorum: 2, hash_every_ticks: 60, require_attestation: false, seal_on_chain: false, bond_required: bond }
        } else {
            VerificationLevel { tier: Tier::Trust, replicas: 1, quorum: 1, hash_every_ticks: 200, require_attestation: false, seal_on_chain: false, bond_required: 0 }
        }
    }
}

/// One replica's signed claim about the cell's state at a tick — the unit the quorum judges. (The hash
/// is `crate::sim::Sim::state_hash`; the node authenticates the signer.)
#[derive(Debug, Clone, PartialEq)]
pub struct StateProof {
    pub host: String,
    pub tick: u64,
    pub hash: u64,
    /// Present iff the level requires attestation; a missing/invalid one is treated as a dissent.
    pub attestation_ok: bool,
}

/// The outcome of judging a tick's proofs against the level.
#[derive(Debug, Clone, PartialEq)]
pub enum Agreement {
    /// Quorum reached on one hash — the result is canonical (and sealed if the level says so).
    Confirmed { hash: u64, agreeing: u8 },
    /// Replicas split: a majority hash exists but dissenters disagree (cheating or faulty) → slash them.
    Diverged { majority: u64, agreeing: u8, dissenters: Vec<String> },
    /// Not enough proofs arrived to reach quorum — re-replicate / widen and retry; never accept blindly.
    Insufficient { have: u8, need: u8 },
}

/// Judge a tick: tally the (valid) proofs by hash, take the plurality, and compare to the quorum.
pub fn judge(proofs: &[StateProof], level: &VerificationLevel) -> Agreement {
    use std::collections::HashMap;
    // Attestation, when required, gates a proof's validity.
    let valid: Vec<&StateProof> = proofs
        .iter()
        .filter(|p| !level.require_attestation || p.attestation_ok)
        .collect();
    if (valid.len() as u8) < level.quorum {
        return Agreement::Insufficient { have: valid.len() as u8, need: level.quorum };
    }
    let mut by_hash: HashMap<u64, Vec<&str>> = HashMap::new();
    for p in &valid {
        by_hash.entry(p.hash).or_default().push(&p.host);
    }
    let (maj_hash, agreeing_hosts) = by_hash
        .iter()
        .max_by_key(|(_, hosts)| hosts.len())
        .map(|(h, hosts)| (*h, hosts.clone()))
        .unwrap();
    let agreeing = agreeing_hosts.len() as u8;
    if agreeing >= level.quorum && by_hash.len() == 1 {
        Agreement::Confirmed { hash: maj_hash, agreeing }
    } else if agreeing >= level.quorum {
        let dissenters: Vec<String> = valid
            .iter()
            .filter(|p| p.hash != maj_hash)
            .map(|p| p.host.clone())
            .collect();
        Agreement::Diverged { majority: maj_hash, agreeing, dissenters }
    } else {
        Agreement::Insufficient { have: valid.len() as u8, need: level.quorum }
    }
}

/// A bond slash to enact on the chain for a divergent (cheating/faulty) replica.
#[derive(Debug, Clone, PartialEq)]
pub struct Slash {
    pub host: String,
    pub amount: Credits,
    pub reason: &'static str,
}

/// The slashes implied by an agreement at a level — dissenters lose their bond. (Confirmed/Insufficient
/// slash nobody; insufficiency is a coverage problem, not a crime.)
pub fn slashes(agreement: &Agreement, level: &VerificationLevel) -> Vec<Slash> {
    match agreement {
        Agreement::Diverged { dissenters, .. } if level.bond_required > 0 => dissenters
            .iter()
            .map(|h| Slash { host: h.clone(), amount: level.bond_required, reason: "state-hash divergence from quorum" })
            .collect(),
        _ => Vec::new(),
    }
}

/// The compute cost/minute of verifying a cell at `level`, given the cell's base hosting cost — roughly
/// linear in replicas. Paid from the cell's rent ([`crate::treasury`]), which is exactly why high-stakes
/// (high-revenue) cells can afford their own heavy scrutiny.
pub fn cost_per_min(level: &VerificationLevel, base_cell_cost_per_min: Credits) -> Credits {
    base_cell_cost_per_min.saturating_mul(level.replicas as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stakes(score_drivers: (u32, u128, bool, bool, f32)) -> Stakes {
        let (players, value, rare, sealed, trust) = score_drivers;
        Stakes { players, carried_value: value, rare_resource: rare, credits_per_min: 0, near_sealed_event: sealed, host_trust: trust }
    }

    #[test]
    fn dial_is_monotonic() {
        let pol = VerificationPolicy::default();
        let empty = pol.level_for(&stakes((0, 0, false, false, 1.0)));
        let busy = pol.level_for(&stakes((150, 0, false, false, 1.0)));
        let jackpot = pol.level_for(&stakes((200, 1_000_000_000_000_000_000_000, true, true, 0.2)));
        assert_eq!(empty.tier, Tier::Trust);
        assert!(busy.replicas >= empty.replicas);
        assert!(jackpot.replicas >= busy.replicas);
        assert_eq!(jackpot.tier, Tier::Maximum);
        assert!(jackpot.seal_on_chain && jackpot.require_attestation);
    }

    #[test]
    fn empty_space_is_free_to_verify() {
        let pol = VerificationPolicy::default();
        let lvl = pol.level_for(&stakes((0, 0, false, false, 1.0)));
        assert_eq!(cost_per_min(&lvl, 100), 100, "1 replica → base cost, no overhead in the void");
        assert_eq!(lvl.bond_required, 0);
    }

    #[test]
    fn quorum_confirms_and_divergence_slashes() {
        let lvl = VerificationLevel { tier: Tier::High, replicas: 5, quorum: 3, hash_every_ticks: 4, require_attestation: false, seal_on_chain: true, bond_required: 1000 };
        let p = |host: &str, hash| StateProof { host: host.into(), tick: 9, hash, attestation_ok: true };
        // Four honest agree, one cheats.
        let proofs = vec![p("a", 111), p("b", 111), p("c", 111), p("d", 111), p("cheat", 999)];
        let outcome = judge(&proofs, &lvl);
        match &outcome {
            Agreement::Diverged { majority, dissenters, .. } => {
                assert_eq!(*majority, 111);
                assert_eq!(dissenters, &vec!["cheat".to_string()]);
            }
            _ => panic!("expected divergence, got {outcome:?}"),
        }
        let s = slashes(&outcome, &lvl);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].host, "cheat");
        assert_eq!(s[0].amount, 1000);
    }

    #[test]
    fn too_few_proofs_is_never_accepted() {
        let lvl = VerificationLevel { tier: Tier::Standard, replicas: 3, quorum: 2, hash_every_ticks: 20, require_attestation: false, seal_on_chain: false, bond_required: 10 };
        let only_one = vec![StateProof { host: "a".into(), tick: 1, hash: 5, attestation_ok: true }];
        assert!(matches!(judge(&only_one, &lvl), Agreement::Insufficient { .. }));
    }
}
