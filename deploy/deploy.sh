#!/usr/bin/env bash
# Deploy spacegame as a FIRST-CLASS ce-net app — exactly like ce-hub / ce-serve / drift, and NOT as a
# bundled "demo". Two halves, both built natively ON the relay (the deploy target), never on the laptop:
#
#   1. BACKEND  — the adaptive-galaxy node: the real Rust `spacegame` binary, installed to
#                 /opt/ce-build/spacegame and run as the `spacegame-node` systemd service (genesis host +
#                 leaderless controller + browser gateway, hot-reloadable ruleset). This is the
#                 planet-scale server (see GALAXY-SCALE.md) — it hosts the genesis cell and splits a hot
#                 leaf into four children across the mesh under load.
#   2. FRONTEND — the browser client (spacegame-wasm: Rust -> WASM + wgpu), built with wasm-pack and
#                 published to the hub as app id `spa`, so it serves at https://spa.ce-net.com/
#                 (the *.ce-net.com -> hub app mapping) and https://ce-net.com/apps/spa/.
#
# Usage:
#   bash deploy/deploy.sh            # build + install both halves on the relay
#   bash deploy/deploy.sh backend    # just the adaptive-galaxy node service
#   bash deploy/deploy.sh frontend   # just the browser client -> hub app
#   bash deploy/deploy.sh dns        # ensure the spa.ce-net.com Cloudflare record
#
# Needs the relay key in your ssh-agent (`ssh-add ~/.ssh/id_ed25519`). DNS needs CLOUDFLARE_API_TOKEN.
set -euo pipefail

RELAY="root@178.105.145.170"
# Authenticate via ssh-agent (any loaded relay key works); no passphrase prompt under BatchMode.
RSH="ssh -o BatchMode=yes -o ServerAliveInterval=15 -o ControlMaster=auto -o ControlPath=/tmp/ce-sg-%C -o ControlPersist=180"
SSH=($RSH)
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"     # the spacegame repo
SIBS="$(cd "$HERE/.." && pwd)"                              # ~/ce-net (where the sibling crates live)
REMOTE=/opt/ce-build
HUB="http://127.0.0.1:8970"                                # the relay's local hub
APP="spa"                                                  # serves at https://spa.ce-net.com/
# Build trees and laptop-absolute cargo config must never ship (they break cargo on the relay).
EXC=(--exclude 'target' --exclude 'target-*' --exclude 'node_modules' --exclude 'dist' --exclude 'pkg' --exclude '.git' --exclude '.cargo')

sync() { # <localdir> <remote-name>
  "${SSH[@]}" "$RELAY" "mkdir -p $REMOTE/$2"
  rsync -az --delete "${EXC[@]}" -e "$RSH" "$1/" "$RELAY:$REMOTE/$2/"
}

