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

## B5 — Editor doesn't actually change the ship on exit
Verbatim:
> "ANd why doesnt the ship actually become what you designed in the editor when you leave the editor!!!"
> (earlier) "the editor does actually have a affect and actually modify my ship."

Status: ❌ Apply IS wired (`take_apply()` → `ClientMsg::FitDesign` → `host.schedule_home`) and
`fit_design` stores the design JSON in `hull` so `resolve_hull` can draw it — but the ship visibly does
not become the design in game. Needs investigation (fit rejected? not reaching the home sim? predicted
sprite drawn from a default? render not reading the fitted hull?).

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

## Ops requests (verbatim)
> "Push and merge everything"
> "and then commit push and deploy"

Status: ✅ Coords branch merged to `development`; SDK/render/wasm/native committed + pushed; seed +
frontend deployed.
