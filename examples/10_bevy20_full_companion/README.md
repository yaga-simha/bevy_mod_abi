# Example 10: Bevy `0.20.1` Full Companion Showcase (`10_bevy20_full_companion`)

End-to-end, `#![forbid(unsafe_code)]`, headless-testable Bevy `0.20.1` companion modding showcase
combining every Phase 3 companion capability with a standalone zero-Bevy Rust C-ABI guest mod
(`guest_mod/`).

## Capabilities Demonstrated

1. **3D Camera, Lights, and `PanOrbitCamera` (`ModCameraLightBridgePlugin`)**
   - Spawns a `ModScoped` 3D camera rig with `FfiCamera3d` (`Hdr`, `Tonemapping::AgX`,
     `DebandDither`, `ScreenSpaceTransmission`), `FfiPanOrbitCamera`, `FfiDirectionalLight`, and
     `FfiPointLight`.
   - Guest Mod A (`companion_core_mod`) queries and animates `FfiPointLight` intensity across the
     C-ABI boundary, synced automatically into Bevy's native `PointLight`.
2. **Custom `.wesl` `SpriteMaterial` & `ExtendedMaterial2d` (`ModWeslMaterialPlugin`, `ModSpriteMaterialPlugin`, `ModSubAppPlugin::<RenderApp>`)**
   - Registers [`assets/shaders/companion_pulse.wesl`](./assets/shaders/companion_pulse.wesl) and an
     `AlignedVec`-backed `ShaderBuffer`.
   - Spawns both a unified `Sprite` + `Mesh2d` + `SpriteMaterial<ModWeslSpriteExtension2d>` and a
     first-class `ModExtendedSpriteMaterial2d`, verifying headless `MaterialExtension2d::specialize`,
     `Material2d::specialize`, and `RenderApp` `DirtySpecializations` extraction.
3. **Flattened `PointerPress` Picking Observers (`ModPickingBridgePlugin`)**
   - Guest Mod A installs runtime entity observers via `cmd_observe` on
     `bevy_picking::events::PointerPress` (`FfiPointerPress`).
   - Triggering a synthetic Bevy `0.20` `PointerPress` forwards cleanly to the guest callback, which
     mutates `companion::Beacon` (`press_count`, `last_button`, `energy`) and boosts
     `companion_core_mod::OrbitalBeacon`.
4. **Spatial `AudioPlayer` (`ModAudioBridgePlugin`)**
   - Spawns a `ModScoped` spatial emitter carrying `FfiAudioPlayerSpec` +
     `FfiAudioPlaybackSettings`, materializing Bevy `AudioPlayer<AudioSource>` + `PlaybackSettings`
     headlessly without an OS audio device, and verifies runtime volume/mode updates.
5. **Retained UI (`Val::Rem` + Headless `TabList` HUD + Feathers `ThemeContext`, `ModUiBridgePlugin`)**
   - Spawns a `ModScoped` HUD panel using `FfiUiNode` (`Val::Rem`, `FixedNode`, elliptical
     `FfiBorderRadius`), `FfiThemeContext::FLOATING`, `FfiUiTextStyle` (`FfiFontSize::Rem`), and a
     headless `FfiTabList` / `FfiTab` / `FfiSelectedTab` control.
   - Guest Mod A observes `FfiValueChange` via `cmd_observe` and reacts when Bevy triggers
     `ValueChange<Option<Entity>>` on tab selection.
6. **Per-Mod Resource Quotas & Live BRP Telemetry (`ModBudgetPolicy`, `ModTelemetryPlugin`)**
   - Configures `ModBudgetPolicy` via `AbiModPlugin::with_budget_policy(...)` and exposes live
     per-system `p50_us` / `p99_us` metrics through `ModBudgetTelemetry`, `DiagnosticsStore`, and
     Bevy Remote Protocol (`bam/list_mods`, `bam/system_metrics`, `bam/component_schemas`).
7. **Versioned Quick-Save / Mod-Disable / Quick-Load Recovery (`ModSaveGamePlugin`)**
   - Saves persistent (`!ModScoped`) studio beacons carrying Host (`companion::Beacon`), Mod A
     (`companion_core_mod::OrbitalBeacon`), and Mod B (`companion_fx_mod::PlasmaAura`) components
     while excluding all `ModScoped` presentation/HUD/camera/light/audio/sprite entities.
   - Verifies JSON & binary archive roundtrip, atomic rejection under
     `ModSaveLoadPolicy::StrictRequireMods` when Mod B is disabled, and clean recovery under
     `ModSaveLoadPolicy::StripMissingModComponents` preserving Host + Mod A state byte-for-byte.

## Guest Mod Layout (`guest_mod/`)

`guest_mod/` is a standalone `#![forbid(unsafe_code)]` Rust `cdylib` crate with **zero `bevy*`
dependencies**, compiled into two distinct native plugins:
- Default (`companion_core_mod`, Mod A): owns `companion_core_mod::OrbitalBeacon`, picking & UI
  `cmd_observe` observers, beacon simulation, and `FfiPointLight` pulsing.
- `--features mod-b` (`companion_fx_mod`, Mod B): owns `companion_fx_mod::PlasmaAura` and an
  ephemeral `ModScoped` entity to exercise multi-mod telemetry and mod-disable save recovery.

## Running & Testing

```bash
# Run headless showcase binary / example
cargo run --manifest-path examples/10_bevy20_full_companion/Cargo.toml --example 10_bevy20_full_companion -- --headless

# Run deterministic headless tests (example target + integration smoke suite)
cargo test --manifest-path examples/10_bevy20_full_companion/Cargo.toml --example 10_bevy20_full_companion
cargo test --manifest-path examples/10_bevy20_full_companion/Cargo.toml

# Lint both host and zero-Bevy guest crates
cargo clippy --manifest-path examples/10_bevy20_full_companion/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path examples/10_bevy20_full_companion/guest_mod/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path examples/10_bevy20_full_companion/guest_mod/Cargo.toml --all-targets --features mod-b -- -D warnings
```
