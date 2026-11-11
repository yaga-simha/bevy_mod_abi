#ifndef BEVY_ABI_MOD_H
#define BEVY_ABI_MOD_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define BEVY_ABI_VERSION_MAJOR 0u
#define BEVY_ABI_VERSION_MINOR 1u
#define BEVY_ABI_VERSION_PATCH 0u
#define BEVY_ABI_VERSION_PACKED \
    ((BEVY_ABI_VERSION_MAJOR << 24) | (BEVY_ABI_VERSION_MINOR << 16) | BEVY_ABI_VERSION_PATCH)

/* FFI-safe boolean byte (`abi_contract::FfiBool`).
 * Rust's `bool` makes byte values 2..=255 undefined behavior across `extern "C"`,
 * so all C-ABI boolean fields and accessors use `uint8_t` (0 = false, non-zero = true). */
typedef uint8_t FfiBool;
#define FFI_BOOL_FALSE ((FfiBool)0u)
#define FFI_BOOL_TRUE  ((FfiBool)1u)

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

/* Packed SemVer helpers (`abi_contract::pack_abi_version`, `abi_version_is_compatible`) */
static inline BEVY_ABI_UNUSED uint32_t bevy_abi_pack_version(uint32_t major, uint32_t minor, uint32_t patch) {
    return ((major & 0xFFu) << 24) | ((minor & 0xFFu) << 16) | (patch & 0xFFFFu);
}

static inline BEVY_ABI_UNUSED uint32_t bevy_abi_version_major(uint32_t packed) {
    return (packed >> 24) & 0xFFu;
}

static inline BEVY_ABI_UNUSED uint32_t bevy_abi_version_minor(uint32_t packed) {
    return (packed >> 16) & 0xFFu;
}

static inline BEVY_ABI_UNUSED uint32_t bevy_abi_version_patch(uint32_t packed) {
    return packed & 0xFFFFu;
}

static inline BEVY_ABI_UNUSED FfiBool bevy_abi_version_is_compatible(uint32_t host_packed, uint32_t guest_packed) {
    return (bevy_abi_version_major(host_packed) == bevy_abi_version_major(guest_packed) &&
            bevy_abi_version_minor(guest_packed) <= bevy_abi_version_minor(host_packed))
        ? FFI_BOOL_TRUE
        : FFI_BOOL_FALSE;
}

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

/* Opaque 64-bit entity identifier (`abi_contract::FfiEntity`).
 * In Bevy 0.20+, `0` (`BAM_ENTITY_NULL`) is guaranteed never to be a valid `Entity`
 * (`Entity::try_from_bits(0) == None`), whereas `UINT64_MAX` is a valid entity bit pattern. */
typedef uint64_t FfiEntity;
#define BAM_ENTITY_NULL ((FfiEntity)0ULL)
#define FFI_ENTITY_NULL BAM_ENTITY_NULL

static inline BEVY_ABI_UNUSED FfiBool bam_entity_is_null(FfiEntity entity) {
    return entity == BAM_ENTITY_NULL ? FFI_BOOL_TRUE : FFI_BOOL_FALSE;
}

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
#define COMPONENT_ACCESS_READ           0u
#define COMPONENT_ACCESS_WRITE          1u
#define COMPONENT_ACCESS_FILTER_ONLY    2u
#define COMPONENT_ACCESS_WITHOUT        3u
#define COMPONENT_ACCESS_OPTIONAL_READ  4u
#define COMPONENT_ACCESS_OPTIONAL_WRITE 5u
#define COMPONENT_ACCESS_CHANGED        6u
#define COMPONENT_ACCESS_ADDED          7u
#define COMPONENT_ACCESS_RESOURCE_READ  8u
#define COMPONENT_ACCESS_RESOURCE_WRITE 9u

/* Component access descriptor (`abi_contract::ComponentAccessDesc`) */
typedef struct ComponentAccessDesc {
    const char* name;
    ComponentAccessMode mode;
} ComponentAccessDesc;