backend() {
  echo "==> sync the SDK + its ce-rs path dep, build the spacegame host natively on the relay"
  # The backend binary needs the mesh feature (ce-rs). ce-rs is a sibling path dep (../ce-rs), so place
  # it beside spacegame under $REMOTE so the relative path resolves on the relay (same as ce-hub).
  sync "$SIBS/ce-rs"     ce-rs
  sync "$HERE"           spacegame
  "${SSH[@]}" "$RELAY" 'source $HOME/.cargo/env; cd '"$REMOTE"'/spacegame && (cargo build --release > /tmp/spacegame-build.log 2>&1; rc=$?; tail -30 /tmp/spacegame-build.log; exit $rc)'
  echo "==> install binary, seed the hot-reloadable ruleset, install + (re)start the adaptive-galaxy node"
  rsync -az -e "$RSH" "$HERE"/deploy/spacegame-node.service "$RELAY:/etc/systemd/system/spacegame-node.service"
  "${SSH[@]}" "$RELAY" '
    # Install runtime artifacts to a dir OUTSIDE the synced source tree, so `deploy.sh frontend` (which
    # re-syncs the sources with rsync --delete) can never delete the running binary or live ruleset.
    mkdir -p /opt/ce-build/spacegame-run &&
    install -m755 '"$REMOTE"'/spacegame/target/release/spacegame /opt/ce-build/spacegame-run/spacegame.new &&
    mv -f /opt/ce-build/spacegame-run/spacegame.new /opt/ce-build/spacegame-run/spacegame &&
    # Seed the live ruleset file the node watches (built-in template) if it is not there yet, so a
    # designer can edit /opt/ce-build/spacegame-run/live.json on the relay and hot-reload the galaxy.
    [ -f /opt/ce-build/spacegame-run/live.json ] || /opt/ce-build/spacegame-run/spacegame ruleset init /opt/ce-build/spacegame-run/live.json &&
    # Retire the old pinned-sector host if it was ever installed; the adaptive node supersedes it.
    systemctl disable --now spacegame-host >/dev/null 2>&1 || true &&
    # The node makes mutating mesh calls (subscribe/publish/mesh_deploy) that need the local CE node API
    # token. The unit runs with ProtectHome=true (so it cannot read ~/.local/share/ce/api.token), so we
    # inject the token via a drop-in the SDK reads from $CE_API_TOKEN — secret kept OUT of the repo unit,
    # exactly like ce-hub/ce-monitor.
    mkdir -p /etc/systemd/system/spacegame-node.service.d &&
    printf "[Service]\nEnvironment=CE_API_TOKEN=%s\n" "$(cat /root/.local/share/ce/api.token)" > /etc/systemd/system/spacegame-node.service.d/api-token.conf &&
    chmod 600 /etc/systemd/system/spacegame-node.service.d/api-token.conf &&
    systemctl daemon-reload && systemctl enable spacegame-node >/dev/null 2>&1 &&
    systemctl restart spacegame-node && sleep 2 &&
    printf "service: " && systemctl is-active spacegame-node &&
    journalctl -u spacegame-node -n 8 --no-pager | sed "s/^/    /"'
  echo "==> spacegame adaptive-galaxy node live on the relay (genesis hosted; controller + gateway running)"
}

frontend() {
  echo "==> sync the frontend crate + its path deps, build the wasm bundle on the relay"
  # spacegame-wasm depends on spacegame (SDK, default-features off) and spacegame-render; both are
  # sibling path deps, so ship all three under $REMOTE with matching names for the paths to resolve.
  sync "$SIBS/ce-rs"             ce-rs
  sync "$HERE"                   spacegame
  sync "$SIBS/spacegame-render"  spacegame-render
  sync "$SIBS/spacegame-wasm"    spacegame-wasm
  # RUSTFLAGS --growable-table: recent rust-lld emits the wasm `__indirect_function_table` with a fixed
  # maximum (min == max), so wasm-bindgen's runtime `table.grow()` for JS closures fails at boot
  # ("WebAssembly.Table.grow() failed to grow table by N"). This flag makes the table growable. (`.cargo/`
  # is excluded from the source sync, so the flag is set here rather than in a committed cargo config.)
  "${SSH[@]}" "$RELAY" 'source $HOME/.cargo/env; cd '"$REMOTE"'/spacegame-wasm &&
    (command -v wasm-pack >/dev/null || cargo install wasm-pack) &&
    (RUSTFLAGS="-C link-arg=--growable-table" wasm-pack build --release --target web --out-dir pkg > /tmp/spacegame-wasm-build.log 2>&1; rc=$?; tail -30 /tmp/spacegame-wasm-build.log; exit $rc)'
  echo "==> publish the client to the hub as app '"$APP"' (serves at https://$APP.ce-net.com/)"
  # Upload index.html, the page's JS, and the built pkg/ to /apps/spacegame/ on the local hub, with the
  # right content types (wasm MUST be application/wasm or the browser refuses to stream-compile it).
  "${SSH[@]}" "$RELAY" '
    set -e; cd '"$REMOTE"'/spacegame-wasm
    ctype() { case "$1" in
      *.html) echo "text/html; charset=utf-8";; *.js|*.mjs) echo "text/javascript; charset=utf-8";;
      *.css) echo "text/css; charset=utf-8";; *.json) echo "application/json";; *.wasm) echo "application/wasm";;
      *.svg) echo "image/svg+xml";; *.png) echo "image/png";; *.ts) echo "text/plain; charset=utf-8";;
      *) echo "application/octet-stream";; esac; }
    # publish the shell files (page, external boot module, in-tab-peer scaffold, gateway directory) +
    # everything under pkg/. The gateway directory serves at /galaxy/gateways.json (same-origin).
    for rel in index.html boot.js galaxy-peer.js galaxy/gateways.json $(find pkg -type f | sed "s|^\./||"); do
      [ -f "$rel" ] || continue
      code=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "'"$HUB"'/apps/'"$APP"'/$rel" \
        -H "content-type: $(ctype "$rel")" --data-binary @"$rel")
      echo "    $rel -> $code"
    done
    # Register the host -> app binding in the hub domain registry (used by host-routed serving).
    curl -s -o /dev/null -w "    domain '"$APP"'.ce-net.com -> %{http_code}\n" -X PUT -H "content-type: application/json" \
      --data "{\"domain\":\"'"$APP"'.ce-net.com\"}" "'"$HUB"'/apps/'"$APP"'/domain"'
  echo "==> spacegame frontend live: https://$APP.ce-net.com/   and   https://ce-net.com/apps/$APP/"
}

