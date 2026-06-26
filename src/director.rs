//! Director — the thin **mesh I/O** layer that turns the pure decision modules
//! ([`crate::shard`], [`crate::snapshot`], [`crate::leaderboard`]) into real CE SDK calls.
//!
//! Everything testable lives in those pure modules; this module is deliberately small and is the one
//! place that touches the network, so the showcase capabilities map to concrete `ce-rs` calls:
//!
//! | Showcase | What the director does | SDK call |
//! |---|---|---|
//! | **Distribution** | place a sector's authoritative cell on a chosen node | [`CeClient::mesh_deploy`] |
//! | **Latency** | gather capacity + latency, score, pick the closest capable host | [`CeClient::atlas`], [`CeClient::netgraph`] |
//! | **Concurrency** | rendezvous-hash a sector to a host so the galaxy spreads across the mesh | (pure: [`crate::shard::shard_for`]) |
//! | **Replication** | snapshot a sector to a content-addressed object, advertise + announce its CID | [`CeClient::put_object`], [`CeClient::advertise_service`], [`CeClient::publish`] |
//! | **Consensus** | seal the galaxy leaderboard's CID against the PoW beacon, broadcast it | [`CeClient::put_object`], [`CeClient::beacon`], [`CeClient::publish`] |
//! | **Economy** | open a payment channel paying the sector host per session, sign rising receipts | [`CeClient::channel_open`], [`CeClient::sign_receipt`] |
//!
//! These functions are `async` and hit the local node, so they are not unit-tested here (the
//! deterministic logic they wrap is). Failures are returned to the caller, which logs and continues —
//! a transient mesh hiccup must never take a sector down.

use anyhow::{Context, Result};
use ce_rs::{Amount, BidSpec, CeClient, Receipt};
use std::collections::HashMap;

use crate::leaderboard::{Commitment, Leaderboard};
use crate::replication::{ReplicaCandidate, StateProof};
use crate::ruleset::Ruleset;
use crate::shard::{best_host, nearest_host, shard_for, HostCandidate, SectorId};
use crate::sim::{Sim, Transit};
use crate::snapshot::SectorSnapshot;
use crate::wire::topics;

/// Galaxy id the leaderboard is sealed under. A deployment hosting one galaxy uses one id.
pub const GALAXY: &str = "main";

/// Minimum capacity the director requires of a host before placing a sector cell there.
pub const MIN_CORES: u32 = 1;
pub const MIN_MEM_MB: u32 = 128;

/// Build the [`HostCandidate`] set from the live node view: the capacity atlas (cores/mem/load and
/// the `spacegame` self-tag that signals "I can host a sector cell") joined with the latency graph
/// (RTT per peer). This is the **latency + distribution** input — `best_host`/`nearest_host`/
/// `shard_for` all consume what this returns.
pub async fn gather_candidates(ce: &CeClient) -> Result<Vec<HostCandidate>> {
    let atlas = ce.atlas().await.context("atlas")?;
    let rtt: HashMap<String, f64> = match ce.netgraph().await {
        Ok(edges) => edges.into_iter().map(|e| (e.peer, e.rtt_ms)).collect(),
        Err(e) => {
            tracing::debug!(error = %e, "netgraph unavailable; placing without measured latency");
            HashMap::new()
        }
    };

    Ok(atlas
        .into_iter()
        .map(|a| HostCandidate {
            rtt_ms: rtt.get(&a.node_id).copied(),
            // A node advertises hostability with the `spacegame` tag; absent that we still consider
            // any general-purpose node (docker/linux) capable, so a fresh mesh can still host.
            can_host: a.has_tag("spacegame") || a.has_tag("docker") || a.has_tag("linux") || a.tags.is_empty(),
            node_id: a.node_id,
            cpu_cores: a.cpu_cores,
            mem_mb: a.mem_mb,
            running_jobs: a.running_jobs,
        })
        .collect())
}

