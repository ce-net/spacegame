//! The **gateway tier** — how millions of browsers reach the galaxy, each as its own mesh peer.
//!
//! A browser cannot open a raw libp2p TCP/QUIC socket, so it joins the mesh one of two ways, behind the
//! identical `window.__ceNode` seam (the app never knows which is live):
//!
//! 1. **In-browser peer (preferred).** The page runs a real js-libp2p node *in the tab* with its own
//!    keypair → its own unspoofable NodeId → its own player. It dials a **gateway** over WebTransport/
//!    wss and speaks gossipsub directly, subscribing to its interest cells and publishing its input.
//!    A million tabs are a million distinct peers; identity scales for free.
//! 2. **Bridge fallback.** Where WebTransport/wss peering is impossible, the tab tunnels to a gateway's
//!    co-located `ce` node. To keep identity-per-player here, each bridged session is issued an
//!    **ephemeral subkey** the gateway signs for, so even bridged browsers are distinct players (never
//!    all collapsed onto the gateway's own NodeId — the bug that the naive single-node bridge has).
//!
//! Gateways are **stateless fan-in/out relays**: they hold no game state, just route a browser's
//! pubsub to the mesh and stream its interest cells back. That makes them trivially horizontal — add
//! gateways, serve more browsers — and lets the autoscaler scale the connection tier independently of
//! the simulation tier. Each browser attaches to its **lowest-RTT** gateway and re-homes seamlessly if
//! one drains, its in-tab prediction covering the blip.
//!
//! ```text
//!   1,000,000 browsers ──nearest──►  [ gateway fleet, autoscaled, stateless ]  ──gossipsub──► galaxy cells
//!        each its own peer id              (wss / WebTransport edge)                 (host nodes)
//! ```

use crate::fleet::RegionHint;

/// How a browser attached to the mesh — recorded so the player's identity model is explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// A real in-tab libp2p peer; `node_id` is the tab's own key. The ideal: free, distinct identity.
    InBrowserPeer { node_id: String },
    /// A bridged session with a gateway-signed ephemeral subkey, so it is still a distinct player.
    BridgedSubkey { gateway: String, subkey_id: String },
}

impl Attachment {
    /// The player's unspoofable id regardless of attachment kind.
    pub fn player_id(&self) -> &str {
        match self {
            Attachment::InBrowserPeer { node_id } => node_id,
            Attachment::BridgedSubkey { subkey_id, .. } => subkey_id,
        }
    }
}

/// A gateway node: a public, well-connected edge that browsers dial. Stateless about the game; it only
/// proxies pubsub and counts its open sessions so the autoscaler can balance it.
#[derive(Debug, Clone)]
pub struct Gateway {
    pub node_id: String,
    pub region: RegionHint,
    /// Public connect endpoints the browser tries in order (WebTransport first, then wss).
    pub endpoints: Vec<String>,
    /// Live browser sessions on this gateway.
    pub sessions: u32,
    /// Soft cap before the autoscaler spins up more gateways in this region.
    pub session_budget: u32,
    /// Smoothed connection load 0..1 (sockets, fan-out bandwidth, CPU).
    pub load: f32,
    pub last_seen: u64,
}

impl Gateway {
    pub fn headroom(&self) -> u32 {
        self.session_budget.saturating_sub(self.sessions)
    }
    pub fn accepting(&self) -> bool {
        self.headroom() > 0 && self.load < 0.85
    }
}

/// The browser-facing directory of gateways. A tab fetches this (tiny, CDN-cached) at boot and picks
/// its nearest accepting gateway; it re-picks on disconnect. This is the only "where do I connect"
/// lookup a player ever does — everything past it is the self-routing mesh.
#[derive(Debug, Clone, Default)]
pub struct GatewayDirectory {
    pub gateways: Vec<Gateway>,
}

