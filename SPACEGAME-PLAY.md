# Playing CE Spacegame locally — computer + phone, over the real mesh

This is the operator guide for running a real, playable game on one machine: the **native app** on your
computer and the **browser** on your phone, both in the same sector, talking only over the real CE mesh.

## One command

```bash
~/ce-net/spacegame/play.sh
```

It brings up everything, waits for the mesh to form, and prints the exact command to launch the native
app plus the URL to open on your phone. `Ctrl-C` tears it all down.

## Why three nodes

A player's identity **is** a CE node's NodeId (free, unspoofable — the wire never carries a player id).
A node does **not** gossipsub-deliver its own publishes back to itself, so a single node cannot be both
the sector **host** and a **player**. Each distinct participant therefore needs its own node:

| Node | Port | Role |
|---|---|---|
| H  | 8861 | runs `spacegame host --sector 0_0` (authoritative sim); no player |
| P1 | 8862 | the **computer**'s node — the native app connects here |
| P2 | 8863 | the **phone**'s node — `ce-serve` serves the WASM frontend pointed here |

The script runs them as an **isolated** mesh (`CE_NO_AUTOBOOTSTRAP=1`, `--no-mdns`) with the two player
nodes bootstrapped **directly** to H, so gossipsub forms a clean direct topic-mesh (no relay/circuit in
the path — a relayed path won't graft into the gossip mesh). Gossipsub topic-meshes graft in **~1-2 s**
between fresh nodes; the script pre-warms and waits, after which players appear the instant they connect.
Both clients publish input continuously (20 Hz), which keeps the reverse topic-mesh warm.

## The pieces (all Rust, one renderer)

- **`spacegame`** (`../spacegame`) — the authoritative sector backend + `host` CLI.
- **`spacegame-render`** — the shared view-model + `Scene` + a CPU rasterizer (also the headless
  `screenshot` tool for visual verification). One renderer description, three frontends.
- **`spacegame-native`** — the desktop app (mac/win/linux): `minifb` window + `ureq` to the local node,
  renders the shared `Scene`.
- **`spacegame-wasm`** — the browser app: Rust→WASM + `wgpu` (WebGL2), renders the same `Scene`,
  talks to the mesh through **ce-serve**'s `window.__ceNode` bridge (the page never holds the node token).
- **`ce-serve`** — the HTTP edge: serves the WASM bundle and tunnels the browser's mesh calls to node P2.

## Building (first time)

```bash
cd ~/ce-net/spacegame        && cargo build --bin spacegame      # host backend
cd ~/ce-net/spacegame-native && cargo build                  # native desktop app
cd ~/ce-net/ce-serve         && cargo build                      # HTTP edge (for the phone)
cd ~/ce-net/spacegame-wasm   && wasm-pack build --target web --out-dir pkg   # browser bundle
```

Then run `spacegame-play.sh`.

## Manual run (what the script automates)

```bash
# 1) host node + two player nodes, isolated, players bootstrapped to the host node
CE_NO_AUTOBOOTSTRAP=1 ce --data-dir /tmp/ce-sg-h  start --api-port 8861 --port 4011 --no-mine --ephemeral --no-mdns
HADDR=/ip4/127.0.0.1/tcp/4011/p2p/$(curl -s 127.0.0.1:8861/bootstrap | python3 -c 'import sys,json;print(json.load(sys.stdin)["peers"][0].split("/p2p/")[-1])')
CE_NO_AUTOBOOTSTRAP=1 ce --data-dir /tmp/ce-sg-p1 start --api-port 8862 --port 4012 --api-bind 0.0.0.0 --no-mine --ephemeral --no-mdns --bootstrap "$HADDR"
CE_NO_AUTOBOOTSTRAP=1 ce --data-dir /tmp/ce-sg-p2 start --api-port 8863 --port 4013 --api-bind 0.0.0.0 --no-mine --ephemeral --no-mdns --bootstrap "$HADDR"

# 2) host the sector on H
CE_BASE_URL=http://127.0.0.1:8861 CE_API_TOKEN=$(cat /tmp/ce-sg-h/api.token) spacegame host --sector 0_0

# 3) computer: native app -> P1
SPACEGAME_NODE=http://127.0.0.1:8862 CE_API_TOKEN=$(cat /tmp/ce-sg-p1/api.token) spacegame-native

# 4) phone: serve the WASM bundle -> P2, open http://<your-LAN-ip>:8790 on the phone
CE_NODE_URL=http://127.0.0.1:8863 CE_API_TOKEN=$(cat /tmp/ce-sg-p2/api.token) \
  CE_SERVE_ROOT=~/ce-net/spacegame-wasm CE_SERVE_PORT=8790 ce-serve
```

## Visual verification (no GPU / headless)

```bash
cd ~/ce-net/spacegame-render && cargo run --bin screenshot -- combat /tmp/sg.png 1280x720
```
renders a `Scene` to a PNG with the CPU rasterizer — the same geometry the native/web clients draw.
