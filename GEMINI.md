# Agent Guide: `bevy_mod_abi` (`bevy_abi_mod`)

Repository guide defining architectural invariants, 16-crate boundaries, verification commands, and Bevy ECS (`v0.20+`) standards for developing **`bevy_mod_abi`** (workspace `bevy_abi_mod`).

---

## 1. Project Identity
- **Name:** `bevy_mod_abi` (canonical top-level host facade crate; `bevy_abi_mod` is a minimal `<80` LoC `#[deprecated]` compatibility shim forwarding to `bevy_mod_abi`)
- **Goal:** Production-grade, zero-serialization, asymmetric C-ABI (`ABI 0.3.0`, `0x0003_0000`) native modding framework for Bevy ECS (`v0.20+`).
- **Core Principle:** **Zero Shared Rust ABI Assumptions**. Guest modules may use mismatched `rustc` versions, debug/release profiles, C99, C++17, or Zig across an `extern "C"` boundary without allocator crossover or serialization.

---

## 2. Workspace Crate Boundaries (16 Crates + `xtask`: `-sys` vs. `#![forbid(unsafe_code)]`)

| Crate / Path | Responsibility | Safety & Allowed Dependencies |
|---|---|---|
| `crates/bevy_abi_mod_sys` | `#[repr(C)]` ABI `0.3.0` descriptors, primitives, 68-slot `BamHostApi`, `offsets` (`OFF64_*`/`OFF32_*`), `ComponentUuid` (SipHash-2-4-128), `FfiResult` (`CYCLE_DETECTED`), `ModComponent`, `AbiPod`, `RawInsertable`. | `#![no_std]`, **ZERO external dependencies** (no `bevy`, no `alloc`). |
| `crates/bevy_abi_mod_macros` | `#[derive(ModComponent, AbiPod, GuestComponent)]`, zero-padding checks, hygienic VTables, `SCHEMA_FINGERPRINT`, `BAM_FIELDS`, `REQUIRED_COMPONENTS`, `#[guest_component(...)]`, `#[mod_component(..., guest)]`, & C/Zig codegen. | **`#![forbid(unsafe_code)]`**, `syn`, `quote`, `proc-macro2`, `proc-macro-crate`. **NO `bevy` dependency.** |
| `crates/bevy_abi_mod_trust` | Pure-Rust Ed25519 OpenSSH `SSHSIG`, CA cert, authority-scoped `SSHKRL` (`DEFAULT_KRL_SIGNER_PRINCIPAL`, `add_krl_signer_*`), `.bam_sig`, & hardened `.bampack` (`VerifiedBamPack::extract_to_dir`). | **`#![forbid(unsafe_code)]`**, pure-Rust crypto/parsing (`ssh-key`, `ssh-encoding`, `ed25519-dalek`, `sha2`, `signature`, `thiserror`). **NO `bevy` dependency.** |
| `crates/bevy_abi_mod_manifest` | `mod.toml` parser, hardened ID/entry-path & symlink validation (`validate_mod_id`, `validate_manifest_entry_path`), SemVer Kahn DAG sorter, & mismatch/quarantine types. | **`#![forbid(unsafe_code)]`**, `bevy_abi_mod_sys`, `semver`, `serde`, `toml`. **NO `bevy` dependency.** |
| `crates/bevy_abi_mod_sdk_gen` | Single-source ISO C99 (`include/bevy_abi_mod.h`), C++17, Zig, & component header generator (`BAM_EXPORT`, `BAM_RESTRICT`, `BAM_HOST_API_OFFSET_*`). | **`#![forbid(unsafe_code)]`**, `bevy_abi_mod_sys`. **NO `bevy` dependency.** |
| `crates/bevy_abi_mod_host_sys` | Host FFI (68/68 `HOST_API`, sealed `ffi/` + `ffi/validation.rs`, `HostIter`, `CascadeHierarchyCache`, `CommandArena`, `ModRelationGraph` incl. opt-in self-referential, `ModScheduleSetRegistry`, `BamSymbolTable`, `ModNamedObjects`, `GuestSystem`, observer `FallbackErrorHandler`/`ErrorContext::Observer`, `ModRegistry`, `ModHost`, `DROP_PAGES`). | `bevy_ecs`, `bevy_app`, `bevy_tasks`, `libloading`, `arc-swap`, `tracing`, `thiserror`, `bevy_abi_mod_sys`, opt. `bevy_abi_mod_trust` / `bevy_reflect`. Isolates all host `unsafe`. |
| `crates/bevy_abi_mod_host` | **Slim headless** Bevy plugin (`AbiModPlugin`, `ModAppExt`, `ModWorldExt` w/ `despawn_all_where` incl. `Allow<Disabled>`, `ComponentOptions::with_summary_tick()`, `ModCommandsExt`, `ModVisibleSets`, `ModUnloadHooks`, `ModBudgetPolicy`/`ModBudgetTelemetry`, `prelude`). | **`#![forbid(unsafe_code)]`**. **ZERO** `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify` deps. |
| `crates/bevy_abi_mod_bridges` | Opt-in Bevy `0.20` bridges (`std_types`+`Disabled`, `state` `SubStates`/`ComputedStates`, `asset`+`AssetOverlayPolicy`, `bsn`, `picking`+`Gamepad`, `bridge-camera-2d`, `camera_light` `PanOrbitCamera`/`SolariLighting`, `audio`, `ui`+real `feathers` BSN) + presets `preset-2d`/`3d`/`ui`/`full` (`ModPreset*Plugin`). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_host` + strictly feature-gated Bevy `0.20` subsystem crates (`default = []`). |
| `crates/bevy_abi_mod_render` | `ModSubAppPlugin<L>`, `ModWeslMaterialPlugin`, `ModSpriteMaterialPlugin` (2D `.wesl` + unified `Mesh2d`), opt-in `ModMeshShaderPlugin`, & `ModRenderRecoveryPlugin` (`RenderErrorPolicy::Recover` for `DeviceLost` + `RenderStartup` reset). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_host`, `bevy_extract`, `bevy_shader`, `bevy_material`, `bevy_render`, `bevy_sprite_render`, `bevy_pbr`. |
| `crates/bevy_abi_mod_reflect` | `ModReflectPlugin` (witness + `BamFieldDesc` `ReflectFromPtr`, BRP, `bevy-settings` TOML), `ModSaveGamePlugin` (`HandleSerializeProcessor`/`HandleDeserializeProcessor`, `EphemeralHandleBehavior`), & `ModTelemetryPlugin` (`bam/*` BRP + 64B digest stream). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_host`, `bevy_reflect`, `bevy_remote`, `bevy-settings`, `serde`, `serde_json`, `toml`. |
| `crates/bevy_abi_mod_watcher` | Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`, `ModHotReloadWatcher`, `ModHotReloadHistory`). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_host`, `notify-debouncer-full`. |
| `crates/bevy_mod_abi` | **Canonical top-level Bevy host facade** re-exporting `bevy_abi_mod_host`, derives, `bevy_abi_mod_sys`, companion crates (18 granular features + 4 curated presets), and unified `prelude`. | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_host`, `bevy_abi_mod_macros`, `bevy_abi_mod_sys`, optional companion crates. |
| `crates/bevy_abi_mod` | Minimal (`<80` LoC) `#[deprecated(note = "Use `bevy_mod_abi` instead")]` compatibility shim forwarding `bevy_mod_abi::*`, `prelude`, and all feature flags 1:1. | **`#![forbid(unsafe_code)]`**. `bevy_mod_abi`. |
| `crates/bevy_abi_mod_guest_sys` | Guest FFI (`GuestInstance`, panic guards, `extern "C"` trampolines, all 68 `BamHostApi` `covers_slot` wrappers, validated slice/row/command/resource/migration wrappers). | `bevy_abi_mod_sys` only. **NO `bevy` dependency.** Isolates all guest `unsafe`. |
| `crates/bevy_abi_mod_guest` | Ergonomic safe Rust guest SDK (`GuestPlugin`, `GuestApp`, `#[derive(AbiPod, GuestComponent)]`, typed `query_typed`/`single_query`, `SystemBuilder`, `ScheduleSetBuilder`, `RelationDescBuilder`, `ObserverBuilder::run_if`/`run_if_fn`, `ChunkIter`, `GuestTransaction`, 16-term `QueryTuple`, `Or<(T0..T7)>`, `GuestScene`, `export_mod!`). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_guest_sys`, `bevy_abi_mod_sys`, opt. `bevy_abi_mod_macros` (`derive`). **NO `bevy` dependency.** |
| `crates/cargo-bam` | Standalone developer CLI (`new`, `export-sdk`/`header`, `build`, `pack`, `watch`, `check-mods`, `doctor`, `keygen`, `sign`, `verify`). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_sys`, `bevy_abi_mod_trust`, `bevy_abi_mod_manifest`, `bevy_abi_mod_sdk_gen` + CLI crates. **NO `bevy` dependency.** |

