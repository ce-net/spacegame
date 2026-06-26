//! spacegame binary — host CE Spacegame **sectors** over the mesh, **place** a sector's authoritative
//! cell on the best mesh node, or **hot-reload the live game ruleset** across the whole galaxy.
//!
//! Connects to the local CE node (`http://127.0.0.1:8844`) which is the libp2p mesh.
//!
//! ```text
//! ce start                                     # the local node must be running
//! spacegame host --sector 0_0                  # host the origin sector here
//! spacegame host --sector 0_0 --sector 1_0     # host several sectors (independent cells) at once
//! spacegame host --sector 0_0 --autoscale      # pre-warm neighbours as the sector fills up
//! spacegame host --sector 0_0 --ruleset live.json
//!                                              # host AND watch a ruleset file: every save hot-reloads
//!                                              # the live game for every host and client, no restart
//! spacegame place --sector 1_0 --image ce-net/spacegame:latest
//!                                              # atlas-guided: pick the best host and deploy the cell
//! spacegame ruleset init live.json             # write the built-in ruleset as an editable template
//! spacegame ruleset push live.json             # push an edited ruleset live to the whole mesh, now
//! spacegame shard --sector 1_0                 # which node rendezvous-hash assigns this sector to
//! spacegame nearest --sector 1_0               # nearest live host of this sector (client view)
//! ```

use anyhow::{anyhow, Result};
use ce_rs::{Amount, CeClient};
use clap::{Parser, Subcommand};
use spacegame::ruleset::Ruleset;
use spacegame::{director, run_sector, SectorConfig};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "spacegame", about = "Authoritative, sector-sharded, hot-reloadable mesh backend for CE Spacegame")]
struct Args {
    /// Override the local node base URL (defaults to the SDK's 127.0.0.1:8844).
    #[arg(long, env = "CE_BASE_URL", global = true)]
    base_url: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Back-compat: `spacegame --sector X` with no subcommand hosts sector(s) locally.
    #[arg(long = "sector", global = true)]
    sectors: Vec<String>,

    /// Authoritative tick rate (Hz). Used by `host`.
    #[arg(long, default_value_t = 20, global = true)]
    hz: u32,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Host one or more sectors locally (authoritative simulation runs in this process). Each sector
    /// is an independent concurrent cell.
    Host {
        /// Sector(s) to host. Repeat to host several from one process.
        #[arg(long = "sector", default_values_t = vec!["0_0".to_string()])]
        sectors: Vec<String>,
        /// Authoritative tick rate (Hz).
        #[arg(long, default_value_t = 20)]
        hz: u32,
        /// HOT RELOAD: watch this ruleset file; every time it changes, push it live to the whole mesh
        /// so the running game re-tunes instantly (no restart, no dropped players).
        #[arg(long)]
        ruleset: Option<PathBuf>,
        /// AUTOSCALE: pre-warm neighbouring sectors when this one gets busy, spreading load.
        #[arg(long, default_value_t = false)]
        autoscale: bool,
        /// Image used when autoscale deploys a neighbouring sector cell.
        #[arg(long, default_value = "ce-net/spacegame:latest")]
        image: String,
    },
    /// DISTRIBUTION + LATENCY: pick the best mesh host (lowest latency, capable) and deploy the
    /// sector's authoritative cell there over the mesh.
    Place {
        #[arg(long, default_value = "0_0")]
        sector: String,
        /// Container image carrying the `spacegame` binary to run as the cell.
        #[arg(long, default_value = "ce-net/spacegame:latest")]
        image: String,
        #[arg(long, default_value_t = 3600)]
        duration_secs: u64,
        #[arg(long, default_value_t = 100)]
        bid: u64,
        /// Optional CE capability token authorizing the deploy on the target host.
        #[arg(long)]
        grant: Option<String>,
    },
    /// HOT RELOAD: manage the live game ruleset (weapons, tech tree, tunables, shaders).
    Ruleset {
        #[command(subcommand)]
        cmd: RulesetCmd,
    },
    /// CONCURRENCY: print the node that rendezvous-hash assigns this sector to (coordinator-free).
    Shard {
        #[arg(long, default_value = "0_0")]
        sector: String,
    },
    /// LATENCY (client view): print the nearest live host of this sector.
    Nearest {
        #[arg(long, default_value = "0_0")]
        sector: String,
    },
}

