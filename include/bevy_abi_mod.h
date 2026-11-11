#ifndef BEVY_ABI_MOD_H
#define BEVY_ABI_MOD_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

#define BEVY_ABI_VERSION_MAJOR 0u
#define BEVY_ABI_VERSION_MINOR 1u
#define BEVY_ABI_VERSION_PATCH 0u
#define BEVY_ABI_VERSION_PACKED \
    ((BEVY_ABI_VERSION_MAJOR << 24) | (BEVY_ABI_VERSION_MINOR << 16) | BEVY_ABI_VERSION_PATCH)

#if defined(_WIN32)
    #define BEVY_ABI_MOD_EXPORT __declspec(dllexport)
#else
    #define BEVY_ABI_MOD_EXPORT __attribute__((visibility("default")))
#endif

#if defined(__GNUC__) || defined(__clang__)
    #define BEVY_ABI_UNUSED __attribute__((unused))
#else
    #define BEVY_ABI_UNUSED
#endif

#if defined(__cplusplus)
    #if __cplusplus >= 201103L
        #define BEVY_ABI_ALIGNOF(type) alignof(type)
    #else
        #define BEVY_ABI_ALIGNOF(type) __alignof__(type)
    #endif
#elif defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 201112L)
    #define BEVY_ABI_ALIGNOF(type) _Alignof(type)
#elif defined(__GNUC__) || defined(__clang__)
    #define BEVY_ABI_ALIGNOF(type) __alignof__(type)
#else
    #define BEVY_ABI_ALIGNOF(type) sizeof(type)
#endif

#if defined(__cplusplus)
    #if __cplusplus >= 201103L
        #define BEVY_ABI_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
    #else
        #define BEVY_ABI_STATIC_ASSERT_INNER(a, b) a##b
        #define BEVY_ABI_STATIC_ASSERT_CONCAT(a, b) BEVY_ABI_STATIC_ASSERT_INNER(a, b)
        #define BEVY_ABI_STATIC_ASSERT(cond, msg) \
            typedef char BEVY_ABI_STATIC_ASSERT_CONCAT(bevy_abi_static_assert_, __LINE__)[(cond) ? 1 : -1]
    #endif
#elif defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 201112L)
    #define BEVY_ABI_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#elif defined(__GNUC__) || defined(__clang__)
    #define BEVY_ABI_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#else
    #define BEVY_ABI_STATIC_ASSERT_INNER(a, b) a##b
    #define BEVY_ABI_STATIC_ASSERT_CONCAT(a, b) BEVY_ABI_STATIC_ASSERT_INNER(a, b)
    #define BEVY_ABI_STATIC_ASSERT(cond, msg) \
        typedef char BEVY_ABI_STATIC_ASSERT_CONCAT(bevy_abi_static_assert_, __LINE__)[(cond) ? 1 : -1]
#endif

/* Status codes (matches `abi_contract::FfiResult`) */
typedef uint32_t FfiResult;
#define FFI_RESULT_OK                 0u
#define FFI_RESULT_INVALID_COMPONENT  1u
#define FFI_RESULT_ACCESS_CONFLICT    2u
#define FFI_RESULT_PANIC_CAUGHT       3u
#define FFI_RESULT_NULL_POINTER       4u
#define FFI_RESULT_INVALID_UTF8       5u
#define FFI_RESULT_PERMISSION_DENIED  6u
#define FFI_RESULT_INVALID_VERSION    7u
#define FFI_RESULT_INTERNAL_ERROR     99u

/* Opaque 64-bit entity identifier (matches `abi_contract::FfiEntity`) */
typedef uint64_t FfiEntity;

/* Non-owning byte/UTF-8 slice (`abi_contract::FfiSlice<u8>` / `StrView`) */
typedef struct FfiByteSlice {
    const uint8_t* ptr;
    size_t len;
} FfiByteSlice;

typedef FfiByteSlice StrView;

/* 128-bit deterministic component UUID (`abi_contract::ComponentUuid`) */
typedef struct ComponentUuid {
    uint8_t bytes[16];
} ComponentUuid;

/**
 * Compute deterministic 128-bit UUID from component name string using 2-lane 64-bit FNV-1a.
 * Matches `abi_contract::ComponentUuid::from_name` byte-for-byte across all platforms.
 */
