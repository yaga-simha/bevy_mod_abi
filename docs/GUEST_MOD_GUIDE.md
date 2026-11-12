# Guest Mod Developer Guide: `bevy_abi_mod` (`ABI 0.2.0`)

This guide covers authoring, compiling, and debugging native guest mods for Bevy ECS (`v0.20+`) applications using **`bevy_abi_mod`** (`ABI 0.2.0`).

---

## 1. Overview, Trust Model & Toolchain Independence

`bevy_abi_mod` communicates across an asymmetric, zero-serialization `extern "C"` boundary (`crates/bevy_abi_mod_sys` and `include/bevy_abi_mod.h`):
- **Language & Compiler Freedom:** Guest mods can be compiled with any Rust `1.87+` toolchain (independent `Cargo.lock` and optimization profile), ISO C99/C11, C++17/20, or Zig.
- **Zero Bevy Dependency:** `bevy_abi_mod_guest`, `bevy_abi_mod_guest_sys`, `bevy_abi_mod_macros`, and `bevy_abi_mod_sys` never pull in `bevy*` crates, keeping guest `.cdylib` compile times under a few seconds.
- **Safe Rust SDK (`#![forbid(unsafe_code)]`):** `bevy_abi_mod_guest` enforces `#![forbid(unsafe_code)]` and verifies component layouts, alignment, `ComponentUuid` identity, chunk slice bounds, and disjoint column borrows automatically.
- **Native In-Process Trust Model:** Native shared libraries (`.so`, `.dylib`, `.dll`) execute in-process with the host's full OS privileges. `bevy_abi_mod` isolates Rust ABI drift, heap allocators, and ECS borrow rules across trusted native mods—it is **not** a sandbox for untrusted binaries (use WebAssembly for untrusted user-submitted mods).

### Two Ways to Access Host Components
1. **Zero-Copy Typed Views (`#[derive(AbiPod)]`):** For `#[repr(C)]` / `#[repr(transparent)]` fixed-width Plain Old Data (`u8..u64`, `i8..i64`, `f32`, `f64`, `FfiBool`, `BamComponent`, `FfiEntity`, `ComponentUuid`, `[T; N]`), derive `AbiPod` (`features = ["derive"]`) and borrow `&[T]` / `&mut [T]` slices directly from `Chunk` (or per-row slices via `Chunk::rows::<T>` / `rows_mut::<T>` for `SparseSet` components). Note that `bool`, `char`, `u128`, `i128`, `usize`, `isize`, and `enum`s are intentionally excluded from `AbiPod`.
2. **Header-Verified `#[repr(C)]` VTables (`resolve_vtable`):** For `#[repr(Rust)]` host components or components containing `String`, `Vec<T>`, `bool` (`FfiBool`), or `#[mod_component(opaque)]`, resolve `"bam.v0.<canonical_name>.vtable"` (validating its `VTableHeader`) and invoke its `FfiResult`-returning C-ABI getters and setters.

---

## 2. Writing an Idiomatic Rust Guest Mod (`bevy_abi_mod_guest`)

### 2.1 `Cargo.toml` Setup

```toml
[package]
name = "combat_mod"
version = "0.2.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = { path = "../../crates/bevy_abi_mod_guest", features = ["derive"] }
```

### 2.2 Manifest, `GuestPlugin` & Unified `SystemBuilder`
Every mod exports `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` via `export_mod!`. Each Bevy `World` that loads your `.cdylib` receives its own heap-allocated `BamInstance` (storing your `GuestPlugin` instance and registered closures) and frees it cleanly on `unload_mod` or `World::drop` with zero process-global state:

