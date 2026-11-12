# Migration Guide: `0.1.x` to `0.2.0`

`bevy_abi_mod` `0.2.0` (`ABI_VERSION = 0x0002_0000`) is a pre-1.0 breaking release that establishes the 7-crate `-sys` / `#![forbid(unsafe_code)]` split, unifies system descriptors, replaces hot-path string hashing with 1-based `BamComponent(u32)` handles, introduces mandatory mod manifests and per-`World` `BamInstance` state, and prefixes all C99 identifiers with `Bam` / `BAM_` / `bam_`.

Per project policy (pre-1.0 clean break), **no `0.1` compatibility aliases are retained**. Use the tables and side-by-side examples below to port host games, Rust guest mods, and C99/C++/Zig guest mods.

---

## 1. Workspace Crate Split & `Cargo.toml` Rename Table (`N-01`)

| `0.1.x` Crate | `0.2.0` Crate(s) | Notes |
|---|---|---|
| `abi_contract` | `bevy_abi_mod_sys` | `#![no_std]`, zero external dependencies, pure `#[repr(C)]` `0.2.0` wire contract. |
| `mod_derive` | `bevy_abi_mod_macros` | Zero-Bevy proc-macro crate (`#[derive(ModComponent, AbiPod)]`). Re-exported by `bevy_abi_mod` (host) and `bevy_abi_mod_guest` (`features = ["derive"]`). |
| `host_runtime` | `bevy_abi_mod` *(recommended)*<br/>`bevy_abi_mod_host`<br/>`bevy_abi_mod_host_sys` | `bevy_abi_mod` is the `#![forbid(unsafe_code)]` top-level host facade; `bevy_abi_mod_host` is the `#![forbid(unsafe_code)]` plugin crate; `bevy_abi_mod_host_sys` encapsulates internal host `unsafe`. |
| `guest_sdk` | `bevy_abi_mod_guest`<br/>`bevy_abi_mod_guest_sys` | Zero Bevy dependencies. `bevy_abi_mod_guest` enforces `#![forbid(unsafe_code)]` (`bevy_abi_mod_guest::raw` re-exports `bevy_abi_mod_sys`); `bevy_abi_mod_guest_sys` encapsulates raw FFI trampolines. |

### Host `Cargo.toml` (`0.1` vs. `0.2`)

```toml
# 0.1.x
[dependencies]
host_runtime = "0.1"
abi_contract = "0.1"
mod_derive = "0.1"

# 0.2.0 (single top-level facade crate)
[dependencies]
bevy_abi_mod = "0.2"
```

### Rust Guest `Cargo.toml` (`0.1` vs. `0.2`)

```toml
# 0.1.x
[lib]
crate-type = ["cdylib"]

[dependencies]
guest_sdk = "0.1"
mod_derive = "0.1"

# 0.2.0
[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = { version = "0.2", features = ["derive"] }
```

---

## 2. Complete Type, Trait, Macro & Constant Rename Table (`N-02`, `C-01..C-13`)