/// Decide where a sector's authoritative cell **should** run, latency-first, over the live mesh.
/// Returns the chosen host NodeId, or `None` if no candidate is capable (the caller then hosts the
/// sector locally as a fallback).
pub async fn choose_host(ce: &CeClient, _sector: &str) -> Result<Option<String>> {
    let cands = gather_candidates(ce).await?;
    Ok(best_host(&cands, MIN_CORES, MIN_MEM_MB).map(|c| c.node_id.clone()))
}

/// Coordinator-free shard assignment: of the currently-capable hosts, which one owns `sector` by
/// rendezvous hash. Every node computes the same answer, so a busy galaxy's sectors spread evenly
/// across the mesh with no scheduler (the **concurrency** story). `None` if no host is capable.
pub async fn shard_owner(ce: &CeClient, sector: &str) -> Result<Option<String>> {
    let cands = gather_candidates(ce).await?;
    let ids: Vec<String> = cands
        .into_iter()
        .filter(|c| c.is_capable(MIN_CORES, MIN_MEM_MB))
        .map(|c| c.node_id)
        .collect();
    Ok(shard_for(sector, &ids).map(|s| s.to_string()))
}

/// **Distribution:** place the cell that hosts `sector` on `node_id` over the mesh (`mesh_deploy`).
/// The deployed cell runs `spacegame host --sector <sector>` — the very binary this crate builds
/// — so the authoritative simulation of that region of space runs on the chosen node. Returns the
/// host-assigned job id.
pub async fn deploy_sector_cell(
    ce: &CeClient,
    node_id: &str,
    sector: &str,
    image: &str,
    duration_secs: u64,
    bid: Amount,
    grant: Option<&str>,
) -> Result<String> {
    let spec = BidSpec {
        image: image.to_string(),
        cmd: vec!["spacegame".into(), "host".into(), "--sector".into(), sector.to_string()],
        cpu_cores: MIN_CORES,
        mem_mb: MIN_MEM_MB as u64,
        duration_secs,
        bid,
    };
    let job_id = ce
        .mesh_deploy(node_id, &spec, grant)
        .await
        .with_context(|| format!("mesh_deploy sector {sector} on {node_id}"))?;
    tracing::info!(sector, node_id, job_id, "deployed authoritative sector cell on chosen host");
    Ok(job_id)
}

/// A client's latency-aware host pick: of the nodes currently advertising `sector` in the DHT, the
/// **nearest** one to this client (lowest measured RTT). `None` if nobody hosts the sector.
pub async fn nearest_sector_host(ce: &CeClient, sector: &str) -> Result<Option<String>> {
    let hosts = ce.find_service(&topics::service(sector)).await.context("find_service")?;
    if hosts.is_empty() {
        return Ok(None);
    }
    let rtt: HashMap<String, f64> = match ce.netgraph().await {
        Ok(edges) => edges.into_iter().map(|e| (e.peer, e.rtt_ms)).collect(),
        Err(_) => HashMap::new(),
    };
    Ok(nearest_host(&hosts, &rtt).map(|s| s.to_string()))
}

/// **Replication:** snapshot a sector's authoritative `sim` to a content-addressed object, advertise
/// the sector's snapshot service, and announce the CID + tick so a recovering host can fail over.
/// Returns the snapshot CID. Every node that fetches the object caches it, so each is a CDN edge —
/// "content-addressed = every node is a CDN edge".
pub async fn replicate_snapshot(ce: &CeClient, sector: &str, sim: &Sim) -> Result<String> {
    let snap = SectorSnapshot::capture(sim);
    let bytes = snap.encode().context("encode snapshot")?;
    let cid = ce.put_object(&bytes).await.context("put_object snapshot")?;
    if let Err(e) = ce.advertise_service(&snapshot_service(sector)).await {
        tracing::debug!(error = %e, sector, "snapshot-service advertise failed (continuing)");
    }
    let ann = SnapshotAnnounce { cid: cid.clone(), tick: snap.tick };
    if let Ok(b) = serde_json::to_vec(&ann)
        && let Err(e) = ce.publish(&snapshot_topic(sector), &b).await
    {
        tracing::debug!(error = %e, sector, "snapshot CID announce failed (continuing)");
    }
    tracing::debug!(sector, cid, tick = snap.tick, "replicated authoritative sector snapshot to blob store");
    Ok(cid)
}

