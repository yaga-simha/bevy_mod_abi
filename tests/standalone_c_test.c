#include "../include/bevy_abi_mod.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>

/* ============================================================================
 * 1. Legacy HostApi Definition (64-byte HostApi prior to register_component)
 * ============================================================================ */
typedef struct LegacyHostApi {
    uint32_t struct_size;
    uint32_t abi_version;
    RegisterSystemFn register_system;
    IterNextChunkFn iter_next_chunk;
    IterColumnReadonlyFn iter_column_readonly;
    IterColumnMutableFn iter_column_mutable;
    IterMarkChangedFn iter_mark_changed;
    IterEntitiesFn iter_entities;
    IterColumnStrideFn iter_column_stride;
} LegacyHostApi;

/* Static assertions on LegacyHostApi */
BEVY_ABI_STATIC_ASSERT(sizeof(LegacyHostApi) == 64, "LegacyHostApi size must be 64");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(LegacyHostApi) == 8, "LegacyHostApi align must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(LegacyHostApi, iter_column_stride) == 56, "LegacyHostApi offset of iter_column_stride must be 56");

/* ============================================================================
 * 2. Static Assertions on Current 64-bit ABI Structures
 * ============================================================================ */
BEVY_ABI_STATIC_ASSERT(sizeof(ModComponentDescriptor) == 64, "ModComponentDescriptor size must be 64");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(ModComponentDescriptor) == 8, "ModComponentDescriptor align must be 8");
BEVY_ABI_STATIC_ASSERT(sizeof(DynamicComponentDescriptor) == 64, "DynamicComponentDescriptor alias size must be 64");
BEVY_ABI_STATIC_ASSERT(sizeof(HostApi) == 72, "HostApi size must be 72");
BEVY_ABI_STATIC_ASSERT(BEVY_ABI_ALIGNOF(HostApi) == 8, "HostApi align must be 8");
BEVY_ABI_STATIC_ASSERT(offsetof(HostApi, register_component) == 64, "HostApi register_component offset must be 64");

/* ============================================================================
 * 3. Mock Context & Implementation for HostApi
 * ============================================================================ */
typedef struct MockHostContext {
    int registration_count;
    ModComponentDescriptor last_registered_desc;
    char last_name_buffer[128];
} MockHostContext;

static FfiResult mock_register_system_dummy(void* host_context, const SystemDescriptor* descriptor) {
    (void)host_context;
    (void)descriptor;
    return FFI_RESULT_OK;
}

static size_t mock_iter_next_chunk_dummy(RawChunkIter* iter) {
    (void)iter;
    return 0;
}

static const uint8_t* mock_iter_col_ro_dummy(const RawChunkIter* iter, size_t index) {
    (void)iter;
    (void)index;
    return NULL;
}

static uint8_t* mock_iter_col_mut_dummy(RawChunkIter* iter, size_t index) {
    (void)iter;
    (void)index;
    return NULL;
}

static FfiResult mock_iter_mark_changed_dummy(RawChunkIter* iter, size_t index, size_t row) {
    (void)iter;
    (void)index;
    (void)row;
    return FFI_RESULT_OK;
}

static const FfiEntity* mock_iter_entities_dummy(const RawChunkIter* iter) {
    (void)iter;
    return NULL;
}

static size_t mock_iter_stride_dummy(const RawChunkIter* iter, size_t index) {
    (void)iter;
    (void)index;
    return 16;
}

static FfiResult mock_register_component(void* host_context, const ModComponentDescriptor* descriptor) {
    if (!host_context || !descriptor) {
        return FFI_RESULT_NULL_POINTER;
    }
    if (descriptor->struct_size < sizeof(ModComponentDescriptor)) {
        return FFI_RESULT_INVALID_VERSION;
    }
    if (!descriptor->name) {
        return FFI_RESULT_NULL_POINTER;
    }
    if (descriptor->align == 0 || (descriptor->align & (descriptor->align - 1)) != 0) {
        return FFI_RESULT_INTERNAL_ERROR;
    }
    if (descriptor->size % descriptor->align != 0) {
        return FFI_RESULT_INTERNAL_ERROR;
    }

    MockHostContext* ctx = (MockHostContext*)host_context;
    ctx->registration_count++;
    ctx->last_registered_desc = *descriptor;
    strncpy(ctx->last_name_buffer, descriptor->name, sizeof(ctx->last_name_buffer) - 1);
    ctx->last_name_buffer[sizeof(ctx->last_name_buffer) - 1] = '\0';
    return FFI_RESULT_OK;
}

