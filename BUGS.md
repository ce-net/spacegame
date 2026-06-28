# spacegame — open bugs & requests (verbatim)

Tracking doc. Each item records **what Leif wrote, verbatim** (in quotes), grouped into the distinct
bugs he flagged ("These are all seperate bugs you need to fix. note all of them verbatim how i wrote
them. document verbatim everything ive said"), plus current status. Verbatim text is never edited.

Status legend: ❌ not started · ◐ partial/groundwork · ✅ done+deployed · ❓ cannot reproduce

---

## B1 — Coordinate system: galaxy 1:1, kill sector-local handling
Verbatim:
> "The "sectors" are still there for bullets and particles etc! You need to CHANGE how coordinates are handled properly for EVERYTHING! And make sure it can never be handled like it was handled before ever again - the galaxy 1:1 scale system is whats supposed to be use from now"
> "No convert to galaxypos - thats the correct approach and you shouldve done it from the start. Its the right approach. Do all 1-4 and then compile and come back to me. stop complaining."
> "Do it. use the proper api so that in case we change the postion system again in the future like use recursive aabb instead of grid cells its only a matter of changing hte galaxypos struct and how it functions.. Yes do all of it right now yourself"
> "do it all dont come back until all errors are resolved and teh conversion is 100% completed"

Status: ◐ The SDK is 100% converted to `GalaxyPos` (ships/bullets/mines/pickups + snapshot + all
~180 sites), all math routes through GalaxyPos methods, 305 tests pass, deployed. BUT this was the
*type* groundwork only — it did NOT remove the sector seams (see B2), because the client still runs
separate per-sector sims (`replica::SectorHost`). The real seam fix is the SectorHost → single
player-anchored continuous frame change (stage 3), still TODO.

## B2 — Bullet/enemy seams at sector edges (THE seam bug)
Verbatim:
> "The seams for bullets at sector edges ARE STILL THERE."
> "you can visually see the seams because bullets just dissapear when they touch it and enemies stops chasing you"

Status: ❌ Root cause: `SectorHost` runs one `Sim` per sector; bullets live in a single sim's `bullets`
Vec and are not transited across the seam, and AI targets only ships within its own sim — so bullets
vanish and enemies stop chasing at the boundary. Fix = collapse SectorHost into ONE continuous
player-anchored sim (the anchor follows the player, re-bases as it moves), OR transit bullets+retarget
AI across sims. Architectural; not yet done.

## B3 — Visual parts palette (real-time, exact in-game look)
Verbatim:
> "Do the visual part previews palette. like i said."
> "the parts and blueprints shouldnt be prerendered they should be rendered in real time in the menu and look EXACTLY like they do in game."
> (earlier) "i want ot see previews of each shape / part and click and drag and have it have a ghost."

Status: ❌ Editor part picker is still a `<select>` dropdown. Must render each part in real time through
the same Scene/wgpu pipeline as the game (NOT prerendered raster thumbnails).

## B4 — Ship blueprint previews (real-time, exact) + proper submenus
Verbatim:
> "I also want ship blueprint previews - exactly how they will look in game it should look and proper submenus."

Status: ❌ Not started. Saved blueprints list is a dropdown; needs a real-time rendered preview of each
full ship + proper submenus.

## B5 — Editor doesn't actually change the ship on exit / design ends up on enemies
Verbatim:
> "ANd why doesnt the ship actually become what you designed in the editor when you leave the editor!!!"
> "Alright i see whats happening the blueprints i make only is set for the enemies for some reason."
> (earlier) "the editor does actually have a affect and actually modify my ship."

Status: ❌ Apply IS wired (`take_apply()` → `ClientMsg::FitDesign` (seq incremented) → `host.schedule_home`
with `player=me_id`) and `fit_design` stores the design JSON in `hull` so `resolve_hull` draws it. The
wasm runs `local_authority=true`, so the local ship draws from the snapshot render-map (`hull: s.hull`),
same path as enemies — meaning if the fit applied, the player's own ship WOULD show it. Hypothesis: the
"on enemies" is the new B6 enemy designs (raider/brawler/cruiser) being mis-attributed, and the real
bug is the fit not landing on the player's ship (timing: scheduled at `tick: cur` not `cur+INPUT_DELAY`
like Join; or `fit_design` returning false; or the home-sim id). Needs runtime debugging.

## B6 — Enemy ship designs
Verbatim:
> "Do the enemy ship designs."
> (earlier) "create a bunch of different ship designs for enemies."

Status: ✅ Added Raider / Brawler / Marauder Cruiser (+ existing Interceptor); marauders now
deterministically fit a varied hull via the player blueprint→loadout path, so a raid is a mixed fleet
with distinct silhouettes + stats. Tests pass, deployed.