/// Restore a sector's [`Sim`] from a replicated snapshot object by CID — what a host that has just
/// taken over a sector calls so play resumes from the last replicated state with at most one
/// snapshot-interval of loss, instead of an empty sector.
pub async fn restore_snapshot(ce: &CeClient, cid: &str) -> Result<Sim> {
    let bytes = ce.get_object(cid).await.with_context(|| format!("get_object {cid}"))?;
    let snap = SectorSnapshot::decode(&bytes).context("decode snapshot")?;
    tracing::info!(cid, tick = snap.tick, ships = snap.ships.len(), "restored sector from replicated snapshot");
    Ok(snap.restore())
}

/// The DHT service name under which holders of a sector's latest replicated snapshot advertise.
pub fn snapshot_service(sector: &str) -> String {
    format!("{}/snap", topics::service(sector))
}

/// The pubsub topic on which a host announces each freshly-replicated snapshot's CID.
pub fn snapshot_topic(sector: &str) -> String {
    format!("{}/snap", topics::service(sector))
}

/// An announcement that a fresh snapshot exists. Published on [`snapshot_topic`] after each
/// [`replicate_snapshot`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SnapshotAnnounce {
    pub cid: String,
    pub tick: u64,
}

/// **Consensus:** seal the galaxy's current standings against the PoW chain and broadcast the
/// commitment. `sim` is one sector's authoritative state; for a single-process multi-sector host this
/// is the union of its sectors' kills (the binary folds them with [`Leaderboard::merge`] before
/// sealing). Returns the published commitment.
pub async fn seal_leaderboard(ce: &CeClient, board: &Leaderboard, host: &str) -> Result<Commitment> {
    let bytes = board.encode().context("encode leaderboard")?;
    let cid = ce.put_object(&bytes).await.context("put_object leaderboard")?;
    let beacon = ce.beacon().await.context("beacon")?;
    let commitment = Commitment::seal(board, &cid, beacon.height, &beacon.hash, host);
    let frame = commitment.encode().context("encode commitment")?;
    ce.publish(&seal_topic(), &frame).await.context("publish seal")?;
    tracing::info!(
        galaxy = %board.galaxy,
        cid,
        height = beacon.height,
        digest = commitment.digest,
        "sealed tamper-proof galaxy leaderboard against the PoW chain"
    );
    Ok(commitment)
}

/// Reduce one sector's sim to its `(id, name, kills)` standings as a [`Leaderboard`] — the per-shard
/// contribution the binary folds across sectors before sealing.
pub fn board_for_sector(sim: &Sim) -> Leaderboard {
    // Only human players are ranked; NPC fleet ships (owner = Some) are excluded.
    let rows = sim
        .ships
        .iter()
        .filter(|(_, s)| s.owner.is_none())
        .map(|(id, s)| (id.clone(), s.name.clone(), s.kills));
    Leaderboard::from_scores(GALAXY, sim.tick, rows)
}

/// The galaxy-wide leaderboard-seal topic: where a host broadcasts [`Commitment`]s and clients listen
/// to render the verified, chain-anchored standings.
pub fn seal_topic() -> String {
    format!("ce-game/{}/{}/seal", topics::GAME, GALAXY)
}

/// **Economy / marketplace:** open a payment channel paying the sector `host` for hosting this
/// player's session, then sign an initial receipt. Per-session billing for compute is the marketplace
/// angle — a player funds the node that simulates their region of space. Returns the channel id and
/// the first signed receipt; the caller signs rising receipts as the session continues (see
/// [`pay_host_tick`]). Best-effort: a node without channels just surfaces the error to the caller.
pub async fn open_host_channel(
    ce: &CeClient,
    host: &str,
    capacity: Amount,
    per_tick: Amount,
) -> Result<(String, Receipt)> {
    let channel_id = ce.channel_open(host, capacity, 0).await.context("channel_open")?;
    let receipt = ce.sign_receipt(&channel_id, host, per_tick).await.context("sign first receipt")?;
    tracing::info!(host, channel_id, "opened payment channel to the sector host (per-session billing)");
    Ok((channel_id, receipt))
}