| `0.1.x` Identifier | `0.2.0` Rust Identifier | `0.2.0` C99 Identifier (`bevy_abi_mod.h`) | Change Summary |
|---|---|---|---|
| `ABI_VERSION_PACKED` / `BEVY_ABI_VERSION_PACKED` | `ABI_VERSION` (`0x0002_0000`) | `BAM_ABI_VERSION` (`0x00020000u`) | `0.x` requires exact minor version match (`C-06`). |
| `BEVY_ABI_MOD_EXPORT` | `#[unsafe(no_mangle)] pub unsafe extern "C"` | `BAM_EXPORT` | Rust 2024 edition; prefixed C export macro. |
| `bevy_abi_mod_abi_version()` *(optional)* | `bevy_abi_mod_manifest() -> *const BamManifest` | `bevy_abi_mod_manifest(void)` | Mandatory static manifest (`L-08`). |
| `bevy_abi_mod_init(gpa, api, ctx)` | `bevy_abi_mod_init(api, load_ctx, instance_out)` | `bevy_abi_mod_init(api, load_ctx, instance_out)` | Per-World `BamInstance**` out-parameter (`L-02`). |
| `bevy_abi_mod_shutdown(ctx) -> FfiResult` | `bevy_abi_mod_shutdown(instance: *mut BamInstance)` | `bevy_abi_mod_shutdown(BamInstance* instance)` | Called once per `World` load session (`L-02`). |
| `HostApi` (`HostApi::LEGACY_64_SIZE` deleted) | `BamHostApi` | `BamHostApi` | 27 `Option<fn>` slots; `MIN_V0_2_SIZE = 224` / `116` (`C-03`). |
| `GetProcAddressFn(name)` | `BamGetProcAddressFn(ctx, name)` | `BamGetProcAddressFn` | Context-carrying `const BamHostCtx*`; `"bam.v0.*"` only (`C-04`). |
| *(none)* | `BamLastErrorFn(ctx, buf, cap)` | `BamLastErrorFn` | Per-context UTF-8 diagnostic buffer (`C-07`). |
| `FfiResult` (+ PascalCase `FfiResult::Ok` aliases) | `FfiResult` (`SCREAMING_SNAKE_CASE` only) | `BamResult` (`BAM_RESULT_*`) | Added codes `8..=15`; deleted PascalCase aliases (`C-07`). |
| `FfiBool = u8` + `ffi_bool_to_rust` | `#[repr(transparent)] struct FfiBool(pub u8)` | `BamBool` (`BAM_FALSE`, `BAM_TRUE`) | `FfiBool::from(bool)` and `.get() -> bool` (`N-02`). |
| `FfiEntity` (`FfiEntity::PLACEHOLDER` deleted) | `FfiEntity(pub u64)` (`FfiEntity::NULL`) | `BamEntity` (`BAM_ENTITY_NULL`) | `0` is sole null sentinel; `PLACEHOLDER` deleted. |
| *(string `StrView` / `*const c_char` names)* | `BamComponent(pub u32)` (`INVALID = 0`) | `BamComponent` (`BAM_COMPONENT_INVALID`) | 1-based pre-resolved component/resource handle (`C-05`). |
| `ComponentAccessMode` | `AccessMode` (`READ`..=`RESOURCE_WRITE`) | `BamAccessMode` (`BAM_ACCESS_*`) | PascalCase aliases deleted (`N-02`, `C-07`). |
| `ComponentAccessDesc` | `BamTermDesc` | `BamTermDesc` | `{ component: BamComponent, mode, flags }` (`12`B). |
| `QuerySpec` / `QueryDesc` | `BamQueryDesc` | `BamQueryDesc` | `{ struct_size, flags, terms, term_count }` (`24`B / `16`B). |
| `SystemDescriptor[V2/V3]`, `MultiQuerySystemDescriptor[V2/V3]` | `BamSystemDesc` | `BamSystemDesc` | All 6 system descriptors unified into 1 (`104`B / `60`B, `C-01`). |
| `register_system` + 6 `register_*_system*` fns | `BamHostApi::register_system` | `api->register_system(load_ctx, &desc)` | Single registration entry point (`C-01`). |
| `ModComponentDescriptor` / `DynamicComponentDescriptor` | `ComponentDesc` | `BamComponentDesc` | Added `flags` (`SPARSE_SET`, `IMMUTABLE`) (`B-08`, `N-02`). |
| *(none)* | `BamColumnInfo` (`iter_column_info`) | `BamColumnInfo` | Column/resource UUID, layout, mode & storage introspection (`C-13`). |
| *(none — bare `size`/`align` fields)* | `VTableHeader` | `BamVTableHeader` | Placed at offset `0` of every `<Struct>VTable` (`M-05`). |
| `ModSchedule` + `ScheduleLabelId` | `ScheduleId` (`CUSTOM_BASE = 1024`) | `BamScheduleId` (`BAM_SCHEDULE_*`) | Unified wire & host schedule identifier (`B-06`, `N-02`). |
| `MAX_QUERY_COMPONENTS` | `MAX_TERMS_PER_QUERY` (`32`) | `BAM_MAX_TERMS_PER_QUERY` | Single source in `bevy_abi_mod_sys` (`N-02`). |
| `MAX_SYSTEM_QUERIES` / `MAX_GUEST_MULTI_QUERIES` | `MAX_QUERIES_PER_SYSTEM` (`8`) | `BAM_MAX_QUERIES_PER_SYSTEM` | Single source in `bevy_abi_mod_sys` (`N-02`). |
| `#[derive(ModExposable)]` + `#[mod_exposable(...)]` | `#[derive(ModComponent)]` + `#[mod_component(...)]` | *(Rust macro)* | Setters now return `FfiResult` (`M-04`, `N-02`). |
| `ExposableComponent` + `IS_POD` | `ModComponent` + `IS_ABI_POD` | *(Rust trait in `bevy_abi_mod_sys`)* | Decoupled from Bevy, `RawInsertable`, and `Default` (`N-02`). |
| `FfiLayout` | `RawInsertable` | *(Rust unsafe trait)* | `u128`/`i128`/`usize`/`isize` are `RawInsertable` only, **not** `AbiPod` (`C-11`). |
| `DynamicCSystem` | `GuestSystem` | *(host internal)* | Renamed in `bevy_abi_mod_host_sys` (`N-02`). |
| `HostChunkIter` / `RawChunkIter` / `GuestChunkIter` | `HostIter` / `BamIter` / `ChunkIter` | `BamIter` | Layer-specific iterator names (`N-02`). |
| `CommandQueueBridge` / `GuestCommandQueue` | `HostCommands` / `GuestCommands` | `BamCommands` | Arena-backed batched commands (`P-05`, `N-02`). |
| `ModInitContext` | `LoadContext` / `BamLoadCtx` | `BamLoadCtx` | Validated header; init-only lifetime (`S-07`, `N-02`). |