#[derive(Subcommand, Debug)]
enum RulesetCmd {
    /// Write the built-in ruleset to a file as an editable starting template.
    Init {
        #[arg(default_value = "ruleset.json")]
        file: PathBuf,
    },
    /// Validate a ruleset file and push it live to the whole mesh, right now. Every running host
    /// re-tunes and every client re-fetches shaders/weapon stats — no restart.
    Push {
        #[arg(default_value = "ruleset.json")]
        file: PathBuf,
        /// Force a specific version (default: keep the file's version, but never lower than live).
        #[arg(long)]
        version: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let ce = match &args.base_url {
        Some(url) => CeClient::new(url.clone()),
        None => CeClient::local(),
    };

    if !ce.health().await.unwrap_or(false) {
        return Err(anyhow!(
            "local CE node is not reachable at {} — start it with `ce start` first",
            ce.base_url()
        ));
    }

    match args.cmd {
        None => {
            let sectors = if args.sectors.is_empty() { vec!["0_0".to_string()] } else { args.sectors.clone() };
            host_sectors(&ce, sectors, args.hz, None, false, "ce-net/spacegame:latest".into()).await
        }
        Some(Cmd::Host { sectors, hz, ruleset, autoscale, image }) => {
            host_sectors(&ce, sectors, hz, ruleset, autoscale, image).await
        }
        Some(Cmd::Place { sector, image, duration_secs, bid, grant }) => {
            let host = director::choose_host(&ce, &sector)
                .await?
                .ok_or_else(|| anyhow!("no capable host found in the atlas for sector {sector}"))?;
            let job_id = director::deploy_sector_cell(
                &ce,
                &host,
                &sector,
                &image,
                duration_secs,
                Amount::from_credits(bid),
                grant.as_deref(),
            )
            .await?;
            println!("placed sector {sector} on host {host} (job {job_id})");
            Ok(())
        }
        Some(Cmd::Ruleset { cmd }) => ruleset_cmd(&ce, cmd).await,
        Some(Cmd::Shard { sector }) => {
            match director::shard_owner(&ce, &sector).await? {
                Some(owner) => println!("sector {sector} -> shard owner {owner}"),
                None => println!("sector {sector}: no capable host in the atlas"),
            }
            Ok(())
        }
        Some(Cmd::Nearest { sector }) => {
            match director::nearest_sector_host(&ce, &sector).await? {
                Some(host) => println!("sector {sector}: nearest live host {host}"),
                None => println!("sector {sector}: no live host advertising the sector"),
            }
            Ok(())
        }
    }
}

/// Host every requested sector concurrently; ctrl-c shuts all of them down. Optionally watch a ruleset
/// file and hot-push it to the mesh on every change.
async fn host_sectors(
    ce: &CeClient,
    sectors: Vec<String>,
    hz: u32,
    ruleset: Option<PathBuf>,
    autoscale: bool,
    image: String,
) -> Result<()> {
    let shutdown = tokio_shutdown();

    // HOT RELOAD: a background task that watches the ruleset file and pushes edits to the whole mesh.
    if let Some(path) = ruleset.clone() {
        let ce2 = ce.clone();
        let mut sd = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = watch_ruleset(&ce2, &path, &mut sd).await {
                tracing::error!(error = %e, "ruleset watcher exited");
            }
        });
    }

    let mut handles = Vec::new();
    for sector in sectors {
        let ce = ce.clone();
        let mut sd = shutdown.clone();
        let image = image.clone();
        handles.push(tokio::spawn(async move {
            let cfg = SectorConfig {
                sector: sector.clone(),
                hz,
                prewarm_every: if autoscale { 200 } else { 0 },
                image,
                ..Default::default()
            };
            if let Err(e) = run_sector(&ce, cfg, async move { sd.changed().await.ok(); }).await {
                tracing::error!(sector = %sector, error = %e, "sector exited with error");
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

/// Manage the live ruleset: `init` writes a template, `push` validates and broadcasts it live.
async fn ruleset_cmd(ce: &CeClient, cmd: RulesetCmd) -> Result<()> {
    match cmd {
        RulesetCmd::Init { file } => {
            let bytes = Ruleset::builtin().encode()?;
            std::fs::write(&file, &bytes)?;
            println!("wrote built-in ruleset template to {} — edit it, then `spacegame ruleset push {}`", file.display(), file.display());
            Ok(())
        }
        RulesetCmd::Push { file, version } => {
            let bytes = std::fs::read(&file)?;
            let mut r = Ruleset::decode(&bytes)?; // validates
            if let Some(v) = version {
                r.version = v;
            }
            let cid = director::publish_ruleset(ce, &r).await?;
            println!("pushed ruleset v{} ({}) live to the mesh: {cid}", r.version, r.label);
            println!("every running host re-tuned and every client will hot-apply it — no restart.");
            Ok(())
        }
    }
}

/// Watch a ruleset file; whenever its contents change, push it live to the mesh. The version is
/// auto-advanced past the currently-live one so a plain save always propagates (a designer need not
/// remember to bump it). Polls the file (no extra dependency) every 700ms.
async fn watch_ruleset(
    ce: &CeClient,
    path: &PathBuf,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut last_fingerprint: u64 = 0;
    let mut last_version: u64 = 0;
    // Seed last_version from whatever is already live, so the first push wins.
    if let Ok(Some(live)) = director::adopt_latest_ruleset(ce).await {
        last_version = live.version;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(700));
    tracing::info!(file = %path.display(), "watching ruleset file for live hot reload");
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tick.tick() => {
                let Ok(bytes) = std::fs::read(path) else { continue };
                let mut r = match Ruleset::decode(&bytes) {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!(error = %e, "ruleset file invalid; not pushing"); continue; }
                };
                let fp = r.fingerprint();
                if fp == last_fingerprint {
                    continue; // no change
                }
                last_fingerprint = fp;
                // Always advance past the live version so the edit takes effect even if unbumped.
                last_version = last_version.max(r.version).max(last_version + 1);
                r.version = last_version;
                match director::publish_ruleset(ce, &r).await {
                    Ok(cid) => tracing::info!(version = r.version, cid, "hot-reloaded ruleset from file change"),
                    Err(e) => tracing::warn!(error = %e, "failed to push ruleset change"),
                }
            }
        }
    }
}

/// A broadcast that fires once on ctrl-c, so every sector task can await its own receiver.
fn tokio_shutdown() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(true);
    });
    rx
}
