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

/* Component access mode (`abi_contract::ComponentAccessMode`) */
typedef uint32_t ComponentAccessMode;
#define COMPONENT_ACCESS_READ        0u
#define COMPONENT_ACCESS_WRITE       1u
#define COMPONENT_ACCESS_FILTER_ONLY 2u

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

/* Dynamic system callback (`abi_contract::DynamicSystemCallback`) */
typedef FfiResult (*DynamicSystemCallback)(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
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
} HostApi;

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