---

## 3. Host Application Migration (`bevy_abi_mod`)

### 3.1 Component Derivation & Registration (`#[derive(ModComponent)]`, `ComponentOptions`)
- Replace `#[derive(ModExposable)]` with `#[derive(ModComponent)]`.
- Canonical names now default to `"<module_path>::<Struct>"` (or explicit `#[mod_component(name = "my_game::Position")]`) and compute a 128-bit SipHash-2-4-128 `ComponentUuid` (`C-10`).
- Replace the 10 `register_mod_component_*` / `try_register_mod_component_*` methods with `register_mod_component::<T>(ComponentOptions)` (`B-01`).
- Note that `u128`, `i128`, `usize`, and `isize` no longer implement `AbiPod` (`C-11`); use fixed-width `u64` / `u32` / `i64` / `i32` in cross-language `#[derive(AbiPod)]` components.

```rust
// --- 0.1.x ---
use bevy::prelude::*;
use host_runtime::prelude::*;

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModExposable, AbiPod)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

#[derive(Component, Default, ModExposable)]
pub struct UnitLabel {
    pub name: String,
    pub active: bool,
}

fn setup_app(app: &mut App) {
    app.add_plugins(AbiModPlugin)
        .register_mod_component_with_default::<Position>()
        .register_mod_component_opaque_with_default::<UnitLabel>();
}
```

```rust
// --- 0.2.0 ---
use bevy::prelude::*;
use bevy_abi_mod::prelude::*;

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent, AbiPod)]
#[mod_component(name = "my_game::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

#[derive(Component, Default, ModComponent)]
#[mod_component(name = "my_game::UnitLabel")]
pub struct UnitLabel {
    pub name: String,
    pub active: bool,
}

fn setup_app(app: &mut App) -> Result<(), ModLoadError> {
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.register_mod_component::<UnitLabel>(ComponentOptions::new().with_default())?;
    Ok(())
}
```

### 3.2 Loading, Shadow-Copying & Unloading Mods (`ModWorldExt`, `LoadOptions`, `ModCommandsExt`)
- All Rust methods returning `FfiResult` (`try_load_mod*` variants, `ModHost::unload_mod(&mut self)`) have been replaced by idiomatic `Result<_, ModLoadError>` APIs requiring exclusive `&mut World` access (`ModWorldExt`, `ModAppExt`, or deferred `ModCommandsExt`, `B-01`, `B-02`, `L-04`).
- Shadow-copy hot-reloading is built directly into `LoadOptions` (`L-03`)—delete custom shadow-copy file helpers in your host code.
- Lifecycle notifications (`ModLoaded`, `ModUnloaded`, `ModSystemFaulted`) emit once as Bevy `Message`s (`MessageReader<ModSystemFaulted>`, `B-09`), and quarantined systems can be re-enabled via `ResMut<ModQuarantine>` (`B-05`).

