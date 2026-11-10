# Stage 4: 2D Visual Kinetic Arena with Runtime Mod Reloading

This example demonstrates the culmination of the `bevy_abi_mod` architectural progression: an interactive, real-time 2D kinetic particle simulation running inside Bevy ECS, rendered in terminal Braille characters via `ratatui` and `crossterm`, with dynamic runtime C-ABI mod loading, hot-reloading, and unloading.

---

## Key Architectural Concepts Demonstrated

1. **Zero Shared Rust ABI Assumptions**:
   Dynamic guest mods compile to standalone `.cdylib` binaries linking only `guest_sdk` and `abi_contract`. They depend on **no Bevy crates**, no host memory allocator, and no specific `rustc` compiler version.
2. **Sub-Pixel Terminal Rendering**:
   Terminal display powered by `ratatui`'s `Canvas` using Unicode Braille patterns (`Marker::Braille`). Each character cell maps to a $2 \times 4$ dot matrix, delivering fluid, high-resolution particle animation ($120 \times 60$ logical arena) directly in standard terminals without windowing or GPU driver prerequisites.
3. **Shadow-Copy Hot-Reloading Without File Locking**:
   The host `ModManager` stages dynamic libraries into a versioned, isolated temporary cache (`std::env::temp_dir()`). This avoids OS dylib file locking on Windows/macOS/Linux, allowing mod developers to recompile their mod source while the visual game is actively running and hot-reload changes on the fly.
4. **Atomic Tombstone System Lifecycle**:
   When unloading a mod, `app.unload_mod(mod_id)` deactivates the mod's systems via atomic tombstone flags and calls `bevy_abi_mod_shutdown` while memory is safely mapped, allowing safe mod swaps without rebuilding Bevy's schedule graph.
5. **Conflict-Free Schedule Phasing**:
   Foreign mod systems execute during Bevy's `Update` schedule phase. Host particle kinetics, wall reflection, and restitution run during `PostUpdate`, eliminating data race ambiguities and borrow checker conflicts.

---

## The 3 Distinct Dynamic Mods

| Key | Mod Name | Access Modes | Visual & Physical Behavior |
|:---:|---|---|---|
| `1` | **Gravity & Kinetic Bouncer** (`mod_gravity`) | `Position` (WRITE)<br>`Velocity` (WRITE) | Accelerates particles downward ($g = 40.0$). Particles crash into the floor and bounce elastically with kinetic energy restitution ($0.85$) and horizontal friction. |
| `2` | **Dynamic Rainbow Spectrum** (`mod_rainbow`) | `ColorRgba` (WRITE)<br>`Position` (READ) | Computes an iridescent animated color wave: maps 2D position and frame ticks into a rotating HSV phase spectrum, cycling particle colors continuously. |
| `3` | **Gravitational Cyclone Vortex** (`mod_vortex`) | `Position` (WRITE)<br>`Velocity` (WRITE) | Exerts centripetal gravitational pull toward the arena center $(60.0, 30.0)$ and tangential spiral acceleration, whipping particles into an orbital whirlpool accretion disk. |
| `u` | **Unload Active Mod** | *None* | Deactivates the active mod systems via atomic tombstone flags. Particles instantly revert to baseline linear drift and neutral coloring. |
| `r` | **Hot-Reload Active Mod** | *Same as active mod* | Stages a fresh shadow copy from disk and increments the reload counter, immediately applying newly compiled mod code. |

---

## Controls

| Key | Action |
|---|---|
| `1` | Load **Gravity & Kinetic Bouncer** mod |
| `2` | Load **Rainbow Spectrum** mod |
| `3` | Load **Gravitational Cyclone Vortex** mod |
| `u` / `U` | **Unload** active mod (revert to baseline drift) |
| `r` / `R` | **Hot-Reload** currently active mod from disk |
| `Space` | **Pause / Resume** simulation |
| `q` / `Esc` | **Quit** and restore terminal |

---

## Running the Game

### Interactive TUI Mode
```bash
# Build the 3 dynamic mods first:
make build-game-mods

# Launch the visual kinetic arena:
make example-4
# or:
cargo run --manifest-path examples/04_visual_game/Cargo.toml
```

### Automated / Headless Verification Mode
For CI/CD and non-interactive testing without opening a terminal GUI:
```bash
make example-4-test
# or:
cargo run --manifest-path examples/04_visual_game/Cargo.toml -- --test-mode --ticks 30
```
This runs a 6-phase programmatic test verifying baseline motion, gravity acceleration, rainbow color shifts, vortex orbital pull, hot-reloading, and unloading, exiting with code 0 on success.