/* Lifecycle Drop callback tracker */
static int g_drop_invocations = 0;
static void test_custom_drop_hook(void* ptr) {
    if (ptr) {
        g_drop_invocations++;
    }
}

/* ============================================================================
 * 4. Test Suites
 * ============================================================================ */

/* Test Suite 1: Layout & Field Offsets */
static void test_layout_and_offsets(void) {
    printf("[1/5] Testing layout and field offsets...\n");

    assert(sizeof(ModComponentDescriptor) == 64);
    assert(BEVY_ABI_ALIGNOF(ModComponentDescriptor) == 8);
    assert(offsetof(ModComponentDescriptor, struct_size) == 0);
    assert(offsetof(ModComponentDescriptor, name) == 8);
    assert(offsetof(ModComponentDescriptor, uuid) == 16);
    assert(offsetof(ModComponentDescriptor, size) == 32);
    assert(offsetof(ModComponentDescriptor, align) == 40);
    assert(offsetof(ModComponentDescriptor, drop_fn) == 48);
    assert(offsetof(ModComponentDescriptor, vtable_ptr) == 56);

    assert(sizeof(HostApi) == 72);
    assert(BEVY_ABI_ALIGNOF(HostApi) == 8);
    assert(offsetof(HostApi, struct_size) == 0);
    assert(offsetof(HostApi, abi_version) == 4);
    assert(offsetof(HostApi, register_system) == 8);
    assert(offsetof(HostApi, iter_next_chunk) == 16);
    assert(offsetof(HostApi, iter_column_readonly) == 24);
    assert(offsetof(HostApi, iter_column_mutable) == 32);
    assert(offsetof(HostApi, iter_mark_changed) == 40);
    assert(offsetof(HostApi, iter_entities) == 48);
    assert(offsetof(HostApi, iter_column_stride) == 56);
    assert(offsetof(HostApi, register_component) == 64);

    printf("  -> OK (all struct sizes and field offsets verified)\n");
}