```rust
// --- 0.1.x ---
// Required `App` at startup, or unsound `ResMut<ModHost>` unload during schedule execution:
let mod_id = app.try_load_mod("mods/libcombat.dylib")?;
app.try_unload_mod(mod_id)?;
```

```rust
// --- 0.2.0 ---
use bevy::prelude::*;
use bevy_abi_mod::prelude::*;

// 1. On `App` or `&mut World` (exclusive system or startup):
let mod_id = app.load_mod_with(
    "mods/libcombat.dylib",
    LoadOptions {
        shadow_copy: true,
        strict: true,
        error_policy: Some(ModErrorPolicy::Quarantine),
        ..Default::default()
    },
)?;

// 2. Inside a normal parallel Bevy system via deferred `ModCommandsExt`:
fn hot_reload_hotkey_system(
    keys: Res<ButtonInput<KeyCode>>,
    host: Res<ModHost>,
    mut commands: Commands,
) {
    if keys.just_pressed(KeyCode::F5) {
        for info in host.loaded_mods() {
            commands.unload_mod(info.id);
            commands.load_mod(info.source_path.clone());
        }
    }
}

// 3. Observing faults and re-enabling quarantined systems:
fn inspect_faults(
    mut faults: MessageReader<ModSystemFaulted>,
    mut quarantine: ResMut<ModQuarantine>,
) {
    for fault in faults.read() {
        if let Some(mod_id) = fault.mod_id {
            // Optionally clear quarantine after reloading config/assets:
            let _ = quarantine.reenable(mod_id, &fault.system_name);
        }
    }
}
```

---

## 4. Rust Guest Mod Migration (`bevy_abi_mod_guest`)

### 4.1 Key SDK Changes
1. **Manifest Metadata (`L-08`, `L-02`):** `export_mod!` and `GuestPlugin` now declare canonical mod `name` and `version` metadata, emitting `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` backed by per-World `BamInstance` heap state (zero process-global `GUEST_ALLOCATIONS`).
2. **Unified `app.system(...)` Builder (`G-02`):** `add_system`, `add_system_if`, `add_multi_query_system`, and `add_multi_query_system_if` are replaced by `app.system(c"name").query(|q| ...)...build(|ctx| ...)`.
3. **Safe `#![forbid(unsafe_code)]` Views (`S-01..S-03`, `C-13`, `P-02`, `N-02`):**
   - `GuestChunkIter` is renamed to `ChunkIter`.
   - Caller-passed `count` parameters on slice helpers are deleted (`ChunkIter` tracks `current_len` internally).
   - Every typed view/insert requires `T: AbiPod` (never `bool`, `char`, `u128`, `usize`, or enums) and checks `BamColumnInfo` UUID/layout metadata at runtime (`C-13`).
   - Resources are fetched in `O(1)` by declaration index (`ctx.resource::<R>(0)`) rather than string lookup (`C-05`).
   - `SparseSet` columns support `chunk.rows::<T>(col)` / `rows_mut::<T>(col)` (`P-02`).
4. **VTable Resolution (`M-04`, `M-05`, `C-04`):** VTable symbol names follow `"bam.v0.<canonical_name>.vtable"`, start with `header: VTableHeader` at offset `0`, and return `FfiResult` from every setter.

```rust
// --- 0.1.x ---
use guest_sdk::prelude::*;
use mod_derive::AbiPod;

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Position { pub x: f32, pub y: f32 }

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Velocity { pub x: f32, pub y: f32 }

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    let terms = [
        ComponentAccessDesc::write(c"Position".as_ptr()),
        ComponentAccessDesc::read(c"Velocity".as_ptr()),
    ];
    app.add_system(c"integrate", &terms, |ctx| {
        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
                return FfiResult::INTERNAL_ERROR;
            };
            for (p, v) in pos.iter_mut().zip(vel.iter()) {
                p.x += v.x;
                p.y += v.y;
            }
        }
        FfiResult::OK
    })
}

export_mod!(setup);
```