```rust
use bevy_abi_mod_guest::prelude::*;

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl GuestComponent for Position {
    const NAME: &'static core::ffi::CStr = c"my_game::Position";
    const UUID: ComponentUuid = ComponentUuid::from_parts(
        "my_game::Position",
        core::mem::size_of::<Self>(),
        core::mem::align_of::<Self>(),
        0,
    );
}

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl GuestComponent for Velocity {
    const NAME: &'static core::ffi::CStr = c"my_game::Velocity";
    const UUID: ComponentUuid = ComponentUuid::from_parts(
        "my_game::Velocity",
        core::mem::size_of::<Self>(),
        core::mem::align_of::<Self>(),
        0,
    );
}

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct SimTime {
    pub dt: f32,
}

impl GuestComponent for SimTime {
    const NAME: &'static core::ffi::CStr = c"my_game::SimTime";
    const UUID: ComponentUuid = ComponentUuid::from_parts(
        "my_game::SimTime",
        core::mem::size_of::<Self>(),
        core::mem::align_of::<Self>(),
        0,
    );
}

#[derive(Default)]
pub struct CombatMod;

impl GuestPlugin for CombatMod {
    const NAME: &'static core::ffi::CStr = c"combat_mod";
    const VERSION: &'static core::ffi::CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"integrate_kinematics")
            .query(|q| q.write::<Position>().read::<Velocity>())
            .resource_read::<SimTime>()
            .schedule(ScheduleId::FIXED_UPDATE)
            .after(c"physics::apply_forces") // cross-mod ordering ("mod::system") or same-mod ("system")
            .build(|ctx| {
                let dt = ctx.resource::<SimTime>(0).map_or(1.0 / 60.0, |t| t.dt);
                let Some(mut iter) = ctx.query(0) else {
                    return FfiResult::OUT_OF_RANGE;
                };

                while let Some(mut chunk) = iter.next_chunk_view() {
                    let Some((entities, vel, pos)) =
                        chunk.entities_and_read_write::<Velocity, Position>(1, 0)
                    else {
                        return FfiResult::TYPE_MISMATCH;
                    };

                    for ((entity, p), v) in entities.iter().zip(pos.iter_mut()).zip(vel.iter()) {
                        if !entity.is_null() {
                            p.x += v.x * dt;
                            p.y += v.y * dt;
                            p.z += v.z * dt;
                        }
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: CombatMod);
```

### 2.3 Multi-Query Systems, Sparse Rows, Untracked Writes & Batched Commands
- **Multiple Disjoint Queries (`0..=8`):** Chain `.query(|q| ...)` up to `MAX_QUERIES_PER_SYSTEM` (`8`) times on `app.system(...)`. Inside the callback, borrow disjoint iterators and call `inner_iter.reset()` for zero-allocation nested $N \times M$ loops.
- **Sparse-Set Columns (`StorageType::SparseSet`):** Use `chunk.rows::<T>(term_idx)` / `chunk.rows_mut::<T>(term_idx)` to iterate batched indirect row pointers across entire archetype runs instead of single-entity chunks.
- **Fine-Grained Change Detection:** When mutating only a subset of rows in a large dense chunk, use untracked mutable access paired with `mark_changed_range(term_idx, start_row, row_count)` so downstream `Changed<T>` queries wake only for modified rows.
- **Handle-Based & Batched Commands (`GuestCommands`):** `ctx.commands()` enqueues deferred commands into a reusable per-system host `CommandArena` (`0` steady-state heap allocations). Prefer batched `spawn` over `spawn_empty` + sequential `insert` calls so Bevy places all components into the final archetype in a single transition.

### 2.4 VTable Access (`VTableHeader` & `FfiResult` Setters)
Macro-generated VTables start with `VTableHeader` at offset `0` and return `FfiResult` from every setter:

```rust
use core::ffi::c_void;
use bevy_abi_mod_guest::raw::{FfiBool, FfiResult, StrView, VTableHeader};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnitLabelVTable {
    pub header: VTableHeader,
    pub get_name: Option<unsafe extern "C" fn(*const c_void) -> StrView>,
    pub set_name: Option<unsafe extern "C" fn(*mut c_void, StrView) -> FfiResult>,
    pub get_active: Option<unsafe extern "C" fn(*const c_void) -> FfiBool>,
    pub set_active: Option<unsafe extern "C" fn(*mut c_void, FfiBool) -> FfiResult>,
}
```

When resolving `"bam.v0.my_game::UnitLabel.vtable"` via `resolve_vtable` (or `get_proc_address` in `bevy_abi_mod_guest_sys`), `VTableHeader::validate` verifies `struct_size`, `abi_version`, and `uuid` before any function pointer is invoked.

---

## 3. Panic & Unwind Policy: `panic = "unwind"` vs. `panic = "abort"` (`G-05`, `S-09`)

