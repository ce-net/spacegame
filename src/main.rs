//! game-spacegame binary — host CE Spacegame **sectors** over the mesh, or **place** a sector's
//! authoritative cell on the best mesh node.
//!
//! Connects to the local CE node (`http://127.0.0.1:8844`) which is the libp2p mesh.
//!
//! ```text
//! ce start                                     # the local node must be running
//! game-spacegame host --sector 0_0             # host the origin sector here
//! game-spacegame host --sector 0_0 --sector 1_0 --sector 0_1
//!                                              # host several sectors (independent cells) from one process
//! game-spacegame place --sector 1_0 --image ce-net/game-spacegame:latest
//!                                              # atlas-guided: pick the best host and deploy the cell there
//! game-spacegame shard --sector 1_0            # which node rendezvous-hash assigns this sector to
//! game-spacegame nearest --sector 1_0          # nearest live host of this sector (client view)
//! ```

use anyhow::{anyhow, Result};
use ce_rs::{Amount, CeClient};
use clap::{Parser, Subcommand};
use game_spacegame::{director, run_sector, SectorConfig};

#[derive(Parser, Debug)]
#[command(name = "game-spacegame", about = "Authoritative, sector-sharded mesh backend for CE Spacegame")]
struct Args {
    /// Override the local node base URL (defaults to the SDK's 127.0.0.1:8844).
    #[arg(long, env = "CE_BASE_URL", global = true)]
    base_url: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Back-compat: `game-spacegame --sector X` with no subcommand hosts sector(s) locally.
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
    },
    /// DISTRIBUTION + LATENCY: pick the best mesh host (lowest latency, capable) and deploy the
    /// sector's authoritative cell there over the mesh.
    Place {
        #[arg(long, default_value = "0_0")]
        sector: String,
        /// Container image carrying the `game-spacegame` binary to run as the cell.
        #[arg(long, default_value = "ce-net/game-spacegame:latest")]
        image: String,
        #[arg(long, default_value_t = 3600)]
        duration_secs: u64,
        #[arg(long, default_value_t = 100)]
        bid: u64,
        /// Optional CE capability token authorizing the deploy on the target host.
        #[arg(long)]
        grant: Option<String>,
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
            host_sectors(&ce, sectors, args.hz).await
        }
        Some(Cmd::Host { sectors, hz }) => host_sectors(&ce, sectors, hz).await,
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

/// Host every requested sector concurrently; ctrl-c shuts all of them down. Each sector runs its own
/// authoritative loop (independent shard), replicates snapshots, and seals its standings.
async fn host_sectors(ce: &CeClient, sectors: Vec<String>, hz: u32) -> Result<()> {
    let shutdown = tokio_shutdown();
    let mut handles = Vec::new();
    for sector in sectors {
        let ce = ce.clone();
        let mut sd = shutdown.clone();
        handles.push(tokio::spawn(async move {
            let cfg = SectorConfig { sector: sector.clone(), hz, ..Default::default() };
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

/// A broadcast that fires once on ctrl-c, so every sector task can await its own receiver.
fn tokio_shutdown() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(true);
    });
    rx
}
