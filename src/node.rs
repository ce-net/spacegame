//! `node` — the **live adaptive-galaxy daemon**. This is the mesh I/O that makes the pure
//! [`crate::galaxy`] / [`crate::autoscale`] / [`crate::orchestrator`] / [`crate::fleet`] design actually
//! run on real `ce-rs` calls. One process per machine; a thousand of them self-organise into one galaxy
//! with no central server.
//!
//! Each node concurrently:
//!
//! 1. **Hosts** the authoritative cells the galaxy assigns it. A hosted cell is the proven
//!    [`crate::run_sector`] loop keyed on the cell's quadtree token (`depth.x.y`) — same tick / publish /
//!    replicate / hot-reload / anti-cheat path, now reporting its measured [`crate::galaxy::CellLoad`].
//!    The quadtree decides *which* cells exist and *who* hosts them; each leaf is an independent arena.
//! 2. **Controls** — the leaderless scaling loop. Each epoch it folds load (shared sink for local cells +
//!    the `/load` gossip for remote ones) and the atlas into one view, asks the [`Autoscaler`] for the
//!    action batch for the cells it *owns* (rendezvous HRW over the live controller set), and executes
//!    each as a real mesh op: split a hot leaf into four children (host the ones assigned here in-process,
//!    `mesh_deploy` the rest onto other nodes) and gossip the [`ShapeCommit`] so the whole mesh converges.
//! 3. **Gateways** (optional) — advertises itself as a browser entry point and heartbeats the control
//!    plane so the gateway directory the browser fetches stays current.
//!
//! Safety on a weak single box: splits are bounded by a soft max depth and a local-cell cap, so a 2-core
//! relay subdivides a hot genesis cell a few times across its own cores rather than fragmenting forever.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ce_rs::{Amount, CeClient};
use futures_util::StreamExt as _;
use tokio::sync::watch;

use crate::autoscale::{AutoscalePolicy, Autoscaler, ScaleAction};
use crate::director;
use crate::fleet::{CapacitySource, Fleet, HostNode, NodeId, RegionHint};
use crate::galaxy::{CellId, CellLoad, Galaxy, MAX_DEPTH};
use crate::galaxymap::MapModel;
use crate::galaxywire::{topics as gtopics, ControlView, Heartbeat, LoadFrame, ShapeCommit, ShapeOp};
use crate::gateway::{Gateway, GatewayDirectory};
use crate::orchestrator::Orchestrator;
use crate::{run_sector, SectorConfig};

/// How a node participates in the galaxy. Defaults make `spacegame node` a one-liner.
#[derive(Clone, Debug)]
pub struct NodeOpts {
    /// Accept browser sessions / advertise as a gateway entry point.
    pub gateway: bool,
    /// Coarse region label for placement (latency clustering is a follow-up; this seeds it).
    pub region: String,
    /// Authoritative tick rate of each hosted cell.
    pub hz: u32,
    /// Image `mesh_deploy` runs as a remote child-cell host when placing a cell on another node.
    pub cell_image: String,
    /// Capability token authorizing remote deploys (`None` = self-rooted / open mesh).
    pub grant: Option<String>,
    /// Seconds per control epoch (one observe→decide→act pass).
    pub epoch_secs: u64,
    /// Soft cap on cells this node hosts in-process before it stops splitting locally.
    pub max_local_cells: u32,
    /// Soft quadtree depth cap so a single weak host can't fragment a hot cell forever.
    pub max_depth: u8,
    /// Player count that makes a cell "hot" (overrides the default for small/demo deployments so the
    /// adaptive behaviour is reachable at low population). `None` keeps the calibrated default.
    pub split_players: Option<u32>,
}

impl Default for NodeOpts {
    fn default() -> Self {
        NodeOpts {
            gateway: false,
            region: RegionHint::UNKNOWN.into(),
            hz: 20,
            cell_image: "ce-net/spacegame:latest".into(),
            grant: None,
            epoch_secs: 3,
            max_local_cells: 8,
            max_depth: 3,
            split_players: None,
        }
    }
}