Every callback across `bevy_abi_mod` uses `extern "C"` (`nounwind` since Rust 1.81), **never** `extern "C-unwind"`. The host never wraps foreign `extern "C"` calls in `std::panic::catch_unwind` because catching a foreign `.cdylib` panic across independent Rust runtimes would free guest-allocated `Box<dyn Any>` payloads on the host allocator (heap corruption) and rely on unstable unwind ABI internals.

### Choosing Your Rust Guest Panic Strategy

| Guest Cargo Profile | What Happens When Guest Code Panics | When to Choose |
|---|---|---|
| **`panic = "unwind"`** *(Cargo default)* | `bevy_abi_mod_guest_sys::catch_guest_*` catches the unwind **inside the guest `.cdylib`**, drops the panic payload on the **guest** allocator, and returns `FfiResult::PANIC_CAUGHT` (`3`) by value. The host discards uncommitted commands from that tick, emits `ModSystemFaulted`, and applies `ModErrorPolicy` (for example, quarantining the offending system while keeping the game running). | **Recommended for most mods.** Enables recoverable per-system fault quarantine without taking down the host application. |
| **`panic = "abort"`** | `std::panic::catch_unwind` compiles to a direct call. Any `panic!`, `.unwrap()`, or out-of-bounds index inside the guest invokes `abort()` immediately inside the guest runtime, terminating the entire OS process cleanly without UB. | Smaller `.cdylib` binary size and zero unwind-table overhead when fail-fast process termination on invariant failure is acceptable. |

```toml
# Optional: smaller guest .cdylib binary, fail-fast process termination on any guest panic:
[profile.release]
panic = "abort"
```

### Non-Rust Fault Rules (C99 / C++ / Zig)
- **ISO C99:** Validate pointers and `BamResult` codes explicitly and return non-zero `BamResult` values to trigger recoverable host `ModErrorPolicy` handling. Calling `abort()`, `longjmp` across `extern "C"`, or dereferencing `NULL` terminates the process (`longjmp` across `extern "C"` is Undefined Behavior).
- **C++17/20:** Mark `extern "C"` callbacks `noexcept` or wrap bodies in `try { ... } catch (...) { return BAM_RESULT_PANIC_CAUGHT; }`. An uncaught C++ exception reaching `extern "C"` invokes `std::terminate()`.
- **Zig:** Map Zig error unions to `BamResult` at the callback boundary; `@panic` aborts the process.

---

## 4. Writing a Mod in Pure ISO C99 (`include/bevy_abi_mod.h`)

`include/bevy_abi_mod.h` is generated from `crates/bevy_abi_mod_sys` via `cargo xtask header` and compiles cleanly under `-std=c99 -Wall -Wextra -Wpedantic -Werror`. Note that `<stdbool.h>` is intentionally omitted (`BamBool` is `uint8_t`: `BAM_FALSE = 0u`, `BAM_TRUE = 1u`).

Store your bounded `BamHostApi` copy and pre-resolved `BamComponent` handles inside a per-instance struct returned through `*instance_out` so loading the same `.so`/`.dylib`/`.dll` into multiple Bevy `World`s stays completely isolated:

