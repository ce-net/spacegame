//! The **treasury** — how the galaxy pays for itself, in CE credits, with no operator footing the bill.
//!
//! The elegant property we want: **a surge funds its own compute.** When ten thousand players pile into
//! a system, the same presence that forces the cells to subdivide also generates the revenue that pays
//! for the extra nodes — including the cloud burst rented to absorb it. Quiet space costs almost
//! nothing; a galactic war pays for the datacentres it lights up, then gives them back.
//!
//! Flows (all in integer base-unit credits, per [`crate::sim`]'s money model — never floats):
//!
//! ```text
//!   player ──channel receipt──► cell rent pool ──cell-minute──► host node (donor or burst)
//!                                     │ protocol cut
//!                                     ▼
//!                               region treasury ──funds──► cloud burst (recouped from the surge)
//! ```
//!
//! Donors set their own price (0 = altruist, hosts for the love of it); burst nodes price at the
//! provider's rate plus a margin so the galaxy never loses money renting them. A controller will only
//! commit a `Burst` whose projected cost the region treasury can cover from *current* revenue — so the
//! system can't bankrupt itself chasing a spike.

use std::collections::HashMap;

use crate::fleet::RegionHint;
use crate::galaxy::CellId;

/// Credits in base units (1 credit = 1e18 base units, wei-style). Aliased so signatures read clearly.
pub type Credits = u128;

/// One cell's rent ledger: what players have paid into it (via signed channel receipts) and what it has
/// paid out to its host. The difference, minus the protocol cut, is the cell's contribution to its
/// region treasury.
#[derive(Debug, Default, Clone)]
pub struct CellRent {
    /// Cumulative credits received from players in this cell (monotonic; from highest channel receipts).
    pub collected: Credits,
    /// Cumulative credits paid out to the host for cell-minutes served.
    pub paid_host: Credits,
}

impl CellRent {
    /// Unspent rent available to pay the host and feed the treasury.
    pub fn balance(&self) -> Credits {
        self.collected.saturating_sub(self.paid_host)
    }
}

/// Pricing knobs for the whole economy.
#[derive(Debug, Clone, Copy)]
pub struct Tariff {
    /// What a player pays per minute of presence (funds their cell). Tiny — the point is many players.
    pub player_per_min: Credits,
    /// Fraction of cell rent skimmed into the region treasury (the rest pays the host). 0.0..1.0.
    pub protocol_cut: f32,
    /// Margin added on top of a burst provider's price so renting is never a loss.
    pub burst_margin: f32,
    /// Free grace minutes for a new player so you can try the game without a balance (subsidised by the
    /// treasury — generosity that scales because most players are in cheap, near-empty space).
    pub free_grace_min: u32,
}

impl Default for Tariff {
    fn default() -> Self {
        Tariff {
            player_per_min: 1_000_000_000_000, // 1e12 base = 0.000001 credit/min — nominal
            protocol_cut: 0.15,
            burst_margin: 0.25,
            free_grace_min: 10,
        }
    }
}

/// A region's treasury: the pooled protocol cut from every cell in the region, the war chest the
/// autoscaler spends on cloud burst *for that region*. Self-contained per region so a busy region
/// funds its own capacity and never subsidises an idle one.
#[derive(Debug, Default, Clone)]
pub struct RegionTreasury {
    pub balance: Credits,
    /// Outstanding committed burst spend not yet settled (so we don't double-commit the same credits).
    pub committed: Credits,
}

impl RegionTreasury {
    /// Credits free to commit to new burst right now.
    pub fn spendable(&self) -> Credits {
        self.balance.saturating_sub(self.committed)
    }

    /// Can this region afford `minutes` of `nodes` burst at `price_per_node_min` (incl. margin)?
    /// The gate that keeps the galaxy solvent under a spike.
    pub fn can_afford(&self, nodes: usize, minutes: u32, price_per_node_min: Credits, tariff: &Tariff) -> bool {
        self.spendable() >= projected_burst_cost(nodes, minutes, price_per_node_min, tariff)
    }
}

