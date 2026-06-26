//! spacegame — an authoritative, **sector-sharded** mesh backend for **CE Spacegame**, a
//! real-time multiplayer space arena, built as a flagship demonstration that CE is a global
//! supercomputer.
//!
//! ## How spacegame is a mesh app
//!
//! A backend instance ([`run_sector`]) connects to the **local CE node** via the `ce-rs` SDK
//! (`CeClient::local()` → `http://127.0.0.1:8844`, which is the libp2p mesh). It hosts one **sector**
//! of the galaxy:
//!
//! 1. subscribes to the sector's input topic `ce-game/spacegame/<sector>/in` and receives player
//!    inputs as authenticated mesh pubsub messages (each carries the verified sender NodeId);
//! 2. runs the authoritative [`sim::Sim`] at a fixed tick — the server is the single source of truth,
//!    so a client can never teleport, exceed max speed, fabricate a kill, or mine a rock it isn't on;
//! 3. broadcasts an authoritative [`wire::Snapshot`] every tick on `ce-game/spacegame/<sector>/state`.
//!
//! ## What it deeply showcases (see the crate README for the capability ↔ code map)
//!
//! * **Distribution** — a sector's authoritative cell is placed on a chosen mesh node via the atlas
//!   and `mesh_deploy` ([`director::choose_host`] / [`director::deploy_sector_cell`]).
//! * **Concurrency** — the galaxy is a grid of sectors, each an independent cell, rendezvous-hashed
//!   across the mesh ([`shard::shard_for`]); many sectors run at once with no coordinator.
//! * **Latency** — placement and the client's host pick are latency-first over the measured netgraph
//!   ([`shard::best_host`] / [`shard::nearest_host`]); clients use interest management
//!   ([`shard::SectorId::neighbors`]) and an SSE push stream, never polling.
//! * **Replication** — each sector snapshots to a content-addressed blob and a standby host fails over
//!   from the latest CID ([`director::replicate_snapshot`] / [`adopt_latest_snapshot`]).
//! * **Consensus** — the galaxy leaderboard is sealed against the PoW chain so final scores are
//!   tamper-proof and verifiable by a stranger ([`director::seal_leaderboard`], [`leaderboard`]).
//! * **Economy** — a player can pay the hosting node per session over a payment channel
//!   ([`director::open_host_channel`]).
//!
//! Players are identified by their CE **NodeId** — free, unspoofable auth.
//!
//! The library half (`sim`, `shard`, `wire`, `room`, `snapshot`, `leaderboard`) is pure and fully
//! unit-tested; this module adds the thin mesh I/O loop that the binary drives.

pub mod aabb;
pub mod build;
pub mod director;
pub mod faction;
pub mod leaderboard;
pub mod physics;
pub mod replication;
pub mod room;
pub mod ruleset;
pub mod shape;
pub mod shard;
pub mod sim;
pub mod snapshot;
pub mod wire;

use anyhow::{anyhow, Result};
use ce_rs::CeClient;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use ruleset::Ruleset;
pub use wire::topics;

/// Configuration for one hosted sector.
#[derive(Debug, Clone)]
pub struct SectorConfig {
    /// Sector token (e.g. `"0_0"`). See [`shard::SectorId::token`].
    pub sector: String,
    /// Authoritative tick rate in Hz.
    pub hz: u32,
    /// How often (ticks) to re-advertise the sector service in the DHT.
    pub advertise_every: u64,
    /// **Replication:** how often (ticks) to snapshot to a content-addressed object. `0` disables.
    pub snapshot_every: u64,
    /// **Consensus:** how often (ticks) to seal this sector's standings against the chain. `0`
    /// disables. (A multi-sector host can instead seal the merged galaxy board centrally; see the
    /// binary.)
    pub seal_every: u64,
    /// **Autoscale:** how often (ticks) a busy host pre-warms neighbouring sectors (`0` disables). The
    /// image deployed for a pre-warmed neighbour is [`SectorConfig::image`].
    pub prewarm_every: u64,
    /// Container image used when this host autoscale-deploys a neighbouring sector cell.
    pub image: String,
}

