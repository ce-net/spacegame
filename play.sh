#!/usr/bin/env bash
# spacegame-play.sh — bring up a real, playable CE Spacegame on this machine so you can play on your
# computer (the native app) and your phone (the browser) in the SAME sector over the real CE mesh.
#
# Topology (proven): a CE node hosts the sector; each PLAYER needs its OWN node (the node's NodeId IS
# the player, and a node never gossipsub-delivers its own publishes to itself — so host and each player
# must be distinct nodes). We run a small ISOLATED game mesh of fresh nodes that bootstrap directly to
# the host node, so gossipsub forms a clean direct topic-mesh with no relay/circuit in the path:
#
#   node H (host)     :8861  — runs `spacegame host --sector 0_0`,        no player
#   node P1 (computer):8862  — the native app connects here
#   node P2 (phone)   :8863  — ce-serve serves the WASM frontend, pointed here; your phone loads it
#
# Gossipsub needs ~30-40s to graft between fresh nodes; this script waits, then keeps running.
# Ctrl-C tears the whole thing down.
set -euo pipefail

CE=${CE:-$HOME/.local/bin/ce}
BIN=${BIN:-$HOME/ce-net/.cargo-shared/debug}
SG=$BIN/spacegame
SG_NATIVE=$BIN/spacegame-native
SERVE=${SERVE:-$BIN/ce-serve}
WASM_DIR=${WASM_DIR:-$HOME/ce-net/spacegame-wasm}
SECTOR=${SECTOR:-0_0}
SERVE_PORT=${SERVE_PORT:-8790}

H_API=8861; H_P2P=4011; H_DIR=/tmp/ce-sg-h
P1_API=8862; P1_P2P=4012; P1_DIR=/tmp/ce-sg-p1
P2_API=8863; P2_P2P=4013; P2_DIR=/tmp/ce-sg-p2

say() { printf '\033[36m[play]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[play] %s\033[0m\n' "$*" >&2; exit 1; }

[ -x "$SG" ]        || die "missing host binary: $SG  (build: cd ~/ce-net/spacegame && cargo build --bin spacegame)"
[ -x "$SG_NATIVE" ] || die "missing native client: $SG_NATIVE  (build: cd ~/ce-net/spacegame-native && cargo build)"

