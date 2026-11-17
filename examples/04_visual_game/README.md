# Example 04: 2D Visual Kinetic Arena — Shadow-Copy Runtime Hot-Reloading

**Concept taught:** Interactive runtime hot-reloading and unloading of native `.cdylib` mods using the core host loader's `LoadOptions { shadow_copy: true, .. }` (`ModWorldExt` / `ModAppExt`) and Bevy `Message` lifecycle streams (`ModLoaded`, `ModUnloaded`, `ModSystemFaulted`).

---

## Overview

A 2D kinetic particle simulation running inside Bevy ECS (`#![forbid(unsafe_code)]`) and rendered in a terminal Braille canvas via `ratatui` and `crossterm`. Three standalone Rust guest `.cdylib` mods (`#![forbid(unsafe_code)]`, zero `bevy` dependencies) can be loaded, hot-reloaded from disk while recompiling, and unloaded cleanly at runtime:

- **Core Shadow-Copy Loader (`L-03`):** Passing `LoadOptions::new().with_shadow_copy(true)` copies each `.dll`/`.so`/`.dylib` to an isolated temporary file prior to `dlopen` / `LoadLibraryW` and unlinks it automatically after `dlclose`, avoiding OS file locks and inode caching without custom host file-management boilerplate.
- **Schedule Phasing:** Guest systems run in `Update`; host integration and wall reflections run in `PostUpdate`.
- **Per-World Guest State (`L-02`):** Each guest mod uses `GuestPlugin` + `export_mod!(plugin: ...)` and `app.system(...)`, keeping frame counters inside its per-`World` `GuestInstance` with zero process globals.

---

## Guest Mods

| Key | Crate | Declared Access | Behavior |
|:---:|---|---|---|
| `1` | `mod_gravity` | `write::<Position>()`, `write::<Velocity>()` | Applies downward acceleration (`g = 40.0`) and elastic floor/wall restitution. |
| `2` | `mod_rainbow` | `write::<ColorRgba>()`, `read::<Position>()` | Animates an HSV color wave from spatial coordinates and per-instance tick state. |
| `3` | `mod_vortex` | `write::<Position>()`, `write::<Velocity>()` | Applies centripetal pull toward `(60.0, 30.0)` and tangential spiral acceleration. |
| `u` | *(Unload)* | — | Calls `app.unload_mod(mod_id)`, excising guest systems and restoring neutral colors. |
| `r` | *(Reload)* | *Active mod* | Unloads and reloads via `LoadOptions::new().with_shadow_copy(true)`. |

---

## Running

### Interactive Terminal UI
```bash
cargo run -p example_04_visual_game
```

Controls: `[1]` Gravity, `[2]` Rainbow, `[3]` Vortex, `[U]` Unload, `[R]` Hot-Reload, `[Space]` Pause, `[Q / Esc]` Quit.

### Automated Headless Verification
```bash
cargo run -p example_04_visual_game -- --test-mode --ticks 30
```
