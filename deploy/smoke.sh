#!/usr/bin/env bash
# POST-DEPLOY SMOKE GATE — assert the DEPLOYED BROWSER DATA PATH actually works, end to end, through the
# real edge (Cloudflare -> nginx -> node). This is the test class the unit/integration suite and the
# local/VM e2e CANNOT cover, and whose absence let three browser-only regressions ship to spa.ce-net.com:
#
#   1. the wasm wouldn't boot (rust-lld emitted a fixed-max function table; wasm-bindgen's runtime
#      table.grow() failed) — invisible to `cargo test`, only fails in a real browser;
#   2. the wasm crashed on a Retina canvas (surface > 2048 max texture) — needs a real GPU;
#   3. a joining player's ship never reached the browser — the live SSE bridge through nginx/Cloudflare
#      delivered nothing, while the node itself self-delivered fine. Backend tests passed; the EDGE was broken.
#
# Leif's directive (verbatim): "THE E2E TESTS SHOULD HAVE CAUGHT THIS WHY NOT?? FIX SO IT DOES." This gate
# is the fix: every deploy now proves, against the LIVE URL, that the page boots and that joining yields a
# ship over the public bridge — the exact thing a player does.
#
# MUST run from a host that can hold a streaming HTTP connection (the relay, or CI) — NOT a sandbox that
# buffers long-lived responses (which silently returns zero frames and would false-fail).
set -euo pipefail
BASE="${1:-https://spa.ce-net.com}"
fail() { echo "SMOKE FAIL: $1" >&2; exit 1; }

echo "==> smoke 1/3: served wasm boots (function table is GROWABLE)"
python3 - "$BASE" <<'PY' || fail "served wasm function table is not growable -> the page would not boot (table.grow)"
import sys, urllib.request
# Cloudflare bot protection 403s a default urllib UA; present a browser UA like a real client does.
UA = {"User-Agent": "Mozilla/5.0 (smoke)"}
req = urllib.request.Request(sys.argv[1] + "/pkg/spacegame_wasm_bg.wasm", headers=UA)
data = urllib.request.urlopen(req, timeout=25).read()
def uleb(d, i):
    r = s = 0
    while True:
        b = d[i]; i += 1; r |= (b & 0x7f) << s; s += 7
        if not (b & 0x80): break
    return r, i
i = 8
while i < len(data):
    sid = data[i]; i += 1
    size, i = uleb(data, i)
    if sid == 4:  # Table section
        j = i
        _, j = uleb(data, j)   # count
        j += 1                 # elemtype
        flag = data[j]; j += 1
        mn, j = uleb(data, j)
        mx = uleb(data, j)[0] if flag == 1 else None
        sys.exit(0 if (flag == 0 or (mx is not None and mx > mn)) else 1)
    i += size
sys.exit(1)
PY
echo "    ok: wasm table growable"

echo "==> smoke 1b: every wasm import is defined by the served glue (no LinkError on boot)"
# The wasm-bindgen glue (.js) must define EVERY import the .wasm declares. If a deploy serves a NEW wasm
# (a new web-sys/js call adds an import) against an OLD/cached glue, the browser fails instantiation with
# "import requires a callable" and the page never boots — invisible to curl and to `cargo test`. (This
# shipped once: adding window.performance introduced __wbg_performance that a cached glue lacked.) We parse
# the wasm import section and assert each import name is defined in the served glue + its snippet modules.
python3 - "$BASE" <<'PY' || fail "a wasm import is not defined by the served glue -> the browser would LinkError on boot (stale/mismatched glue)"
import sys, re, urllib.request
base = sys.argv[1]; UA = {"User-Agent": "Mozilla/5.0 (smoke)"}
def fetch(p, b=False):
    r = urllib.request.urlopen(urllib.request.Request(base + p, headers=UA), timeout=25)
    return r.read() if b else r.read().decode("utf-8", "ignore")
wasm = fetch("/pkg/spacegame_wasm_bg.wasm", True)
defs = fetch("/pkg/spacegame_wasm.js")
for sp in set(re.findall(r"\./(snippets/[^\"']+\.js)", defs)):
    try: defs += fetch("/pkg/" + sp)
    except Exception: pass
def uleb(d, i):
    r = s = 0
    while True:
        b = d[i]; i += 1; r |= (b & 0x7f) << s; s += 7
        if not (b & 0x80): break
    return r, i
