# C-ABI Specification & Non-Rust Mod Authoring Guide

**Target Audience:** Non-Rust mod developers targeting ISO C99, C++17, and Zig, as well as tooling engineers implementing language bindings.  
**Framework Version:** `bevy_abi_mod` v0.1.0 (Compatible with Bevy ECS v0.20+)  
**Canonical C Header:** `include/bevy_abi_mod.h`

---

## Table of Contents
1. [Core Philosophy & Architecture](#1-core-philosophy--architecture)
   - [1.1 The Rust ABI Instability Barrier](#11-the-rust-abi-instability-barrier)
   - [1.2 The Asymmetric Zero-Serialization Solution](#12-the-asymmetric-zero-serialization-solution)
2. [Header & Type Definitions (`include/bevy_abi_mod.h`)](#2-header--type-definitions-includebevy_abi_modh)
   - [2.1 Memory Layout & Struct Offsets (64-Bit Platforms)](#21-memory-layout--struct-offsets-64-bit-platforms)
   - [2.2 Calling Conventions & Pointer Rules](#22-calling-conventions--pointer-rules)
   - [2.3 Status Codes (`FfiResult`)](#23-status-codes-ffiresult)
3. [Handshake & Lifecycle Protocol](#3-handshake--lifecycle-protocol)
   - [3.1 Exported Dynamic Symbols](#31-exported-dynamic-symbols)
   - [3.2 Lifecycle Sequence & State Machine](#32-lifecycle-sequence--state-machine)
   - [3.3 VTable Resolution via `GetProcAddressFn`](#33-vtable-resolution-via-getprocaddressfn)
4. [Dynamic System Registration & Execution](#4-dynamic-system-registration--execution)
   - [4.1 System Descriptors & Access Declarations](#41-system-descriptors--access-declarations)
   - [4.2 Scheduler Concurrency & Borrow Checking](#42-scheduler-concurrency--borrow-checking)
   - [4.3 The System Execution Callback](#43-the-system-execution-callback)
   - [4.4 Column Chunk Iteration APIs](#44-column-chunk-iteration-apis)
   - [4.5 Multi-Query Dynamic Systems](#45-multi-query-dynamic-systems)
5. [Deferred Command Queue APIs & Allocator Isolation](#5-deferred-command-queue-apis--allocator-isolation)
   - [5.1 Why Commands Must Be Deferred](#51-why-commands-must-be-deferred)
   - [5.2 Immediate Entity ID Generation](#52-immediate-entity-id-generation)
   - [5.3 Strict Memory Allocator Isolation](#53-strict-memory-allocator-isolation)
   - [5.4 Command Function Signatures](#54-command-function-signatures)
6. [Component VTables & Reflection Model](#6-component-vtables--reflection-model)
   - [6.1 Generated Accessors & Mirror Structs](#61-generated-accessors--mirror-structs)
   - [6.2 String Views & Self-Aliasing Safety](#62-string-views--self-aliasing-safety)
7. [Complete Reference Implementations](#7-complete-reference-implementations)
   - [7.1 ISO C99 Reference Mod](#71-iso-c99-reference-mod)
   - [7.2 Modern C++17 Reference Mod](#72-modern-c17-reference-mod)
   - [7.3 Zig Reference Mod](#73-zig-reference-mod)
8. [Build Matrix & Cross-Platform Tooling](#8-build-matrix--cross-platform-tooling)
   - [8.1 Compiler Commands & Dynamic Flags](#81-compiler-commands--dynamic-flags)
   - [8.2 Common Pitfalls for Non-Rust Authors](#82-common-pitfalls-for-non-rust-authors)

---

## 1. Core Philosophy & Architecture

### 1.1 The Rust ABI Instability Barrier
In modern game engines built with Rust, dynamic plugin loading has historically been hindered by the absence of a stable Rust Application Binary Interface (ABI):
- **Compiler Incompatibility:** Dynamic libraries compiled with `rustc` version $X$ cannot be loaded safely by a host compiled with `rustc` version $Y$.
- **Flags and Profile Drift:** Changes to `-C opt-level`, target features (e.g. AVX2, NEON), or standard library minor updates change struct field packing, padding, and layout niche-filling.
- **Allocator Cross-Talk:** Mixing memory allocated by a host standard library with a guest standard library leads to fatal runtime crashes (`malloc: pointer being freed was not allocated`).
- **Panic Propagation:** Rust unwinding across foreign stack boundaries results in undefined behavior or immediate process aborts.

### 1.2 The Asymmetric Zero-Serialization Solution
`bevy_abi_mod` breaks this barrier by establishing an **asymmetric, zero-serialization C-ABI boundary**:
1. **Zero Shared Rust ABI:** The host and guest dynamic library share **no Rust types, no Rust standard library, and no Rust allocator state**. The boundary is defined strictly by ISO C primitives and `extern "C"` function pointers.
2. **Single-Handshake Symbol Resolution:** Modeled after Godot Engine's GDExtension architecture (`gdextension_interface.h`), the host passes a single symbol resolver function pointer (`GetProcAddressFn`) during initialization. The guest resolves all component VTables and host APIs through this pointer, eliminating brittle platform-specific dynamic linker symbol exports (`dlsym`/`GetProcAddress`).
3. **Hardware-Speed Contiguous Traversal:** Unlike serializing data over JSON, Bincode, or Protobuf (which introduces latency that shatters 60/120 FPS frame budgets), `bevy_abi_mod` grants systems direct raw pointers to contiguous Bevy Archetype `Table` columns. Access is 100% cache-friendly Structure-of-Arrays (SoA) memory traversal.
4. **First-Class Bevy Scheduler Integration:** Foreign systems register component read and write permissions with Bevy's `FilteredAccessSet`. Bevy's multithreaded scheduler executes guest C99/C++/Zig systems in parallel across worker threads with full compile-time borrow safety.

```text
┌────────────────────────────────────────────────────────────────────────┐
│                        Bevy Host Game Engine                           │
│  - Archetype Table Storage (Contiguous Column Slices)                  │
│  - Multithreaded Scheduler (FilteredAccessSet Borrow Checking)        │
│  - ModRegistry (Static VTables & Symbol Resolvers)                     │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
                  ══════════════════╪══════════════════
                  Strict ISO C-ABI  │ (bevy_abi_mod.h)
                  ══════════════════╪══════════════════
                                    │
    ┌───────────────────────────────┼───────────────────────────────┐
    │                               │                               │
    ▼                               ▼                               ▼
┌───────────────────────┐   ┌───────────────────────┐   ┌───────────────────────┐
│     ISO C99 Mod       │   │    Modern C++17 Mod   │   │       Zig Mod         │
│ - Pure C headers      │   │ - RAII wrappers       │   │ - extern struct       │
│ - Raw chunk iterators │   │ - Exception safety    │   │ - @export callbacks   │
│ - Zero dependencies   │   │ - Typed chunk views   │   │ - Safe comptime types │
└───────────────────────┘   └───────────────────────┘   └───────────────────────┘
```

---

## 2. Header & Type Definitions (`include/bevy_abi_mod.h`)

The canonical header file is located at `include/bevy_abi_mod.h`. It is completely standalone and depends **only** on `<stdint.h>` and `<stddef.h>` (`<stdbool.h>` is intentionally omitted; all C-ABI boolean values use `FfiBool` / `uint8_t` with `FFI_BOOL_FALSE = 0u` and `FFI_BOOL_TRUE = 1u` so non-canonical byte patterns `2..=255` can never trigger Rust `bool` UB).

### 2.1 Memory Layout & Struct Offsets (64-Bit Platforms)
All types have guaranteed `sizeof`, `alignof`, and `offsetof` across standard 64-bit systems (System V AMD64 ABI on Linux/macOS and Microsoft x64 ABI on Windows), verified at compile time by `BEVY_ABI_STATIC_ASSERT` in `include/bevy_abi_mod.h` (including 32-bit `ILP32` assertions):

| # | Type | `sizeof` | `alignof` | Field Name | Field Type | Field `offsetof` | Field `sizeof` | Notes / Padding |
|---|---|---|---|---|---|---|---|---|
| 1 | **`FfiBool`** | **1** | **1** | `(value)` | `uint8_t` | `0` | `1` | `0` = false, non-zero = true (`FFI_BOOL_FALSE` / `FFI_BOOL_TRUE`). |
| 2 | **`FfiResult`** | **4** | **4** | `(value)` | `uint32_t` | `0` | `4` | `#[repr(transparent)]` status code. |
| 3 | **`FfiEntity`** | **8** | **8** | `(value)` | `uint64_t` | `0` | `8` | Opaque 64-bit entity handle; `BAM_ENTITY_NULL = 0ULL` is the sole invalid sentinel. |
| 4 | **`FfiByteSlice`** | **16** | **8** | `ptr`<br>`len` | `const uint8_t*`<br>`size_t` | `0`<br>`8` | `8`<br>`8` | Borrowed byte/string slice (`StrView`). |
| 5 | **`ComponentUuid`** | **16** | **1** | `bytes[16]` | `uint8_t[16]` | `0` | `16` | Deterministic 128-bit 2-lane FNV-1a hash. |
| 6 | **`ComponentAccessMode`** | **4** | **4** | `(value)` | `uint32_t` | `0` | `4` | `READ=0`, `WRITE=1`, `FILTER_ONLY=2`, `WITHOUT=3`. |
| 7 | **`ScheduleLabelId`** | **4** | **4** | `(value)` | `uint32_t` | `0` | `4` | `UPDATE=0`, `STARTUP=1`, `PRE_UPDATE=2`, `POST_UPDATE=3`, `FIXED_UPDATE=4`, `LAST=5`. |
| 8 | **`ComponentAccessDesc`** | **16** | **8** | `name`<br>`mode`<br>*(padding)* | `const char*`<br>`ComponentAccessMode`<br>`-` | `0`<br>`8`<br>`12` | `8`<br>`4`<br>`4` | 4 bytes tail padding to satisfy 8-byte pointer alignment. |
| 9 | **`QuerySpec`** | **16** | **8** | `queries`<br>`query_count` | `const ComponentAccessDesc*`<br>`size_t` | `0`<br>`8` | `8`<br>`8` | Array of component descriptors (`query_count <= 32`). |
| 10 | **`SystemDescriptor`** | **48** | **8** | `struct_size`<br>*(padding)*<br>`name`<br>`queries`<br>`query_count`<br>`callback`<br>`user_data` | `uint32_t`<br>`-`<br>`const char*`<br>`const ComponentAccessDesc*`<br>`size_t`<br>`DynamicSystemCallback`<br>`void*` | `0`<br>`4`<br>`8`<br>`16`<br>`24`<br>`32`<br>`40` | `4`<br>`4`<br>`8`<br>`8`<br>`8`<br>`8`<br>`8` | v1 single-query descriptor (`query_count <= 32`). |
| 11 | **`ScheduledSystemDescriptor`** | **88** | **8** | `base`<br>`schedule`<br>`flags`<br>`after`<br>`after_count`<br>`before`<br>`before_count` | `SystemDescriptor`<br>`ScheduleLabelId`<br>`uint32_t`<br>`const char* const*`<br>`size_t`<br>`const char* const*`<br>`size_t` | `0`<br>`48`<br>`52`<br>`56`<br>`64`<br>`72`<br>`80` | `48`<br>`4`<br>`4`<br>`8`<br>`8`<br>`8`<br>`8` | Alias `SystemDescriptorV2`; layout-prefixed by `SystemDescriptor` at offset `0`. |
| 12 | **`MultiQuerySystemDescriptor`** | **48** | **8** | `struct_size`<br>*(padding)*<br>`name`<br>`query_specs`<br>`query_spec_count`<br>`callback`<br>`user_data` | `uint32_t`<br>`-`<br>`const char*`<br>`const QuerySpec*`<br>`size_t`<br>`MultiQuerySystemCallback`<br>`void*` | `0`<br>`4`<br>`8`<br>`16`<br>`24`<br>`32`<br>`40` | `4`<br>`4`<br>`8`<br>`8`<br>`8`<br>`8`<br>`8` | v1 multi-query descriptor (`1 <= query_spec_count <= 8`). |
| 13 | **`ScheduledMultiQuerySystemDescriptor`** | **88** | **8** | `base`<br>`schedule`<br>`flags`<br>`after`<br>`after_count`<br>`before`<br>`before_count` | `MultiQuerySystemDescriptor`<br>`ScheduleLabelId`<br>`uint32_t`<br>`const char* const*`<br>`size_t`<br>`const char* const*`<br>`size_t` | `0`<br>`48`<br>`52`<br>`56`<br>`64`<br>`72`<br>`80` | `48`<br>`4`<br>`4`<br>`8`<br>`8`<br>`8`<br>`8` | Alias `MultiQuerySystemDescriptorV2`; layout-prefixed by `MultiQuerySystemDescriptor`. |
| 14 | **`ModComponentDescriptor`** | **64** | **8** | `struct_size`<br>*(padding)*<br>`name`<br>`uuid`<br>`size`<br>`align`<br>`drop_fn`<br>`vtable_ptr` | `uint32_t`<br>`-`<br>`const char*`<br>`ComponentUuid`<br>`size_t`<br>`size_t`<br>`ComponentDropFn`<br>`const void*` | `0`<br>`4`<br>`8`<br>`16`<br>`32`<br>`40`<br>`48`<br>`56` | `4`<br>`4`<br>`8`<br>`16`<br>`8`<br>`8`<br>`8`<br>`8` | Alias `DynamicComponentDescriptor`; runtime component registration. |
| 15 | **`HostApi`** | **72** | **8** | `struct_size`<br>`abi_version`<br>`register_system`<br>`iter_next_chunk`<br>`iter_column_readonly`<br>`iter_column_mutable`<br>`iter_mark_changed`<br>`iter_entities`<br>`iter_column_stride`<br>`register_component` | `uint32_t`<br>`uint32_t`<br>`RegisterSystemFn`<br>`IterNextChunkFn`<br>`IterColumnReadonlyFn`<br>`IterColumnMutableFn`<br>`IterMarkChangedFn`<br>`IterEntitiesFn`<br>`IterColumnStrideFn`<br>`RegisterComponentFn` | `0`<br>`4`<br>`8`<br>`16`<br>`24`<br>`32`<br>`40`<br>`48`<br>`56`<br>`64` | `4`<br>`4`<br>`8`<br>`8`<br>`8`<br>`8`<br>`8`<br>`8`<br>`8`<br>`8` | Backward-compatible with 64-byte legacy mods via `struct_size >= 64`. |

### 2.2 Calling Conventions & Pointer Rules
1. **Calling Convention:** All dynamic entry points and function pointer callbacks strictly adhere to standard C calling conventions (`cdecl` on x86, standard register conventions on x86_64 and AArch64).
2. **String Pointers:**
   - Component names (`ComponentAccessDesc::name`) and system names (`SystemDescriptor::name`) must be valid, non-null, null-terminated ASCII or UTF-8 C strings (`*const char`).
3. **Handle Opaqueness:**
   - `RawChunkIter` and `RawCommandQueue` are opaque structs (`typedef struct RawChunkIter RawChunkIter;`). Guests must never dereference or allocate these pointers; they must only pass them back to host API functions.
4. **Column Pointer Validity:**
   - Pointers returned by `iter_column_readonly` and `iter_column_mutable` point directly to contiguous table memory for the **active chunk only**.
   - Calling `iter_next_chunk` advances the archetype table cursor; any column pointers obtained from previous chunks become invalid.
   - If an index was registered as `COMPONENT_ACCESS_READ`, calling `iter_column_mutable` returns `NULL`.
   - If an index was registered as `COMPONENT_ACCESS_FILTER_ONLY` or `COMPONENT_ACCESS_WITHOUT`, both column getters return `NULL`.
5. **Context Pointers:**
   - `user_data` in `SystemDescriptor` is guest-owned memory. The host forwards it untouched to the system callback and never reads, modifies, or deallocates it.

### 2.3 Status Codes (`FfiResult`)
Status codes are defined as 32-bit integers. Any value other than `FFI_RESULT_OK` (0) signals an error:

```c
#define FFI_RESULT_OK                 0u  /* Operation completed successfully */
#define FFI_RESULT_INVALID_COMPONENT  1u  /* Component name/UUID not found in host registry */
#define FFI_RESULT_ACCESS_CONFLICT    2u  /* Borrow conflict in query declaration */
#define FFI_RESULT_PANIC_CAUGHT       3u  /* Guest caught an internal panic/exception */
#define FFI_RESULT_NULL_POINTER       4u  /* Null pointer passed for mandatory argument */
#define FFI_RESULT_INVALID_UTF8       5u  /* String argument contains invalid UTF-8 bytes */
#define FFI_RESULT_PERMISSION_DENIED  6u  /* Unauthorized access (e.g. write to read-only column) */
#define FFI_RESULT_INVALID_VERSION    7u  /* Host/guest ABI version mismatch */
#define FFI_RESULT_INTERNAL_ERROR     99u /* Host internal error */
```

---

## 3. Handshake & Lifecycle Protocol

### 3.1 Exported Dynamic Symbols
A mod shared library must export dynamic symbols using the platform visibility macro `BEVY_ABI_MOD_EXPORT`:
```c
#if defined(_WIN32)
    #define BEVY_ABI_MOD_EXPORT __declspec(dllexport)
#else
    #define BEVY_ABI_MOD_EXPORT __attribute__((visibility("default")))
#endif
```

The dynamic library exports three core lifecycle entry points:
1. `BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void);`
   - *Optional but recommended.* Returns `BEVY_ABI_VERSION_PACKED` (`(0 << 24) | (1 << 16) | 0`). Allows host to check ABI compatibility before initializing.
2. `BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(GetProcAddressFn get_proc_address, const HostApi* host_api, void* host_context);`
   - *Mandatory.* Primary initialization hook. The mod verifies version compatibility, caches `host_api`, resolves VTables and command functions, and registers dynamic ECS systems.
3. `BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context);`
   - *Mandatory.* Invoked when unloading or reloading the mod. Cleans up guest memory, resets global state, and returns `FFI_RESULT_OK`.

### 3.2 Lifecycle Sequence & State Machine

```text
   Host Engine (Bevy)                           Guest Mod (.so / .dll)
        │                                                │
        │ 1. Load library (dlopen / LoadLibrary)        │
        ├───────────────────────────────────────────────>│
        │ 2. bevy_abi_mod_abi_version()                  │
        ├───────────────────────────────────────────────>│
        │<───────────────────────────────────────────────┤ Returns BEVY_ABI_VERSION_PACKED
        │                                                │
        │ 3. bevy_abi_mod_init(get_proc, api, ctx)      │
        ├───────────────────────────────────────────────>│
        │                                                │ 3a. Validate host_api->struct_size
        │                                                │ 3b. Cache host_api pointer
        │                                                │ 3c. get_proc_address("Position_vtable")
        │<───────────────────────────────────────────────┤
        │───────────────────────────────────────────────>│ Returns static VTable pointer
        │                                                │ 3d. host_api->register_system(ctx, &desc)
        │<───────────────────────────────────────────────┤
        │ Registers DynamicCSystem in Bevy Schedule     │
        │<───────────────────────────────────────────────┤ Returns FFI_RESULT_OK
        │                                                │
        │ 4. Frame Execution Loop                        │
        │ ── DynamicCSystem::run ──────────────────────>│
        │    Invokes DynamicSystemCallback(iter, cmd)    │
        │    Iterates chunks, reads/writes SoA columns   │
        │<───────────────────────────────────────────────┤ Returns FFI_RESULT_OK
        │                                                │
        │ 5. Teardown / Hot-Reload                       │
        │    bevy_abi_mod_shutdown(ctx)                  │
        ├───────────────────────────────────────────────>│
        │                                                │ Free guest heap, reset globals
        │<───────────────────────────────────────────────┤ Returns FFI_RESULT_OK
        │ 6. Unload library (dlclose / FreeLibrary)     │
        ▼                                                ▼
```

### 3.3 VTable & Extension Resolution via `GetProcAddressFn`
The guest retrieves pointers by passing string names to `get_proc_address`:
- **Component VTables:** `"<ComponentName>_vtable"` (e.g., `"Position_vtable"`, `"Velocity_vtable"`). Returns `const <Component>VTable*`.
- **Deferred Command Bridge:**
  - `"cmd_spawn_empty"`: `CmdSpawnEmptyFn`
  - `"cmd_despawn"`: `CmdDespawnFn`
  - `"cmd_insert_component"`: `CmdInsertComponentFn`
- **Iteration Controls:**
  - `"iter_reset"`: `IterResetFn`
- **Advanced System & Component Registration:**
  - `"register_scheduled_system"`: `RegisterScheduledSystemFn`
  - `"register_multi_query_system"`: `RegisterMultiQuerySystemFn`
  - `"register_component"`: `RegisterComponentFn` (also directly on `HostApi::register_component`)

SemVer handshake compatibility is checked via `bevy_abi_version_is_compatible(host_api->abi_version, BEVY_ABI_VERSION_PACKED)` (`major ==` and `guest_minor <= host_minor`), while extensible descriptors (`SystemDescriptor`, `ScheduledSystemDescriptor`, `MultiQuerySystemDescriptor`, `ScheduledMultiQuerySystemDescriptor`, `ModComponentDescriptor`) enforce `struct_size >= MIN_V1_SIZE` and zero-fill trailing fields when older minor-version descriptors are passed.

---

## 4. Dynamic System Registration & Execution

### 4.1 System Descriptors & Access Declarations
A dynamic system registers its component requirements via `SystemDescriptor`:
```c
typedef struct SystemDescriptor {
    uint32_t struct_size;               /* Must be sizeof(SystemDescriptor) */
    const char* name;                   /* System name for diagnostics and profiling */
    const ComponentAccessDesc* queries; /* Pointer to array of component accesses */
    size_t query_count;                 /* Number of queries */
    DynamicSystemCallback callback;     /* Execution function pointer */
    void* user_data;                    /* Opaque context passed to callback */
} SystemDescriptor;
```

Each component query is specified by a `ComponentAccessDesc`:
```c
typedef struct ComponentAccessDesc {
    const char* name;                   /* Registered component name */
    ComponentAccessMode mode;           /* Access mode */
} ComponentAccessDesc;
```

There are four access modes:
1. `COMPONENT_ACCESS_READ (0)`: Read-only access (`&T`). Multiple systems may read concurrently.
2. `COMPONENT_ACCESS_WRITE (1)`: Exclusive mutable access (`&mut T`). Bevy's scheduler guarantees that no other system accesses this component during execution.
3. `COMPONENT_ACCESS_FILTER_ONLY (2)`: Archetype presence filter (`With<T>`). Matches entities having `T` without reading or allocating column buffers.
4. `COMPONENT_ACCESS_WITHOUT (3)`: Archetype absence filter (`Without<T>`). Matches entities lacking `T`. Proves query disjointness to Bevy's parallel scheduler.

### 4.2 Scheduler Concurrency & Borrow Checking
When `host_api->register_system(host_context, &desc)` is called:
1. The host resolves each component name to a Bevy `ComponentId`.
2. The host constructs a `FilteredAccessSet<ComponentId>`.
3. If two dynamic systems (or a dynamic system and an engine system) declare conflicting accesses (e.g. simultaneous `WRITE` and `READ`/`WRITE` on the same component without disjoint `Without<T>` filters), Bevy's scheduler prevents concurrent execution and orders them sequentially.
4. If invalid query configurations are declared (e.g., duplicate component names with conflicting modes), registration immediately fails with `FFI_RESULT_ACCESS_CONFLICT`.

### 4.3 The System Execution Callback
The execution callback must match `DynamicSystemCallback`:
```c
typedef FfiResult (*DynamicSystemCallback)(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
);
```
- `iter`: Opaque handle to the active chunk iterator.
- `commands`: Opaque handle for issuing deferred structural commands (spawns, despawns, component insertions).
- `user_data`: The custom pointer provided in `SystemDescriptor::user_data`.
- **Return Value:** Must return `FFI_RESULT_OK` on success. If an unrecoverable error or caught exception occurs, return `FFI_RESULT_PANIC_CAUGHT` or an appropriate error code.

### 4.4 Column Chunk Iteration APIs
The host processes archetype tables matching the system's queries. The guest drives this using the `HostApi` function pointers:

```c
/* Standard chunk traversal loop */
while (1) {
    /* 1. Advance to next archetype table chunk */
    size_t count = host_api->iter_next_chunk(iter);
    if (count == 0) {
        break; /* All matching tables exhausted */
    }

    /* 2. Retrieve column base pointers for declared queries */
    uint8_t* pos_col = host_api->iter_column_mutable(iter, 0);     /* Query 0 (WRITE) */
    size_t pos_stride = host_api->iter_column_stride(iter, 0);

    const uint8_t* vel_col = host_api->iter_column_readonly(iter, 1); /* Query 1 (READ) */
    size_t vel_stride = host_api->iter_column_stride(iter, 1);

    const FfiEntity* entities = host_api->iter_entities(iter);

    /* 3. Iterate contiguous entities in SoA order */
    for (size_t i = 0; i < count; ++i) {
        void* p_ptr = (void*)(pos_col + (i * pos_stride));
        const void* v_ptr = (const void*)(vel_col + (i * vel_stride));

        /* Access fields via resolved VTables */
        float z = pos_vtable->get_z(p_ptr);
        float vz = vel_vtable->get_z(v_ptr);
        pos_vtable->set_z(p_ptr, z + vz);

        /* 4. Mark change tick for changed row */
        host_api->iter_mark_changed(iter, 0, i);
    }
}
```

- **`iter_next_chunk(iter)`**: Advances to the next matching archetype table chunk. Returns the number of entities in that chunk, or `0` when traversal is finished.
- **`iter_column_readonly(iter, index)`**: Returns a `const uint8_t*` to the start of column `index`.
- **`iter_column_mutable(iter, index)`**: Returns a `uint8_t*` to the start of column `index` and automatically marks the chunk's component ticks as changed.
- **`iter_column_stride(iter, index)`**: Returns `sizeof(Component)` in bytes. Element $i$ is located at `base + (i * stride)`.
- **`iter_entities(iter)`**: Returns a pointer to the contiguous array `[FfiEntity; count]` identifying the entities in the current chunk.
- **`iter_mark_changed(iter, index, row)`**: Explicitly marks row `row` in column `index` as changed. This allows precise Bevy `Changed<T>` change detection downstream.

### 4.5 Multi-Query Dynamic Systems
For complex operations requiring nested queries (e.g. pairwise entity physics checks or spatial queries), mods can register multi-query systems:
```c
typedef struct QuerySpec {
    const ComponentAccessDesc* queries;
    size_t query_count;
} QuerySpec;

typedef struct MultiQuerySystemDescriptor {
    uint32_t struct_size;
    const char* name;
    const QuerySpec* query_specs;
    size_t query_spec_count;
    MultiQuerySystemCallback callback;
    void* user_data;
} MultiQuerySystemDescriptor;
```
The callback receives an array of independent chunk iterators:
```c
FfiResult callback(
    RawChunkIter* const* iters,
    size_t iter_count,
    RawCommandQueue* commands,
    void* user_data
);
```
To restart the inner loop cursor across outer loop iterations with zero memory allocations, call `iter_reset`:
```c
typedef FfiResult (*IterResetFn)(RawChunkIter* iter);
IterResetFn iter_reset = (IterResetFn)get_proc_address("iter_reset");
iter_reset(iters[1]); // Rewinds inner query iterator
```

Bounds enforced on all descriptors:
- `query_count <= 32` (`MAX_QUERY_COMPONENTS`); exceeding `32` returns `FFI_RESULT_INVALID_COMPONENT` (never silently truncating filter/access clauses).
- `1 <= query_spec_count <= 8` (`MAX_SYSTEM_QUERIES`); `0` or `> 8` returns `FFI_RESULT_INVALID_COMPONENT`.

### 4.6 Scheduled System Registration & Explicit Ordering (`ScheduledSystemDescriptor`)
To target non-default Bevy schedules (`BAM_SCHEDULE_STARTUP`, `BAM_SCHEDULE_PRE_UPDATE`, `BAM_SCHEDULE_UPDATE`, `BAM_SCHEDULE_POST_UPDATE`, `BAM_SCHEDULE_FIXED_UPDATE`, `BAM_SCHEDULE_LAST`) or declare explicit `.after(...)` / `.before(...)` ordering against named mod systems, populate `ScheduledSystemDescriptor` (`SystemDescriptorV2`) or `ScheduledMultiQuerySystemDescriptor` (`MultiQuerySystemDescriptorV2`):
```c
const char* after_names[] = { "input_gather_system" };
ScheduledSystemDescriptor sdesc;
memset(&sdesc, 0, sizeof(sdesc));
sdesc.base.struct_size = (uint32_t)sizeof(ScheduledSystemDescriptor);
sdesc.base.name = "physics_step_system";
sdesc.base.queries = queries;
sdesc.base.query_count = 2;
sdesc.base.callback = c_kinematics_system;
sdesc.schedule = BAM_SCHEDULE_FIXED_UPDATE;
sdesc.after = after_names;
sdesc.after_count = 1;

RegisterScheduledSystemFn reg_sched =
    (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");
FfiResult res = reg_sched
    ? reg_sched(host_context, &sdesc)
    : host_api->register_system(host_context, &sdesc.base);
```
Because `ScheduledSystemDescriptor` embeds `SystemDescriptor` at offset `0` with `base.struct_size == sizeof(ScheduledSystemDescriptor)`, both `register_scheduled_system(ctx, &sdesc)` and `host_api->register_system(ctx, &sdesc.base)` decode the schedule and ordering extensions seamlessly.

---

## 5. Deferred Command Queue APIs & Allocator Isolation

### 5.1 Why Commands Must Be Deferred
Bevy ECS organizes entities in densely packed archetype tables. Structural mutations—such as spawning/despawning entities or inserting/removing components—require reallocating and moving table rows. Performing structural changes synchronously during chunk iteration would invalidate active column pointers, resulting in use-after-free corruption.

Therefore, dynamic systems receive a `RawCommandQueue*` handle. Structural mutations are buffered during system execution and applied at Bevy's schedule synchronization boundary.

### 5.2 Immediate Entity ID Generation & `BAM_ENTITY_NULL`
Mods frequently need to spawn an entity and immediately attach components to it within the same frame. When a mod calls `cmd_spawn_empty(commands)`, the host invokes Bevy's atomic `EntityAllocator`:
- A valid, unique 64-bit `FfiEntity` ID is allocated immediately and returned to the guest (or `BAM_ENTITY_NULL` / `0ULL` if `commands` is null).
- Note that in Bevy 0.20+, `BAM_ENTITY_NULL` (`0ULL`, tested via `bam_entity_is_null(entity)`) is the **sole invalid entity sentinel** (`Entity::try_from_bits(0) == None`, whereas `UINT64_MAX` is a valid entity bit pattern). Passing `BAM_ENTITY_NULL` or `Entity::PLACEHOLDER` (`0x1`) to `cmd_despawn` or `cmd_insert_component` returns `FFI_RESULT_INVALID_COMPONENT`.
- A deferred command `world.spawn_empty_at(entity)` is queued, so the guest can immediately pass the returned `FfiEntity` to `cmd_insert_component`!

### 5.3 Strict Memory Allocator Isolation & Raw-Insertability Guards
Rule 2 of the ABI contract mandates: **Memory allocated by the guest must be freed by the guest; memory allocated by the host must be freed by the host.**

When a guest calls `cmd_insert_component`:
1. The host verifies that the target component allows raw-byte insertion (`IS_RAW_INSERTABLE == true` on static POD `ModExposable` components or runtime-registered `ModComponentDescriptor` types). Attempting to overwrite heap-owning (`String`, `Vec<T>`), `bool`-bearing, or `#[mod_component(opaque)]` host components via raw bytes is rejected with `FFI_RESULT_PERMISSION_DENIED`.
2. The guest passes a pointer to its own stack or heap memory: `data` with exact byte length `len == sizeof(Component)`.
3. The host immediately allocates a host-owned aligned buffer and **copies** the guest bytes.
4. The guest may immediately free, reuse, or overwrite its buffer upon function return; unloading the guest `.so`/`.dll` can never cause a dangling pointer on the host.

### 5.4 Command Function Signatures
Resolved via `get_proc_address`:

```c
/* 1. Deferred Spawn: Immediately returns a 64-bit entity ID */
typedef FfiEntity (*CmdSpawnEmptyFn)(RawCommandQueue* queue);

/* 2. Deferred Despawn: Marks an entity for deletion */
typedef FfiResult (*CmdDespawnFn)(RawCommandQueue* queue, FfiEntity entity);

/* 3. Deferred Component Insertion: Host copies `len` bytes from `data` */
typedef FfiResult (*CmdInsertComponentFn)(
    RawCommandQueue* queue,
    FfiEntity entity,
    const char* component_name,
    const uint8_t* data,
    size_t len
);
```

---

## 6. Component VTables & Reflection Model

### 6.1 Generated Accessors & Mirror Structs
Host components deriving `#[derive(Component, ModExposable)]` generate static C-ABI VTables. In non-Rust languages, mod authors define matching struct mirrors:

For example, given a host component:
```rust
#[derive(Component, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}
```
The host generates and exposes `PositionVTable`:
```c
typedef struct PositionVTable {
    size_t size;                          /* sizeof(Position) */
    size_t align;                         /* alignof(Position) */
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
} PositionVTable;
```
Resolved at runtime via:
```c
const PositionVTable* pos_vt = (const PositionVTable*)get_proc_address("Position_vtable");
```

### 6.2 String Views & Self-Aliasing Safety
For string components (e.g. `MetadataTag` with `label: String`):
- **Getters** return an `FfiByteSlice` (`StrView`):
  ```c
  StrView view = tag_vt->get_label(ptr);
  /* view.ptr is a borrowed pointer into host heap; valid for current chunk callback */
  ```
- **Setters** accept a byte pointer and byte length:
  ```c
  const char* new_text = "elite_warrior";
  tag_vt->set_label(ptr, (const uint8_t*)new_text, strlen(new_text));
  ```
- **Self-Aliasing Protection:** If a mod passes a component's own getter result back into its setter (e.g. modifying or re-applying its own label), `mod_derive` automatically checks pointer bounds and allocates an intermediate temporary to avoid undefined behavior.

---

## 7. Complete Reference Implementations

### 7.1 ISO C99 Reference Mod
This reference mod demonstrates complete C99 compatibility, registering a 3D kinematics system and issuing deferred entity commands.

```c
/* ============================================================================
 * reference_c_mod.c — Production Reference C99 Dynamic Mod for bevy_abi_mod
 * Compile: clang -std=c99 -shared -fPIC -Iinclude reference_c_mod.c -o libreference_c_mod.so
 * ============================================================================ */

#include "bevy_abi_mod.h"
#include <string.h>

/* Component VTable Mirrors */
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

typedef struct VelocityVTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
} VelocityVTable;

/* Deferred Command Signatures */
typedef FfiEntity (*CmdSpawnEmptyFn)(RawCommandQueue* queue);
typedef FfiResult (*CmdDespawnFn)(RawCommandQueue* queue, FfiEntity entity);
typedef FfiResult (*CmdInsertComponentFn)(
    RawCommandQueue* queue,
    FfiEntity entity,
    const char* component_name,
    const uint8_t* data,
    size_t len
);

/* Global Cached Host State */
static const HostApi* g_host_api = NULL;
static const PositionVTable* g_pos_vtable = NULL;
static const VelocityVTable* g_vel_vtable = NULL;
static CmdSpawnEmptyFn g_cmd_spawn_empty = NULL;
static CmdInsertComponentFn g_cmd_insert_component = NULL;

/* Kinematics System: Position (WRITE), Velocity (READ) */
static FfiResult c_kinematics_system(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
) {
    (void)commands;
    (void)user_data;

    if (!g_host_api || !g_pos_vtable) {
        return FFI_RESULT_INTERNAL_ERROR;
    }

    while (1) {
        size_t count = g_host_api->iter_next_chunk(iter);
        if (count == 0) {
            break;
        }

        uint8_t* pos_base = g_host_api->iter_column_mutable(iter, 0);
        size_t pos_stride = g_host_api->iter_column_stride(iter, 0);
        if (!pos_base) {
            return FFI_RESULT_NULL_POINTER;
        }

        const uint8_t* vel_base = NULL;
        size_t vel_stride = 0;
        if (g_vel_vtable) {
            vel_base = g_host_api->iter_column_readonly(iter, 1);
            vel_stride = g_host_api->iter_column_stride(iter, 1);
        }

        for (size_t i = 0; i < count; ++i) {
            void* p_ptr = (void*)(pos_base + (i * pos_stride));
            float z = g_pos_vtable->get_z(p_ptr);

            if (vel_base && g_vel_vtable) {
                const void* v_ptr = (const void*)(vel_base + (i * vel_stride));
                float vz = g_vel_vtable->get_z(v_ptr);
                g_pos_vtable->set_z(p_ptr, z + (vz != 0.0f ? vz : 1.0f));
            } else {
                g_pos_vtable->set_z(p_ptr, z + 1.0f);
            }

            /* Explicitly mark change detection for this row */
            g_host_api->iter_mark_changed(iter, 0, i);
        }
    }

    return FFI_RESULT_OK;
}

/* Mod Lifecycle Exports */

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api || host_api->struct_size < sizeof(HostApi)) {
        return FFI_RESULT_INVALID_VERSION;
    }
    if (!bevy_abi_version_is_compatible(host_api->abi_version, BEVY_ABI_VERSION_PACKED)) {
        return FFI_RESULT_INVALID_VERSION;
    }
    if (!get_proc_address) {
        return FFI_RESULT_NULL_POINTER;
    }

    g_host_api = host_api;

    /* Resolve VTables */
    g_pos_vtable = (const PositionVTable*)get_proc_address("Position_vtable");
    g_vel_vtable = (const VelocityVTable*)get_proc_address("Velocity_vtable");

    /* Resolve Command Functions */
    g_cmd_spawn_empty = (CmdSpawnEmptyFn)get_proc_address("cmd_spawn_empty");
    g_cmd_insert_component = (CmdInsertComponentFn)get_proc_address("cmd_insert_component");

    /* Register Kinematics System */
    if (host_context && host_api->register_system && g_pos_vtable) {
        static ComponentAccessDesc queries[2];
        queries[0].name = "Position";
        queries[0].mode = COMPONENT_ACCESS_WRITE;
        queries[1].name = "Velocity";
        queries[1].mode = COMPONENT_ACCESS_READ;

        SystemDescriptor desc;
        memset(&desc, 0, sizeof(desc));
        desc.struct_size = (uint32_t)sizeof(SystemDescriptor);
        desc.name = "c_kinematics";
        desc.queries = queries;
        desc.query_count = g_vel_vtable ? 2 : 1;
        desc.callback = c_kinematics_system;
        desc.user_data = NULL;

        FfiResult res = host_api->register_system(host_context, &desc);
        if (res != FFI_RESULT_OK) {
            return res;
        }
    }

    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    g_host_api = NULL;
    g_pos_vtable = NULL;
    g_vel_vtable = NULL;
    g_cmd_spawn_empty = NULL;
    g_cmd_insert_component = NULL;
    return FFI_RESULT_OK;
}
```

---

### 7.2 Modern C++17 Reference Mod
This reference mod encapsulates the C-ABI inside typed RAII wrappers, provides safe exception containment (`try ... catch (...) -> FFI_RESULT_PANIC_CAUGHT`), and handles `const void*` to function-pointer casting properly via `const_cast`.

```cpp
// ============================================================================
// reference_cpp_mod.cpp — Modern C++17 Dynamic Mod for bevy_abi_mod
// Compile: clang++ -std=c++17 -shared -fPIC -Iinclude reference_cpp_mod.cpp -o libreference_cpp_mod.so
// ============================================================================

#include "bevy_abi_mod.h"
#include <cstdint>
#include <cstddef>
#include <utility>

namespace bevy_abi {

/* VTable Mirror Types */
struct PositionVTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
};

struct VelocityVTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
};

using CmdSpawnEmptyFn = FfiEntity (*)(RawCommandQueue*);
using CmdDespawnFn = FfiResult (*)(RawCommandQueue*, FfiEntity);
using CmdInsertComponentFn = FfiResult (*)(
    RawCommandQueue*, FfiEntity, const char*, const uint8_t*, size_t
);

/* Singleton Host Service Locator */
class HostContext {
public:
    static void init(GetProcAddressFn get_proc, const HostApi* api) noexcept {
        s_api = api;
        s_pos_vtable = reinterpret_cast<const PositionVTable*>(get_proc("Position_vtable"));
        s_vel_vtable = reinterpret_cast<const VelocityVTable*>(get_proc("Velocity_vtable"));

        // Note: const_cast is required in C++ when converting GetProcAddressFn's const void* to fn ptrs
        s_cmd_spawn = reinterpret_cast<CmdSpawnEmptyFn>(const_cast<void*>(get_proc("cmd_spawn_empty")));
        s_cmd_despawn = reinterpret_cast<CmdDespawnFn>(const_cast<void*>(get_proc("cmd_despawn")));
        s_cmd_insert = reinterpret_cast<CmdInsertComponentFn>(const_cast<void*>(get_proc("cmd_insert_component")));
    }

    static void shutdown() noexcept {
        s_api = nullptr;
        s_pos_vtable = nullptr;
        s_vel_vtable = nullptr;
        s_cmd_spawn = nullptr;
        s_cmd_despawn = nullptr;
        s_cmd_insert = nullptr;
    }

    static const HostApi* api() noexcept { return s_api; }
    static const PositionVTable* pos_vtable() noexcept { return s_pos_vtable; }
    static const VelocityVTable* vel_vtable() noexcept { return s_vel_vtable; }

    static FfiEntity spawn_empty(RawCommandQueue* q) noexcept {
        return s_cmd_spawn ? s_cmd_spawn(q) : 0;
    }

    static FfiResult despawn(RawCommandQueue* q, FfiEntity e) noexcept {
        return s_cmd_despawn ? s_cmd_despawn(q, e) : FFI_RESULT_INTERNAL_ERROR;
    }

    template <typename T>
    static FfiResult insert_component(
        RawCommandQueue* q, FfiEntity e, const char* name, const T& val
    ) noexcept {
        if (!s_cmd_insert) return FFI_RESULT_INTERNAL_ERROR;
        return s_cmd_insert(q, e, name, reinterpret_cast<const uint8_t*>(&val), sizeof(T));
    }

private:
    inline static const HostApi* s_api = nullptr;
    inline static const PositionVTable* s_pos_vtable = nullptr;
    inline static const VelocityVTable* s_vel_vtable = nullptr;
    inline static CmdSpawnEmptyFn s_cmd_spawn = nullptr;
    inline static CmdDespawnFn s_cmd_despawn = nullptr;
    inline static CmdInsertComponentFn s_cmd_insert = nullptr;
};

/* Type-Safe RAII Chunk Iterator */
class ChunkIterator {
public:
    explicit ChunkIterator(RawChunkIter* raw, const HostApi* api) noexcept
        : m_raw(raw), m_api(api), m_count(0) {}

    bool next() noexcept {
        m_count = m_api->iter_next_chunk(m_raw);
        return m_count > 0;
    }

    size_t count() const noexcept { return m_count; }

    template <typename ColumnPtr = uint8_t*>
    ColumnPtr column_mut(size_t index) noexcept {
        return reinterpret_cast<ColumnPtr>(m_api->iter_column_mutable(m_raw, index));
    }

    template <typename ColumnPtr = const uint8_t*>
    ColumnPtr column_read(size_t index) const noexcept {
        return reinterpret_cast<ColumnPtr>(m_api->iter_column_readonly(m_raw, index));
    }

    size_t stride(size_t index) const noexcept {
        return m_api->iter_column_stride(m_raw, index);
    }

    const FfiEntity* entities() const noexcept {
        return m_api->iter_entities(m_raw);
    }

    void mark_changed(size_t index, size_t row) noexcept {
        m_api->iter_mark_changed(m_raw, index, row);
    }

private:
    RawChunkIter* m_raw;
    const HostApi* m_api;
    size_t m_count;
};

} // namespace bevy_abi

extern "C" {

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

/* C++ Dynamic System with Exception Containment */
static FfiResult cpp_kinematics_system(
    RawChunkIter* raw_iter,
    RawCommandQueue* cmd_queue,
    void* user_data
) {
    (void)cmd_queue;
    (void)user_data;

    // Boundary rule: Never allow C++ exceptions to unwind into the C-ABI host!
    try {
        const auto* api = bevy_abi::HostContext::api();
        const auto* pos_vt = bevy_abi::HostContext::pos_vtable();
        const auto* vel_vt = bevy_abi::HostContext::vel_vtable();

        if (!api || !pos_vt) {
            return FFI_RESULT_INTERNAL_ERROR;
        }

        bevy_abi::ChunkIterator iter(raw_iter, api);
        while (iter.next()) {
            auto* pos_col = iter.column_mut(0);
            size_t pos_stride = iter.stride(0);

            const auto* vel_col = vel_vt ? iter.column_read(1) : nullptr;
            size_t vel_stride = vel_vt ? iter.stride(1) : 0;

            for (size_t i = 0; i < iter.count(); ++i) {
                void* p = pos_col + (i * pos_stride);
                float z = pos_vt->get_z(p);

                if (vel_col && vel_vt) {
                    const void* v = vel_col + (i * vel_stride);
                    float vz = vel_vt->get_z(v);
                    pos_vt->set_z(p, z + (vz != 0.0f ? vz : 1.0f));
                } else {
                    pos_vt->set_z(p, z + 1.0f);
                }
                iter.mark_changed(0, i);
            }
        }
        return FFI_RESULT_OK;
    } catch (...) {
        return FFI_RESULT_PANIC_CAUGHT;
    }
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api || host_api->struct_size < sizeof(HostApi) ||
        host_api->abi_version != BEVY_ABI_VERSION_PACKED) {
        return FFI_RESULT_INVALID_VERSION;
    }

    bevy_abi::HostContext::init(get_proc_address, host_api);

    if (host_context && host_api->register_system) {
        static ComponentAccessDesc queries[2];
        queries[0].name = "Position";
        queries[0].mode = COMPONENT_ACCESS_WRITE;
        queries[1].name = "Velocity";
        queries[1].mode = COMPONENT_ACCESS_READ;

        SystemDescriptor desc{};
        desc.struct_size = static_cast<uint32_t>(sizeof(SystemDescriptor));
        desc.name = "cpp_kinematics";
        desc.queries = queries;
        desc.query_count = 2;
        desc.callback = cpp_kinematics_system;
        desc.user_data = nullptr;

        return host_api->register_system(host_context, &desc);
    }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    bevy_abi::HostContext::shutdown();
    return FFI_RESULT_OK;
}

} // extern "C"
```

---

### 7.3 Zig Reference Mod
This reference mod demonstrates complete Zig (0.11+ / 0.13+) compatibility using `extern struct`, `callconv(.C)`, `export fn`, and typed pointer casting.

```zig
// ============================================================================
// reference_zig_mod.zig — Idiomatic Zig Dynamic Mod for bevy_abi_mod
// Compile: zig build-lib -dynamic -OReleaseFast reference_zig_mod.zig
// ============================================================================

const std = @import("std");

// Status Codes
pub const FfiResult = u32;
pub const FFI_RESULT_OK: FfiResult = 0;
pub const FFI_RESULT_INVALID_COMPONENT: FfiResult = 1;
pub const FFI_RESULT_ACCESS_CONFLICT: FfiResult = 2;
pub const FFI_RESULT_PANIC_CAUGHT: FfiResult = 3;
pub const FFI_RESULT_NULL_POINTER: FfiResult = 4;
pub const FFI_RESULT_INVALID_UTF8: FfiResult = 5;
pub const FFI_RESULT_PERMISSION_DENIED: FfiResult = 6;
pub const FFI_RESULT_INVALID_VERSION: FfiResult = 7;
pub const FFI_RESULT_INTERNAL_ERROR: FfiResult = 99;

pub const FfiEntity = u64;

pub const ComponentAccessMode = u32;
pub const COMPONENT_ACCESS_READ: ComponentAccessMode = 0;
pub const COMPONENT_ACCESS_WRITE: ComponentAccessMode = 1;
pub const COMPONENT_ACCESS_FILTER_ONLY: ComponentAccessMode = 2;
pub const COMPONENT_ACCESS_WITHOUT: ComponentAccessMode = 3;

pub const ComponentAccessDesc = extern struct {
    name: [*:0]const u8,
    mode: ComponentAccessMode,
};

// Opaque Engine Handles
pub const RawChunkIter = opaque {};
pub const RawCommandQueue = opaque {};

// Host API Function Pointers
pub const IterNextChunkFn = *const fn (iter: *RawChunkIter) callconv(.C) usize;
pub const IterColumnReadonlyFn = *const fn (iter: *const RawChunkIter, index: usize) callconv(.C) ?*const u8;
pub const IterColumnMutableFn = *const fn (iter: *RawChunkIter, index: usize) callconv(.C) ?*u8;
pub const IterMarkChangedFn = *const fn (iter: *RawChunkIter, index: usize, row: usize) callconv(.C) FfiResult;
pub const IterEntitiesFn = *const fn (iter: *const RawChunkIter) callconv(.C) ?[*]const FfiEntity;
pub const IterColumnStrideFn = *const fn (iter: *const RawChunkIter, index: usize) callconv(.C) usize;

pub const DynamicSystemCallback = *const fn (
    iter: *RawChunkIter,
    commands: *RawCommandQueue,
    user_data: ?*anyopaque,
) callconv(.C) FfiResult;

pub const SystemDescriptor = extern struct {
    struct_size: u32,
    name: [*:0]const u8,
    queries: ?[*]const ComponentAccessDesc,
    query_count: usize,
    callback: DynamicSystemCallback,
    user_data: ?*anyopaque,
};

pub const RegisterSystemFn = *const fn (
    host_context: ?*anyopaque,
    descriptor: *const SystemDescriptor,
) callconv(.C) FfiResult;

pub const GetProcAddressFn = *const fn (name: [*:0]const u8) callconv(.C) ?*const anyopaque;

pub const HostApi = extern struct {
    struct_size: u32,
    abi_version: u32,
    register_system: RegisterSystemFn,
    iter_next_chunk: IterNextChunkFn,
    iter_column_readonly: IterColumnReadonlyFn,
    iter_column_mutable: IterColumnMutableFn,
    iter_mark_changed: IterMarkChangedFn,
    iter_entities: IterEntitiesFn,
    iter_column_stride: IterColumnStrideFn,
    register_component: ?*const anyopaque,
};

// VTable Definitions
pub const PositionVTable = extern struct {
    size: usize,
    align_val: usize,
    get_x: *const fn (ptr: *const anyopaque) callconv(.C) f32,
    set_x: *const fn (ptr: *anyopaque, val: f32) callconv(.C) void,
    get_y: *const fn (ptr: *const anyopaque) callconv(.C) f32,
    set_y: *const fn (ptr: *anyopaque, val: f32) callconv(.C) void,
    get_z: *const fn (ptr: *const anyopaque) callconv(.C) f32,
    set_z: *const fn (ptr: *anyopaque, val: f32) callconv(.C) void,
};

pub const VelocityVTable = extern struct {
    size: usize,
    align_val: usize,
    get_x: *const fn (ptr: *const anyopaque) callconv(.C) f32,
    set_x: *const fn (ptr: *anyopaque, val: f32) callconv(.C) void,
    get_y: *const fn (ptr: *const anyopaque) callconv(.C) f32,
    set_y: *const fn (ptr: *anyopaque, val: f32) callconv(.C) void,
    get_z: *const fn (ptr: *const anyopaque) callconv(.C) f32,
    set_z: *const fn (ptr: *anyopaque, val: f32) callconv(.C) void,
};

// Global Mod State
var g_host_api: ?*const HostApi = null;
var g_pos_vtable: ?*const PositionVTable = null;
var g_vel_vtable: ?*const VelocityVTable = null;

const BEVY_ABI_VERSION_PACKED: u32 = (0 << 24) | (1 << 16) | 0;

// Dynamic System Execution Callback
fn zig_kinematics_system(
    iter: *RawChunkIter,
    commands: *RawCommandQueue,
    user_data: ?*anyopaque,
) callconv(.C) FfiResult {
    _ = commands;
    _ = user_data;

    const api = g_host_api orelse return FFI_RESULT_INTERNAL_ERROR;
    const pos_vt = g_pos_vtable orelse return FFI_RESULT_INTERNAL_ERROR;

    while (true) {
        const count = api.iter_next_chunk(iter);
        if (count == 0) break;

        const pos_base = api.iter_column_mutable(iter, 0) orelse return FFI_RESULT_NULL_POINTER;
        const pos_stride = api.iter_column_stride(iter, 0);

        const vel_base = if (g_vel_vtable != null) api.iter_column_readonly(iter, 1) else null;
        const vel_stride = if (g_vel_vtable != null) api.iter_column_stride(iter, 1) else 0;

        var i: usize = 0;
        while (i < count) : (i += 1) {
            const p_ptr: *anyopaque = @ptrCast(pos_base + (i * pos_stride));
            const z = pos_vt.get_z(p_ptr);

            if (vel_base != null and g_vel_vtable != null) {
                const v_ptr: *const anyopaque = @ptrCast(vel_base.? + (i * vel_stride));
                const vz = g_vel_vtable.?.get_z(v_ptr);
                pos_vt.set_z(p_ptr, z + (if (vz != 0.0) vz else 1.0));
            } else {
                pos_vt.set_z(p_ptr, z + 1.0);
            }

            _ = api.iter_mark_changed(iter, 0, i);
        }
    }

    return FFI_RESULT_OK;
}

// Exported Dynamic Entry Points

export fn bevy_abi_mod_abi_version() callconv(.C) u32 {
    return BEVY_ABI_VERSION_PACKED;
}

var queries = [_]ComponentAccessDesc{
    .{ .name = "Position", .mode = COMPONENT_ACCESS_WRITE },
    .{ .name = "Velocity", .mode = COMPONENT_ACCESS_READ },
};

export fn bevy_abi_mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: ?*const HostApi,
    host_context: ?*anyopaque,
) callconv(.C) FfiResult {
    const api = host_api orelse return FFI_RESULT_NULL_POINTER;

    if (api.struct_size < @sizeOf(HostApi) or api.abi_version != BEVY_ABI_VERSION_PACKED) {
        return FFI_RESULT_INVALID_VERSION;
    }

    g_host_api = api;

    if (get_proc_address("Position_vtable")) |ptr| {
        g_pos_vtable = @ptrCast(@alignCast(ptr));
    }
    if (get_proc_address("Velocity_vtable")) |ptr| {
        g_vel_vtable = @ptrCast(@alignCast(ptr));
    }

    if (host_context != null and g_pos_vtable != null) {
        const desc = SystemDescriptor{
            .struct_size = @sizeOf(SystemDescriptor),
            .name = "zig_kinematics",
            .queries = &queries,
            .query_count = if (g_vel_vtable != null) 2 else 1,
            .callback = zig_kinematics_system,
            .user_data = null,
        };

        return api.register_system(host_context, &desc);
    }

    return FFI_RESULT_OK;
}

export fn bevy_abi_mod_shutdown(host_context: ?*anyopaque) callconv(.C) FfiResult {
    _ = host_context;
    g_host_api = null;
    g_pos_vtable = null;
    g_vel_vtable = null;
    return FFI_RESULT_OK;
}
```

---

## 8. Build Matrix & Cross-Platform Tooling

### 8.1 Compiler Commands & Dynamic Flags

| Platform | Shared Ext | Compiler | Recommended Build Command |
|---|---|---|---|
| **Linux** | `.so` | GCC / Clang | `clang -std=c99 -shared -fPIC -fvisibility=hidden -Iinclude mod.c -o libmod.so` |
| **macOS** | `.dylib` | Apple Clang | `clang -std=c99 -dynamiclib -fPIC -Iinclude mod.c -o libmod.dylib` |
| **Windows** | `.dll` | MSVC (`cl`) | `cl /LD /O2 /Iinclude mod.c /link /OUT:mod.dll` |
| **Cross-Platform** | Any | Zig | `zig build-lib -dynamic -OReleaseFast -Iinclude mod.c` |

### 8.2 Common Pitfalls for Non-Rust Authors
1. **Omitting `extern "C"` in C++:**
   - Forgetting `extern "C"` causes C++ name mangling (e.g. `__Z20bevy_abi_mod_initv`), preventing the host dynamic loader from finding the initialization symbol.
2. **Casting Away `const` in C++:**
   - In standard C++, converting `const void*` from `GetProcAddressFn` to a function pointer produces a compiler error (`casts away qualifiers`). Mod authors must use `reinterpret_cast<FnPtr>(const_cast<void*>(get_proc(...)))`.
3. **Leaking Exceptions Across the C ABI:**
   - If a C++ exception unwinds past `bevy_abi_mod_init` or `DynamicSystemCallback`, the C runtime terminates the entire host process with `SIGABRT`. Always wrap callbacks in `try { ... } catch (...) { return FFI_RESULT_PANIC_CAUGHT; }`.
4. **Caching Column Pointers Across Chunks:**
   - Pointers returned by `iter_column_mutable` or `iter_column_readonly` are valid **only for the current chunk**. Caching these pointers into global or heap variables across `iter_next_chunk` calls causes silent memory corruption.
5. **Modifying Struct Field Order:**
   - Foreign code must never alter the order of struct members or assume 32-bit alignment for pointers. Always compile with `#include "bevy_abi_mod.h"`.
