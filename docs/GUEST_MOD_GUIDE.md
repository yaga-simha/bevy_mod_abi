# Guest Mod Developer Guide: `bevy_abi_mod`

This guide explains how to develop, compile, and distribute native mods for games using the **`bevy_abi_mod`** framework.

---

## 1. Overview & Toolchain Independence

Unlike traditional Rust dynamic plugins, `bevy_abi_mod` enforces **zero shared Rust ABI assumptions**.
- You can write your mod in **Rust** (any `rustc` version), **C99/C11**, **C++17/20**, or **Zig**.
- Your mod links against **zero Bevy dependencies**.
- You can optimize your mod independently without affecting or needing to match the host game's compiler flags.

---

## 2. Writing a Mod in Rust (using `guest_sdk`)

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
# Light-weight, Bevy-independent SDK providing C-ABI wrappers
guest_sdk = { path = "../../crates/guest_sdk" }
```

### 2.2 Define Component VTable Mirrors
To interact with host components, define a `#[repr(C)]` struct matching the component's exposed VTable:

```rust
use core::ffi::c_void;

#[repr(C)]
pub struct PositionVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct VelocityVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}
```

### 2.3 Implement Your ECS System
Wrap your system function with `wrap_system_callback!` to ensure panics are trapped inside your `.cdylib` runtime:

```rust
use guest_sdk::prelude::*;
use core::ffi::c_void;

static mut POS_VTABLE: Option<&'static PositionVTable> = None;
static mut VEL_VTABLE: Option<&'static VelocityVTable> = None;
static mut HOST_API: Option<&'static HostApi> = None;

unsafe fn my_kinematics_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api = HOST_API.expect("HostApi not initialized");
    let pos_vt = POS_VTABLE.expect("PositionVTable not resolved");
    let vel_vt = VEL_VTABLE.expect("VelocityVTable not resolved");

    let mut chunk_iter = GuestChunkIter::new(iter, api);

    // Iterate over matching archetype tables
    while chunk_iter.next_chunk() > 0 {
        let count = chunk_iter.next_chunk(); // or count returned by next_chunk()
        let pos_col = chunk_iter.column_mutable(0);
        let vel_col = chunk_iter.column_readonly(1);

        for row in 0..count {
            let pos_ptr = pos_col.add(row * pos_vt.size) as *mut c_void;
            let vel_ptr = vel_col.add(row * vel_vt.size) as *const c_void;

            let cur_x = (pos_vt.get_x)(pos_ptr);
            let vel_x = (vel_vt.get_x)(vel_ptr);

            // Update position
            (pos_vt.set_x)(pos_ptr, cur_x + vel_x * 0.016);

            // Notify Bevy's change detection
            chunk_iter.mark_changed(0, row);
        }
    }

    FfiResult::OK
}

// Generate C-compatible callback with panic isolation
wrap_system_callback!(fn c_kinematics_system calls my_kinematics_system);
```

### 2.4 Mod Initialization & Registration
Export the lifecycle hooks using the `export_mod!` macro:

```rust
unsafe fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    HOST_API = Some(&*host_api);

    // Resolve component VTables by symbol name
    let pos_sym = get_proc_address(b"Position_vtable\0".as_ptr() as *const _);
    let vel_sym = get_proc_address(b"Velocity_vtable\0".as_ptr() as *const _);

    if pos_sym.is_null() || vel_sym.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }

    POS_VTABLE = Some(&*(pos_sym as *const PositionVTable));
    VEL_VTABLE = Some(&*(vel_sym as *const VelocityVTable));

    // Register our system into Bevy's schedule
    let queries = [
        ComponentAccessDesc {
            name: b"Position\0".as_ptr() as *const _,
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: b"Velocity\0".as_ptr() as *const _,
            mode: ComponentAccessMode::READ,
        },
    ];

    let descriptor = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: b"MyKinematicsSystem\0".as_ptr() as *const _,
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: c_kinematics_system,
        user_data: core::ptr::null_mut(),
    };

    let api = &*host_api;
    (api.register_system)(host_context, &descriptor)
}

unsafe fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    // Clean up any mod-allocated memory or background workers
    FfiResult::OK
}

// Export lifecycle entry points: bevy_abi_mod_abi_version, bevy_abi_mod_init, bevy_abi_mod_shutdown
export_mod!(mod_init, mod_shutdown);
```

---

## 3. Writing a Mod in Pure C99 / C++

Include the canonical header `include/bevy_abi_mod.h` and compile with any C compiler (`clang`, `gcc`, `MSVC`).

### 3.1 C99 Implementation Example (`mod.c`)

```c
#include "bevy_abi_mod.h"
#include <stdio.h>

static const HostApi* g_host_api = NULL;

/* VTable mirror */
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

/* System callback */
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

        for (size_t i = 0; i < count; ++i) {
            void* pos_ptr = pos_col + (i * g_pos_vt->size);
            float cur_y = g_pos_vt->get_y(pos_ptr);
            g_pos_vt->set_y(pos_ptr, cur_y + 1.0f);
            g_host_api->iter_mark_changed(iter, 0, i);
        }
    }
    return FFI_RESULT_OK;
}

/* Mod Lifecycle Hooks */
BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
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
    return FFI_RESULT_OK;
}
```

### 3.2 Compilation Command
```bash
# macOS
clang -std=c99 -shared -fPIC -O3 -I../../include -o libmy_c_mod.dylib mod.c

# Linux
gcc -std=c99 -shared -fPIC -O3 -I../../include -o libmy_c_mod.so mod.c

# Windows
cl /LD /O2 /I..\..\include mod.c /Fe:my_c_mod.dll
```

---

## 4. Deferred Commands: Spawning and Despawning

Mods can queue deferred entity operations executed during Bevy's `apply_deferred` phase:

| Command | Function Signature | Description |
|---|---|---|
| **Spawn Empty** | `cmd_spawn_empty(queue) -> FfiEntity` | Allocates a new entity ID. |
| **Despawn** | `cmd_despawn(queue, entity) -> FfiResult` | Despawns entity and drops components. |
| **Insert Component** | `cmd_insert_component(queue, entity, name, bytes, len) -> FfiResult` | Adds a component to entity by raw byte copy. |

In Rust with `GuestCommandQueue`:
```rust
let mut queue = GuestCommandQueue::new(commands, cmd_vtable);
let new_entity = queue.spawn_empty();
queue.insert_bytes(new_entity, b"Position\0".as_ptr() as *const _, data_ptr, data_len);
```

---

## 5. Modding Best Practices & Safety Checklist

- [x] **Always check return status**: Verify `get_proc_address` does not return `NULL` for required components.
- [x] **Declare exact permissions**: Use `ComponentAccessMode::READ` whenever writing is not required; this maximizes parallel CPU execution.
- [x] **Avoid allocating memory across FFI**: Never attempt to `free()` a pointer returned by the host.
- [x] **Always notify change detection**: Call `mark_changed(col, row)` when modifying component fields if downstream systems use `Changed<T>`.
- [x] **Catch all guest panics**: Always use `wrap_system_callback!` in Rust mods to prevent abnormal process termination.