PIDS=()
cleanup() {
  say "shutting down…"
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  pkill -f "ce --data-dir /tmp/ce-sg-" 2>/dev/null || true
  pkill -f "spacegame host --sector $SECTOR" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Fresh start.
pkill -f "ce --data-dir /tmp/ce-sg-" 2>/dev/null || true
pkill -f "spacegame host --sector" 2>/dev/null || true
sleep 1
rm -rf "$H_DIR" "$P1_DIR" "$P2_DIR"; mkdir -p "$H_DIR" "$P1_DIR" "$P2_DIR"

start_node() { # dir api p2p [bootstrap]
  local dir=$1 api=$2 p2p=$3 boot=${4:-}
  local args=(--data-dir "$dir" start --api-port "$api" --port "$p2p" --api-bind 0.0.0.0 --no-mine --ephemeral --no-mdns)
  [ -n "$boot" ] && args+=(--bootstrap "$boot")
  CE_NO_AUTOBOOTSTRAP=1 RUST_LOG=warn nohup "$CE" "${args[@]}" >"$dir/node.log" 2>&1 &
  PIDS+=($!)
}

wait_api() { # api
  for _ in $(seq 30); do curl -sf -m 2 "http://127.0.0.1:$1/status" >/dev/null 2>&1 && return 0; sleep 0.5; done
  die "node on :$1 did not come up (see its node.log)"
}

say "starting host node H (:$H_API)…"
start_node "$H_DIR" "$H_API" "$H_P2P"
wait_api "$H_API"
HPEER=$(curl -s "http://127.0.0.1:$H_API/bootstrap" | python3 -c "import sys,json;print(json.load(sys.stdin)['peers'][0].split('/p2p/')[-1])")
HADDR="/ip4/127.0.0.1/tcp/$H_P2P/p2p/$HPEER"
say "host node peer: ${HPEER:0:16}…"

say "starting player nodes P1 (:$P1_API, computer) and P2 (:$P2_API, phone), bootstrapped to H…"
start_node "$P1_DIR" "$P1_API" "$P1_P2P" "$HADDR"
start_node "$P2_DIR" "$P2_API" "$P2_P2P" "$HADDR"
wait_api "$P1_API"; wait_api "$P2_API"

say "starting the sector host (spacegame host --sector $SECTOR on H)…"
CE_BASE_URL="http://127.0.0.1:$H_API" CE_API_TOKEN="$(cat "$H_DIR/api.token")" RUST_LOG=info \
  nohup "$SG" host --sector "$SECTOR" >/tmp/ce-sg-host.log 2>&1 &
PIDS+=($!)

# Pre-warm the gossipsub topic mesh in BOTH directions so players are seen the instant you connect.
warm() { # api token
  local b="http://127.0.0.1:$1" t=$2
  curl -s -X POST "$b/mesh/subscribe" -H "Authorization: Bearer $t" -H 'content-type: application/json' \
    -d "{\"topic\":\"ce-game/spacegame/$SECTOR/state\"}" >/dev/null || true
  local join; join=$(printf '{"t":"join","name":"warmup"}' | xxd -p | tr -d '\n')
  curl -s -X POST "$b/mesh/publish" -H "Authorization: Bearer $t" -H 'content-type: application/json' \
    -d "{\"topic\":\"ce-game/spacegame/$SECTOR/in\",\"payload_hex\":\"$join\"}" >/dev/null || true
}
say "warming the mesh (gossipsub topic-mesh grafts in ~1-2s)…"
for _ in 1 2 3; do
  warm "$P1_API" "$(cat "$P1_DIR/api.token")"
  warm "$P2_API" "$(cat "$P2_DIR/api.token")"
  sleep 1
done

# Start ce-serve for the phone, pointed at P2, serving the WASM frontend.
LANIP=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || echo 127.0.0.1)
if [ -x "$SERVE" ] && [ -f "$WASM_DIR/pkg/spacegame_wasm.js" ]; then
  say "starting ce-serve for the phone (serving $WASM_DIR → node P2)…"
  CE_NODE_URL="http://127.0.0.1:$P2_API" CE_API_TOKEN="$(cat "$P2_DIR/api.token")" \
    CE_SERVE_ROOT="$WASM_DIR" CE_SERVE_PORT="$SERVE_PORT" CE_SERVE_DEFAULT_HOST="" \
    nohup "$SERVE" >/tmp/ce-sg-serve.log 2>&1 &
  PIDS+=($!)
  PHONE_URL="http://$LANIP:$SERVE_PORT/"
else
  PHONE_URL="(build the web bundle first: cd $WASM_DIR && wasm-pack build --target web --out-dir pkg ; and build ce-serve: cd ~/ce-net/ce-serve && cargo build)"
fi

printf '\n\033[32m============================ SPACEGAME IS UP ============================\033[0m\n'
cat <<EOF

  COMPUTER (native app) — run this in another terminal:

      SPACEGAME_NODE=http://127.0.0.1:$P1_API \\
      CE_API_TOKEN=$(cat "$P1_DIR/api.token") \\
      $SG_NATIVE

  PHONE (browser, same Wi-Fi) — open:

      $PHONE_URL

  Controls (native): WASD move - mouse aim - Space/Left-click fire - Esc quit
  Both players appear in sector $SECTOR. Give it a few seconds after connecting.

  Logs: /tmp/ce-sg-host.log  $H_DIR/node.log  /tmp/ce-sg-serve.log
  Ctrl-C here tears everything down.
EOF
printf '\033[32m========================================================================\033[0m\n'

# Keep the mesh and all nodes alive until Ctrl-C.
while true; do sleep 3600; done