/// Shared, content-addressed load table: hosted cells write their measured load here (no round-trip) and
/// `/load` gossip from remote hosts is folded in too, so the controller decides over one unified view.
type LoadTable = Arc<Mutex<HashMap<CellId, CellLoad>>>;

/// The running daemon state. Single-owner (the control loop), so no locks beyond the shared `loads`.
struct Node {
    ce: CeClient,
    me: NodeId,
    opts: NodeOpts,
    galaxy: Galaxy,
    control: ControlView,
    autoscaler: Autoscaler,
    map: MapModel,
    loads: LoadTable,
    /// Cells hosted in this process → the shutdown switch for each host task.
    local: HashMap<CellId, watch::Sender<bool>>,
    /// Our position in the shape-commit chain (the CID of the last commit we authored/applied).
    head: String,
    epoch: u64,
}

impl Node {
    /// Start (or no-op if already running) an in-process authoritative host for `cell`, reusing the
    /// proven sector loop keyed on the cell token, wired to report load into the shared table.
    fn host_cell(&mut self, cell: CellId) {
        if self.local.contains_key(&cell) {
            return;
        }
        let (tx, mut rx) = watch::channel(false);
        let ce = self.ce.clone();
        // The genesis (root) cell hosts on the legacy "0_0" play token so the shipped browser/native
        // client connects unchanged; every other cell hosts on its quadtree token. Either way the cell
        // reports its load under its canonical CellId so the controller can decide split/merge.
        let play_token = if cell == CellId::ROOT { "0_0".to_string() } else { cell.token() };
        let cfg = SectorConfig {
            sector: play_token,
            hz: self.opts.hz,
            load_sink: Some(self.loads.clone()),
            cell: Some(cell),
            ..Default::default()
        };
        tokio::spawn(async move {
            if let Err(e) = run_sector(&ce, cfg, async move { let _ = rx.changed().await; }).await {
                tracing::warn!(error = %e, "hosted cell exited with error");
            }
        });
        self.local.insert(cell, tx);
        tracing::info!(cell = %cell.token(), "now hosting cell in-process");
    }

    /// Stop hosting `cell` here (it stopped being a leaf, or moved). Best-effort.
    fn unhost_cell(&mut self, cell: &CellId) {
        if let Some(tx) = self.local.remove(cell) {
            let _ = tx.send(true);
            if let Ok(mut m) = self.loads.lock() {
                m.remove(cell);
            }
            tracing::info!(cell = %cell.token(), "stopped hosting cell");
        }
    }

    /// The live fleet view, built from the capacity atlas. Donor nodes appear the moment they advertise;
    /// we never call the stubbed `Fleet::observe` — this is the real, simple atlas→hosts fold.
    async fn build_fleet(&self) -> Fleet {
        let atlas = self.ce.atlas().await.unwrap_or_default();
        let mut hosts: Vec<HostNode> = atlas
            .iter()
            .map(|a| {
                // Cell budget from cores (each cell wants ~one core's worth of headroom), with a floor.
                let budget = a.cpu_cores.max(1) * 2;
                HostNode {
                    node_id: a.node_id.clone(),
                    source: CapacitySource::Donated,
                    cores: a.cpu_cores,
                    mem_mb: a.mem_mb as u64,
                    region: RegionHint(self.opts.region.clone()),
                    cells: a.running_jobs,
                    cell_budget: budget,
                    load: (a.running_jobs as f32 / budget as f32).min(1.0),
                    price_per_min: 0,
                    last_seen: a.last_seen_secs,
                }
            })
            .collect();
        // Make sure *this* node is in the fleet even if the atlas hasn't surfaced it yet — it can always
        // host the cells it's already running plus a little more.
        if !hosts.iter().any(|h| h.node_id == self.me) {
            let local = self.local.len() as u32;
            hosts.push(HostNode {
                node_id: self.me.clone(),
                source: CapacitySource::Donated,
                cores: 2,
                mem_mb: 4096,
                region: RegionHint(self.opts.region.clone()),
                cells: local,
                cell_budget: self.opts.max_local_cells,
                load: (local as f32 / self.opts.max_local_cells as f32).min(1.0),
                price_per_min: 0,
                last_seen: 0,
            });
        }
        Fleet { hosts, providers: vec![], cell_boot: self.opts.cell_image.clone() }
    }

