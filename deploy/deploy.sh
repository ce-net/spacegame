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
  echo "==> publish the client as a CONTENT-ADDRESSED BUNDLE via ce-serve (the ONLY way to serve a ce-net app)"
  # ce-serve is ce-net's single public HTTP edge: it resolves Host -> bundle, serves each file from the
  # node blob store (wasm as application/wasm), and AUTO-INJECTS /__ce/mesh-bridge.js, which gives the page
  # `window.__ceNode` over ONE same-origin WebSocket (/mesh-bridge) — the transport the WASM client speaks.
  # The hub is the registry/tracker, NOT a web server. So we ce-serve-publish a clean bundle dir (which
  # blob-uploads each file, builds the {spa,files} manifest, and registers the host->bundle in ce-hub).
  "${SSH[@]}" "$RELAY" '
    set -e; cd '"$REMOTE"'/spacegame-wasm
    STAGE=/opt/ce-build/spa-bundle
    rm -rf "$STAGE" && mkdir -p "$STAGE/galaxy"
    cp index.html boot.js galaxy-peer.js "$STAGE/"
    cp galaxy/gateways.json "$STAGE/galaxy/"
    cp -r pkg "$STAGE/pkg"
    CE_API_TOKEN=$(cat /root/.local/share/ce/api.token) \
      /opt/ce-serve/ce-serve-publish "$STAGE" '"$APP"'.ce-net.com '"$APP"
  echo "==> spacegame frontend live via ce-serve: https://$APP.ce-net.com/"
}

# Ensure NO bespoke nginx block shadows spa.ce-net.com. ce-serve must own it: the *.ce-net.com regex
# server in the main `ce` site already proxies `/` and the `/mesh-bridge` WebSocket to ce-serve (:8790),
# so once any old exact-name `spa-serve` block is gone, the host resolves to our published bundle and the
# page gets the injected mesh bridge. (Earlier we wrongly served spacegame straight from the hub with a
# custom block — that path has no mesh bridge, so the browser had no transport. ce-serve is the only way.)
unshadow() {
  echo "==> ensure ce-serve owns $APP.ce-net.com (remove any bespoke nginx block)"
  "${SSH[@]}" "$RELAY" '
    rm -f /etc/nginx/sites-enabled/spa-serve /etc/nginx/sites-available/spa-serve /etc/nginx/sites-available/spa-serve.tmpl
    nginx -t >/dev/null 2>&1 && systemctl reload nginx && echo "    nginx reloaded (spa.ce-net.com -> ce-serve)"'
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
  unshadow) unshadow ;;
  smoke)    smoke ;;
  all)      dns || echo "    (skipped DNS — no CLOUDFLARE_API_TOKEN)"; backend; frontend; unshadow; smoke ;;
  *) echo "usage: deploy.sh [all|backend|frontend|dns|unshadow|smoke]"; exit 1 ;;
esac
echo "==> done"