/* Test Suite 2: Component Declaration & Mock Registration */
static void test_dynamic_component_registration(void) {
    printf("[2/5] Testing dynamic component declaration & registration in C...\n");

    MockHostContext ctx;
    memset(&ctx, 0, sizeof(ctx));

    HostApi host_api;
    memset(&host_api, 0, sizeof(host_api));
    host_api.struct_size = (uint32_t)sizeof(HostApi);
    host_api.abi_version = BEVY_ABI_VERSION_PACKED;
    host_api.register_system = mock_register_system_dummy;
    host_api.iter_next_chunk = mock_iter_next_chunk_dummy;
    host_api.iter_column_readonly = mock_iter_col_ro_dummy;
    host_api.iter_column_mutable = mock_iter_col_mut_dummy;
    host_api.iter_mark_changed = mock_iter_mark_changed_dummy;
    host_api.iter_entities = mock_iter_entities_dummy;
    host_api.iter_column_stride = mock_iter_stride_dummy;
    host_api.register_component = mock_register_component;

    /* A. Plain Old Data (POD) Component */
    ModComponentDescriptor pod_desc;
    memset(&pod_desc, 0, sizeof(pod_desc));
    pod_desc.struct_size = (uint32_t)sizeof(ModComponentDescriptor);
    pod_desc.name = "Health";
    memset(pod_desc.uuid.bytes, 0x11, 16);
    pod_desc.size = sizeof(uint32_t);
    pod_desc.align = BEVY_ABI_ALIGNOF(uint32_t);
    pod_desc.drop_fn = NULL;
    pod_desc.vtable_ptr = NULL;

    FfiResult res = host_api.register_component(&ctx, &pod_desc);
    assert(res == FFI_RESULT_OK);
    assert(ctx.registration_count == 1);
    assert(strcmp(ctx.last_name_buffer, "Health") == 0);
    assert(ctx.last_registered_desc.size == 4);
    assert(ctx.last_registered_desc.align == 4);
    assert(ctx.last_registered_desc.drop_fn == NULL);

    /* B. Component with Lifecycle Drop Hook */
    ModComponentDescriptor drop_desc;
    memset(&drop_desc, 0, sizeof(drop_desc));
    drop_desc.struct_size = (uint32_t)sizeof(ModComponentDescriptor);
    drop_desc.name = "ManagedString";
    memset(drop_desc.uuid.bytes, 0x22, 16);
    drop_desc.size = 16;
    drop_desc.align = 8;
    drop_desc.drop_fn = test_custom_drop_hook;
    drop_desc.vtable_ptr = NULL;

    res = host_api.register_component(&ctx, &drop_desc);
    assert(res == FFI_RESULT_OK);
    assert(ctx.registration_count == 2);
    assert(strcmp(ctx.last_name_buffer, "ManagedString") == 0);
    assert(ctx.last_registered_desc.drop_fn == test_custom_drop_hook);

    /* Verify executing drop hook through function pointer */
    int dummy_resource = 42;
    ctx.last_registered_desc.drop_fn(&dummy_resource);
    assert(g_drop_invocations == 1);

    /* C. Zero-Sized Type (ZST) Marker Component */
    ModComponentDescriptor zst_desc;
    memset(&zst_desc, 0, sizeof(zst_desc));
    zst_desc.struct_size = (uint32_t)sizeof(ModComponentDescriptor);
    zst_desc.name = "PlayerTag";
    memset(zst_desc.uuid.bytes, 0x33, 16);
    zst_desc.size = 0;
    zst_desc.align = 1;
    zst_desc.drop_fn = NULL;
    zst_desc.vtable_ptr = NULL;

    res = host_api.register_component(&ctx, &zst_desc);
    assert(res == FFI_RESULT_OK);
    assert(ctx.registration_count == 3);
    assert(strcmp(ctx.last_name_buffer, "PlayerTag") == 0);
    assert(ctx.last_registered_desc.size == 0);

    /* D. Over-aligned Type (SIMD Vector 16-byte alignment) */
    ModComponentDescriptor simd_desc;
    memset(&simd_desc, 0, sizeof(simd_desc));
    simd_desc.struct_size = (uint32_t)sizeof(ModComponentDescriptor);
    simd_desc.name = "SimdVec4";
    memset(simd_desc.uuid.bytes, 0x44, 16);
    simd_desc.size = 32;
    simd_desc.align = 16;
    simd_desc.drop_fn = NULL;

    res = host_api.register_component(&ctx, &simd_desc);
    assert(res == FFI_RESULT_OK);
    assert(ctx.registration_count == 4);
    assert(strcmp(ctx.last_name_buffer, "SimdVec4") == 0);
    assert(ctx.last_registered_desc.align == 16);

    printf("  -> OK (POD, drop-hook, ZST, and overaligned components registered successfully)\n");
}

/* Test Suite 3: Defensive Validation Against Adversarial Descriptors */
static void test_adversarial_descriptors(void) {
    printf("[3/5] Testing defensive validation against adversarial descriptors...\n");

    MockHostContext ctx;
    memset(&ctx, 0, sizeof(ctx));

    HostApi host_api;
    memset(&host_api, 0, sizeof(host_api));
    host_api.struct_size = (uint32_t)sizeof(HostApi);
    host_api.abi_version = BEVY_ABI_VERSION_PACKED;
    host_api.register_component = mock_register_component;

    ModComponentDescriptor valid_desc;
    memset(&valid_desc, 0, sizeof(valid_desc));
    valid_desc.struct_size = (uint32_t)sizeof(ModComponentDescriptor);
    valid_desc.name = "TestComp";
    valid_desc.size = 8;
    valid_desc.align = 8;

    /* A. Null Host Context */
    FfiResult res = host_api.register_component(NULL, &valid_desc);
    assert(res == FFI_RESULT_NULL_POINTER);

    /* B. Null Descriptor Pointer */
    res = host_api.register_component(&ctx, NULL);
    assert(res == FFI_RESULT_NULL_POINTER);

    /* C. Truncated struct_size (< 64) */
    ModComponentDescriptor truncated = valid_desc;
    truncated.struct_size = 48;
    res = host_api.register_component(&ctx, &truncated);
    assert(res == FFI_RESULT_INVALID_VERSION);

    /* D. Null Name Pointer */
    ModComponentDescriptor null_name = valid_desc;
    null_name.name = NULL;
    res = host_api.register_component(&ctx, &null_name);
    assert(res == FFI_RESULT_NULL_POINTER);

    /* E. Alignment == 0 */
    ModComponentDescriptor zero_align = valid_desc;
    zero_align.align = 0;
    res = host_api.register_component(&ctx, &zero_align);
    assert(res == FFI_RESULT_INTERNAL_ERROR);

    /* F. Non-power-of-two Alignment */
    ModComponentDescriptor bad_align = valid_desc;
    bad_align.align = 7;
    bad_align.size = 14;
    res = host_api.register_component(&ctx, &bad_align);
    assert(res == FFI_RESULT_INTERNAL_ERROR);

    /* G. Misaligned Size (size % align != 0) */
    ModComponentDescriptor bad_size = valid_desc;
    bad_size.size = 13;
    bad_size.align = 8;
    res = host_api.register_component(&ctx, &bad_size);
    assert(res == FFI_RESULT_INTERNAL_ERROR);

    /* H. Forward Compatible struct_size (> 64) */
    ModComponentDescriptor extended = valid_desc;
    extended.struct_size = 128;
    res = host_api.register_component(&ctx, &extended);
    assert(res == FFI_RESULT_OK);

    printf("  -> OK (all defensive error checks passed)\n");
}