/// Sign the next rising receipt on an open host channel — call periodically to pay for ongoing
/// hosting. `cumulative` is the monotonic total authorized so far; the host redeems the highest one.
pub async fn pay_host_tick(ce: &CeClient, channel_id: &str, host: &str, cumulative: Amount) -> Result<Receipt> {
    ce.sign_receipt(channel_id, host, cumulative).await.context("sign receipt")
}

// ----------------------------------------------------------------------------------------------
// INFINITE MAP: cross-sector transit delivery.
// ----------------------------------------------------------------------------------------------

/// The pubsub topic on which a host receives ships arriving from neighbouring sectors. A host
/// subscribes to its own sector's transit topic; a neighbour publishes a [`Transit`] here when one of
/// its ships crosses the shared edge.
pub fn transit_topic(sector: &str) -> String {
    format!("{}/transit", topics::service(sector))
}

/// Deliver a ship that left this sector to its destination sector over the mesh. Whichever host is
/// running the destination sector is subscribed to that sector's [`transit_topic`] and will
/// [`accept_transit`](crate::sim::Sim::accept_transit) it. Best-effort: a transient publish failure is
/// surfaced so the caller can apply its solo-wrap fallback.
pub async fn publish_transit(ce: &CeClient, t: &Transit) -> Result<()> {
    let topic = transit_topic(&t.to.token());
    let bytes = serde_json::to_vec(t).context("encode transit")?;
    ce.publish(&topic, &bytes).await.with_context(|| format!("publish transit to {}", t.to.token()))?;
    tracing::debug!(to = %t.to.token(), ship = %t.ship.id, "handed ship to neighbouring sector");
    Ok(())
}

// ----------------------------------------------------------------------------------------------
// HOT RELOAD: distribute a new ruleset across the mesh, live.
// ----------------------------------------------------------------------------------------------

/// The galaxy-wide config topic on which the live ruleset's CID + version is announced. Every sector
/// host and every client subscribes; a higher version wins, so a balance/shader edit reaches the whole
/// mesh instantly with no restart.
pub fn ruleset_topic() -> String {
    format!("ce-game/{}/{}/ruleset", topics::GAME, GALAXY)
}

/// The DHT service holders of the latest ruleset advertise, so a node that joins late can pull the
/// current ruleset without waiting for the next announcement.
pub fn ruleset_service() -> String {
    format!("ce-game/{}/{}/ruleset", topics::GAME, GALAXY)
}

/// An announcement that a new ruleset is live: its content-addressed CID and its version. Published on
/// [`ruleset_topic`] after a designer pushes an edit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RulesetAnnounce {
    pub cid: String,
    pub version: u64,
}

/// **Hot reload (publish side):** store the new ruleset as a content-addressed object, advertise the
/// ruleset service, and announce its CID + version on the config topic. Every live host re-tunes and
/// every live client re-fetches shaders/weapon stats — instantly, no restart. Returns the CID.
pub async fn publish_ruleset(ce: &CeClient, ruleset: &Ruleset) -> Result<String> {
    ruleset.validate().map_err(|e| anyhow::anyhow!("refusing to publish invalid ruleset: {e}"))?;
    let bytes = ruleset.encode().context("encode ruleset")?;
    let cid = ce.put_object(&bytes).await.context("put_object ruleset")?;
    if let Err(e) = ce.advertise_service(&ruleset_service()).await {
        tracing::debug!(error = %e, "ruleset-service advertise failed (continuing)");
    }
    let ann = RulesetAnnounce { cid: cid.clone(), version: ruleset.version };
    let frame = serde_json::to_vec(&ann).context("encode ruleset announce")?;
    ce.publish(&ruleset_topic(), &frame).await.context("publish ruleset announce")?;
    tracing::info!(version = ruleset.version, cid, label = %ruleset.label, "published live ruleset to the mesh (hot reload)");
    Ok(cid)
}

