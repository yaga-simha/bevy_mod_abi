# Guest Mod Developer Guide: `bevy_abi_mod`

This guide explains how to develop, compile, and distribute native mods for games using the **`bevy_abi_mod`** framework.

---

## 1. Overview & Toolchain Independence

Unlike traditional Rust dynamic plugins, `bevy_abi_mod` enforces **zero shared Rust ABI assumptions**:
- You can write your mod in **Rust** (any `rustc` version), **ISO C99/C11**, **C++17/20**, or **Zig**.
- Your mod links against **zero Bevy dependencies** (`guest_sdk` depends solely on `#![no_std]` `abi_contract`).
- You can optimize or debug your mod independently without matching the host game's compiler version or Cargo profile.

### Two Safe Ways to Access Host Components
1. **Zero-Copy Typed Column Slices (`#[derive(AbiPod)]`)**: When a host component is `#[repr(C)]` / `#[repr(transparent)]` Plain Old Data (`f32`, integers, `FfiEntity`, arrays), define the matching `#[repr(C)] #[derive(Clone, Copy, AbiPod)]` struct in your mod and borrow `&[T]` / `&mut [T]` slices directly via `Chunk` split-borrow views (`chunk.read_write`, `chunk.column_mut`, `chunk.write_2`).
2. **Generated `#[repr(C)]` VTables (`resolve_vtable`)**: When a host component uses `#[repr(Rust)]`, `String`, `Vec<T>`, `bool` (`FfiBool = u8`), or `#[mod_component(opaque)]`, resolve `<Component>_vtable` via `app.resolve_vtable` and invoke its C-ABI getters and setters—never relying on `#[repr(Rust)]` field offsets or raw byte copies.

---

## 2. Writing an Idiomatic Rust Mod (`GuestApp` & `Chunk` Split Borrows)

### 2.1 `Cargo.toml` Setup
Create a standalone Cargo package configured as a C dynamic library (`cdylib`):

```toml
[package]
name = "my_custom_mod"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
guest_sdk = { path = "../../crates/guest_sdk" }
mod_derive = { path = "../../crates/mod_derive" } # optional: for #[derive(AbiPod)]
```

### 2.2 Single-Query & Multi-Column Split Borrows with `GuestApp`
`GuestApp` eliminates `static mut` FFI boilerplate and `Box::into_raw` memory leaks. Any closures, `alloc_user_data` state, or `on_shutdown` hooks registered on `GuestApp` roll back automatically if `init` fails and are freed in LIFO order during `bevy_abi_mod_shutdown`:

```rust
use guest_sdk::prelude::*;
use mod_derive::AbiPod;

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    let queries = [
        ComponentAccessDesc::write(c"Position".as_ptr()),
        ComponentAccessDesc::read(c"Velocity".as_ptr()),
    ];

    app.add_system(c"kinematics_system", &queries, |ctx: &mut SystemContext<'_>| {
        // `next_chunk_view` yields a scoped `Chunk` that statically prevents advancing
        // or resetting the iterator while any column or entity slice borrow is live.
        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            // Borrow entity IDs (&[FfiEntity]), column 1 (&[Velocity]), and column 0 (&mut [Position])
            // simultaneously with alignment, stride, index, and byte-range overlap checks:
            let Some((entities, vel, pos)) =
                chunk.entities_and_read_write::<Velocity, Position>(1, 0)
            else {
                return FfiResult::INTERNAL_ERROR;
            };

            for ((entity, p), v) in entities.iter().zip(pos.iter_mut()).zip(vel.iter()) {
                if !entity.is_null() {
                    p.x += v.x * 0.016;
                    p.y += v.y * 0.016;
                    p.z += v.z * 0.016;
                }
            }
        }
        FfiResult::OK
    })
}

// Exports `bevy_abi_mod_abi_version`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`
// wrapped in `catch_guest_panic` so guest panics never unwind across `extern "C"`.
export_mod!(setup);
```

 You can also structure mods using the `GuestPlugin` trait (`export_mod!(plugin: MyPlugin)`), or borrow two mutable columns simultaneously via `chunk.write_2::<Pos, Vel>(0, 1)` or `chunk.columns_mut([0, 1], count)`.

### 2.3 Nested Multi-Query Systems (`MultiQueryContext`)
For $N \times M$ entity interactions (up to `MAX_GUEST_MULTI_QUERIES = 8` disjoint queries), use `GuestApp::add_multi_query_system`:

```rust
let unit_q = [
    ComponentAccessDesc::write(c"Position".as_ptr()),
    ComponentAccessDesc::without(c"StaticEmitter".as_ptr()),
];
let emitter_q = [
    ComponentAccessDesc::read(c"Position".as_ptr()),
    ComponentAccessDesc::filter_only(c"StaticEmitter".as_ptr()),
];