    /// Publish our liveness + role heartbeat so other controllers see us in the rendezvous set and
    /// gateways/hosts can be discovered without an atlas round-trip.
    async fn heartbeat(&self) {
        let local = self.local.len() as u32;
        let hb = Heartbeat {
            node: self.me.clone(),
            hosts: true,
            controls: true,
            gateway: self.opts.gateway,
            free_cell_slots: self.opts.max_local_cells.saturating_sub(local),
            load: (local as f32 / self.opts.max_local_cells as f32).min(1.0),
            region: self.opts.region.clone(),
            epoch: self.epoch,
        };
        if let Ok(b) = serde_json::to_vec(&hb) {
            let _ = self.ce.publish(gtopics::CONTROL, &b).await;
        }
    }

    /// Gossip a shape commit so the whole mesh applies the same reshape. Chains on our `head`.
    async fn commit_shape(&mut self, op: ShapeOp) {
        let prev = self.head.clone();
        // A cheap content id over the op + chain position; the node's blob store would mint the real one,
        // but a stable hash is enough to dedup and order here.
        let cid = format!("sc{:016x}", fnv(&format!("{prev}|{}|{:?}", self.epoch, op)));
        let commit = ShapeCommit { cid: cid.clone(), prev, controller: self.me.clone(), epoch: self.epoch, op };
        if let Ok(b) = serde_json::to_vec(&commit) {
            let _ = self.ce.publish(gtopics::SHAPE, &b).await;
        }
        self.head = cid;
    }

    /// Carry out one autoscaler decision as real mesh effects.
    async fn execute(&mut self, action: ScaleAction) {
        match action {
            ScaleAction::Split { cell, place_on } => {
                // Guard a weak single host from fragmenting forever: respect the depth cap, and if the
                // children would all land back on us while we're already saturated, hold instead.
                if cell.depth + 1 > self.opts.max_depth.min(MAX_DEPTH) {
                    tracing::debug!(cell = %cell.token(), "split suppressed: at max depth");
                    return;
                }
                let children = cell.children();
                if children.len() < 4 {
                    return; // quadtree floor; never split past it
                }
                let all_local = place_on.iter().all(|h| *h == self.me);
                if all_local && self.local.len() as u32 + 4 > self.opts.max_local_cells + 1 {
                    tracing::debug!(cell = %cell.token(), "split suppressed: local cell cap reached and no other host");
                    return;
                }
                for (child, host) in children.iter().zip(place_on.iter()) {
                    if *host == self.me {
                        self.host_cell(*child);
                    } else if let Err(e) = director::deploy_sector_cell(
                        &self.ce,
                        host,
                        &child.token(),
                        &self.opts.cell_image,
                        3600,
                        Amount::from_credits(100),
                        self.opts.grant.as_deref(),
                    )
                    .await
                    {
                        tracing::debug!(error = %e, host = %host, child = %child.token(), "remote child deploy failed; hosting locally as fallback");
                        self.host_cell(*child);
                    }
                }
                self.unhost_cell(&cell);
                self.galaxy.split(cell);
                let op = ShapeOp::Split {
                    cell,
                    children: [children[0], children[1], children[2], children[3]],
                    place_on: place_on.clone(),
                    from_snapshot: String::new(),
                };
                self.map.on_shape(self.epoch, &op);
                self.commit_shape(op).await;
                tracing::info!(cell = %cell.token(), "SPLIT into 4 children");
            }
            ScaleAction::Merge { parent, place_on } => {
                for child in parent.children() {
                    self.unhost_cell(&child);
                }
                if place_on == self.me {
                    self.host_cell(parent);
                }
                self.galaxy.merge(parent);
                let op = ShapeOp::Merge {
                    parent,
                    from_snapshots: [String::new(), String::new(), String::new(), String::new()],
                    place_on: place_on.clone(),
                };
                self.map.on_shape(self.epoch, &op);
                self.commit_shape(op).await;
                tracing::info!(parent = %parent.token(), "MERGE four siblings");
            }
            ScaleAction::Place { cell, node } => {
                if node == self.me {
                    self.host_cell(cell);
                } else if let Err(e) = director::deploy_sector_cell(
                    &self.ce, &node, &cell.token(), &self.opts.cell_image, 3600, Amount::from_credits(100), self.opts.grant.as_deref(),
                )
                .await
                {
                    tracing::debug!(error = %e, "place deploy failed; hosting locally");
                    self.host_cell(cell);
                }
                let op = ShapeOp::Place { cell, node: node.clone(), from_snapshot: None };
                self.map.on_shape(self.epoch, &op);
                self.commit_shape(op).await;
            }
            ScaleAction::Migrate { cell, from, to } => {
                // Snapshot handoff is carried by the replication layer; here we just (re)home + gossip.
                let _ = director::deploy_sector_cell(
                    &self.ce, &to, &cell.token(), &self.opts.cell_image, 3600, Amount::from_credits(100), self.opts.grant.as_deref(),
                )
                .await;
                let op = ShapeOp::Migrate { cell, from: from.clone(), to: to.clone(), snapshot: String::new() };
                self.map.on_shape(self.epoch, &op);
                self.commit_shape(op).await;
            }
            ScaleAction::Burst { count, region } => {
                // Cloud burst needs a wired CloudProvider (see cloud_hetzner); without one we can only
                // record the unmet demand. Donor capacity still absorbs growth.
                tracing::info!(count, region = %region.0, "burst requested (no cloud provider wired; relying on donor capacity)");
            }
            ScaleAction::Retire { node } => {
                tracing::info!(node = %node, "retire requested (burst lifecycle; no-op without a provider)");
            }
        }
    }

