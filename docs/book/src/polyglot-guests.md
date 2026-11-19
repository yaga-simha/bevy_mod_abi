# ISO C99, C++17 & Zig Guest Guide

All non-Rust languages target the single auto-generated header [`include/bevy_abi_mod.h`](../../../include/bevy_abi_mod.h) emitted by `bevy_abi_mod_sdk_gen` (`cargo bam export-sdk --out include` or `cargo xtask header`), which also produces `bevy_abi_mod.hpp` (C++17), `bevy_abi_mod.zig` (Zig), and platform `BAM_EXPORT` macros (`__declspec(dllexport)` on Windows `_WIN32`, visibility attributes on ELF/Mach-O). Every wire struct is verified at compile time via `BAM_STATIC_ASSERT` (`sizeof`, `alignof`, `offsetof`) on both 64-bit and 32-bit targets.

---

## 1. Mandatory Entry Points & Wire Rules

Every shared library must export three `extern "C"` symbols (`BAM_EXPORT`):
1. `const BamManifest* bevy_abi_mod_manifest(void)`
2. `BamResult bevy_abi_mod_init(const BamHostApi* api, BamLoadCtx* load_ctx, BamInstance** instance_out)`
3. `void bevy_abi_mod_shutdown(BamInstance* instance)`

**Core Rules Across All Foreign Languages:**
1. **Copy `BamHostApi` Boundedly:** Call `bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api)` in `init` so newer hosts with larger `struct_size` tables work seamlessly.
2. **Zero Process Globals:** Store `local_api` and resolved `BamComponent` handles in a heap-allocated struct returned via `*instance_out` and freed in `bevy_abi_mod_shutdown(instance)`.
3. **Never Unwind Across `extern "C"`:** Every callback is `nounwind`. Catch C++ exceptions or map Zig error unions to `BamResult` before returning.
4. **Never Cache Scoped Handles:** `BamLoadCtx*`, `BamSystemCtx*`, `BamIter*`, and `BamCommands*` are valid strictly for the duration of the enclosing callback; debug builds and `strict-ffi-checks` poison their headers on return.

---

## 2. ISO C99 Guest Pattern (`examples/03_cross_language_interop`)

Compile with `-std=c99 -Wall -Wextra -Wpedantic -Werror -Iinclude -shared -fPIC`:

```c
#include "bevy_abi_mod.h"
#include <stdlib.h>

typedef struct CModState {
    BamHostApi   api;
    BamComponent pos_comp;
} CModState;

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION, // ABI 0.3.0 (0x00030000)
    .name        = "my_c99_mod",
    .version     = "0.2.0",
    .flags       = 0u,
    .reserved0   = 0u
};

static BamResult step_positions(BamSystemCtx* sys_ctx, void* user_data) {
    CModState* st = (CModState*)user_data;
    BamIter* iter = NULL;
    size_t len = 0u;

    BamResult res = st->api.sys_query(sys_ctx, 0u, &iter);
    if (res != BAM_RESULT_OK) return res;

    while (st->api.iter_next_chunk(iter, &len) != BAM_FALSE) {
        float* xyz = (float*)st->api.iter_column_mutable(iter, 0u);
        if (xyz == NULL) return BAM_RESULT_NULL_POINTER;
        for (size_t i = 0u; i < len * 3u; ++i) {
            xyz[i] += 1.0f;
        }
    }
    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) { return &MANIFEST; }

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    CModState* st = (CModState*)calloc(1u, sizeof(CModState));
    if (st == NULL) return BAM_RESULT_INTERNAL_ERROR;

    BamResult res = bam_host_api_copy_bounded(&st->api, sizeof(st->api), host_api);
    if (res != BAM_RESULT_OK) { free(st); return res; }

    const BamHostCtx* hctx = st->api.load_host_ctx(load_ctx);
    res = st->api.resolve_component(hctx, "example::Position", NULL, &st->pos_comp);
    if (res != BAM_RESULT_OK) { free(st); return res; }

    BamTermDesc term = bam_term_desc(st->pos_comp, BAM_ACCESS_WRITE, 0u);
    BamQueryDesc q = { .struct_size = sizeof(BamQueryDesc), .terms = &term, .term_count = 1u };
    BamSystemDesc s = {
        .struct_size = sizeof(BamSystemDesc),
        .name        = "c99_step",
        .queries     = &q,
        .query_count = 1u,
        .callback    = step_positions,
        .user_data   = st,
        .schedule    = BAM_SCHEDULE_UPDATE
    };
    res = st->api.register_system(load_ctx, &s);
    if (res != BAM_RESULT_OK) { free(st); return res; }

    *instance_out = (BamInstance*)st;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    free(instance);
}
```

---

## 3. ISO C++17 Guest Pattern (`examples/07_cpp17_guest`)

In C++17 (`c++ -std=c++17 -Wall -Wextra -Wpedantic -Werror`), `BAM_EXPORT` expands to `extern "C"` with visibility attributes. Wrap `init`, `shutdown`, and every `noexcept` callback trampoline in `try { ... } catch (...)` and manage per-`World` state with `std::unique_ptr`:

```cpp
static BamResult kinematics_trampoline(BamSystemCtx* sys_ctx, void* user_data) noexcept {
    try {
        return static_cast<CppModInstance*>(user_data)->run(sys_ctx);
    } catch (...) {
        // Caught on the C++ guest runtime; host receives PANIC_CAUGHT and quarantines cleanly!
        return BAM_RESULT_PANIC_CAUGHT;
    }
}
```

See [`examples/07_cpp17_guest/cpp_mod/mod.cpp`](../../../examples/07_cpp17_guest/cpp_mod/mod.cpp) for the full working C++17 implementation and multi-World RAII test.

---

## 4. Zig Guest Pattern (`examples/08_zig_guest`)

Zig (`0.14`–`0.16`, `0.16.0` recommended) imports `include/bevy_abi_mod.h` directly via `@cImport` with zero manual bindings. Use `errdefer` with `std.heap.c_allocator` in `init` and map Zig error unions to `c.BamResult` inside `callconv(.c)` callbacks:

```zig
const c = @cImport({ @cInclude("bevy_abi_mod.h"); });

fn zigSystemCallback(sys_ctx_opt: ?*c.BamSystemCtx, ud: ?*anyopaque) callconv(.c) c.BamResult {
    const sys_ctx = sys_ctx_opt orelse return c.BAM_RESULT_NULL_POINTER;
    const self: *ZigModInstance = @ptrCast(@alignCast(ud orelse return c.BAM_RESULT_NULL_POINTER));
    self.runStep(sys_ctx) catch |err| return mapErrorToBamResult(err);
    return c.BAM_RESULT_OK;
}
```

See [`examples/08_zig_guest/zig_mod/src/root.zig`](../../../examples/08_zig_guest/zig_mod/src/root.zig) for the complete `build.zig` + `root.zig` implementation.