/// **Hot reload (fetch side):** fetch and decode a ruleset object by CID (validating it). Called by a
/// host or client when it sees a higher version announced.
pub async fn fetch_ruleset(ce: &CeClient, cid: &str) -> Result<Ruleset> {
    let bytes = ce.get_object(cid).await.with_context(|| format!("get_object ruleset {cid}"))?;
    Ruleset::decode(&bytes).context("decode ruleset")
}

/// Pull the current live ruleset on startup, if any holder advertises one — so a freshly started host
/// joins already running the latest balance, not the built-in default. `None` if none is published.
pub async fn adopt_latest_ruleset(ce: &CeClient) -> Result<Option<Ruleset>> {
    use futures_util::StreamExt as _;
    let topic = ruleset_topic();
    ce.subscribe(&topic).await?;
    let mut best: Option<RulesetAnnounce> = None;
    let listen = async {
        if let Ok(stream) = ce.messages_stream().await {
            tokio::pin!(stream);
            while let Some(Ok(m)) = stream.next().await {
                if m.topic != topic {
                    continue;
                }
                if let Ok(bytes) = m.payload()
                    && let Ok(ann) = serde_json::from_slice::<RulesetAnnounce>(&bytes)
                    && best.as_ref().map(|b| ann.version > b.version).unwrap_or(true)
                {
                    best = Some(ann);
                }
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), listen).await;
    match best {
        Some(ann) => Ok(Some(fetch_ruleset(ce, &ann.cid).await?)),
        None => Ok(None),
    }
}

// ----------------------------------------------------------------------------------------------
// AUTOSCALE: spread load across the mesh as a region fills up.
// ----------------------------------------------------------------------------------------------

/// A sector is "under pressure" once it carries this many ships — the cue to pre-warm its neighbours so
/// players spilling across an edge land on a host that is already simulating that region.
pub const PREWARM_THRESHOLD: usize = 80;

/// **Autoscale:** when a sector is busy, proactively place its still-unhosted neighbouring sectors on
/// the best mesh hosts, so the seamless infinite map keeps spreading load instead of piling everyone
/// onto one cell. Returns the sectors that were newly placed. Idempotent: a neighbour that already has
/// a live host is left alone. Safe to call periodically from a busy host.
pub async fn prewarm_neighbors(
    ce: &CeClient,
    sector: &str,
    image: &str,
    duration_secs: u64,
    bid: Amount,
    grant: Option<&str>,
) -> Result<Vec<String>> {
    let Some(here) = SectorId::parse(sector) else { return Ok(vec![]) };
    let cands = gather_candidates(ce).await?;
    let mut placed = Vec::new();
    for n in here.neighbors() {
        if n == here {
            continue;
        }
        let tok = n.token();
        // Already hosted? Skip.
        if !ce.find_service(&topics::service(&tok)).await.unwrap_or_default().is_empty() {
            continue;
        }
        let Some(host) = best_host(&cands, MIN_CORES, MIN_MEM_MB).map(|c| c.node_id.clone()) else {
            continue;
        };
        match deploy_sector_cell(ce, &host, &tok, image, duration_secs, bid, grant).await {
            Ok(job) => {
                tracing::info!(sector = %tok, host, job, "autoscale: pre-warmed neighbouring sector");
                placed.push(tok);
            }
            Err(e) => tracing::debug!(error = %e, sector = %tok, "autoscale pre-warm failed (continuing)"),
        }
    }
    Ok(placed)
}

// ----------------------------------------------------------------------------------------------
// FAULT TOLERANCE: proximity-replica heartbeats + candidate gathering.
// ----------------------------------------------------------------------------------------------

/// The pubsub topic a region's replica holders heartbeat on, so the set can detect a dropped replica
/// and agree on takeover. Region = sector token.
pub fn replica_topic(region: &str) -> String {
    format!("{}/replica", topics::service(region))
}

/// A liveness heartbeat from a node that holds a high-precision replica of `region`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Heartbeat {
    pub region: String,
    pub node: String,
    pub tick: u64,
}

