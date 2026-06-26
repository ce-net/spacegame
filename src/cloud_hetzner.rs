//! A concrete [`crate::fleet::CloudProvider`] — **Hetzner Cloud burst**. When the donor pool is tight,
//! the galaxy rents real servers here, boots them straight into the mesh as game hosts, and tears them
//! down when the surge passes. (The Hetzner API token already lives in `ce/.env`; the relay is Hetzner
//! too, so these nodes are born close to the bootstrap.)
//!
//! The whole trick is the **cloud-init**: a fresh box installs `ce`, starts a node that bootstraps to
//! the relay, and launches the spacegame cell image — so within ~a minute the node is advertising
//! capacity in the atlas and the autoscaler can place cells on it like any donor. No image baking, no
//! config server; the node self-assembles from the one-liner installer the project already ships.

use crate::fleet::{CapacitySource, CloudProvider, HostNode, RegionHint};

/// Hetzner server types by rough capability, so the fleet can ask for "enough to host N cells" rather
/// than memorise SKUs. Prices are indicative (credits/node-min) and refreshed from the treasury tariff.
#[derive(Debug, Clone, Copy)]
pub enum Size {
    /// 2 vCPU / 4 GB — a handful of light cells. The relay is this class (cpx22-ish).
    Small,
    /// 4 vCPU / 8 GB — a busy region's workhorse.
    Medium,
    /// 8 vCPU / 16 GB — a battle node; hosts a deep funnel of split cells.
    Large,
}

impl Size {
    pub fn server_type(&self) -> &'static str {
        match self {
            Size::Small => "cpx21",
            Size::Medium => "cpx31",
            Size::Large => "cpx41",
        }
    }
    pub fn cell_budget(&self) -> u32 {
        match self {
            Size::Small => 3,
            Size::Medium => 6,
            Size::Large => 14,
        }
    }
}

/// Hetzner locations mapped to our latency regions. The autoscaler asks for a region; we pick the
/// nearest Hetzner datacentre.
fn location_for(region: &RegionHint) -> &'static str {
    match region.0.as_str() {
        "us-east" | "us" => "ash", // Ashburn, VA
        "us-west" => "hil",        // Hillsboro, OR
        "ap" | "asia" => "sin",    // Singapore
        _ => "fsn1",               // Falkenstein, DE — also where the relay lives
    }
}

/// The cloud-init that turns a bare Ubuntu box into a galaxy host. `ubuntu-24.04` so relay-built
/// binaries' glibc matches (per the e2e notes). Joins via the public relay bootstrap, advertises a
/// hosting capability, and runs the node + cell image.
pub fn cloud_init(cell_image: &str, relay_multiaddr: &str) -> String {
    format!(
        r#"#cloud-config
package_update: true
runcmd:
  - curl -sSL https://raw.githubusercontent.com/ce-net/ce/main/install.sh | bash
  - |
    cat >/etc/systemd/system/ce.service <<UNIT
    [Unit]
    Description=ce node (spacegame burst host)
    After=network-online.target
    [Service]
    Environment=CE_BOOTSTRAP_PEERS={relay}
    ExecStart=/usr/local/bin/ce start --no-mine --light
    Restart=always
    [Install]
    WantedBy=multi-user.target
    UNIT
  - systemctl enable --now ce
  - |
    cat >/etc/systemd/system/spacegame-cell.service <<UNIT
    [Unit]
    Description=spacegame cell host
    After=ce.service
    [Service]
    # The node self-advertises hosting capacity; controllers place cells here over the mesh
    # (mesh_deploy of {image}). Nothing to configure per-cell — the galaxy decides.
    ExecStart=/usr/local/bin/spacegame node --host-only --image {image}
    Restart=always
    [Install]
    WantedBy=multi-user.target
    UNIT
  - systemctl enable --now spacegame-cell
"#,
        relay = relay_multiaddr,
        image = cell_image,
    )
}

/// The Hetzner burst provider. Holds the API token + the relay address new nodes bootstrap to.
pub struct HetznerBurst {
    pub api_token: String,
    pub relay_multiaddr: String,
    pub size: Size,
    /// Credits/node-min this region costs us (provider price; the treasury adds margin on top).
    pub price_per_min: u128,
}

impl CloudProvider for HetznerBurst {
    fn name(&self) -> &str {
        "hetzner"
    }

    fn provision(&self, count: usize, region: &RegionHint, cloud_init: &str) -> Vec<HostNode> {
        // Real impl (Hetzner Cloud API, the same token ce-deploy uses):
        //   POST https://api.hetzner.cloud/v1/servers  for each:
        //     { name: "sg-burst-<rand>", server_type: self.size.server_type(),
        //       location: location_for(region), image: "ubuntu-24.04",
        //       user_data: cloud_init, ssh_keys: ["ce-deploy"] }
        //   then wait until each node reports into the atlas (it has joined + advertised capacity),
        //   and return it as a HostNode tagged CloudBurst so the fleet/treasury bill + reap it.
        let _ = (count, region, cloud_init);
        unimplemented!("HetznerBurst::provision — POST /v1/servers x count, await atlas presence")
    }

    fn destroy(&self, instance_id: &str) {
        // DELETE https://api.hetzner.cloud/v1/servers/{instance_id} — after the node is drained.
        let _ = instance_id;
        unimplemented!("HetznerBurst::destroy — DELETE /v1/servers/{instance_id}")
    }

    fn price_per_min(&self, _region: &RegionHint) -> u64 {
        self.price_per_min.min(u64::MAX as u128) as u64
    }
}

impl HetznerBurst {
    /// Build the source tag for a node this provider created, so the fleet knows it's ours to reap.
    pub fn source(region: &RegionHint, instance_id: String) -> CapacitySource {
        CapacitySource::CloudBurst {
            provider: "hetzner".into(),
            region: region.0.clone(),
            instance_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_init_joins_relay_and_runs_cell() {
        let ci = cloud_init("ce-net/spacegame:latest", "/ip4/178.105.145.170/tcp/4001/p2p/RELAY");
        assert!(ci.contains("install.sh"));
        assert!(ci.contains("CE_BOOTSTRAP_PEERS=/ip4/178.105.145.170"));
        assert!(ci.contains("spacegame node"));
    }

    #[test]
    fn region_maps_to_nearest_datacentre() {
        assert_eq!(location_for(&RegionHint("us-east".into())), "ash");
        assert_eq!(location_for(&RegionHint("whatever".into())), "fsn1");
    }
}