app.add_multi_query_system(
    c"aura_repulsion",
    &[&unit_q, &emitter_q],
    |ctx: &mut MultiQueryContext<'_>| {
        let Some((units, emitters, _commands)) = ctx.iters_2_and_commands(0, 1) else {
            return FfiResult::INTERNAL_ERROR;
        };

        while let Some(mut u_chunk) = units.next_chunk_view() {
            let Some(u_pos) = u_chunk.column_mut::<Position>(0) else {
                return FfiResult::INTERNAL_ERROR;
            };
            // O(1) zero-allocation cursor rewind on the inner query:
            let _ = emitters.reset();
            while let Some(e_chunk) = emitters.next_chunk_view() {
                let Some(e_pos) = e_chunk.column::<Position>(0) else {
                    return FfiResult::INTERNAL_ERROR;
                };
                for u in u_pos.iter_mut() {
                    for e in e_pos.iter() {
                        u.x += (u.x - e.x) * 0.001;
                    }
                }
            }
        }
        FfiResult::OK
    },
);
```

---

## 3. VTable Access (`String`, `Vec`, `bool`) & Scheduled Ordering

### 3.1 Accessing Non-POD Host Components via VTables
For `#[repr(Rust)]` components or components containing `String`, `Vec<T>`, or `bool`, `mod_derive` generates a `#[repr(C)]` VTable where booleans use `FfiBool` (`u8`: `0` = false, non-zero = true) and strings use borrowed `FfiSlice<u8>` (`StrView`):

```rust
use core::ffi::c_void;
use guest_sdk::prelude::*;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnitStatusVTable {
    pub size: usize,
    pub align: usize,
    pub get_active: unsafe extern "C" fn(*const c_void) -> u8, // FfiBool (u8, never Rust bool)
    pub set_active: unsafe extern "C" fn(*mut c_void, u8),
    pub get_label: unsafe extern "C" fn(*const c_void) -> FfiSlice<u8>,
    pub set_label: unsafe extern "C" fn(*mut c_void, *const u8, usize),
}

fn setup_vtable_system(app: &mut GuestApp<'_>) -> FfiResult {
    // SAFETY: `UnitStatusVTable` matches the host's generated `#[repr(C)]` VTable layout.
    let Some(vt) = (unsafe { app.resolve_vtable::<UnitStatusVTable>(c"UnitStatus_vtable") }) else {
        return FfiResult::INVALID_COMPONENT;
    };
    let vt_copy = *vt;
    let queries = [ComponentAccessDesc::write(c"UnitStatus".as_ptr())];

    app.add_system(c"toggle_status", &queries, move |ctx| {
        let iter = ctx.iter();
        loop {
            let count = iter.next_chunk();
            if count == 0 {
                break;
            }
            let base = iter.column_mutable(0);
            for row in 0..count {
                // SAFETY: `row < count` and `vt_copy.size` is the host component's exact stride.
                unsafe {
                    let elem = base.add(row * vt_copy.size) as *mut c_void;
                    if (vt_copy.get_active)(elem) == 0 {
                        (vt_copy.set_active)(elem, 1);
                        let tag = b"activated";
                        (vt_copy.set_label)(elem, tag.as_ptr(), tag.len());
                        iter.mark_changed(0, row);
                    }
                }
            }
        }
        FfiResult::OK
    })
}
```

### 3.2 Targeting Specific Bevy Schedules & Ordering (`ScheduledSystemDescriptor`)
Guests can register systems into `ScheduleLabelId::{STARTUP, PRE_UPDATE, UPDATE, POST_UPDATE, FIXED_UPDATE, LAST}` with explicit `.after(...)` / `.before(...)` system-name ordering using `SystemDescriptor::new(...).with_schedule(...)`:

```rust
use abi_contract::{RegisterScheduledSystemFn, ScheduleLabelId, SystemDescriptor};

let after = [c"input_gather".as_ptr()];
let sdesc = SystemDescriptor::new(c"physics_step".as_ptr(), queries.as_ptr(), queries.len(), cb)
    .with_schedule(ScheduleLabelId::FIXED_UPDATE)
    .with_ordering(after.as_ptr(), after.len(), core::ptr::null(), 0);
```

---

## 4. Writing a Mod in Pure ISO C99 / C++

Include `include/bevy_abi_mod.h` (`-std=c99 -Wall -Wextra -Werror`). Notice `<stdbool.h>` is omitted—use `FfiBool` (`FFI_BOOL_TRUE` / `FFI_BOOL_FALSE`) and `BAM_ENTITY_NULL` (`0ULL`):

```c
#include "bevy_abi_mod.h"

static const HostApi* g_host_api = NULL;

typedef struct PositionVTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
} PositionVTable;

static const PositionVTable* g_pos_vt = NULL;

