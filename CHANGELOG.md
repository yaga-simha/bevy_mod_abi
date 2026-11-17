# Changelog

All notable changes to **`bevy_abi_mod`** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (while `MAJOR == 0`, minor version bumps indicate breaking wire and API changes).

---

## [0.2.0] - 2026-11-12

### Added
- **7-Crate `-sys` vs. Safe Rust Workspace Split:** Split the 4 legacy crates into 7 focused crates isolating all `unsafe` FFI code inside `bevy_abi_mod_sys`, `bevy_abi_mod_host_sys`, and `bevy_abi_mod_guest_sys`, while enforcing `#![forbid(unsafe_code)]` on `bevy_abi_mod` (new top-level host facade), `bevy_abi_mod_host`, and `bevy_abi_mod_guest`.
- **Mandatory Mod Identity Manifest:** Added `BamManifest` (`32`B on 64-bit / `24`B on 32-bit) and required guest export `bevy_abi_mod_manifest() -> *const BamManifest`. Missing manifests fail deterministically with `ModLoadError::MissingManifest`; `ModId(u64)` is now strictly monotonic.
- **Symmetric Per-World `BamInstance` Lifecycle:** Updated `bevy_abi_mod_init(api, load_ctx, instance_out)` and `bevy_abi_mod_shutdown(instance)` so each Bevy `World` load session allocates and frees an isolated guest-heap `BamInstance*`, eliminating process-global `GUEST_ALLOCATIONS` state.
- **Complete 38-Slot Versioned `BamHostApi`:** Placed core host operations directly into `BamHostApi` as `Option<unsafe extern "C" fn(...)>` slots (`224`B core `0.2` prefix / `312`B full on 64-bit), added context-carrying `get_proc_address(ctx, name)` for `"bam.v0.*"` custom symbols and VTables, and added per-context `last_error(ctx, buf, cap)` UTF-8 diagnostics.
- **Pre-Resolved `BamComponent(u32)` Handles & `SipHash-2-4-128` UUIDs:** Added 1-based world-scoped `BamComponent` handles (`resolve_component`), declaration-indexed singleton resource access (`res_ptr` / `res_mut_ptr`), and canonical namespaced `"<namespace>::<Name>"` `ComponentUuid` generation using streaming `const` SipHash-2-4-128 over name and layout/schema fingerprints (mirrored in C99 by `bam_uuid_from_parts`).
- **Column Introspection & VTable Headers:** Added `BamColumnInfo` (`iter_column_info`) for runtime UUID/layout/mode checks and prepended `VTableHeader` (`struct_size`, `abi_version`, `uuid`, `size`, `align`) at offset `0` of every macro-generated `<Struct>VTable`.
- **Arena-Backed Batched Spawns & Zero-Alloc Steady-State Commands:** Added per-system reusable `MaybeUninit<u8>` bump `CommandArena` and batched `cmd_spawn` (`EntityWorldMut::insert_by_ids`) achieving single-archetype placement and zero steady-state heap allocations after warmup.
- **Batched Indirect Sparse-Set Iteration & Untracked Mutable Columns:** Added `iter_column_rows_readonly` / `iter_column_rows_mutable` (`Chunk::rows::<T>` / `rows_mut::<T>`) so `SparseSet` queries iterate multi-entity archetype runs, plus `iter_column_mutable_untracked` and `iter_mark_changed_range`.
- **Main-Thread System Flag, Component Storage Flags & Custom Schedules:** Added `BAM_SYSTEM_FLAG_MAIN_THREAD` (`NON_SEND`), `BAM_COMPONENT_FLAG_SPARSE_SET`, `BAM_COMPONENT_FLAG_IMMUTABLE`, and `ScheduleId::CUSTOM_BASE` (`1024`) backed by `ModScheduleMap` (`register_mod_schedule`).
- **Core Loader Shadow Copying, World/Commands Extensions & Quarantine Recovery:** Added `LoadOptions` (`shadow_copy`, `shadow_dir`, `schedule_override`, `strict`, `error_policy`), `ComponentOptions`, `ModWorldExt`, `ModCommandsExt`, and `ModQuarantine::reenable(mod_id, system_name)`.
- **Unified Guest `SystemBuilder` & Raw Re-Export:** Added `app.system(c"...")` builder supporting multi-query, schedules, `before`/`after` ordering, `main_thread()`, and `run_if(...)`, plus `bevy_abi_mod_guest::raw`.
- **Single-Source C99 Header Generator & Workspace Automation:** Added `cargo xtask` (`header`, `fixtures`, `lint`, `ci`, `miri`) generating `include/bevy_abi_mod.h` with `Bam*`/`BAM_*`/`bam_*` identifiers and 64-bit/32-bit `BAM_STATIC_ASSERT` layout checks, plus [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md).

