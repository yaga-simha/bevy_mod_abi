# Stage 5: Cute 3D Slime Meadow (`winit` + `wgpu`)

An interactive **3D toy-diorama window** powered by `winit` + `wgpu` showcasing zero-serialization runtime dynamic mod loading, hot-reloading, and unloading across **both Rust (`.cdylib`) and pure ISO C99 (`.dylib`/`.so`/`.dll`)** guest mods!

Instead of a terminal TUI, 32 kawaii pastel bunny-slimes (complete with floppy ears, glossy espresso eyes, catchlight sparkles, squash-and-stretch jelly bodies, and rosy blush cheeks) live on a floating 3D meadow island around a giant spotted plush mushroom, puffball sakura/mint trees, flowers, and drifting clouds.

---

## Exposed 3D C-ABI Components (`#[derive(ModExposable)]`)

- `Position { x, y, z }` — 3D world-space coordinates on the meadow island.
- `Velocity { x, y, z }` — 3D linear velocity vector.
- `Scale3D { x, y, z }` — Non-uniform volume-preserving jelly squash-and-stretch.
- `ColorRgba { r, g, b, a }` — Pastel macaron body coat color.
- `CuteSpirit { yaw, ear_wiggle, blush }` — Facing angle, bunny-ear flop angle, and cheek blush intensity.

---

## Avian 3D Physics Engine & Capsule Colliders

The simulation leverages **Avian 3D (`avian3d` v0.8.0)**, Bevy's premier ECS-native physics engine:
- **Simpler Capsule Mesh & Colliders**: Each bunny-slime is simulated using an optimal, watertight **Capsule collider** (`Collider::capsule(0.42, 0.36)`) paired with a lightweight, seamless procedural capsule mesh (`build_capsule(0.48, 0.28, 22, 7)`). This provides far superior contact stability, eliminates edge-catching artifacts common with spherical colliders, and reduces vertex overhead.
- **Island Environment Colliders**: Static cylinder colliders protect the central giant plush mushroom trunk (`Collider::cylinder(1.35, 3.0)`) and the floating island grass bed foundation (`Collider::cylinder(MEADOW_RADIUS + 0.5, 1.0)`).
- **Avian Contact Manifold Tracking**: Active contact states are tracked via `CollidingEntities`. Slimes react to both world obstacles and peer bunny-slimes with restitution ($e = 0.88$), friction ($0.20$), locked rotation axes, and collision impulse pops.
- **C-ABI Sync Bridge**: The host runtime bidirectionally synchronizes the `Position` and `Velocity` C-ABI components with Avian's `Position` and `LinearVelocity` ECS components, allowing both Rust and pure C99 mods to influence physical trajectories.

---

## Pairwise Jelly Collisions & Elastic Bouncing

All bunny-slimes possess physical pairwise collision detection and lively jelly bounce responses:
- **Elastic Momentum Reflection**: When two slimes bump into each other, they bounce apart with high jelly restitution ($e = 0.88$) plus a playful impulse pop.
- **Vertical Startle Boing & Squash**: Collisions trigger an upward vertical hop (`+1.2` velocity) and non-uniform volume-preserving squash deformation.
- **Emotional Reaction**: Upon bonking into a fellow slime, both slimes instantly blush bright rosy pink (`blush = 1.0`) and flap their floppy bunny ears!
- **High-Capacity Instanced Rendering**: The instanced GPU pipeline comfortably supports scaling from small parties up to 1,000+ colliding slimes in real-time.

---

## The 3 Cute Dynamic 3D Mods

| Key | Mod | Language | Mutated Components | Cute 3D Behavior |
|:---:|---|---|---|---|
| `1` | **Jelly Boing Hop** (`mod_boing`) | Rust `.cdylib` | `Position`, `Velocity`, `Scale3D`, `CuteSpirit` | Joyful 3D rhythmic hops across the meadow! Squashes into a plump pancake on landing, stretches tall at jump launch, flops bunny ears with vertical velocity, and blushes bright pink on every boing. |
| `2` | **Pastel Carousel Waltz** (`mod_carousel`) | Rust `.cdylib` | `Position`, `Velocity`, `Scale3D`, `ColorRgba`, `CuteSpirit` | Steers slimes onto two counter-rotating concentric 3D merry-go-round rings with carousel bobbing waves, synchronized ear dances, and animated pastel macaron rainbow hues! |
| `3` | **Cozy Cuddle Huddle** (`mod_cuddle_c99`) | **Pure ISO C99** | `Position`, `Velocity`, `Scale3D`, `CuteSpirit` | Every slime waddles cozily inward into a sunflower spiral cuddle pile around the central giant mushroom, facing inward with droopy happy ears, synchronized breathing squishes, and maximum rosy blushes. |
| `R` | **Hot-Reload Active Mod** | Rust / C99 | *Active mod* | Re-copies the latest `.dylib`/`.so`/`.dll` into an isolated shadow cache and hot-swaps the mod live while the 3D window stays open! |
| `U` | **Unload Active Mod** | — | — | Cleanly unloads the dynamic library (`dlclose`) via atomic tombstoning; slimes smoothly return to their signature pastel coats and peaceful meadow stroll. |

---

## Controls

| Input | Action |
|---|---|
| `1` | Load **Jelly Boing Hop** (`mod_boing`, Rust) |
| `2` | Load **Pastel Carousel Waltz** (`mod_carousel`, Rust) |
| `3` | Load **Cozy Cuddle Huddle** (`mod_cuddle_c99`, Pure C99) |
| `+` / `=` / `B` | **Spawn More Slimes** (+8 bunny-slimes into the meadow) |
| `-` | **Remove Slimes** (-8 bunny-slimes, down to 1 minimum) |
| `R` | **Hot-Reload** active mod from disk (recompile while window is open!) |
| `U` / `0` | **Unload** active mod (return to peaceful Meadow Stroll) |
| `Space` | **Pause / Resume** simulation |
| `O` | Toggle smooth **Auto-Orbit** 3D camera |
| `Left-Click + Drag` or `WASD` / `Arrows` | Orbit 3D camera around the meadow |
| `Mouse Wheel` | Zoom camera in / out |
| `Q` / `Esc` | Cleanly unload active mod and close window |

---

## Running the 3D Example

### Interactive 3D Window (`winit` + `wgpu`)
```bash
# Build all 3 guest mods (Rust + C99) and launch the 3D meadow window:
make example-5
# or directly via Cargo (auto-builds any missing mod on demand):
cargo run -p example_05_cute_3d_winit
```

### Automated Headless 3D Verification
```bash
make example-5-test
# or:
cargo run -p example_05_cute_3d_winit -- --test-mode --ticks 45
```