```rust
// --- 0.2.0 ---
use bevy_abi_mod_guest::prelude::*;

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Position { pub x: f32, pub y: f32 }

impl GuestComponent for Position {
    const NAME: &'static core::ffi::CStr = c"my_game::Position";
    const UUID: ComponentUuid = ComponentUuid::from_parts(
        "my_game::Position",
        core::mem::size_of::<Self>(),
        core::mem::align_of::<Self>(),
        0, // or matching schema hash when declared with #[derive(ModComponent)]
    );
}

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Velocity { pub x: f32, pub y: f32 }

impl GuestComponent for Velocity {
    const NAME: &'static core::ffi::CStr = c"my_game::Velocity";
    const UUID: ComponentUuid = ComponentUuid::from_parts(
        "my_game::Velocity",
        core::mem::size_of::<Self>(),
        core::mem::align_of::<Self>(),
        0,
    );
}

pub struct KinematicsMod;

impl GuestPlugin for KinematicsMod {
    const NAME: &'static core::ffi::CStr = c"kinematics_mod";
    const VERSION: &'static core::ffi::CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"integrate")
            .query(|q| q.write::<Position>().read::<Velocity>())
            .schedule(ScheduleId::FIXED_UPDATE)
            .after(c"spawn")
            .build(|ctx| {
                let Some(mut iter) = ctx.query(0) else {
                    return FfiResult::OUT_OF_RANGE;
                };
                while let Some(mut chunk) = iter.next_chunk_view() {
                    let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
                        return FfiResult::TYPE_MISMATCH;
                    };
                    for (p, v) in pos.iter_mut().zip(vel.iter()) {
                        p.x += v.x;
                        p.y += v.y;
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: KinematicsMod);
```

### 4.2 Cargo Panic Profile (`panic = "unwind"` vs. `panic = "abort"`, `G-05` / `S-09`)
- Keep Cargo's default `panic = "unwind"` if you want guest panics caught inside `bevy_abi_mod_guest_sys` and translated into `FfiResult::PANIC_CAUGHT` (`ModSystemFaulted` / `ModErrorPolicy::Quarantine`).
- Setting `panic = "abort"` in `[profile.release]` is ABI-valid, and any panic inside the guest `.cdylib` will terminate the host OS process immediately (the host never wraps `extern "C"` calls in `catch_unwind`).

---

## 5. ISO C99 / C++ Guest Migration (`include/bevy_abi_mod.h`)

### Summary of C99 Wire Changes
1. **Prefixed Namespace (`C-12`):** Replace `HostApi` -> `BamHostApi`, `FfiResult` -> `BamResult`, `FFI_RESULT_OK` -> `BAM_RESULT_OK`, `FfiBool` -> `BamBool` (`BAM_TRUE` / `BAM_FALSE`), `FfiEntity` -> `BamEntity` (`BAM_ENTITY_NULL`), `SystemDescriptor` -> `BamSystemDesc`, `BEVY_ABI_MOD_EXPORT` -> `BAM_EXPORT`.
2. **Manifest & Per-Instance State (`L-08`, `L-02`):** Export `bevy_abi_mod_manifest(void)` returning `const BamManifest*`. Store your bounded `BamHostApi` copy and resolved `BamComponent` handles in a heap-allocated `struct` returned through `*instance_out` (and passed as `user_data` to `BamSystemDesc`), freeing it in `bevy_abi_mod_shutdown(BamInstance* instance)` rather than storing globals that collide across multiple Bevy `World`s.
3. **Pre-Resolved Handles & `BamSystemCtx` (`C-01`, `C-05`):** Resolve components once in `bevy_abi_mod_init` via `api.resolve_component(host_ctx, "my_game::Position", NULL, &state->pos_comp)`. System callbacks now have signature `BamResult fn(BamSystemCtx* sys_ctx, void* user_data)` and obtain iterators/commands via `api.sys_query(sys_ctx, 0, &iter)` and `api.sys_commands(sys_ctx, &cmds)`.
4. **VTable Header & Setter Results (`M-04`, `M-05`):** Prepend `BamVTableHeader header;` to C VTable structs, resolve `"bam.v0.<canonical_name>.vtable"` via `api.get_proc_address(host_ctx, ...)`, and check `BamResult` returns from setters.

