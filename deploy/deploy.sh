#!/usr/bin/env bash
# Deploy spacegame as a FIRST-CLASS ce-net app — exactly like ce-hub / ce-serve / drift, and NOT as a
# bundled "demo". Spacegame is a DECENTRALIZED game: the active PLAYERS are the server (each player's node
# runs the full authoritative Sim for the region it is in; they reconcile by quorum state-hash merge). The
# relay is NOT the game authority — it is ce-net TRANSPORT (libp2p relay / NAT traversal so players reach
# each other over the global internet) plus, optionally, one warm genesis SEED replica. Two halves, both
# built natively ON the relay (the deploy target), never on the laptop:
#
#   1. SEED     — a single lightweight, non-authoritative `spacegame host` replica pinned to the genesis
#                 ring (the `spacegame` CEAPP daemon — `ce app install`, supervised by `ce`; NEVER
#                 systemd), so the region stays warm and the first
#                 player always has a peer to bootstrap/merge against. It is one vote in the quorum,
#                 outvoted by the player majority. This REPLACES the old planet-scale `spacegame-node`
#                 (gateway + leaderless controller + autoscale); that machinery (see GALAXY-SCALE.md) is
#                 no longer deployed — players carry the compute.
#   2. FRONTEND — the browser client (spacegame-wasm: Rust -> WASM + wgpu), built with wasm-pack and
#                 published to the hub as app id `spa`, so it serves at https://spa.ce-net.com/
#                 (the *.ce-net.com -> hub app mapping) and https://ce-net.com/apps/spa/.
#
# Usage:
#   bash deploy/deploy.sh            # build + install both halves on the relay
#   bash deploy/deploy.sh seed       # just the genesis seed replica service (alias: backend)
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