    /// Apply a shape commit heard from another controller, keeping our galaxy + map + hosting in sync and
    /// hosting any resulting cell this node is assigned.
    fn apply_remote_commit(&mut self, c: &ShapeCommit) {
        match &c.op {
            ShapeOp::Split { cell, place_on, children, .. } => {
                self.galaxy.split(*cell);
                self.unhost_cell(cell);
                for (child, host) in children.iter().zip(place_on.iter()) {
                    if *host == self.me {
                        self.host_cell(*child);
                    }
                }
            }
            ShapeOp::Merge { parent, place_on, .. } => {
                self.galaxy.merge(*parent);
                for child in parent.children() {
                    self.unhost_cell(&child);
                }
                if *place_on == self.me {
                    self.host_cell(*parent);
                }
            }
            ShapeOp::Place { cell, node, .. } => {
                if *node == self.me {
                    self.host_cell(*cell);
                }
            }
            ShapeOp::Migrate { cell, to, from, .. } => {
                if *to == self.me {
                    self.host_cell(*cell);
                } else if *from == self.me {
                    self.unhost_cell(cell);
                }
            }
        }
        self.map.on_shape(c.epoch, &c.op);
        self.head = c.cid.clone();
    }

    /// One control pass: observe → decide (for owned cells) → act → heartbeat.
    async fn control_pass(&mut self) {
        self.epoch += 1;
        self.control.prune(self.epoch, 6);

        // Make sure we're subscribed to the `/load` topic of every current leaf (remote cells gossip
        // their load there; local cells write the shared table directly, but subscribing is harmless).
        let leaves: Vec<CellId> = self.galaxy.leaves().copied().collect();
        for leaf in &leaves {
            let _ = self.ce.subscribe(&gtopics::load(&leaf.token())).await;
        }

        let loads_snapshot: HashMap<CellId, CellLoad> =
            self.loads.lock().map(|m| m.clone()).unwrap_or_default();

        let fleet = self.build_fleet().await;

        // The live controller set the rendezvous owner runs over — folded heartbeats plus ourselves
        // (our own heartbeat isn't delivered back to us).
        let mut controllers = self.control.controllers();
        if !controllers.iter().any(|c| c == &self.me) {
            controllers.push(self.me.clone());
        }
        let me = self.me.clone();
        let owns = move |c: &CellId| Orchestrator::owner(c, &controllers).as_deref() == Some(me.as_str());

        let actions =
            self.autoscaler.decide(self.epoch, &self.galaxy, &loads_snapshot, &fleet, &owns);
        for action in actions {
            self.execute(action).await;
        }

        self.heartbeat().await;

        // Keep the local map fresh from the load table so `spacegame galaxy` and logs reflect reality.
        for (cell, load) in loads_snapshot.iter() {
            self.map.on_load(LoadFrame { cell: *cell, host: self.me.clone(), load: *load });
        }
        let s = self.map.summary();
        tracing::debug!(
            epoch = self.epoch,
            leaves = s.leaf_cells,
            players = s.players,
            hosts = s.host_nodes,
            local_cells = self.local.len(),
            "galaxy control pass"
        );
    }
}