/* Test Suite 4: Monotonic Version Compatibility: Older Mod (64B) against Newer Host (72B) */
static void test_legacy_mod_on_newer_host(void) {
    printf("[4/5] Testing legacy 64-byte mod against newer 72-byte HostApi...\n");

    /* Simulates a modern 72-byte HostApi instance supplied by host */
    HostApi modern_host;
    memset(&modern_host, 0, sizeof(modern_host));
    modern_host.struct_size = (uint32_t)sizeof(HostApi); /* 72 bytes */
    modern_host.abi_version = BEVY_ABI_VERSION_PACKED;
    modern_host.register_system = mock_register_system_dummy;
    modern_host.iter_next_chunk = mock_iter_next_chunk_dummy;
    modern_host.iter_column_readonly = mock_iter_col_ro_dummy;
    modern_host.iter_column_mutable = mock_iter_col_mut_dummy;
    modern_host.iter_mark_changed = mock_iter_mark_changed_dummy;
    modern_host.iter_entities = mock_iter_entities_dummy;
    modern_host.iter_column_stride = mock_iter_stride_dummy;
    modern_host.register_component = mock_register_component;

    /* A legacy C mod compiled against LegacyHostApi (64 bytes) receives modern_host */
    const LegacyHostApi* legacy_view = (const LegacyHostApi*)(const void*)&modern_host;

    /* 1. Legacy mod performs monotonic struct_size negotiation */
    assert(legacy_view->struct_size >= sizeof(LegacyHostApi));
    assert(legacy_view->struct_size == 72);
    assert(legacy_view->abi_version == BEVY_ABI_VERSION_PACKED);

    /* 2. Legacy mod accesses its recognized function pointers (offsets 8..56) */
    assert(legacy_view->register_system == mock_register_system_dummy);
    assert(legacy_view->iter_next_chunk == mock_iter_next_chunk_dummy);
    assert(legacy_view->iter_column_readonly == mock_iter_col_ro_dummy);
    assert(legacy_view->iter_column_mutable == mock_iter_col_mut_dummy);
    assert(legacy_view->iter_mark_changed == mock_iter_mark_changed_dummy);
    assert(legacy_view->iter_entities == mock_iter_entities_dummy);
    assert(legacy_view->iter_column_stride == mock_iter_stride_dummy);

    /* 3. Invoke a legacy function through the pointer */
    size_t stride = legacy_view->iter_column_stride(NULL, 0);
    assert(stride == 16);

    printf("  -> OK (72-byte HostApi backward-compatible with 64-byte mod; no memory corruption)\n");
}

