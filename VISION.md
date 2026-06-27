# Spacegame — raw requirements (verbatim)

Every instruction Leif has given for spacegame, reproduced **exactly as written** (typos, casing and
spacing preserved), in order. This is the unedited source of intent; the implementation, `README.md`,
`SCALING.md`, `SYSTEMS.md` and the e2e harness (`../e2e/SPACEGAME-E2E.md`) are derived from it. Do not
paraphrase or "correct" the quotes below.

---

### 1 — initial brief

> develop spacegame. infinate procedural map. physics. focus on backend and deployment to make it support 1000000+ users at once with  recursive aabb optimized for latency. its a 2d game.

### 2 — rename

> rename game-spacegame to just spacegame. very annoying. also rename gitrepo and references

### 3 — hot reload + more weapons

> Keep
>   all game systems hot reloadable for production so that i can develop the game, add featurs and items, tweek things, add items, expand tech tree, tweak shaders during ppeople are playing and it hot reloads for them
>   while they are playing my changes are applied instantly.
>
>  More weapons! Homing missile launchers! railguns! Lasers!

### 4 — working style

> Dont verify the code ever. just keep building and hallucinating code until its just how i vision it to be

### 5 — working style

> dont compile it or test it

### 6 — working style

> just build

### 7 — large-scale rigidbody physics, factions, fault tolerance, nested AABBs

> Implement advanced 2d rigidbody physics which runs on large scale. they run on my node and very closeby nodes for high precision high framerate physics - the further out hte lower precision is
> registered for other players.
> Your swarm and factory and faction you build,
> You get resources and build and upgrade.
> Your faction freely uses your resources automatically to build stuff and uprade for you - its idle and always simulated in the background even when your away - your faction is always alive.
>
>  - What spacegame makes us optmize is both crosscompatability with gpu support but also fault tolerance and how we handle critical systems at scale when compute devices can disconnect at any moment. devices of players who are close in game must have high precision replicas of the world each so that if one of them exists suddenly the others can take over and the high precision map is copied to the next best to satisfy the replication constraint. This is such an important system and spacegame is a very good development project for it. Our recursive aabbs should be able to hold other recursive aabbs and dynamicly follow players, ships, debris, asteroids, planets and objects around.

### 8 — e2e tests

> Write e2e tests with real vms to test all systems working otgether on fresh machines - installing and running ce on both native and wasm only mobile phones - the game should be mobile in browser also and later mobile natively

### 9 — this document

> document everything ive said, raw

### 10 — factions are NPC ships you command

> Yeah we need to keep track of factions. Factions are actual npc ships under your command.

### 11 — more weapons

> Add weapons like homing missile launchers and different laser weapon types.

### 12 — local-first, replicated authority (anti-cheat + redundancy)

> Since local state is computed on my local node on ce-net there should be zero delay for whats going on around me and my node should auto sync backend with other nodes - multiple replicas simulating the same thing so that no one can cheat and for redundancy.

### 13 — missiles are real and explode

> Missiles should be real simulated and have physics and go and explode

### 14 — free-form building system + recursive blueprints

> Free form building system - place shapes and objects like weapons, turrens, guns, thrusters, armor, structure blocks. Then inside structure blocks you can place command centers and radars and sensors. Armor and structure should have many different shapes and be customizable. Rectangle of variable height and width, triangle of variable height and width and angles etc. Lots of different objects for structure and then upgrades whcih are also placable inside structures. Objeccts and items are hot reloadable. Storage tank and container are also object types of all different kinds of shapes. First make the dynamic shape system which block types of variable shapes can reuse.  Recursive blueprint system, blueprints can have blueprints which can have blueprints. And bluepritnts can define settings and customization and be dynamic and we resolve blueprints during runtime.

### 15 — everything hot-reloadable + recursive procedural ship generation

> Shapes and blueprints and objects and items and weapons everything should be hot reloadable. Make a recursive procedural ship generation system which takes blueprints and a system which defines how blueprints can and should be placed and then makes a bunch of different ship designs with cool hapes and functions.

### 16 — recursive shape blueprints, materials, GPU flattening

> So shapes should also have their own shape blueprints and system for defining, saving and making new shapes. Also recursvive and shapes can be complicated with lots of shape details because its recursvie. shapes auto build aabb on root for collision and physics. Shapes will create the graphics eventually and each shape can have  materials. it needs to be easily sent to gpu with proper flatting and memory defining with aabb, material and all shapes.

### 17 — a whole ship collapses to one shape blueprint (render + cached AABB)

> So the entire ship should not be able to be converted into one single shapeblueprint for rendering the entire ship + getting an aabb of it efficently and cashing it

(Read as: the entire ship should now be convertible into one single ShapeBlueprint for rendering the
whole ship in one pass, getting its AABB efficiently, and caching it.)

### 18 — tests + frontend interaction architecture

> Write tests for the physics , destruction, weapons, systems and other stuff. Prepare how the frontend (native application, browser on desktop, browser on mobile device) will interact with it efficently when its distributed and fault tolerant and insanely scalable.

### 19 — browser frontend (Rust WASM + wgpu, using the SDK), and test everything

> Start writing the browser frontend for spacegame.

> Test everything.

> browser frontend should be wasm rust wgpu

> using the rust spacegame backend skd

> sdk

### 20 — visual verification + screenshots + native app, web/native coexisting

> Make a system so that you can visually verify it and take screenshots and drive design development. Write a native app to access spacegame locally also - web and browser should coexist and work together seamlessly.

### 21 — seamless player-following world, planet scale, smarter AI, physics-based mining & building

> make chunks / sections seamless - no lag transitions and ideally no transition required at all because its smarter than now and optimizes it - our own player server must of course follow us. and the current chunk system is not capable of what we want to achieve - a huge open world with real earth size playing as a human - so the sections and chunking needs to be a lot more efficient and dynamic and follow the player ships and factions and simulate them for other players - if each player takes care of itself and its environment for other players it will scale very well. recursive aabb which follows the player and its ships and factions.
>
> Later used for broad phase physics.
>
> reporting system
> brutal karma and trust - but you still have access to trana so you can build it up again but it takes time and energy
>
> say "By playing this you donate your compute to science!"
>
> Make ai smarter with objectives and goals
> Generate procedural asteroids you can mine to get "alloys" - you destroy them and have to pick it up its not instant - they glide towards you and you collect them and its satisfying.
>
> make it more physics based instead of this arcade.
> real build system - build your ships and blueprints.
>
>  - look in github in ce-net at all of the spacegame directories and clone them in and work on them with the stuff i said.