### Optional Host Feature Flags (`bevy_mod_abi`, forwarded by `bevy_abi_mod`)
- **Curated Bridge Presets (`bevy_abi_mod_bridges`):**
  - `preset-2d` (`ModPreset2dPlugin`): `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-picking`, `bridge-audio`, `bridge-camera-2d`, `bridge-ui`
  - `preset-3d` (`ModPreset3dPlugin`): `preset-2d`, `bridge-light`, `bridge-camera-controller`, `bridge-bsn`
  - `preset-ui` (`ModPresetUiPlugin`): `bridge-std-types`, `bridge-asset`, `bridge-picking`, `bridge-ui`, `bridge-feathers`
  - `preset-full` (`ModPresetFullPlugin`): `preset-3d`, `preset-ui`, `bridge-solari`
- **Core (`bevy_abi_mod_host`):** `strict-ffi-checks`, `diagnostics`, `signature-verification`
- **Watcher (`bevy_abi_mod_watcher`):** `hot-reload-watcher`
- **Granular Bridges (`bevy_abi_mod_bridges`):**
  - `bridge-std-types` (`Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, `ButtonInput<*>`, `Disabled`, `FfiMouseMotion`/`FfiMouseScroll`)
  - `bridge-asset` (`ModAssetPlugin` isolated `mod://<namespace>/...` `AssetSource`, refcounted `FfiAssetHandle`, `AssetOverlayPolicy`)
  - `bridge-state` (`ModStateExt` dynamic `ModStateSlab`, `States`, `SubStates`, read-only `ComputedStates`, transition schedules, `in_state`)
  - `bridge-bsn` (`ModBsnPlugin` Bevy `0.20` `bsn!` scenes, `ScenePatch`, `cmd_spawn_scene`, `bevy_scene::Ready` observers)
  - `bridge-picking` (`ModPickingBridgePlugin` flattened `Pointer*` observers + `FfiEntityCursor`, `ModInputExtBridgePlugin` mouse + `Gamepad`/rumble)
  - `bridge-audio` (`ModAudioBridgePlugin` `AudioPlayer<AudioSource>`, `PlaybackSettings`, `SpatialAudioSink`/`AudioSink`)
  - `bridge-camera-2d` (lightweight `Camera2d` bridge without `bevy_pbr`/`bevy_light`)
  - `bridge-light` (`Camera2d`, `Camera3d` + `Tonemapping`/`DebandDither`/`Exposure`/`Hdr`/`ScreenSpaceTransmission`, lights; implies `bridge-camera-2d`)
  - `bridge-camera-controller` (`FfiPanOrbitCamera`), `bridge-solari` (`FfiSolariLighting`)
  - `bridge-ui` (`ModUiBridgePlugin` retained UI `Val::Em`/`Rem`, `FixedNode`, `EditableText`/`TextInput`, `InlineBox`/`InlineImage`, headless `TabList`/`ValueChange`), `bridge-feathers` (real `bevy_feathers` BSN controls + `ThemeContext`, implies `bridge-ui`)
- **Render (`bevy_abi_mod_render`):** `sub-app-extract` (`ModSubAppPlugin<L>`, `ModWeslMaterialPlugin`, `ModSpriteMaterialPlugin`, `ModMeshShaderPlugin`, `ModRenderRecoveryPlugin`)
- **Reflect, Savegame & Telemetry (`bevy_abi_mod_reflect`):** `bridge-reflect` (`ModReflectPlugin`, `ModSaveGamePlugin` w/ `HandleSerializeProcessor`/`HandleDeserializeProcessor`, `ModTelemetryPlugin`)

---

## 3. Strict Engineering Rules

1. **ABI Purity (`bevy_abi_mod_sys`):** Every wire struct must be `#[repr(C)]` or `#[repr(transparent)]` with zero implicit padding and frozen `MIN_V0_2_SIZE`/`MIN_V0_3_SIZE`; all 68 `BamHostApi` slots use `Option<unsafe extern "C" fn(...)>` and match `bevy_abi_mod_sys::offsets`; strings use `StrView`.
2. **Memory & Allocator Isolation:** Host allocations are freed only by the host; guest allocations are freed only by the guest. Never pass Rust `Box`, `Vec`, `String`, or fat pointers across FFI.
3. **Concurrency, Headless Host & `-sys` Isolation:** All 13 safe crates (`bevy_mod_abi`, `bevy_abi_mod`, `bevy_abi_mod_host`, `bevy_abi_mod_bridges`, `bevy_abi_mod_render`, `bevy_abi_mod_reflect`, `bevy_abi_mod_watcher`, `bevy_abi_mod_guest`, `bevy_abi_mod_macros`, `bevy_abi_mod_trust`, `bevy_abi_mod_manifest`, `bevy_abi_mod_sdk_gen`, `cargo-bam`, plus `xtask`) enforce `#![forbid(unsafe_code)]`. `bevy_abi_mod_host` must never depend on `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify`. All `unsafe` belongs in `-sys` crates with `// SAFETY:` comments and `# Safety` docs, declaring exact Bevy `FilteredAccessSet` permissions.
4. **Documentation Alignment:** Keep [`spec.md`](./spec.md), [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md), and [`docs/book/`](./docs/book/src/SUMMARY.md) synchronized with any ABI or architectural changes, and verify Rust, ISO C99, C++17, and Zig integration fixtures.

---

## 4. Key Verification Commands

```bash
# Verify zero Bevy dependencies in ABI, macro, trust, manifest, sdk_gen, guest, & CLI crates (must print NOTHING)
cargo tree -p bevy_abi_mod_sys -p bevy_abi_mod_macros -p bevy_abi_mod_trust -p bevy_abi_mod_manifest -p bevy_abi_mod_sdk_gen -p bevy_abi_mod_guest_sys -p bevy_abi_mod_guest -p cargo-bam | sed 's/ (.*//' | grep -v bevy_abi_mod | grep -i bevy

# Verify slim headless bevy_abi_mod_host isolation (must print NOTHING)
cargo tree -p bevy_abi_mod_host --all-features | grep -E "bevy_render|bevy_reflect|bevy_scene|bevy_asset|notify"

# Run full lint, header, fixture build, & workspace test suite
cargo xtask ci

# Fast formatting, clippy (default + --all-features), zero-Bevy, headless isolation, header, & C99 conformance checks
cargo xtask lint
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo xtask header --check --test-c99
cargo xtask gen-component-headers --check

# Rebuild all fixtures (including examples & benchmarks) and run E2E, BQL parity, fuzz/soak, Miri, & Loom suites
cargo xtask fixtures --all
BAM_FIXTURE_DIR="$(pwd)/target/fixtures" cargo test --workspace --all-features
cargo test --test bql_wire_contract_test
cargo test --test schedule_shuffle_fuzz --all-features
cargo test --test soak_10k_reloads --release --all-features
MIRIFLAGS="-Zmiri-tree-borrows" cargo xtask miri --all
cargo test -p bevy_abi_mod_host_sys --test loom_drop_trampoline --release

# Documentation & spellcheck gates
typos
mdbook build docs/book
```

---

## 5. Idiomatic Bevy ECS Standards (`v0.20+`)

- **Borrow Precision:** Prefer `Res<T>` and `&T` over `ResMut<T>` and `&mut T` to maximize scheduler parallelism; narrow queries with `With<T>`/`Without<T>` and `Ref<T>`/`Added<T>`/`Changed<T>`.
- **Single-Responsibility Systems & Ordering:** Use `SystemSet`s ordered via `app.configure_sets(...)` (including `.chain_weak()` where weak dependency ordering improves parallelism), declarative `run_if(...)`, and `ParamSet` when disjoint mutable queries are required.
- **Modern Bevy Patterns:** Use `#[require(...)]`, `On<E>` observers (`app.add_observer(...)`), `despawn_all_where`, and component lifecycle hooks where relational invariants belong, keeping all safe host and companion crates `#![forbid(unsafe_code)]`.
