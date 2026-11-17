# Example 05: Cute 3D Slime Meadow (`winit` + `wgpu` + `avian3d`)

**Concept taught:** Integrating `bevy_abi_mod` into a custom `winit` + `wgpu` 3D rendering loop and `avian3d` physics world with live-swappable Rust (`.cdylib`) and ISO C99 (`.dylib`/`.so`/`.dll`) guest mods.

---

## Overview

Thirty-two 3D bunny-slime entities live on a floating meadow island simulated by Bevy ECS + `avian3d` capsule colliders and rendered via a custom `wgpu` instanced pipeline. Five `#[repr(C)]` `ModComponent` types (`cute3d::Position`, `cute3d::Velocity`, `cute3d::Scale3D`, `cute3d::ColorRgba`, `cute3d::CuteSpirit`) are exposed across the `ABI 0.2.0` wire contract:

- **Core Shadow-Copy Hot-Reloading:** Loading and reloading mods uses `app.load_mod_with(&path, LoadOptions::new().with_shadow_copy(true))`, allowing guest libraries to be recompiled on disk while the 3D window stays open.
- **Rust + ISO C99 Guest Parity:** `mod_boing` and `mod_carousel` use `#![forbid(unsafe_code)]` `bevy_abi_mod_guest` (`GuestPlugin`, `app.system(...)`), while `mod_cuddle_c99` is compiled directly from ISO C99 against `include/bevy_abi_mod.h` with per-instance `ModState*` heap state.

---

## Guest Mods

| Key | Mod | Language | Mutated Components | Behavior |
|:---:|---|---|---|---|
| `1` | `mod_boing` | Rust `.cdylib` | `Position`, `Velocity`, `Scale3D`, `CuteSpirit` | Rhythmic 3D hops, volume-preserving squash-and-stretch, ear flop, and landing blush. |
| `2` | `mod_carousel` | Rust `.cdylib` | `Position`, `Velocity`, `Scale3D`, `ColorRgba`, `CuteSpirit` | Two counter-rotating concentric 3D carousel rings with bobbing waves and pastel hue cycling. |
| `3` | `mod_cuddle_c99` | ISO C99 | `Position`, `Velocity`, `Scale3D`, `CuteSpirit` | Sunflower spiral cuddle huddle around the central mushroom with synchronized breathing squishes. |
| `R` | *(Reload)* | Rust / C99 | *Active mod* | Reloads via `LoadOptions::new().with_shadow_copy(true)`. |
| `U` | *(Unload)* | — | — | Calls `app.unload_mod(mod_id)` and restores `HomeStyle` defaults. |

---

## Running

### Interactive 3D Window (`winit` + `wgpu`)
```bash
cargo run -p example_05_cute_3d_winit
```

Controls: `[1]` Boing, `[2]` Carousel, `[3]` C99 Cuddle, `[+]` / `[-]` Spawn/Remove Slimes, `[R]` Hot-Reload, `[U]` Unload, `[Space]` Pause, `[O]` Auto-Orbit, `[Left-Drag]` Orbit, `[Scroll]` Zoom, `[Q / Esc]` Quit.

### Automated Headless 3D Verification
```bash
cargo run -p example_05_cute_3d_winit -- --test-mode --ticks 45
```