/// Announce that this node holds a live replica of `region` (one heartbeat). Other replica holders
/// [`observe`](crate::replication::ReplicaSet::observe) it; a missed heartbeat is how a drop is found.
pub async fn publish_heartbeat(ce: &CeClient, region: &str, node: &str, tick: u64) -> Result<()> {
    let hb = Heartbeat { region: region.to_string(), node: node.to_string(), tick };
    let bytes = serde_json::to_vec(&hb).context("encode heartbeat")?;
    ce.publish(&replica_topic(region), &bytes).await.context("publish heartbeat")?;
    Ok(())
}

/// The pubsub topic on which replicas publish their per-checkpoint [`StateProof`] (the deterministic
/// state hash), so the set can detect a host that cheated or desynced by comparing hashes.
pub fn proof_topic(region: &str) -> String {
    format!("{}/proof", topics::service(region))
}

/// Publish this replica's state hash for `tick` — its claim about the authoritative world. Honest
/// replicas of the same sector publish the same hash; a cheater's differs and is outvoted
/// ([`crate::replication::agree`]).
pub async fn publish_state_proof(ce: &CeClient, region: &str, node: &str, tick: u64, hash: u64) -> Result<()> {
    let p = StateProof { region: region.to_string(), node: node.to_string(), tick, hash };
    let bytes = serde_json::to_vec(&p).context("encode state proof")?;
    ce.publish(&proof_topic(region), &bytes).await.context("publish state proof")?;
    Ok(())
}

/// Build the candidate set for replica placement from the live mesh view. In-game proximity is the
/// strongest signal but is not visible at the mesh layer, so we approximate it with measured latency
/// (a low-RTT node is usually a node whose player is engaging the same region); the host can refine
/// `in_game_dist` from actual player positions before calling [`ReplicaSet::plan`].
pub async fn gather_replica_candidates(ce: &CeClient) -> Result<Vec<ReplicaCandidate>> {
    let cands = gather_candidates(ce).await?;
    Ok(cands
        .into_iter()
        .map(|c| ReplicaCandidate {
            in_game_dist: c.rtt_ms.unwrap_or(150.0) as f32,
            rtt_ms: c.rtt_ms,
            free_cores: c.cpu_cores,
            alive: true,
            node_id: c.node_id,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The director's logic is wired SDK I/O; the deterministic decisions it composes are tested in
    // their own modules. These pin the pure string/topic helpers clients and standby hosts must agree
    // on exactly, plus the per-sector board reduction.

    #[test]
    fn replica_topic_is_stable() {
        assert_eq!(replica_topic("3_4"), "ce-game/spacegame/3_4/replica");
    }

    #[test]
    fn transit_and_ruleset_topics_are_stable() {
        assert_eq!(transit_topic("1_0"), "ce-game/spacegame/1_0/transit");
        assert_eq!(ruleset_topic(), "ce-game/spacegame/main/ruleset");
    }

    #[test]
    fn seal_topic_is_galaxy_scoped() {
        assert_eq!(seal_topic(), "ce-game/spacegame/main/seal");
    }

    #[test]
    fn snapshot_service_is_distinct_from_host_service() {
        assert_eq!(snapshot_service("0_0"), "ce-game/spacegame/0_0/snap");
        assert_ne!(snapshot_service("0_0"), topics::service("0_0"));
    }

    #[test]
    fn board_for_sector_reduces_kills() {
        let mut sim = Sim::new();
        sim.join("nodeA", "Ace", 1);
        sim.join("nodeB", "Bee", 2);
        sim.ships.get_mut("nodeA").unwrap().kills = 5;
        sim.ships.get_mut("nodeB").unwrap().kills = 2;
        let board = board_for_sector(&sim);
        assert_eq!(board.galaxy, GALAXY);
        assert_eq!(board.champion().unwrap().id, "nodeA");
        assert_eq!(board.champion().unwrap().kills, 5);
    }
}