## B7 — Camera: zoom + mouse glide + smooth follow
Verbatim:
> "add zooming and make the camera glide a bit off center smoothly based on where the mouse is. make the camera smoothly follow the player"

Status: ✅ Mouse-wheel zoom, off-centre mouse glide, smooth follow — deployed.

## B8 — Zoom step proportionality
Verbatim:
> "Zooming control speed should be proporsional to zoom squared to make it feel responsive"
> "No not squared actually just do step * zoom thats enough"

Status: ✅ Wheel step scales with current zoom (`step * zoom`). Deployed.

## B9 — Camera glide smoother + acceleration push-back
Verbatim:
> "The camera movement from mouse position must be much smoother maybe half of what it is right now and also be pushed back by acceleration so it feels faster when going faster - the faster you go the more it glides behind."

Status: ✅ Mouse glide halved (0.30→0.15) + slower easing; added a speed-proportional trail so the
camera lags further behind the faster you go. Deployed (needs feel-tuning confirmation).

## B10 — Asteroids not visible
Verbatim:
> "And the asteroids are not visible!"
> "No asteroids were never visible it isnt zoom"

Status: ❓ Cannot reproduce headlessly — a fresh auto-login at sector 0 renders dozens of asteroid
discs. Likely account/sector-specific or environment-specific. NEEDS: confirm whether they move when
you fly, which account/sector, and what's expected. (Separately fixed the zoom-out cell-walk range,
which the user says is NOT the cause.)

## B11 — Zoom-out doesn't scale the background
Verbatim:
> "zooming out does NOT scale the background how it should!"

Status: ❌ The starfield/nebula background does not scale with zoom (it isn't multiplied by the zoomed
`ppw`). Needs the background draw to use the zoomed scale.

## B12 — Controls / do the buttons do anything
Verbatim:
> "what are the conrols in the ui? am i supposed to see something happening when i press the buttons it says"

Status: ◐ Answered (1-6 buy upgrades and cost minerals/alloys; F1-F4 fleet; editor buttons are the only
clickable ones). Open question whether the build buttons feel responsive / give feedback.

## B13 — Background stars: fainter, smaller, react to zoom less than foreground
Verbatim:
> "Background stars should look more faint and be smaller and react somewhat due to zoom but less than foreground."

Status: ❌ Stars need to be drawn fainter + smaller, and scale with zoom only partially (a reduced zoom
response vs the foreground — a parallax-like feel). Related to B11.

## Session outcome (after the "fix B2/B3/B4/B5/B10/B11 at once" order)
Deployed: frontend `16fe95a7`, seed on new SDK. SDK 7459993 / render 5dd45fc / wasm ad6dfcb.
- B2 ◐ — bullets now traverse the seam (no longer dropped at the sector edge; expire by `die_at`).
  The deeper "enemies stop chasing across the seam" (cross-sim AI) + fully dynamic sectors still need
  the SectorHost→single continuous frame change.
- B3 ✅ — in-canvas parts palette with category-tab submenus, parts rendered real-time (verified live).
- B4 ◐ — live blueprint previews render (bottom row) from real parts; the row overlaps the HUD bars and
  needs repositioning.
- B5 ✅ — editor now fits via `apply_local_now(me_id, …)` so it lands on YOUR OWN ship only (verify
  in-game). Enemy ships keep their own designs (B6).
- B10 ◐ — asteroids brightened; could not reproduce "never visible" headlessly (renders fine).
- B11 ✅ — nebula + stars now scale with zoom (gently, less than foreground).
- Note: `deploy/deploy.sh seed` exits 255 at its install step; worked around with a manual
  `ce app install spacegame --yes && ce app daemon enable spacegame`. deploy.sh needs fixing.

## Directive (verbatim, 2026-06-28)
> "Yes the sectors should be dynamic and not chunks AND they should be able to transparently traverse chunks without noticing. the api should handle it for them. document verbatim.  Just to be clear: when you edit ships ONLY YOUR OWN SHIP is supposed to be modified. YOUR BUILDING YOUR OWN SHIP THE REST OF THE ENEMIES AND WORLD STAYS THE SAME. FIX ALL OF THESE ITEMS AT ONCE. DONT COMPILE AND VERIFY UNTIL THE END. ALL OF THE B2, B3, B4, B5, B10 AND B11 SHOULD ALL BE DONE AT ONCE. THIS IS AN ORDER"

Clarifications captured:
- B2: sectors must be DYNAMIC, not fixed chunks; bullets/ships/AI traverse boundaries transparently;
  the API (SectorHost) handles traversal so callers never notice a seam.
- B5: editing a ship modifies ONLY YOUR OWN ship — enemies and the world are untouched.

## Ops requests (verbatim)
> "Push and merge everything"
> "and then commit push and deploy"

Status: ✅ Coords branch merged to `development`; SDK/render/wasm/native committed + pushed; seed +
frontend deployed.