static inline BEVY_ABI_UNUSED ComponentUuid bevy_abi_uuid_from_name(const char* name) {
    ComponentUuid id = {{0}};
    if (!name) {
        return id;
    }
    uint64_t h1 = 0xcbf29ce484222325ULL;
    uint64_t h2 = 0x84222325cbf29ce4ULL;
    size_t i = 0;
    while (name[i] != '\0') {
        uint64_t b = (uint8_t)name[i];
        h1 ^= b;
        h1 *= 0x100000001b3ULL;
        h2 ^= b ^ (i * 0x9e3779b97f4a7c15ULL);
        h2 *= 0x100000001b3ULL;
        i++;
    }
    for (int j = 0; j < 8; ++j) {
        id.bytes[j] = (uint8_t)((h1 >> (j * 8)) & 0xff);
        id.bytes[8 + j] = (uint8_t)((h2 >> (j * 8)) & 0xff);
    }
    return id;
}

/* Component access mode (`abi_contract::ComponentAccessMode`) */
typedef uint32_t ComponentAccessMode;
#define COMPONENT_ACCESS_READ        0u
#define COMPONENT_ACCESS_WRITE       1u
#define COMPONENT_ACCESS_FILTER_ONLY 2u
#define COMPONENT_ACCESS_WITHOUT     3u

/* Component access descriptor (`abi_contract::ComponentAccessDesc`) */
typedef struct ComponentAccessDesc {
    const char* name;
    ComponentAccessMode mode;
} ComponentAccessDesc;

/* Opaque handles for chunk iteration and deferred commands */
typedef struct RawChunkIter RawChunkIter;
typedef struct RawCommandQueue RawCommandQueue;

/* Host chunk iteration function pointer types */
typedef size_t (*IterNextChunkFn)(RawChunkIter* iter);
typedef const uint8_t* (*IterColumnReadonlyFn)(const RawChunkIter* iter, size_t index);
typedef uint8_t* (*IterColumnMutableFn)(RawChunkIter* iter, size_t index);
typedef FfiResult (*IterMarkChangedFn)(RawChunkIter* iter, size_t index, size_t row);
typedef const FfiEntity* (*IterEntitiesFn)(const RawChunkIter* iter);
typedef size_t (*IterColumnStrideFn)(const RawChunkIter* iter, size_t index);
typedef FfiResult (*IterResetFn)(RawChunkIter* iter);

/* Dynamic system callback (`abi_contract::DynamicSystemCallback`) */
typedef FfiResult (*DynamicSystemCallback)(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
);

/* Multi-query clause and descriptor (`abi_contract::QuerySpec` / `MultiQuerySystemDescriptor`) */
typedef struct QuerySpec {
    const ComponentAccessDesc* queries;
    size_t query_count;
} QuerySpec;

typedef FfiResult (*MultiQuerySystemCallback)(
    RawChunkIter* const* iters,
    size_t iter_count,
    RawCommandQueue* commands,
    void* user_data
);

typedef struct MultiQuerySystemDescriptor {
    uint32_t struct_size;
    const char* name;
    const QuerySpec* query_specs;
    size_t query_spec_count;
    MultiQuerySystemCallback callback;
    void* user_data;
} MultiQuerySystemDescriptor;

typedef FfiResult (*RegisterMultiQuerySystemFn)(
    void* host_context,
    const MultiQuerySystemDescriptor* descriptor
);

/* System registration descriptor (`abi_contract::SystemDescriptor`) */
typedef struct SystemDescriptor {
    uint32_t struct_size;
    const char* name;
    const ComponentAccessDesc* queries;
    size_t query_count;
    DynamicSystemCallback callback;
    void* user_data;
} SystemDescriptor;

/* System registration function signature (`abi_contract::RegisterSystemFn`) */
typedef FfiResult (*RegisterSystemFn)(
    void* host_context,
    const SystemDescriptor* descriptor
);

/* Symbol resolver function pointer (`abi_contract::GetProcAddressFn`) */
typedef const void* (*GetProcAddressFn)(const char* name);

/* Component lifecycle drop callback (`abi_contract::ComponentDropFn`) */
typedef void (*ComponentDropFn)(void* ptr);

/* Dynamic component registration descriptor (`abi_contract::ModComponentDescriptor`) */
typedef struct ModComponentDescriptor {
    uint32_t struct_size;
    const char* name;
    ComponentUuid uuid;
    size_t size;
    size_t align;
    ComponentDropFn drop_fn;
    const void* vtable_ptr;
} ModComponentDescriptor;

/* Ergonomic type alias */
typedef ModComponentDescriptor DynamicComponentDescriptor;