i = 8; imports = []
while i < len(wasm):
    sid = wasm[i]; i += 1
    size, i = uleb(wasm, i); end = i + size
    if sid == 2:  # import section
        n, j = uleb(wasm, i)
        for _ in range(n):
            mlen, j = uleb(wasm, j); j += mlen
            flen, j = uleb(wasm, j); fld = wasm[j:j+flen].decode("utf-8", "ignore"); j += flen
            kind = wasm[j]; j += 1
            if kind == 0: _, j = uleb(wasm, j)          # func typeidx
            elif kind == 3: j += 2                       # global: valtype+mut
            else: break                                  # table/mem imports unused here
            imports.append(fld)
        break
    i = end
missing = [f for f in imports if f not in defs]
print("    parsed %d wasm imports; %d undefined in glue" % (len(imports), len(missing)))
for f in missing[:20]: print("      MISSING:", f)
sys.exit(1 if missing else 0)
PY
echo "    ok: all wasm imports satisfied by the served glue"

echo "==> smoke 2/3: served by ce-serve WITH the mesh bridge injected"
page=$(curl -s -m 15 -A "Mozilla/5.0 (smoke)" "$BASE/")
[ -n "$page" ] || fail "page $BASE/ empty"
# ce-serve injects <script src="/__ce/mesh-bridge.js"> into every HTML page it serves. Its ABSENCE means
# the page is NOT served by ce-serve (e.g. straight from the hub) -> the browser gets no window.__ceNode
# -> no transport -> "player id local" -> no ship. This is the exact failure that shipped; assert it gone.
echo "$page" | grep -q "/__ce/mesh-bridge.js" || fail "page is not served by ce-serve (no /__ce/mesh-bridge.js bridge injected -> the browser would have no mesh transport)"
sid=$(curl -s -m 15 -A "Mozilla/5.0 (smoke)" "$BASE/ce/status" | grep -oE '"node_id":"[0-9a-f]+"' | head -1) || true
[ -n "$sid" ] || fail "/ce/status returned no node_id"
echo "    ok: ce-serve serving + mesh bridge injected; node $sid"

# DECENTRALIZED MODEL: spacegame has no relay game authority — the active PLAYERS are the server (each
# runs the full Sim and they reconcile by quorum; see NETCODE.md). The relay keeps only ce-net TRANSPORT
# plus ONE warm genesis SEED replica so a region near origin is never cold. This step proves that seed +
# the live public bridge: publish a join on the genesis `/in` and assert a `/state` carrying our ship
# comes back. (The player-hosted cross-sector path — and the teleport fix — is covered deterministically
# by the `spacegame::replica` unit tests, which a deploy gate can't drive through a real browser.)
echo "==> smoke 3/3: the genesis seed answers a join with our ship over the live /ce SSE bridge"
python3 - "$BASE" <<'PY' || fail "joining player's ship never arrived over the public /ce SSE bridge (genesis seed down, or the no-ship bridge regression)"
import sys, json, binascii, threading, urllib.request
base = sys.argv[1]
UA = {"User-Agent": "Mozilla/5.0 (smoke)"}
def get(path):
    return urllib.request.urlopen(urllib.request.Request(base + path, headers=UA), timeout=15)
me = json.load(get("/ce/status"))["node_id"]
def post(path, obj):
    urllib.request.urlopen(urllib.request.Request(
        base + path, data=json.dumps(obj).encode(),
        headers={"content-type": "application/json", **UA}), timeout=15).read()
post("/ce/mesh/subscribe", {"topic": "ce-game/spacegame/0_0/state"})
post("/ce/mesh/publish", {"topic": "ce-game/spacegame/0_0/in",
     "payload_hex": binascii.hexlify(b'{"t":"join","name":"smoke"}').decode()})
found = [False]
def stream():
    r = get("/ce/mesh/messages/stream")
    for raw in r:
        line = raw.decode(errors="ignore").strip()
        if not line.startswith("data:"):
            continue
        try:
            m = json.loads(line[5:])
        except Exception:
            continue
        if "0_0/state" not in m.get("topic", ""):
            continue
        try:
            snap = json.loads(binascii.unhexlify(m["payload_hex"]))
        except Exception:
            continue
        if any(s.get("id") == me for s in snap.get("ships", [])):
            found[0] = True
            return
t = threading.Thread(target=stream, daemon=True)
t.start()
t.join(12)
sys.exit(0 if found[0] else 1)
PY
echo "    ok: ship for the joining node delivered through the live bridge"

echo "SMOKE PASS: $BASE boots and the live browser data path delivers a player's ship end to end."