impl GatewayDirectory {
    /// Choose the gateway a browser in `region` should attach to: nearest region with headroom, least
    /// loaded, with a sprinkle of spread so a thundering herd doesn't all pile on the single closest one.
    pub fn pick(&self, region: &RegionHint, spread_seed: u64) -> Option<&Gateway> {
        let mut accepting: Vec<&Gateway> = self.gateways.iter().filter(|g| g.accepting()).collect();
        accepting.sort_by(|a, b| {
            let ra = (a.region == *region) as u8;
            let rb = (b.region == *region) as u8;
            rb.cmp(&ra).then(a.load.partial_cmp(&b.load).unwrap_or(std::cmp::Ordering::Equal))
        });
        // Among the top few least-loaded in-region gateways, deterministically spread by the tab's seed.
        let top = &accepting[..accepting.len().min(4)];
        if top.is_empty() {
            None
        } else {
            Some(top[(spread_seed as usize) % top.len()])
        }
    }

    /// Aggregate connection headroom, the signal the autoscaler uses to add/retire gateways.
    pub fn slack(&self, region: &RegionHint) -> f32 {
        let (mut free, mut total) = (0u64, 0u64);
        for g in self.gateways.iter().filter(|g| g.region == *region) {
            free += g.headroom() as u64;
            total += g.session_budget as u64;
        }
        if total == 0 {
            0.0
        } else {
            free as f32 / total as f32
        }
    }
}

/// What the browser does to come online — the in-tab join, expressed as the API the WASM client calls.
/// (Mirrored in TypeScript for the page; see `../spacegame-wasm` / `web/ce-app/client`.) The whole of
/// "connect a million players" is: get the directory, pick a gateway, bring up your peer, subscribe to
/// your interest cells, publish input. No matchmaking server, no session backend.
pub struct JoinPlan {
    pub attachment: Attachment,
    pub gateway_endpoint: String,
    pub region: RegionHint,
}

impl JoinPlan {
    /// Sketch of the browser join (the spec, in one place):
    /// ```text
    /// dir       = fetch("/galaxy/gateways")            // tiny, CDN-cached gateway directory
    /// region    = probe_rtt(dir)                        // measure, don't geolocate
    /// gw        = dir.pick(region, tab_seed)
    /// node      = InBrowserPeer.start() ?? Bridged(gw)  // own key if we can, else signed subkey
    /// node.dial(gw.endpoint)                            // WebTransport / wss
    /// galaxy    = subscribe("/galaxy")                  // learn the live leaf set
    /// loop:
    ///   cells   = galaxy.interest(me.x, me.y, view)     // a handful of leaves, never the whole galaxy
    ///   for c in cells: subscribe("ce-game/spacegame/"+c.token()+"/state")
    ///   on input:    publish(my_cell.token()+"/in", input); predict locally
    ///   on snapshot: reconcile + render
    ///   on edge:     interest shifts to the neighbour leaf — transit is invisible
    /// ```
    pub const SPEC: &'static str = "see JoinPlan docs";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw(id: &str, region: &str, sessions: u32, budget: u32, load: f32) -> Gateway {
        Gateway {
            node_id: id.into(),
            region: RegionHint(region.into()),
            endpoints: vec![format!("wss://{id}.gw.ce-net.com")],
            sessions,
            session_budget: budget,
            load,
            last_seen: 0,
        }
    }

    #[test]
    fn picks_nearest_accepting_gateway() {
        let dir = GatewayDirectory {
            gateways: vec![
                gw("far", "us", 0, 1000, 0.1),
                gw("near", "eu", 10, 1000, 0.2),
                gw("near-full", "eu", 1000, 1000, 0.99),
            ],
        };
        let pick = dir.pick(&RegionHint("eu".into()), 0).unwrap();
        assert_eq!(pick.node_id, "near", "in-region, accepting, least loaded");
    }

    #[test]
    fn bridged_sessions_are_distinct_players() {
        let a = Attachment::BridgedSubkey { gateway: "g".into(), subkey_id: "sub-1".into() };
        let b = Attachment::BridgedSubkey { gateway: "g".into(), subkey_id: "sub-2".into() };
        assert_ne!(a.player_id(), b.player_id(), "even bridged browsers must not collapse to one player");
    }
}