# Install the dedicated nginx server block for spa.ce-net.com (exact server_name beats the *.ce-net.com
# regex), substituting the relay's CE node API token into the /ce bridge. Idempotent; reloads nginx.
nginxblock() {
  echo "==> install spa.ce-net.com nginx block (per-file app store + /ce bridge)"
  rsync -az -e "$RSH" "$HERE"/deploy/spa-serve.nginx "$RELAY:/etc/nginx/sites-available/spa-serve.tmpl"
  "${SSH[@]}" "$RELAY" '
    tok=$(cat /root/.local/share/ce/api.token) &&
    sed "s/__CE_API_TOKEN__/$tok/" /etc/nginx/sites-available/spa-serve.tmpl > /etc/nginx/sites-available/spa-serve &&
    rm -f /etc/nginx/sites-available/spa-serve.tmpl &&
    ln -sf /etc/nginx/sites-available/spa-serve /etc/nginx/sites-enabled/spa-serve &&
    nginx -t >/dev/null 2>&1 && systemctl reload nginx && echo "    nginx reloaded"'
}

dns() {
  : "${CLOUDFLARE_API_TOKEN:?set CLOUDFLARE_API_TOKEN (see ce/.env) to manage DNS}"
  ZONE="1e8cbab8bc00451a218db1683bca8f1b"     # ce-net.com zone
  NAME="$APP.ce-net.com"
  echo "==> ensure $NAME -> relay (proxied) in Cloudflare"
  # Proxied A record at the relay IP, so the *.ce-net.com nginx app-subdomain block answers it.
  existing=$(curl -s -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
    "https://api.cloudflare.com/client/v4/zones/$ZONE/dns_records?name=$NAME" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4 || true)
  body='{"type":"A","name":"'"$NAME"'","content":"178.105.145.170","ttl":1,"proxied":true}'
  if [ -n "$existing" ]; then
    curl -s -X PUT -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" -H "content-type: application/json" \
      "https://api.cloudflare.com/client/v4/zones/$ZONE/dns_records/$existing" --data "$body" >/dev/null
    echo "    updated $NAME"
  else
    curl -s -X POST -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" -H "content-type: application/json" \
      "https://api.cloudflare.com/client/v4/zones/$ZONE/dns_records" --data "$body" >/dev/null
    echo "    created $NAME"
  fi
}

# POST-DEPLOY SMOKE GATE — prove the LIVE browser data path (wasm boots + a joining player's ship comes
# back over the public /ce bridge). This is the coverage the unit/integration suite and the local/VM e2e
# can't give, and whose absence let browser-only regressions ship (see deploy/smoke.sh). It runs ON THE
# RELAY because a laptop/sandbox may buffer the SSE stream and false-fail. A failure FAILS the deploy.
smoke() {
  echo "==> POST-DEPLOY SMOKE: assert the live browser path (boot + join -> ship over the bridge)"
  rsync -az -e "$RSH" "$HERE"/deploy/smoke.sh "$RELAY:/opt/ce-build/spacegame-run/smoke.sh"
  "${SSH[@]}" "$RELAY" "bash /opt/ce-build/spacegame-run/smoke.sh https://$APP.ce-net.com"
}

case "${1:-all}" in
  backend)  backend ;;
  frontend) frontend ;;
  dns)      dns ;;
  nginx)    nginxblock ;;
  smoke)    smoke ;;
  all)      dns || echo "    (skipped DNS — no CLOUDFLARE_API_TOKEN)"; backend; frontend; nginxblock; smoke ;;
  *) echo "usage: deploy.sh [all|backend|frontend|dns|nginx|smoke]"; exit 1 ;;
esac
echo "==> done"