seed() {
  echo "==> sync the SDK + its ce-rs path dep, build the spacegame binary natively on the relay"
  # The binary needs the mesh feature (ce-rs). ce-rs is a sibling path dep (../ce-rs), so place it beside
  # spacegame under $REMOTE so the relative path resolves on the relay (same as ce-hub).
  sync "$SIBS/ce-rs"     ce-rs
  sync "$HERE"           spacegame
  "${SSH[@]}" "$RELAY" 'source $HOME/.cargo/env; cd '"$REMOTE"'/spacegame && (cargo build --release > /tmp/spacegame-build.log 2>&1; rc=$?; tail -30 /tmp/spacegame-build.log; exit $rc)'
  echo "==> publish spacegame as a CEAPP + (re)install its supervised genesis-seed daemon (NO systemd)"
  # CE services are CEAPPS, never systemd units (Leif's mandate — see ../OPERATIONS.md). ce-publish
  # blob-uploads the freshly built binary, stamps its content digest into ceapp.toml's [native.artifacts],
  # signs + uploads the manifest to ce-hub; then `ce app install` materializes (fetch + sha256-verify) the
  # binary and the single `ce` node supervises the daemon (restart=on-failure). The daemon args in
  # ceapp.toml host the genesis plus-ring; negative sectors use the `n` token form (n1_0/0_n1).
  "${SSH[@]}" "$RELAY" '
    set -e
    export CE_API_TOKEN=$(cat /root/.local/share/ce/api.token)
    cd '"$REMOTE"'/spacegame
    # Seed the hot-reloadable ruleset OUTSIDE the synced tree (a frontend re-sync cannot clobber it), so a
    # designer can edit /opt/ce-build/spacegame-run/live.json on the relay. (The ceapp host loads it from
    # there via the daemon CWD; see ceapp.toml.)
    mkdir -p /opt/ce-build/spacegame-run
    [ -f /opt/ce-build/spacegame-run/live.json ] || ./target/release/spacegame ruleset init /opt/ce-build/spacegame-run/live.json
    # MIGRATE off every legacy systemd unit (the old systemd seed + the retired authoritative node/host).
    for u in spacegame-seed spacegame-node spacegame-host; do
      systemctl disable --now "$u" >/dev/null 2>&1 || true
      rm -f "/etc/systemd/system/$u.service"; rm -rf "/etc/systemd/system/$u.service.d"
    done
    systemctl daemon-reload >/dev/null 2>&1 || true
    # Publish the ceapp (upload the fresh binary as a blob + stamp its digest + sign + upload manifest),
    # then re-install so `ce app install` materializes the NEW digest (uninstall first: install skips an
    # already-installed app, and uninstall does not stop a running instance, so we also pkill it).
    ce-publish app ceapp.toml --bin target/release/spacegame --target linux-amd64 --hub '"$HUB"'
    ce app uninstall spacegame >/dev/null 2>&1 || true
    pkill -f "apps/spacegame/.*/spacegame host" >/dev/null 2>&1 || true
    ce app install spacegame --yes --registry '"$HUB"'
    ce app daemon enable spacegame >/dev/null 2>&1 || true
    sleep 6
    printf "daemon supervised: "; ce app daemon ls 2>/dev/null | grep -q spacegame && echo yes || echo NO
    ps -eo pid,etimes,cmd | grep "spacegame host" | grep -v grep | sed "s/^/    /"'
  echo "==> spacegame genesis seed live as a ceapp daemon (warm peer near origin; players are the server)"
}
# Back-compat alias: the relay half used to be the authoritative "backend"; it is now just a seed.
backend() { seed; }

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
    # Opt-in in-tab libp2p peer (each tab is its own player over the relay circuit relay). Pre-bundled
    # locally with esbuild (npm run build:peer) and committed, so no npm is needed here. boot.js loads it
    # only with ?peer and falls back to the bridge on failure.
    if [ -f galaxy-peer.bundle.js ]; then cp galaxy-peer.bundle.js "$STAGE/"; else echo "    WARN: galaxy-peer.bundle.js missing; run npm i and npm run build:peer"; fi
    # Account/start-menu bundle (encrypted ce-iam vault + start menu + node auth) + its wasm. Pre-built
    # locally (npm run build:account); boot.js loads account.bundle.js before the game.
    if [ -f account.bundle.js ]; then cp account.bundle.js "$STAGE/"; else echo "    WARN: account.bundle.js missing; run npm run build:account"; fi
    if [ -f ce_iam_core_wasm_bg.wasm ]; then cp ce_iam_core_wasm_bg.wasm "$STAGE/"; fi
    cp galaxy/gateways.json "$STAGE/galaxy/"
    # The spacegame galaxy map lives UNDER the spacegame app at /map (it is spacegame-specific). The
    # bare map.ce-net.com is reserved for a future ce-net-wide donator map, so do NOT publish there.
    mkdir -p "$STAGE/map"
    cp ../spacegame/galaxymap-web/index.html "$STAGE/map/index.html"
    cp -r pkg "$STAGE/pkg"
    # CACHE-BUST: stamp boot.js with the wasm content hash so the browser loads a fresh, MATCHED glue+wasm
    # pair every build (defeats stale ES-module caching that LinkErrors a new wasm against an old glue).
    V=$(sha256sum pkg/spacegame_wasm_bg.wasm | cut -c1-16)
    sed -i "s/__SGV__/$V/g" "$STAGE/boot.js" "$STAGE/index.html"
    if grep -rq "__SGV__" "$STAGE/boot.js" "$STAGE/index.html"; then echo "FAILED to stamp cache-bust version"; exit 1; fi
    echo "    cache-bust version stamped (boot.js + index.html): $V"
    # Publish via the ce-publish app (web-bundle mode). Falls back to the legacy ce-serve-publish binary
    # still on the box until ce-publish is installed here, so the deploy never breaks mid-migration.
    if command -v ce-publish >/dev/null 2>&1; then PUB="ce-publish bundle"; else PUB="/opt/ce-serve/ce-serve-publish"; fi
    CE_API_TOKEN=$(cat /root/.local/share/ce/api.token) \
      $PUB "$STAGE" '"$APP"'.ce-net.com '"$APP"
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

# MONITOR: the spacegame galaxy map is served UNDER the spacegame app at https://spa.ce-net.com/map/
# (staged into the spa bundle by frontend()). It is spacegame-specific, so it is NOT published to the
# bare map.ce-net.com — that host is reserved for a future ce-net-wide donator/network map.

case "${1:-all}" in
  seed)     seed ;;
  backend)  seed ;;   # back-compat alias
  frontend) frontend ;;
  dns)      dns ;;
  unshadow) unshadow ;;
  smoke)    smoke ;;
  all)      dns || echo "    (skipped DNS — no CLOUDFLARE_API_TOKEN)"; seed; frontend; unshadow; smoke ;;
  *) echo "usage: deploy.sh [all|seed|frontend|dns|unshadow|smoke]"; exit 1 ;;
esac
echo "==> done"
