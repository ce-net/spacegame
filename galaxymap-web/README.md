# Live Galaxy Map

A real-time, zoomable map of the whole adaptive galaxy — watch cells **split** across the planet, the
**frontier** creep outward, hosts light up, and load breathe as crowds move. It is the visual face of
[`src/galaxymap.rs`](../src/galaxymap.rs): it folds the same `/galaxy` shape commits + `/load` frames
and renders them.

Single self-contained page — no build step, no wasm. Canvas 2D, ~1 file.

## Run it

**Demo (now, no mesh):** just open `index.html` in a browser. It boots a synthetic galaxy that grows,
splits, merges, and pushes its frontier — driven through the *exact same* `applyShape` / `applyLoad`
path the live data uses, so what you see is the real thing fed by a stand-in.

**Live (over the real mesh):** serve it through ce-serve so the page gets the `window.__ceNode` bridge:

```bash
CE_NODE_URL=http://127.0.0.1:8844 \
CE_SERVE_ROOT=~/ce-net/spacegame/galaxymap-web CE_SERVE_PORT=8792 ce-serve
# open http://<host>:8792/
```

It subscribes to `ce-game/spacegame/galaxy` (and `…/<cell>/load`), decodes each message's `payload_hex`
JSON, and animates the reshapes as they happen. The source indicator (bottom-right) shows `live mesh`
once a node is reachable, else `demo`.

## What you're looking at

- **Cells** — quadtree leaves; fill/border heat goes cool→hot with load (the closer to a split, the
  hotter), faint danger tint underneath. Zoom in to see each cell's host + player count.
- **Pulses** — a cyan ring expands when a cell splits into four; an amber ring implodes on a merge.
- **Frontier** — green edges mark charted cells bordering the unsimulated void; they advance as players
  explore.
- **HUD** — leaf-cell count, players, host nodes, max depth, deepest frontier ring.
- **Event feed** — a live ticker of splits / merges / migrations / frontier pushes.

Drag to pan, wheel to zoom, `fit` to auto-frame the active galaxy, `pause` to freeze.