### Changed
- **Unified `BamSystemDesc`:** Collapsed all six `0.1` system descriptors (`SystemDescriptor[V2/V3]`, `MultiQuerySystemDescriptor[V2/V3]`) and seven registration functions into `BamTermDesc` -> `BamQueryDesc` -> `BamSystemDesc` and `BamHostApi::register_system`.
- **Consistent Type & Trait Vocabulary:** Renamed `#[derive(ModExposable)]` / `ExposableComponent` to `#[derive(ModComponent)]` / `ModComponent`, `FfiLayout` to `RawInsertable`, `IS_POD` to `IS_ABI_POD`, `DynamicCSystem` to `GuestSystem`, `HostChunkIter` / `RawChunkIter` / `GuestChunkIter` to `HostIter` / `BamIter` / `ChunkIter`, `CommandQueueBridge` / `GuestCommandQueue` to `HostCommands` / `GuestCommands`, `ModComponentDescriptor` to `ComponentDesc`, `ComponentAccessMode` to `AccessMode`, `ScheduleLabelId` + `ModSchedule` to `ScheduleId`, and `FfiBool = u8` to `#[repr(transparent)] struct FfiBool(pub u8)`.
- **Cargo-Aligned `0.x` SemVer & Frozen `MIN_V0_2_SIZE`:** `abi_version_is_compatible` now requires exact minor matches while `major == 0`; `MIN_V0_2_SIZE` is frozen via `offset_of!`; unknown `flags` bits return `FfiResult::UNSUPPORTED`; and expanded `FfiResult` codes (`INVALID_ARGUMENT`, `OUT_OF_RANGE`, `NOT_FOUND`, `INVALID_STATE`, `INVALID_ENTITY`, `CAPACITY_EXCEEDED`, `UNSUPPORTED`, `TYPE_MISMATCH`) replace overloaded `INTERNAL_ERROR` uses.
- **Error-Returning VTable Setters:** Macro-generated VTable setters now return `FfiResult` (`NULL_POINTER`, `INVALID_ARGUMENT`, `INVALID_UTF8`, `OK`) instead of silently ignoring errors.
- **Registry, Tick Filter & Trampoline Internals:** Switched `ModRegistry` to a generation-tracked CoW `Arc<RegistrySnapshot>` with foldhash and `Arc<str>` keys; switched `DROP_SLOTS` to `ArcSwapOption<DropSlotSnapshot>` + `WorldDropGuard` two-condition reclamation; cached table tick slices and once-per-chunk mutable tick bitmasks; replaced guest closure `Mutex::try_lock` with lock-free `UnsafeCell` + `AtomicBool` re-entrancy guards; derived host errors with `thiserror`; and consolidated lifecycle notifications to single-write Bevy `Messages`.
- **Toolchain, Edition & Hermetic Test Suites:** Migrated workspace to Rust 2024 edition (`rust-version >= 1.87`) with centralized `[workspace.lints]`, migrated macro compile-fail tests to `trybuild`, and consolidated 31 legacy integration test binaries into 4 behavior-named hermetic suites (`ffi`, `scheduling`, `lifecycle`, `e2e`).