impl Default for SectorConfig {
    fn default() -> Self {
        SectorConfig {
            sector: "0_0".into(),
            hz: 20,
            advertise_every: 100,
            snapshot_every: 100,
            seal_every: 600,
            prewarm_every: 0,
            image: "ce-net/spacegame:latest".into(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Compare the replicas' state proofs collected for one checkpoint `tick` and log the verdict — the
/// anti-cheat agreement step. `votes` maps node id -> state hash. If a strict majority agrees and this
/// node's hash differs, this node has diverged (a bug, a desync, or — if it were the cheat — itself)
/// and should resync; if another node is the lone dissenter, it is the suspect. Pure logging here; the
/// existing replica machinery handles the actual takeover/resync.
fn judge_proofs(host_id: &str, region: &str, tick: u64, votes: &std::collections::HashMap<String, u64>) {
    if votes.len() < 2 {
        return; // need at least two replicas to cross-check
    }
    let proofs: Vec<replication::StateProof> = votes
        .iter()
        .map(|(node, &hash)| replication::StateProof { region: region.to_string(), node: node.clone(), tick, hash })
        .collect();
    let a = replication::agree(&proofs);
    if !a.has_quorum {
        tracing::warn!(region, tick, replicas = votes.len(), "no replica quorum on state hash (split) — cannot accept a result this checkpoint");
        return;
    }
    if a.dissent.is_empty() {
        tracing::debug!(region, tick, replicas = a.agree.len(), "all replicas agree on the world hash (verified)");
    } else if a.dissent.iter().any(|d| d == host_id) {
        tracing::warn!(region, tick, "THIS node diverged from the replica quorum — resyncing from the agreed snapshot");
    } else {
        tracing::warn!(region, tick, suspects = ?a.dissent, "replica(s) disagree with the quorum — faulty or cheating, will be re-synced/excluded");
    }
}

/// Where to re-admit a transiting ship into *this* sector when its destination neighbour has no live
/// host (solo / not-yet-autoscaled world): place it just inside the edge it tried to leave, so it
/// bounces back into play instead of vanishing. `t.ship.x/y` are in the neighbour's local frame; we
/// flip them back to this sector's boundary based on the crossing direction.
fn readmit_coords(here: shard::SectorId, t: &sim::Transit) -> (f32, f32) {
    use sim::{SECTOR_SIZE, SHIP_R};
    let dsx = t.to.sx - here.sx;
    let dsy = t.to.sy - here.sy;
    let mut x = t.ship.x.clamp(0.0, SECTOR_SIZE);
    let mut y = t.ship.y.clamp(0.0, SECTOR_SIZE);
    if dsx == 1 {
        x = SECTOR_SIZE - SHIP_R - 1.0;
    } else if dsx == -1 {
        x = SHIP_R + 1.0;
    }
    if dsy == 1 {
        y = SECTOR_SIZE - SHIP_R - 1.0;
    } else if dsy == -1 {
        y = SHIP_R + 1.0;
    }
    (x, y)
}

/// Host one sector until `shutdown` resolves.
///
/// The real authoritative loop:
/// - subscribe to the sector's `/in` topic and advertise its service for discovery;
/// - drain the node's inbound message stream, applying each authenticated input to the [`sim::Sim`];
/// - on a fixed timer, tick the simulation and publish a [`wire::Snapshot`] on the `/state` topic;
/// - periodically replicate a snapshot (failover) and seal the leaderboard (consensus).
///
/// The message stream is reconnected with backoff, and snapshot/advertise/seal failures are logged
/// but never crash the loop — a transient mesh hiccup must not take a sector down.
pub async fn run_sector(
    ce: &CeClient,
    cfg: SectorConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    use futures_util::StreamExt as _;

    let host_id = ce.status().await.map(|s| s.node_id).unwrap_or_else(|_| "local".to_string());
    let in_topic = topics::input(&cfg.sector);
    let state_topic = topics::state(&cfg.sector);
    let service = topics::service(&cfg.sector);
    let transit_topic = director::transit_topic(&cfg.sector);
    let ruleset_topic = director::ruleset_topic();
    let replica_topic = director::replica_topic(&cfg.sector);
    let sector_id = shard::SectorId::parse(&cfg.sector).unwrap_or(shard::SectorId::new(0, 0));

    ce.subscribe(&in_topic).await?;
    // INFINITE MAP: receive ships arriving from neighbouring sectors.
    if let Err(e) = ce.subscribe(&transit_topic).await {
        tracing::debug!(error = %e, "transit subscribe failed (continuing)");
    }
    // HOT RELOAD: listen for live ruleset updates pushed to the whole galaxy.
    if let Err(e) = ce.subscribe(&ruleset_topic).await {
        tracing::debug!(error = %e, "ruleset subscribe failed (continuing)");
    }
    // FAULT TOLERANCE: heartbeat with the region's other replica holders.
    if let Err(e) = ce.subscribe(&replica_topic).await {
        tracing::debug!(error = %e, "replica subscribe failed (continuing)");
    }
    // ANTI-CHEAT: exchange deterministic state-hash proofs with the other replicas.
    let proof_topic = director::proof_topic(&cfg.sector);
    if let Err(e) = ce.subscribe(&proof_topic).await {
        tracing::debug!(error = %e, "proof subscribe failed (continuing)");
    }
    // Collected proofs per checkpoint tick: tick -> (node -> state hash).
    let mut proofs: std::collections::HashMap<u64, std::collections::HashMap<String, u64>> =
        std::collections::HashMap::new();
    // How often (ticks) every replica hashes the SAME checkpoint and publishes a proof.
    let proof_every: u64 = 100;
    // The local view of who holds a high-precision replica of this region, for K-replica takeover.
    let mut replicas =
        replication::ReplicaSet::new(&cfg.sector, &host_id, replication::ReplicationConstraint::default(), 0);
    // How often (ticks) to heartbeat + run the replication-constraint maintenance.
    let replica_every: u64 = 40;
    let replica_timeout: u64 = 120;
    if let Err(e) = ce.advertise_service(&service).await {
        tracing::warn!(error = %e, "initial advertise_service failed (continuing)");
    }

    let hz = cfg.hz.max(1);

    // HOT RELOAD: start from the latest published ruleset if one exists, else the built-in default.
    let mut rules: Arc<Ruleset> = match director::adopt_latest_ruleset(ce).await {
        Ok(Some(r)) => {
            tracing::info!(version = r.version, label = %r.label, "adopted live ruleset on startup");
            Arc::new(r)
        }
        _ => Arc::new(Ruleset::builtin()),
    };

    // REPLICATION / FAILOVER: adopt a previous host's latest snapshot for this sector so play resumes
    // from that state instead of an empty sector. Best-effort — a fresh sector starts clean.
    let mut sim = match adopt_latest_snapshot(ce, &cfg.sector).await {
        Ok(Some(s)) => {
            tracing::info!(sector = %cfg.sector, tick = s.tick, ships = s.player_count(), "adopted replicated snapshot on startup");
            s
        }
        Ok(None) => sim::Sim::for_sector(sector_id, rules.clone()),
        Err(e) => {
            tracing::debug!(error = %e, "snapshot adoption failed; starting fresh");
            sim::Sim::for_sector(sector_id, rules.clone())
        }
    };
    // Make sure the restored/fresh sim runs the current sector + live ruleset.
    sim.sector = sector_id;
    sim.apply_ruleset(rules.clone());
    let mut tick_timer = tokio::time::interval(Duration::from_millis((1000 / hz as u64).max(1)));
    tick_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut backoff = Duration::from_millis(250);
    tokio::pin!(shutdown);

    tracing::info!(sector = %cfg.sector, host = %host_id, hz, in_topic = %in_topic, state_topic = %state_topic, "spacegame sector hosting started");

    loop {
        let stream = match ce.messages_stream().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "messages_stream open failed; backing off");
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        backoff = Duration::from_millis(250);
        tokio::pin!(stream);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!(sector = %cfg.sector, "shutdown requested; stopping sector");
                    return Ok(());
                }

                // Fixed-rate authoritative tick + snapshot broadcast.
                _ = tick_timer.tick() => {
                    sim.tick(1.0);

                    // INFINITE MAP: hand off ships that crossed an edge to their destination sector. If
                    // no host runs that neighbour yet, re-admit the ship at our edge so a solo / not-yet-
                    // scaled world never loses a player (the autoscaler pre-warms neighbours separately).
                    for t in sim.take_transits() {
                        match ce.find_service(&topics::service(&t.to.token())).await {
                            Ok(hosts) if !hosts.is_empty() => {
                                let ce2 = ce.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = director::publish_transit(&ce2, &t).await {
                                        tracing::debug!(error = %e, "transit publish failed (transient)");
                                    }
                                });
                            }
                            _ => {
                                let mut snap = t.ship.clone();
                                let (x, y) = readmit_coords(sector_id, &t);
                                snap.x = x;
                                snap.y = y;
                                sim.accept_transit(snap);
                            }
                        }
                    }

                    let snap = room::build_snapshot(&sim, &cfg.sector, &host_id, now_ms());
                    match snap.encode() {
                        Ok(bytes) => {
                            if let Err(e) = ce.publish(&state_topic, &bytes).await {
                                tracing::debug!(error = %e, "snapshot publish failed (transient)");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "snapshot encode failed"),
                    }

                    if sim.tick.is_multiple_of(cfg.advertise_every)
                        && let Err(e) = ce.advertise_service(&service).await
                    {
                        tracing::debug!(error = %e, "re-advertise failed (transient)");
                    }

                    // REPLICATION: snapshot off the tick on a clone so blob I/O never stalls the loop.
                    if cfg.snapshot_every > 0 && sim.tick.is_multiple_of(cfg.snapshot_every) {
                        let ce2 = ce.clone();
                        let sector2 = cfg.sector.clone();
                        let sim2 = sim.clone();
                        tokio::spawn(async move {
                            if let Err(e) = director::replicate_snapshot(&ce2, &sector2, &sim2).await {
                                tracing::debug!(error = %e, "snapshot replication failed (transient)");
                            }
                        });
                    }

                    // CONSENSUS: periodically seal this sector's standings against the PoW chain.
                    if cfg.seal_every > 0 && sim.tick.is_multiple_of(cfg.seal_every) && sim.player_count() > 0 {
                        let ce2 = ce.clone();
                        let board = director::board_for_sector(&sim);
                        let host2 = host_id.clone();
                        tokio::spawn(async move {
                            if let Err(e) = director::seal_leaderboard(&ce2, &board, &host2).await {
                                tracing::debug!(error = %e, "leaderboard seal failed (transient)");
                            }
                        });
                    }

                    // AUTOSCALE: a busy sector pre-warms its neighbours so the seamless map keeps
                    // spreading load across the mesh instead of crowding one cell.
                    if cfg.prewarm_every > 0
                        && sim.tick.is_multiple_of(cfg.prewarm_every)
                        && sim.player_count() >= director::PREWARM_THRESHOLD
                    {
                        let ce2 = ce.clone();
                        let sector2 = cfg.sector.clone();
                        let image2 = cfg.image.clone();
                        tokio::spawn(async move {
                            if let Err(e) = director::prewarm_neighbors(
                                &ce2, &sector2, &image2, 3600, ce_rs::Amount::from_credits(100), None,
                            )
                            .await
                            {
                                tracing::debug!(error = %e, "autoscale pre-warm failed (transient)");
                            }
                        });
                    }

                    // FAULT TOLERANCE: heartbeat, detect dropped replicas, and keep K high-precision
                    // replicas alive on the nodes of nearby players. The decision is deterministic
                    // (replication::ReplicaSet::plan) so every holder agrees on takeover.
                    if sim.tick.is_multiple_of(replica_every) {
                        // Heartbeat that we hold this region.
                        {
                            let ce2 = ce.clone();
                            let region = cfg.sector.clone();
                            let me = host_id.clone();
                            let t = sim.tick;
                            tokio::spawn(async move {
                                let _ = director::publish_heartbeat(&ce2, &region, &me, t).await;
                            });
                        }
                        replicas.observe(&host_id, sim.tick);
                        replicas.expire(sim.tick, replica_timeout);
                        let candidates = director::gather_replica_candidates(ce).await.unwrap_or_default();
                        let plan = replicas.plan(&candidates);
                        if plan.promote.is_some() || !plan.copy_to.is_empty() || !plan.drop.is_empty() {
                            if let Some(p) = &plan.promote {
                                tracing::info!(region = %cfg.sector, new_primary = %p, "replica takeover: promoting new primary");
                            }
                            if !plan.copy_to.is_empty() {
                                tracing::info!(region = %cfg.sector, targets = ?plan.copy_to, "replicating high-precision map to restore K replicas");
                                // Ensure a fresh snapshot exists for the new replicas to adopt.
                                let ce2 = ce.clone();
                                let sector2 = cfg.sector.clone();
                                let sim2 = sim.clone();
                                tokio::spawn(async move {
                                    let _ = director::replicate_snapshot(&ce2, &sector2, &sim2).await;
                                });
                            }
                            replicas.apply(&plan, sim.tick);
                        }
                    }

                    // ANTI-CHEAT: at each checkpoint, publish our deterministic state hash and judge
                    // it against the other replicas' — honest replicas agree; a cheat is outvoted.
                    if sim.tick.is_multiple_of(proof_every) {
                        let hash = sim.state_hash();
                        let t = sim.tick;
                        proofs.entry(t).or_default().insert(host_id.clone(), hash);
                        {
                            let ce2 = ce.clone();
                            let region = cfg.sector.clone();
                            let me = host_id.clone();
                            tokio::spawn(async move {
                                let _ = director::publish_state_proof(&ce2, &region, &me, t, hash).await;
                            });
                        }
                        if let Some(v) = proofs.get(&t) {
                            judge_proofs(&host_id, &cfg.sector, t, v);
                        }
                        // Bound the buffer: keep only recent checkpoints.
                        proofs.retain(|&k, _| t.saturating_sub(k) <= proof_every * 4);
                    }
                }

                // Inbound mesh messages: player inputs, neighbour transits, and live ruleset updates.
                item = stream.next() => match item {
                    Some(Ok(m)) => {
                        let payload = match m.payload() {
                            Ok(p) => p,
                            Err(e) => { tracing::debug!(error = %e, "bad payload hex"); continue; }
                        };
                        if m.topic == in_topic {
                            match wire::ClientMsg::decode(&payload) {
                                Ok(msg) => { room::apply_client_msg(&mut sim, &m.from, msg); }
                                Err(e) => tracing::debug!(error = %e, from = %m.from, "undecodable client msg"),
                            }
                        } else if m.topic == transit_topic {
                            // INFINITE MAP: a ship arrived from a neighbouring sector.
                            match serde_json::from_slice::<sim::Transit>(&payload) {
                                Ok(t) if t.to == sector_id => sim.accept_transit(t.ship),
                                Ok(_) => {}
                                Err(e) => tracing::debug!(error = %e, "undecodable transit"),
                            }
                        } else if m.topic == replica_topic {
                            // FAULT TOLERANCE: another node holds a replica of this region. Track it so
                            // a drop is detected and the set can fail over.
                            if let Ok(hb) = serde_json::from_slice::<director::Heartbeat>(&payload)
                                && hb.node != host_id
                            {
                                replicas.admit(&hb.node, sim.tick);
                                replicas.observe(&hb.node, sim.tick);
                            }
                        } else if m.topic == proof_topic {
                            // ANTI-CHEAT: another replica's state-hash claim for a checkpoint.
                            if let Ok(p) = serde_json::from_slice::<replication::StateProof>(&payload)
                                && p.node != host_id
                            {
                                proofs.entry(p.tick).or_default().insert(p.node, p.hash);
                                if let Some(v) = proofs.get(&p.tick) {
                                    judge_proofs(&host_id, &cfg.sector, p.tick, v);
                                }
                            }
                        } else if m.topic == ruleset_topic {
                            // HOT RELOAD: a newer ruleset was published — fetch and apply it live.
                            if let Ok(ann) = serde_json::from_slice::<director::RulesetAnnounce>(&payload)
                                && ann.version > sim.rules.version
                            {
                                match director::fetch_ruleset(ce, &ann.cid).await {
                                    Ok(r) => {
                                        let v = r.version;
                                        rules = Arc::new(r);
                                        sim.apply_ruleset(rules.clone());
                                        tracing::info!(version = v, sector = %cfg.sector, "hot-applied live ruleset (no restart)");
                                    }
                                    Err(e) => tracing::warn!(error = %e, "failed to fetch announced ruleset"),
                                }
                            }
                        }
                    }
                    Some(Err(e)) => { tracing::warn!(error = %e, "message stream error; reconnecting"); break; }
                    None => { tracing::warn!("message stream ended; reconnecting"); break; }
                }
            }
        }
    }
}