/* Test Suite 5: Monotonic Version Compatibility: Newer Mod (72B) against Older Host (64B) */
static void test_newer_mod_on_legacy_host(void) {
    printf("[5/5] Testing newer 72-byte mod against older 64-byte HostApi...\n");

    /* Simulates an older host providing only a 64-byte LegacyHostApi */
    LegacyHostApi legacy_host;
    memset(&legacy_host, 0, sizeof(legacy_host));
    legacy_host.struct_size = (uint32_t)sizeof(LegacyHostApi); /* 64 bytes */
    legacy_host.abi_version = BEVY_ABI_VERSION_PACKED;
    legacy_host.register_system = mock_register_system_dummy;
    legacy_host.iter_next_chunk = mock_iter_next_chunk_dummy;
    legacy_host.iter_column_readonly = mock_iter_col_ro_dummy;
    legacy_host.iter_column_mutable = mock_iter_col_mut_dummy;
    legacy_host.iter_mark_changed = mock_iter_mark_changed_dummy;
    legacy_host.iter_entities = mock_iter_entities_dummy;
    legacy_host.iter_column_stride = mock_iter_stride_dummy;

    /* Newer mod expects HostApi (72 bytes): inspect header via raw_struct_size without
     * forming a 72-byte HostApi lvalue over the 64-byte legacy allocation. */
    const HostApi* modern_view = (const HostApi*)(const void*)&legacy_host;
    uint32_t raw_size = bam_host_api_raw_struct_size(modern_view);
    assert(raw_size == sizeof(LegacyHostApi));

    /* Newer mod checks whether register_component is available */
    FfiBool has_register_component = (raw_size >= (offsetof(HostApi, register_component) + sizeof(RegisterComponentFn))) ? FFI_BOOL_TRUE : FFI_BOOL_FALSE;
    assert(!has_register_component);

    /* Also verify strict sizeof check */
    FfiBool meets_full_size = (raw_size >= sizeof(HostApi)) ? FFI_BOOL_TRUE : FFI_BOOL_FALSE;
    assert(!meets_full_size);

    /* Materialize bounded local HostApi copy; trailing fields past 64 bytes stay NULL */
    HostApi bounded_api;
    bam_host_api_copy_bounded(modern_view, &bounded_api);
    assert(bounded_api.struct_size >= sizeof(LegacyHostApi));
    assert(bounded_api.register_component == NULL);
    assert(bounded_api.register_system == mock_register_system_dummy);
    assert(bounded_api.iter_column_stride(NULL, 0) == 16);

    printf("  -> OK (boundary check correctly prevents reading or calling past 64-byte boundary)\n");
}

/* Test Suite 6: Entity Null Sentinel, SemVer Helpers, FfiBool & Scheduled Descriptors */
static void test_entity_null_semver_and_scheduled_descriptors(void) {
    printf("[6/6] Testing BAM_ENTITY_NULL, SemVer helpers, FfiBool & ScheduledSystemDescriptor...\n");

    /* 1. Entity null sentinel (0ULL, never UINT64_MAX) */
    assert(BAM_ENTITY_NULL == 0ULL);
    assert(FFI_ENTITY_NULL == 0ULL);
    assert(bam_entity_is_null(BAM_ENTITY_NULL) == FFI_BOOL_TRUE);
    assert(bam_entity_is_null(1ULL) == FFI_BOOL_FALSE);
    assert(bam_entity_is_null(UINT64_MAX) == FFI_BOOL_FALSE);

    /* 2. SemVer compatibility helper */
    uint32_t v0_1_0 = bevy_abi_pack_version(0, 1, 0);
    uint32_t v0_1_5 = bevy_abi_pack_version(0, 1, 5);
    uint32_t v0_2_0 = bevy_abi_pack_version(0, 2, 0);
    uint32_t v1_0_0 = bevy_abi_pack_version(1, 0, 0);
    assert(v0_1_0 == BEVY_ABI_VERSION_PACKED);
    assert(bevy_abi_version_is_compatible(v0_1_0, v0_1_5) == FFI_BOOL_TRUE);
    assert(bevy_abi_version_is_compatible(v0_2_0, v0_1_0) == FFI_BOOL_TRUE);
    assert(bevy_abi_version_is_compatible(v0_1_0, v0_2_0) == FFI_BOOL_FALSE);
    assert(bevy_abi_version_is_compatible(v1_0_0, v0_1_0) == FFI_BOOL_FALSE);

    /* 3. ScheduleLabelId constants & ScheduledSystemDescriptor layout */
    assert(SCHEDULE_LABEL_UPDATE == 0u);
    assert(SCHEDULE_LABEL_STARTUP == 1u);
    assert(SCHEDULE_LABEL_PRE_UPDATE == 2u);
    assert(SCHEDULE_LABEL_POST_UPDATE == 3u);
    assert(SCHEDULE_LABEL_FIXED_UPDATE == 4u);
    assert(SCHEDULE_LABEL_LAST == 5u);

    const char* after_names[1] = { "physics_step" };
    ScheduledSystemDescriptor sched_desc;
    memset(&sched_desc, 0, sizeof(sched_desc));
    sched_desc.base.struct_size = (uint32_t)sizeof(ScheduledSystemDescriptor);
    sched_desc.base.name = "render_sync";
    sched_desc.schedule = BAM_SCHEDULE_POST_UPDATE;
    sched_desc.after = after_names;
    sched_desc.after_count = 1;

    assert(sched_desc.base.struct_size == sizeof(ScheduledSystemDescriptor));
    assert(((const SystemDescriptor*)&sched_desc)->struct_size == sizeof(ScheduledSystemDescriptor));
    assert(sched_desc.schedule == 3u);
    assert(sched_desc.after_count == 1u);

    printf("  -> OK (BAM_ENTITY_NULL=0, SemVer matrix, FfiBool & ScheduledSystemDescriptor verified)\n");
}