```c
/* --- 0.1.x --- */
#include "bevy_abi_mod.h"

static const HostApi* g_api = NULL;

static FfiResult step_sys(RawChunkIter* iter, RawCommandQueue* cmds, void* ud) {
    (void)cmds; (void)ud;
    size_t n;
    while ((n = g_api->iter_next_chunk(iter)) > 0) {
        float* x = (float*)g_api->iter_column_mutable(iter, 0);
        for (size_t i = 0; i < n; ++i) { x[i] += 1.0f; }
    }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn gpa, const HostApi* api, void* ctx
) {
    (void)gpa;
    g_api = api;
    ComponentAccessDesc q[] = {{ .name = "Position", .mode = COMPONENT_ACCESS_WRITE }};
    SystemDescriptor d = {
        .struct_size = sizeof(SystemDescriptor),
        .name = "step_sys", .queries = q, .query_count = 1, .callback = step_sys
    };
    return api->register_system(ctx, &d);
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* ctx) {
    (void)ctx; g_api = NULL; return FFI_RESULT_OK;
}
```

```c
/* --- 0.2.0 --- */
#include "bevy_abi_mod.h"
#include <stdlib.h>

typedef struct Position { float x; float y; } Position;

typedef struct ModState {
    BamHostApi   api;
    BamComponent pos_comp;
} ModState;

static const BamManifest MANIFEST = {
    sizeof(BamManifest),
    BAM_ABI_VERSION,
    "c_step_mod",
    "0.2.0",
    0u,
    0u
};

static BamResult step_sys(BamSystemCtx* sys_ctx, void* user_data) {
    ModState* st = (ModState*)user_data;
    BamIter* iter = NULL;
    size_t count = 0u;
    BamResult res = st->api.sys_query(sys_ctx, 0u, &iter);
    if (res != BAM_RESULT_OK) return res;

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        Position* pos = (Position*)st->api.iter_column_mutable(iter, 0u);
        if (pos == NULL) return BAM_RESULT_NULL_POINTER;
        for (size_t i = 0u; i < count; ++i) {
            pos[i].x += 1.0f;
        }
    }
    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx*       load_ctx,
    BamInstance**     instance_out
) {
    BamHostApi local_api;
    const BamHostCtx* host_ctx;
    ModState* st;
    BamTermDesc terms[1];
    BamQueryDesc queries[1];
    BamSystemDesc sys_desc;
    BamResult res;

    if (load_ctx == NULL || instance_out == NULL) return BAM_RESULT_NULL_POINTER;
    *instance_out = NULL;

    res = bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api);
    if (res != BAM_RESULT_OK) return res;

    st = (ModState*)calloc(1u, sizeof(ModState));
    if (st == NULL) return BAM_RESULT_INTERNAL_ERROR;
    st->api = local_api;

    host_ctx = st->api.load_host_ctx(load_ctx);
    res = st->api.resolve_component(host_ctx, "my_game::Position", NULL, &st->pos_comp);
    if (res != BAM_RESULT_OK) { free(st); return res; }

    terms[0] = bam_term_desc(st->pos_comp, BAM_ACCESS_WRITE, 0u);
    memset(&queries[0], 0, sizeof(BamQueryDesc));
    queries[0].struct_size = sizeof(BamQueryDesc);
    queries[0].terms       = terms;
    queries[0].term_count  = 1u;

    memset(&sys_desc, 0, sizeof(BamSystemDesc));
    sys_desc.struct_size = sizeof(BamSystemDesc);
    sys_desc.name        = "step_sys";
    sys_desc.queries     = queries;
    sys_desc.query_count = 1u;
    sys_desc.callback    = step_sys;
    sys_desc.user_data   = st;
    sys_desc.schedule    = BAM_SCHEDULE_UPDATE;

    res = st->api.register_system(load_ctx, &sys_desc);
    if (res != BAM_RESULT_OK) { free(st); return res; }

    *instance_out = (BamInstance*)st;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    if (instance != NULL) {
        free(instance);
    }
}
```