/* Dynamic component registration function signature (`abi_contract::RegisterComponentFn`) */
typedef FfiResult (*RegisterComponentFn)(
    void* host_context,
    const ModComponentDescriptor* descriptor
);

/* Host API VTable (`abi_contract::HostApi`) */
typedef struct HostApi {
    uint32_t struct_size;
    uint32_t abi_version;
    RegisterSystemFn register_system;
    IterNextChunkFn iter_next_chunk;
    IterColumnReadonlyFn iter_column_readonly;
    IterColumnMutableFn iter_column_mutable;
    IterMarkChangedFn iter_mark_changed;
    IterEntitiesFn iter_entities;
    IterColumnStrideFn iter_column_stride;
    RegisterComponentFn register_component;
} HostApi;

/* Static ABI layout assertions for 64-bit architectures */
#if defined(__LP64__) || defined(_WIN64) || (defined(__SIZEOF_POINTER__) && __SIZEOF_POINTER__ == 8)
BEVY_ABI_STATIC_ASSERT(sizeof(ModComponentDescriptor) == 64, "ModComponentDescriptor 64-bit size must be 64");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(ModComponentDescriptor) == 8, "ModComponentDescriptor 64-bit align must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, struct_size) == 0, "offset of struct_size must be 0");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, name) == 8, "offset of name must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, uuid) == 16, "offset of uuid must be 16");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, size) == 32, "offset of size must be 32");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, align) == 40, "offset of align must be 40");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, drop_fn) == 48, "offset of drop_fn must be 48");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, vtable_ptr) == 56, "offset of vtable_ptr must be 56");

BEVY_ABI_STATIC_ASSERT(sizeof(HostApi) == 72, "HostApi 64-bit size must be 72");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(HostApi) == 8, "HostApi 64-bit align must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, struct_size) == 0, "offset of struct_size must be 0");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, abi_version) == 4, "offset of abi_version must be 4");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, register_system) == 8, "offset of register_system must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_next_chunk) == 16, "offset of iter_next_chunk must be 16");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_column_readonly) == 24, "offset of iter_column_readonly must be 24");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_column_mutable) == 32, "offset of iter_column_mutable must be 32");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_mark_changed) == 40, "offset of iter_mark_changed must be 40");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_entities) == 48, "offset of iter_entities must be 48");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_column_stride) == 56, "offset of iter_column_stride must be 56");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, register_component) == 64, "offset of register_component must be 64");

/* Static ABI layout assertions for 32-bit architectures */
#elif defined(__ILP32__) || (defined(__SIZEOF_POINTER__) && __SIZEOF_POINTER__ == 4)
BEVY_ABI_STATIC_ASSERT(sizeof(ModComponentDescriptor) == 40, "ModComponentDescriptor 32-bit size must be 40");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(ModComponentDescriptor) == 4, "ModComponentDescriptor 32-bit align must be 4");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, struct_size) == 0, "offset of struct_size must be 0");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, name) == 4, "offset of name must be 4");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, uuid) == 8, "offset of uuid must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, size) == 24, "offset of size must be 24");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, align) == 28, "offset of align must be 28");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, drop_fn) == 32, "offset of drop_fn must be 32");
BEVY_ABI_STATIC_ASSERT(offsetof(ModComponentDescriptor, vtable_ptr) == 36, "offset of vtable_ptr must be 36");

BEVY_ABI_STATIC_ASSERT(sizeof(HostApi) == 40, "HostApi 32-bit size must be 40");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(HostApi) == 4, "HostApi 32-bit align must be 4");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, struct_size) == 0, "offset of struct_size must be 0");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, abi_version) == 4, "offset of abi_version must be 4");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, register_system) == 8, "offset of register_system must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_next_chunk) == 12, "offset of iter_next_chunk must be 12");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_column_readonly) == 16, "offset of iter_column_readonly must be 16");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_column_mutable) == 20, "offset of iter_column_mutable must be 20");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_mark_changed) == 24, "offset of iter_mark_changed must be 24");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_entities) == 28, "offset of iter_entities must be 28");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, iter_column_stride) == 32, "offset of iter_column_stride must be 32");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, register_component) == 36, "offset of register_component must be 36");
#endif

/* Mod lifecycle entry points */
typedef uint32_t (*ModAbiVersionFn)(void);
typedef FfiResult (*ModInitFn)(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
);
typedef FfiResult (*ModShutdownFn)(void* host_context);

#ifdef __cplusplus
}
#endif

#endif /* BEVY_ABI_MOD_H */