static FfiBool mock_run_condition(void* user_data) {
    return user_data != NULL ? FFI_BOOL_TRUE : FFI_BOOL_FALSE;
}

/* Test Suite 7: Phase 3 & Phase 4 Access Modes, Resource/Command Typedefs & Conditional Descriptors */
static void test_phase3_phase4_abi_extensions(void) {
    printf("[7/7] Testing Phase 3/4 access modes, command/resource fn typedefs & ConditionalSystemDescriptor...\n");

    assert(COMPONENT_ACCESS_READ == 0u);
    assert(COMPONENT_ACCESS_WRITE == 1u);
    assert(COMPONENT_ACCESS_FILTER_ONLY == 2u);
    assert(COMPONENT_ACCESS_WITHOUT == 3u);
    assert(COMPONENT_ACCESS_OPTIONAL_READ == 4u);
    assert(COMPONENT_ACCESS_OPTIONAL_WRITE == 5u);
    assert(COMPONENT_ACCESS_CHANGED == 6u);
    assert(COMPONENT_ACCESS_ADDED == 7u);
    assert(COMPONENT_ACCESS_RESOURCE_READ == 8u);
    assert(COMPONENT_ACCESS_RESOURCE_WRITE == 9u);

    ComponentAccessDesc opt_ro = bam_access_desc("Velocity", COMPONENT_ACCESS_OPTIONAL_READ);
    ComponentAccessDesc chg = bam_access_desc("Position", COMPONENT_ACCESS_CHANGED);
    ComponentAccessDesc res_rw = bam_access_desc("ScoreBoard", COMPONENT_ACCESS_RESOURCE_WRITE);
    assert(opt_ro.mode == COMPONENT_ACCESS_OPTIONAL_READ);
    assert(chg.mode == COMPONENT_ACCESS_CHANGED);
    assert(res_rw.mode == COMPONENT_ACCESS_RESOURCE_WRITE);

    int flag = 1;
    ConditionalSystemDescriptor cond_desc;
    memset(&cond_desc, 0, sizeof(cond_desc));
    cond_desc.sched.base.struct_size = (uint32_t)sizeof(ConditionalSystemDescriptor);
    cond_desc.sched.base.name = "conditional_sys";
    cond_desc.sched.schedule = BAM_SCHEDULE_UPDATE;
    cond_desc.run_if = mock_run_condition;
    cond_desc.run_if_user_data = &flag;

    assert(((const SystemDescriptor*)&cond_desc)->struct_size == sizeof(ConditionalSystemDescriptor));
    assert(cond_desc.run_if(cond_desc.run_if_user_data) == FFI_BOOL_TRUE);
    assert(cond_desc.run_if(NULL) == FFI_BOOL_FALSE);

    /* Verify function pointer typedef usability */
    IterResourceReadonlyFn fn_res_ro = NULL;
    IterResourceMutableFn fn_res_mut = NULL;
    CmdRemoveComponentFn fn_cmd_rm = NULL;
    CmdInsertDefaultFn fn_cmd_def = NULL;
    assert(fn_res_ro == NULL && fn_res_mut == NULL && fn_cmd_rm == NULL && fn_cmd_def == NULL);

    printf("  -> OK (Phase 3/4 constants, typedefs & ConditionalSystemDescriptor verified)\n");
}

/* ============================================================================
 * Main Entry Point
 * ============================================================================ */
int main(void) {
    printf("=== Starting C Header & ABI Compatibility Harness ===\n");
    test_layout_and_offsets();
    test_dynamic_component_registration();
    test_adversarial_descriptors();
    test_legacy_mod_on_newer_host();
    test_newer_mod_on_legacy_host();
    test_entity_null_semver_and_scheduled_descriptors();
    test_phase3_phase4_abi_extensions();
    printf("=== All C Header & ABI Compatibility Tests Passed! ===\n");
    return 0;
}