### Fixed
- **Runtime-Loaded `Startup` Systems:** `ScheduleId::STARTUP` systems registered after Bevy's `Startup` schedule has already completed now execute once immediately upon load commit.
- **Scoped Named System Sets:** Scoped `ModNamedSystemSet { mod_name, system_name }` prevents accidental cross-mod coupling on shared system names, supports `"other_mod::system"` cross-mod ordering even when the target mod declared no ordering constraints of its own, and cleans up named sets on unload.
- **Explicit `SystemInitError` Diagnostics:** Surfaced query/access validation failures synchronously and via `ModSystemFaulted`, and cached `DebugName` in `GuestSystem::name()`.
- **Macro Crate Resolution & Hygiene:** Resolved `bevy_abi_mod_sys` / facade paths via `proc-macro-crate`, scoped generated `extern "C"` functions inside anonymous `const` blocks, and added compile-time type assertions preventing spoofed `String`/`Vec` structs.

### Removed
- Deleted all `0.1` compatibility shims, PascalCase `FfiResult`/`AccessMode`/`ScheduleId` aliases, `FfiEntity::PLACEHOLDER`, `HostApi::LEGACY_64_SIZE`, `ExtensibleStruct::validate_raw_slot`, `CURRENT_REGISTRY`, `GLOBAL_SYMBOLS`, `ACTIVE_INIT_CONTEXT`, `GUEST_ALLOCATIONS`, `CommandBridgeVTable*`, `ResourceBridgeVTable`, `GuestApp::add_system*` methods, Rust host methods returning `FfiResult`, and `reset_drop_trampolines_for_testing`.

### Security / Soundness
- **Eliminated Safe-Rust Raw Pointer & Slice-Length UB in Guest SDK:** Restricted raw-pointer constructors to `unsafe` in `bevy_abi_mod_guest_sys`, removed caller-supplied element counts from safe `ChunkIter` APIs in favor of iterator-tracked `current_len`, bounded all typed views/inserts by `T: AbiPod` (excluding `bool`, `char`, `u128`, `i128`, `usize`, `isize`, and enums), and enforced `#![forbid(unsafe_code)]` on `bevy_abi_mod_guest`.
- **Eliminated Padding-Byte `&[u8]` UB on Raw Inserts:** Copied guest struct bytes through `MaybeUninit<u8>` (`ptr::copy_nonoverlapping` + `read_unaligned`) without forming `&[u8]` over uninitialized padding.
- **Eliminated Use-After-`dlclose` on Unload & Failed Init:** Added RAII `LoadTransaction` rollback disarming drop trampolines before unmapping failed loads, purged dynamic VTable pointers and `"bam.v0.*"` symbols on unload, and restricted unload to `&mut World` (`ModWorldExt`) so `dlclose` cannot race running systems.
- **Handle Header Validation & Active-Context Enforcement:** Added `{ magic, generation }` headers to host FFI handles (poisoned on `Drop`), rejected registration calls outside the active initialization scope with `FfiResult::INVALID_STATE`, and added `Entity` bit-representation assertions.
- **Removed Dead Host `catch_unwind` Wrappers & Documented `nounwind` Threat Model:** Removed unreachable host `catch_unwind` wrappers around `extern "C"` calls and documented the native in-process trust model alongside `panic = "unwind"` vs `panic = "abort"` behavior.

---

## [0.1.0] - 2026-10-15

### Added
- Initial pre-release 4-crate workspace (`abi_contract`, `mod_derive`, `host_runtime`, `guest_sdk`) and `include/bevy_abi_mod.h` targeting Bevy ECS `0.20`.
- Asymmetric `extern "C"` chunk iteration (`RawChunkIter`), VTable generation (`#[derive(ModExposable, AbiPod)]`), `FilteredAccessSet` parallel Bevy scheduling (`DynamicCSystem`), runtime component registration, deferred command queue bridge, and Rust + ISO C99 integration fixtures and examples.