/// Discover the NodeId of a backend currently hosting `sector`, if any (via the DHT service record).
pub async fn discover_host(ce: &CeClient, sector: &str) -> Result<Option<String>> {
    let providers = ce.find_service(&topics::service(sector)).await?;
    Ok(providers.into_iter().next())
}

/// **Replication / failover:** find the freshest replicated snapshot for `sector` and restore its
/// [`Sim`], or `None` if none is available right now. Subscribes to the sector's snapshot topic, waits
/// a bounded window for the announcement with the highest tick, then fetches and restores that object.
pub async fn adopt_latest_snapshot(ce: &CeClient, sector: &str) -> Result<Option<sim::Sim>> {
    use futures_util::StreamExt as _;

    let topic = director::snapshot_topic(sector);
    ce.subscribe(&topic).await?;

    let mut best: Option<director::SnapshotAnnounce> = None;
    let listen = async {
        if let Ok(stream) = ce.messages_stream().await {
            tokio::pin!(stream);
            while let Some(Ok(m)) = stream.next().await {
                if m.topic != topic {
                    continue;
                }
                if let Ok(bytes) = m.payload()
                    && let Ok(ann) = serde_json::from_slice::<director::SnapshotAnnounce>(&bytes)
                    && best.as_ref().map(|b| ann.tick > b.tick).unwrap_or(true)
                {
                    best = Some(ann);
                }
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(6), listen).await;

    match best {
        Some(ann) => Ok(Some(director::restore_snapshot(ce, &ann.cid).await?)),
        None => Ok(None),
    }
}

/// Convenience: connect to the local node and host `sector` forever (until ctrl-c).
pub async fn host_local(sector: &str, hz: u32) -> Result<()> {
    let ce = CeClient::local();
    if !ce.health().await.unwrap_or(false) {
        return Err(anyhow!(
            "local CE node is not reachable at {} — start it with `ce start`",
            ce.base_url()
        ));
    }
    let cfg = SectorConfig { sector: sector.to_string(), hz, ..Default::default() };
    run_sector(&ce, cfg, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_names_are_sector_keyed() {
        assert_eq!(topics::input("0_0"), "ce-game/spacegame/0_0/in");
        assert_eq!(topics::state("1_2"), "ce-game/spacegame/1_2/state");
        assert_eq!(topics::service("1_2"), "ce-game/spacegame/1_2");
        assert_ne!(topics::input("0_0"), topics::input("0_1"));
    }

    #[test]
    fn default_config_is_sane() {
        let c = SectorConfig::default();
        assert_eq!(c.sector, "0_0");
        assert_eq!(c.hz, 20);
        assert!(c.advertise_every > 0);
    }

    #[test]
    fn now_ms_is_nonzero() {
        assert!(now_ms() > 0);
    }
}
