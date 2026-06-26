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

pub mod director;
pub mod leaderboard;
pub mod room;
pub mod shard;
pub mod sim;
pub mod snapshot;
pub mod wire;

use anyhow::{anyhow, Result};
use ce_rs::CeClient;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
}

impl Default for SectorConfig {
    fn default() -> Self {
        SectorConfig {
            sector: "0_0".into(),
            hz: 20,
            advertise_every: 100,
            snapshot_every: 100,
            seal_every: 600,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
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

    ce.subscribe(&in_topic).await?;
    if let Err(e) = ce.advertise_service(&service).await {
        tracing::warn!(error = %e, "initial advertise_service failed (continuing)");
    }

    let hz = cfg.hz.max(1);
    // REPLICATION / FAILOVER: adopt a previous host's latest snapshot for this sector so play resumes
    // from that state instead of an empty sector. Best-effort — a fresh sector starts clean.
    let mut sim = match adopt_latest_snapshot(ce, &cfg.sector).await {
        Ok(Some(s)) => {
            tracing::info!(sector = %cfg.sector, tick = s.tick, ships = s.player_count(), "adopted replicated snapshot on startup");
            s
        }
        Ok(None) => sim::Sim::new(),
        Err(e) => {
            tracing::debug!(error = %e, "snapshot adoption failed; starting fresh");
            sim::Sim::new()
        }
    };
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
                }

                // Inbound mesh messages: authenticated player inputs.
                item = stream.next() => match item {
                    Some(Ok(m)) => {
                        if m.topic != in_topic {
                            continue;
                        }
                        let payload = match m.payload() {
                            Ok(p) => p,
                            Err(e) => { tracing::debug!(error = %e, "bad payload hex"); continue; }
                        };
                        match wire::ClientMsg::decode(&payload) {
                            Ok(msg) => { room::apply_client_msg(&mut sim, &m.from, msg); }
                            Err(e) => tracing::debug!(error = %e, from = %m.from, "undecodable client msg"),
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