static FfiResult my_c_system(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
) {
    (void)commands;
    (void)user_data;

    size_t count;
    while ((count = g_host_api->iter_next_chunk(iter)) > 0) {
        uint8_t* pos_col = g_host_api->iter_column_mutable(iter, 0);
        if (!pos_col) return FFI_RESULT_NULL_POINTER;

        for (size_t i = 0; i < count; ++i) {
            void* pos_ptr = pos_col + (i * g_pos_vt->size);
            float cur_y = g_pos_vt->get_y(pos_ptr);
            g_pos_vt->set_y(pos_ptr, cur_y + 1.0f);
            g_host_api->iter_mark_changed(iter, 0, i);
        }
    }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api || host_api->struct_size < sizeof(HostApi) ||
        !bevy_abi_version_is_compatible(host_api->abi_version, BEVY_ABI_VERSION_PACKED)) {
        return FFI_RESULT_INVALID_VERSION;
    }
    g_host_api = host_api;
    g_pos_vt = (const PositionVTable*)get_proc_address("Position_vtable");
    if (!g_pos_vt) return FFI_RESULT_INVALID_COMPONENT;

    ComponentAccessDesc queries[] = {
        { .name = "Position", .mode = COMPONENT_ACCESS_WRITE }
    };
    SystemDescriptor desc = {
        .struct_size = sizeof(SystemDescriptor),
        .name = "C_PositionElevationSystem",
        .queries = queries,
        .query_count = 1,
        .callback = my_c_system,
        .user_data = NULL
    };
    return host_api->register_system(host_context, &desc);
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    g_host_api = NULL;
    g_pos_vt = NULL;
    return FFI_RESULT_OK;
}
```

### Compilation Commands
```bash
# macOS
clang -std=c99 -Wall -Wextra -Werror -shared -fPIC -O3 -I../../include -o libmy_c_mod.dylib mod.c

# Linux
gcc -std=c99 -Wall -Wextra -Werror -shared -fPIC -O3 -I../../include -o libmy_c_mod.so mod.c

# Windows (MSVC or MinGW)
cl /LD /O2 /I..\..\include mod.c /Fe:my_c_mod.dll
```

---

## 5. Runtime Dynamic Components & Deferred Commands

Mods can register brand-new ECS components unknown to the host at compile time (`app.register_component::<MyTag>(c"MyMod::Tag")` or `impl GuestComponent`) and enqueue deferred structural commands via `ctx.commands()`:

| Command | Behavior & Safety Contract |
|---|---|
| `commands.spawn_empty() -> FfiEntity` | Immediately allocates a valid 64-bit `FfiEntity` (`!entity.is_null()`, i.e. `!= FfiEntity::NULL` / `0`); queues deferred spawn. |
| `commands.insert_value(entity, c"Position", &val)` | Copies `sizeof(T)` bytes into a host-owned aligned buffer. Allowed on POD/raw-insertable components and runtime-registered components; returns `FfiResult::PERMISSION_DENIED` on heap-owning (`String`/`Vec`), `bool`-bearing, or `opaque` host components. |
| `commands.insert_default(entity, c"NameTag")` | Invokes the host's registered `Default` constructor (`register_mod_component_with_default`), safely attaching even opaque `String`/`Vec` host components without raw byte copies. |
| `commands.remove_component(entity, c"Position")` | Queues deferred component removal (`entity_mut.remove_by_id`). |
| `commands.despawn(entity)` | Queues recursive Bevy despawn (respecting `ChildOf` hierarchies and foreign `ComponentDropFn` hooks). |

`GuestApp` also supports optional columns (`ComponentAccessDesc::optional_read` / `optional_write` with `chunk.optional_column::<T>` / `chunk.optional_column_mut::<T>`), row-level tick filters (`ComponentAccessDesc::changed` / `added`), singleton host resources (`ComponentAccessDesc::resource_read` / `resource_write` via `ctx.iter().guest_resource::<R>()` / `guest_resource_mut::<R>()` or `MultiQueryContext::guest_resource`), and conditional system execution (`app.add_system_if` / `app.add_multi_query_system_if` backed by `ConditionalSystemDescriptor`).

---

## 6. Modding Best Practices & Safety Checklist

- [x] **Use `#[derive(AbiPod)]` for typed slices**: Never cast `#[repr(Rust)]`, `String`, `Vec`, or `bool` host components to typed guest slices—use their generated VTables instead.
- [x] **Prefer `GuestApp` & `Chunk` views**: `next_chunk_view()` + `read_write` / `entities_and_read_write` prevent stale chunk pointers and overlapping column borrows at compile time and runtime.
- [x] **Check `FfiEntity::NULL` (`0`)**: Never use `u64::MAX` as an invalid entity sentinel (`u64::MAX` is a valid Bevy 0.20+ entity; `0` / `BAM_ENTITY_NULL` is the sole null sentinel).
- [x] **Avoid allocator crossover**: Never `free()` host pointers; let `GuestApp` or `bevy_abi_mod_shutdown` release guest-allocated memory.
- [x] **Catch all guest panics/exceptions**: Always use `export_mod!` / `wrap_system_callback!` in Rust and `try/catch` in C++.

---

## 7. Related Documentation

- **[C-ABI Specification & Non-Rust Guide](C_ABI_SPECIFICATION.md)**: Formal struct offsets, SemVer rules, and C99, C++17, and Zig reference mods.
- **[Host Integration Guide](HOST_INTEGRATION_GUIDE.md)**: Exposing components, `ModErrorPolicy`, `ModLoadError`, schedules, and hot-reloading.
- **[System Architecture](ARCHITECTURE.md)**: Formal memory safety proofs, `Table` vs. `SparseSet` chunk iteration, and concurrency invariants.