static inline BEVY_ABI_UNUSED ComponentAccessDesc bam_access_desc(
    const char* name,
    ComponentAccessMode mode
) {
    ComponentAccessDesc d;
    d.name = name;
    d.mode = mode;
    return d;
}

/* Opaque handles for chunk iteration and deferred commands */
typedef struct RawChunkIter RawChunkIter;
typedef struct RawCommandQueue RawCommandQueue;

/* Host chunk iteration & resource accessor function pointer types */
typedef size_t (*IterNextChunkFn)(RawChunkIter* iter);
typedef const uint8_t* (*IterColumnReadonlyFn)(const RawChunkIter* iter, size_t index);
typedef uint8_t* (*IterColumnMutableFn)(RawChunkIter* iter, size_t index);
typedef FfiResult (*IterMarkChangedFn)(RawChunkIter* iter, size_t index, size_t row);
typedef const FfiEntity* (*IterEntitiesFn)(const RawChunkIter* iter);
typedef size_t (*IterColumnStrideFn)(const RawChunkIter* iter, size_t index);
typedef FfiResult (*IterResetFn)(RawChunkIter* iter);
typedef const uint8_t* (*IterResourceReadonlyFn)(const RawChunkIter* iter, const char* resource_name);
typedef uint8_t* (*IterResourceMutableFn)(RawChunkIter* iter, const char* resource_name);

/* Deferred command queue bridge function pointer types (resolvable via `GetProcAddressFn`) */
typedef FfiEntity (*CmdSpawnEmptyFn)(RawCommandQueue* queue);
typedef FfiResult (*CmdDespawnFn)(RawCommandQueue* queue, FfiEntity entity);
typedef FfiResult (*CmdInsertComponentFn)(
    RawCommandQueue* queue,
    FfiEntity entity,
    const char* component_name,
    const uint8_t* data_ptr,
    size_t data_size
);
typedef FfiResult (*CmdRemoveComponentFn)(
    RawCommandQueue* queue,
    FfiEntity entity,
    const char* component_name
);
typedef FfiResult (*CmdInsertDefaultFn)(
    RawCommandQueue* queue,
    FfiEntity entity,
    const char* component_name
);

/* Optional guest run-condition predicate (`abi_contract::RunConditionCallback`) */
typedef FfiBool (*RunConditionCallback)(void* user_data);

/* Dynamic system callback (`abi_contract::DynamicSystemCallback`) */
typedef FfiResult (*DynamicSystemCallback)(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
);

/* Bevy schedule identifier (`abi_contract::ScheduleLabelId`) */
typedef uint32_t ScheduleLabelId;
#define SCHEDULE_LABEL_UPDATE       0u
#define SCHEDULE_LABEL_STARTUP      1u
#define SCHEDULE_LABEL_PRE_UPDATE   2u
#define SCHEDULE_LABEL_POST_UPDATE  3u
#define SCHEDULE_LABEL_FIXED_UPDATE 4u
#define SCHEDULE_LABEL_LAST         5u

#define BAM_SCHEDULE_UPDATE         SCHEDULE_LABEL_UPDATE
#define BAM_SCHEDULE_STARTUP        SCHEDULE_LABEL_STARTUP
#define BAM_SCHEDULE_PRE_UPDATE     SCHEDULE_LABEL_PRE_UPDATE
#define BAM_SCHEDULE_POST_UPDATE    SCHEDULE_LABEL_POST_UPDATE
#define BAM_SCHEDULE_FIXED_UPDATE   SCHEDULE_LABEL_FIXED_UPDATE
#define BAM_SCHEDULE_LAST           SCHEDULE_LABEL_LAST

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