```c
#include "bevy_abi_mod.h"
#include <stdlib.h>

typedef struct Position {
    float x;
    float y;
    float z;
} Position;

typedef struct ElevationInstance {
    BamHostApi   api;
    BamComponent pos_comp;
} ElevationInstance;

static const BamManifest MOD_MANIFEST = {
    sizeof(BamManifest),
    BAM_ABI_VERSION,
    "c_elevation_mod",
    "0.2.0",
    0u,
    0u
};

static BamResult elevate_system(BamSystemCtx* sys_ctx, void* user_data) {
    ElevationInstance* inst = (ElevationInstance*)user_data;
    BamIter* iter = NULL;
    size_t chunk_len = 0u;
    BamResult res = inst->api.sys_query(sys_ctx, 0u, &iter);
    if (res != BAM_RESULT_OK) {
        return res;
    }

    while (inst->api.iter_next_chunk(iter, &chunk_len) != BAM_FALSE) {
        Position* pos = (Position*)inst->api.iter_column_mutable(iter, 0u);
        if (pos == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }
        for (size_t i = 0u; i < chunk_len; ++i) {
            pos[i].y += 1.0f;
        }
    }
    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MOD_MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx*       load_ctx,
    BamInstance**     instance_out
) {
    BamHostApi local_api;
    const BamHostCtx* host_ctx;
    ElevationInstance* inst;
    BamTermDesc terms[1];
    BamQueryDesc queries[1];
    BamSystemDesc sys_desc;
    BamResult res;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    res = bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api);
    if (res != BAM_RESULT_OK) {
        return res;
    }

    inst = (ElevationInstance*)calloc(1u, sizeof(ElevationInstance));
    if (inst == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    inst->api = local_api;

    host_ctx = inst->api.load_host_ctx(load_ctx);
    res = inst->api.resolve_component(host_ctx, "my_game::Position", NULL, &inst->pos_comp);
    if (res != BAM_RESULT_OK) {
        free(inst);
        return res;
    }

    terms[0] = bam_term_desc(inst->pos_comp, BAM_ACCESS_WRITE, 0u);
    memset(&queries[0], 0, sizeof(BamQueryDesc));
    queries[0].struct_size = sizeof(BamQueryDesc);
    queries[0].terms       = terms;
    queries[0].term_count  = 1u;

    memset(&sys_desc, 0, sizeof(BamSystemDesc));
    sys_desc.struct_size = sizeof(BamSystemDesc);
    sys_desc.name        = "elevate_system";
    sys_desc.queries     = queries;
    sys_desc.query_count = 1u;
    sys_desc.callback    = elevate_system;
    sys_desc.user_data   = inst;
    sys_desc.schedule    = BAM_SCHEDULE_UPDATE;

    res = inst->api.register_system(load_ctx, &sys_desc);
    if (res != BAM_RESULT_OK) {
        free(inst);
        return res;
    }

    *instance_out = (BamInstance*)inst;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    if (instance != NULL) {
        free(instance);
    }
}
```

### Compiling C99 Shared Libraries

```bash
# macOS
clang -std=c99 -Wall -Wextra -Wpedantic -Werror -shared -fPIC -O3 -I../../include -o libc_elevation_mod.dylib mod.c

# Linux
gcc -std=c99 -Wall -Wextra -Wpedantic -Werror -shared -fPIC -O3 -I../../include -o libc_elevation_mod.so mod.c

# Windows (MSVC)
cl /LD /O2 /W4 /WX /I..\..\include mod.c /Fe:c_elevation_mod.dll
```

---

## 5. Threading, Handle Lifetimes & Safety Checklist

- **Worker-Thread Safety (`S-11`, `B-07`):** Registered systems run on Bevy's multi-threaded `ComputeTaskPool` worker threads by default. Never mutate unsynchronized C `static` globals across parallel systems; store mutable state inside `BamInstance` / `user_data` or pass `BAM_SYSTEM_FLAG_MAIN_THREAD` (`.main_thread()` in Rust) to pin execution to Bevy's main thread.
- **Scoped Handle Lifetimes (`S-08`):** `BamLoadCtx*` is valid only inside `bevy_abi_mod_init`. `BamSystemCtx*`, `BamIter*`, and `BamCommands*` are valid strictly during the active `BamSystemFn` call (enforced by host `{ magic, generation }` handle headers in debug / `strict-ffi-checks` builds). Column pointers are invalidated on the next `iter_next_chunk` or `iter_reset`.
- **Null Entity Sentinel:** Use `FfiEntity::NULL` / `BAM_ENTITY_NULL` (`0ULL`) as the invalid entity sentinel (`u64::MAX` is a valid Bevy 0.20+ entity).
- **Inspect Host Error Messages:** In C/C++/Zig mods, call `api.last_error(host_ctx, buf, sizeof(buf))` whenever an API call returns a non-`OK` `BamResult` to retrieve the exact human-readable host diagnostic.

---

## 6. Related References

- **[Specification (`spec.md`)](../spec.md):** Authoritative `ABI 0.2.0` wire specification, 7-crate boundaries, panic/threading rules, and host lifecycle internals.
- **[Migration Guide (`0.1` -> `0.2`)](MIGRATION_0.1_TO_0.2.md):** Complete crate, type, macro, and C99 identifier rename tables with side-by-side examples.
- **[Architecture Decision Records (`docs/adr/`)](adr/0002-abi-v0.2.md):** Design rationale for drop trampolines (`0001`), ABI `0.2` (`0002`), handles & arena commands (`0003`), panic policy (`0004`), per-World `BamInstance` (`0005`), and the `-sys` crate split (`0006`).