/// FNV-1a over a string — a cheap stable id for shape-commit chaining/dedup (no crypto needed here).
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

/// Run the adaptive-galaxy node daemon until `shutdown` resolves. This is the entire server side of the
/// planet-scale game in one call: it hosts the cells assigned to it, runs the leaderless controller, and
/// (if `gateway`) advertises itself as a browser entry point.
pub async fn run_node(
    ce: &CeClient,
    opts: NodeOpts,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let me = ce.status().await.map(|s| s.node_id).unwrap_or_else(|_| "local".to_string());

    // The control topics: shape (split/merge commits) and control (heartbeats).
    ce.subscribe(gtopics::SHAPE).await?;
    if let Err(e) = ce.subscribe(gtopics::CONTROL).await {
        tracing::debug!(error = %e, "control subscribe failed (continuing)");
    }

    // Calibrate the autoscaler, optionally lowering the split threshold for a small/demo deployment.
    let mut policy = AutoscalePolicy::default();
    if let Some(p) = opts.split_players {
        policy.scale.split_players = p;
    }

    let mut node = Node {
        ce: ce.clone(),
        me: me.clone(),
        opts: opts.clone(),
        galaxy: Galaxy::genesis(),
        control: ControlView::default(),
        autoscaler: Autoscaler::new(policy),
        map: MapModel::new(),
        loads: Arc::new(Mutex::new(HashMap::new())),
        local: HashMap::new(),
        head: String::new(),
        epoch: 0,
    };

    // GENESIS: exactly one public node must anchor the root cell so it always exists and browsers have an
    // arena to join. We make that the GATEWAY (the public entry point). Donor nodes (no `--gateway`) host
    // ONLY the cells the galaxy assigns them via splits/placements they own — so two nodes never
    // split-brain the genesis cell into divergent authoritative sims. Run exactly one gateway as the
    // genesis anchor (or, with several gateways, the ROOT rendezvous owner among them).
    if opts.gateway {
        node.host_cell(CellId::ROOT);
        node.commit_shape(ShapeOp::Place { cell: CellId::ROOT, node: me.clone(), from_snapshot: None }).await;
    }

    if opts.gateway {
        // Advertise as a browser gateway entry point so the directory builder / peers can find us.
        if let Err(e) = ce.advertise_service("ce-game/spacegame/gateway").await {
            tracing::debug!(error = %e, "gateway advertise failed (continuing)");
        }
    }

    let mut epoch_timer = tokio::time::interval(Duration::from_secs(opts.epoch_secs.max(1)));
    epoch_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut backoff = Duration::from_millis(250);
    tokio::pin!(shutdown);

    tracing::info!(node = %me, gateway = opts.gateway, region = %opts.region, max_depth = opts.max_depth, "spacegame adaptive-galaxy node started (genesis hosted)");

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
                    tracing::info!("shutdown requested; stopping all hosted cells");
                    let cells: Vec<CellId> = node.local.keys().copied().collect();
                    for c in cells { node.unhost_cell(&c); }
                    return Ok(());
                }

                _ = epoch_timer.tick() => {
                    node.control_pass().await;
                }

                item = stream.next() => match item {
                    Some(Ok(m)) => {
                        let payload = match m.payload() {
                            Ok(p) => p,
                            Err(e) => { tracing::debug!(error = %e, "bad payload hex"); continue; }
                        };
                        if m.topic == gtopics::SHAPE {
                            if let Ok(c) = serde_json::from_slice::<ShapeCommit>(&payload)
                                && c.controller != node.me
                            {
                                node.apply_remote_commit(&c);
                            }
                        } else if m.topic == gtopics::CONTROL {
                            if let Ok(hb) = serde_json::from_slice::<Heartbeat>(&payload)
                                && hb.node != node.me
                            {
                                node.control.observe(hb);
                            }
                        } else if m.topic.starts_with("ce-game/spacegame/") && m.topic.ends_with("/load") {
                            if let Ok(f) = serde_json::from_slice::<LoadFrame>(&payload)
                                && f.host != node.me
                                && let Ok(mut tbl) = node.loads.lock()
                            {
                                tbl.insert(f.cell, f.load);
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

/// Build a snapshot of the live galaxy by listening to the shape + control gossip for a bounded window —
/// the data behind `spacegame galaxy` (the heatmap of leaf cells + their hosts).
pub async fn observe_galaxy(ce: &CeClient, window: Duration) -> Result<(Galaxy, ControlView, MapModel)> {
    ce.subscribe(gtopics::SHAPE).await?;
    let _ = ce.subscribe(gtopics::CONTROL).await;

    let mut galaxy = Galaxy::genesis();
    let mut control = ControlView::default();
    let mut map = MapModel::new();

    let listen = async {
        if let Ok(stream) = ce.messages_stream().await {
            tokio::pin!(stream);
            while let Some(Ok(m)) = stream.next().await {
                let Ok(payload) = m.payload() else { continue };
                if m.topic == gtopics::SHAPE {
                    if let Ok(c) = serde_json::from_slice::<ShapeCommit>(&payload) {
                        match &c.op {
                            ShapeOp::Split { cell, .. } => galaxy.split(*cell),
                            ShapeOp::Merge { parent, .. } => galaxy.merge(*parent),
                            _ => {}
                        }
                        map.on_shape(c.epoch, &c.op);
                    }
                } else if m.topic == gtopics::CONTROL {
                    if let Ok(hb) = serde_json::from_slice::<Heartbeat>(&payload) {
                        control.observe(hb);
                    }
                } else if m.topic.ends_with("/load") {
                    if let Ok(f) = serde_json::from_slice::<LoadFrame>(&payload) {
                        map.on_load(f);
                    }
                }
            }
        }
    };
    let _ = tokio::time::timeout(window, listen).await;
    Ok((galaxy, control, map))
}

/// The browser gateway directory this node would serve (the tiny, CDN-cached list a tab fetches to pick
/// its nearest entry point). Built from the live control view's gateways plus ourselves.
pub fn gateway_directory(me: &str, region: &str, control: &ControlView, public_endpoint: &str) -> GatewayDirectory {
    let mut gateways = vec![Gateway {
        node_id: me.to_string(),
        region: RegionHint(region.to_string()),
        endpoints: vec![public_endpoint.to_string()],
        sessions: 0,
        session_budget: 10_000,
        load: 0.0,
        last_seen: 0,
    }];
    for hb in control.gateways() {
        if hb.node == me {
            continue;
        }
        gateways.push(Gateway {
            node_id: hb.node.clone(),
            region: RegionHint(hb.region.clone()),
            endpoints: vec![],
            sessions: 0,
            session_budget: hb.free_cell_slots.max(1) * 1000,
            load: hb.load,
            last_seen: hb.epoch,
        });
    }
    GatewayDirectory { gateways }
}