/// Projected cost of a burst, with the safety margin folded in.
pub fn projected_burst_cost(nodes: usize, minutes: u32, price_per_node_min: Credits, tariff: &Tariff) -> Credits {
    let base = price_per_node_min
        .saturating_mul(nodes as u128)
        .saturating_mul(minutes as u128);
    base + (base as f64 * tariff.burst_margin as f64) as u128
}

/// The galaxy's whole economy view: per-cell rent + per-region treasuries. Updated from channel
/// receipts (income) and host settlements + burst settlements (outgo).
#[derive(Debug, Default)]
pub struct Treasury {
    pub tariff: Tariff,
    cells: HashMap<CellId, CellRent>,
    regions: HashMap<RegionHint, RegionTreasury>,
}

impl Treasury {
    pub fn new(tariff: Tariff) -> Self {
        Treasury { tariff, ..Default::default() }
    }

    /// A player's latest signed channel receipt for `cell` raised the collected total to `cumulative`.
    /// Receipts are monotonic, so we take the max — exactly how off-chain payment channels settle.
    pub fn on_receipt(&mut self, cell: CellId, region: &RegionHint, cumulative: Credits) {
        let rent = self.cells.entry(cell).or_default();
        if cumulative > rent.collected {
            let delta = cumulative - rent.collected;
            rent.collected = cumulative;
            // Skim the protocol cut into the region treasury; the rest stays as host-payable rent.
            let cut = (delta as f64 * self.tariff.protocol_cut as f64) as u128;
            self.regions.entry(region.clone()).or_default().balance += cut;
        }
    }

    /// Pay the host for `minutes` of service on `cell` at its asked `price_per_min`, out of collected
    /// rent. Returns what was actually paid (capped by the rent balance — a host is never paid more than
    /// players funded, which is what keeps donors honest about uptime).
    pub fn settle_host(&mut self, cell: CellId, minutes: u32, price_per_min: Credits) -> Credits {
        let rent = self.cells.entry(cell).or_default();
        let owed = price_per_min.saturating_mul(minutes as u128);
        let pay = owed.min(rent.balance());
        rent.paid_host += pay;
        pay
    }

    /// Reserve treasury credits for a burst the controller is about to commit (so two controllers can't
    /// both spend the same war chest). Returns false if the region can't afford it.
    pub fn commit_burst(&mut self, region: &RegionHint, nodes: usize, minutes: u32, price_per_node_min: Credits) -> bool {
        let cost = projected_burst_cost(nodes, minutes, price_per_node_min, &self.tariff);
        let t = self.regions.entry(region.clone()).or_default();
        if t.spendable() >= cost {
            t.committed += cost;
            true
        } else {
            false
        }
    }

    /// Settle a burst once the nodes are torn down: move the actual cost out of the balance and release
    /// the committed reservation.
    pub fn settle_burst(&mut self, region: &RegionHint, reserved: Credits, actual: Credits) {
        let t = self.regions.entry(region.clone()).or_default();
        t.committed = t.committed.saturating_sub(reserved);
        t.balance = t.balance.saturating_sub(actual.min(t.balance));
    }

    pub fn region(&self, region: &RegionHint) -> RegionTreasury {
        self.regions.get(region).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surge_funds_its_own_burst() {
        let mut t = Treasury::new(Tariff::default());
        let region = RegionHint("eu".into());
        // 5,000 players each having paid into their cells funnels a cut into the region war chest.
        for i in 0..5000u32 {
            let cell = CellId { depth: 6, x: i % 64, y: i / 64 };
            t.on_receipt(cell, &region, 50_000_000_000_000); // 5e13 base each
        }
        // The region can now afford to rent burst capacity it could not have at rest.
        let price = 1_000_000_000_000u128; // 1e12/node-min
        assert!(t.region(&region).spendable() > 0);
        assert!(t.commit_burst(&region, 8, 10, price), "the surge pays for its own 8-node, 10-min burst");
    }

    #[test]
    fn host_is_never_paid_more_than_players_funded() {
        let mut t = Treasury::new(Tariff::default());
        let cell = CellId::ROOT;
        t.on_receipt(cell, &RegionHint("eu".into()), 1000);
        let paid = t.settle_host(cell, 99, 1_000_000); // asks far more than collected
        assert!(paid <= 1000, "payout capped by collected rent, got {paid}");
    }
}