/* Extended multi-query descriptor (`abi_contract::ScheduledMultiQuerySystemDescriptor`) */
typedef struct ScheduledMultiQuerySystemDescriptor {
    MultiQuerySystemDescriptor base;
    ScheduleLabelId schedule;
    uint32_t flags;
    const char* const* after;
    size_t after_count;
    const char* const* before;
    size_t before_count;
} ScheduledMultiQuerySystemDescriptor;

typedef ScheduledMultiQuerySystemDescriptor MultiQuerySystemDescriptorV2;

/* Extended v3 multi-query descriptor adding `run_if` (`abi_contract::ConditionalMultiQuerySystemDescriptor`) */
typedef struct ConditionalMultiQuerySystemDescriptor {
    ScheduledMultiQuerySystemDescriptor sched;
    RunConditionCallback run_if;
    void* run_if_user_data;
} ConditionalMultiQuerySystemDescriptor;

typedef ConditionalMultiQuerySystemDescriptor MultiQuerySystemDescriptorV3;

typedef FfiResult (*RegisterMultiQuerySystemFn)(
    void* host_context,
    const MultiQuerySystemDescriptor* descriptor
);

typedef FfiResult (*RegisterConditionalMultiQuerySystemFn)(
    void* host_context,
    const ConditionalMultiQuerySystemDescriptor* descriptor
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

/* Extended system descriptor (`abi_contract::ScheduledSystemDescriptor`) */
typedef struct ScheduledSystemDescriptor {
    SystemDescriptor base;
    ScheduleLabelId schedule;
    uint32_t flags;
    const char* const* after;
    size_t after_count;
    const char* const* before;
    size_t before_count;
} ScheduledSystemDescriptor;

typedef ScheduledSystemDescriptor SystemDescriptorV2;

/* Extended v3 system descriptor adding `run_if` (`abi_contract::ConditionalSystemDescriptor`) */
typedef struct ConditionalSystemDescriptor {
    ScheduledSystemDescriptor sched;
    RunConditionCallback run_if;
    void* run_if_user_data;
} ConditionalSystemDescriptor;

typedef ConditionalSystemDescriptor SystemDescriptorV3;

/* System registration function signatures (`abi_contract::RegisterSystemFn` / `RegisterScheduledSystemFn` / `RegisterConditionalSystemFn`) */
typedef FfiResult (*RegisterSystemFn)(
    void* host_context,
    const SystemDescriptor* descriptor
);

typedef FfiResult (*RegisterScheduledSystemFn)(
    void* host_context,
    const ScheduledSystemDescriptor* descriptor
);

typedef FfiResult (*RegisterConditionalSystemFn)(
    void* host_context,
    const ConditionalSystemDescriptor* descriptor
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

/* Safely reads the 4-byte `struct_size` header at offset 0 of a potentially older/smaller
 * `HostApi` buffer without dereferencing `sizeof(HostApi)` bytes (preventing C99 strict-aliasing
 * and UBSan `-fsanitize=object-size` violations on legacy buffers). */
static inline BEVY_ABI_UNUSED uint32_t bam_host_api_raw_struct_size(const HostApi* api) {
    if (!api) {
        return 0u;
    }
    return *(const uint32_t*)(const void*)api;
}

/* Copies at most `min(src->struct_size, sizeof(HostApi))` bytes from `src` into `dst` and
 * zero-fills any trailing fields not provided by an older host. */
static inline BEVY_ABI_UNUSED void bam_host_api_copy_bounded(const HostApi* src, HostApi* dst) {
    unsigned char* dst_bytes;
    const unsigned char* src_bytes;
    size_t raw_size;
    size_t copy_len;
    size_t i;
    if (!dst) {
        return;
    }
    dst_bytes = (unsigned char*)(void*)dst;
    for (i = 0; i < sizeof(HostApi); ++i) {
        dst_bytes[i] = 0u;
    }
    if (!src) {
        return;
    }
    raw_size = (size_t)bam_host_api_raw_struct_size(src);
    copy_len = raw_size < sizeof(HostApi) ? raw_size : sizeof(HostApi);
    src_bytes = (const unsigned char*)(const void*)src;
    for (i = 0; i < copy_len; ++i) {
        dst_bytes[i] = src_bytes[i];
    }
}

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

BEVY_ABI_STATIC_ASSERT(sizeof(SystemDescriptor) == 48, "SystemDescriptor 64-bit size must be 48");
BEVY_ABI_STATIC_ASSERT(sizeof(ScheduledSystemDescriptor) == 88, "ScheduledSystemDescriptor 64-bit size must be 88");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(ScheduledSystemDescriptor) == 8, "ScheduledSystemDescriptor 64-bit align must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, base) == 0, "offset of base must be 0");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, schedule) == 48, "offset of schedule must be 48");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, flags) == 52, "offset of flags must be 52");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, after) == 56, "offset of after must be 56");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, after_count) == 64, "offset of after_count must be 64");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, before) == 72, "offset of before must be 72");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, before_count) == 80, "offset of before_count must be 80");
BEVY_ABI_STATIC_ASSERT(sizeof(ScheduledMultiQuerySystemDescriptor) == 88, "ScheduledMultiQuerySystemDescriptor 64-bit size must be 88");
BEVY_ABI_STATIC_ASSERT(sizeof(ConditionalSystemDescriptor) == 104, "ConditionalSystemDescriptor 64-bit size must be 104");
BEVY_ABI_STATIC_ASSERT(offsetof(ConditionalSystemDescriptor, sched) == 0, "offset of sched must be 0");
BEVY_ABI_STATIC_ASSERT(offsetof(ConditionalSystemDescriptor, run_if) == 88, "offset of run_if must be 88");
BEVY_ABI_STATIC_ASSERT(offsetof(ConditionalSystemDescriptor, run_if_user_data) == 96, "offset of run_if_user_data must be 96");
BEVY_ABI_STATIC_ASSERT(sizeof(ConditionalMultiQuerySystemDescriptor) == 104, "ConditionalMultiQuerySystemDescriptor 64-bit size must be 104");

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

BEVY_ABI_STATIC_ASSERT(sizeof(SystemDescriptor) == 24, "SystemDescriptor 32-bit size must be 24");
BEVY_ABI_STATIC_ASSERT(sizeof(ScheduledSystemDescriptor) == 48, "ScheduledSystemDescriptor 32-bit size must be 48");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(ScheduledSystemDescriptor) == 4, "ScheduledSystemDescriptor 32-bit align must be 4");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, base) == 0, "offset of base must be 0");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, schedule) == 24, "offset of schedule must be 24");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, flags) == 28, "offset of flags must be 28");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, after) == 32, "offset of after must be 32");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, after_count) == 36, "offset of after_count must be 36");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, before) == 40, "offset of before must be 40");
BEVY_ABI_STATIC_ASSERT(offsetof(ScheduledSystemDescriptor, before_count) == 44, "offset of before_count must be 44");
BEVY_ABI_STATIC_ASSERT(sizeof(ScheduledMultiQuerySystemDescriptor) == 48, "ScheduledMultiQuerySystemDescriptor 32-bit size must be 48");
BEVY_ABI_STATIC_ASSERT(sizeof(ConditionalSystemDescriptor) == 56, "ConditionalSystemDescriptor 32-bit size must be 56");
BEVY_ABI_STATIC_ASSERT(offsetof(ConditionalSystemDescriptor, run_if) == 48, "offset of run_if must be 48");
BEVY_ABI_STATIC_ASSERT(offsetof(ConditionalSystemDescriptor, run_if_user_data) == 52, "offset of run_if_user_data must be 52");
BEVY_ABI_STATIC_ASSERT(sizeof(ConditionalMultiQuerySystemDescriptor) == 56, "ConditionalMultiQuerySystemDescriptor 32-bit size must be 56");
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
