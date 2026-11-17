/*
 * Standalone ISO C99 Conformance, Layout & Mock Lifecycle Suite (`ABI 0.2.0`)
 * Compiled with: `cc -std=c99 -Wall -Wextra -Werror -pedantic -Iinclude tests/standalone_c_test.c`
 */

#include "bevy_abi_mod.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ==========================================================================
 * Mock Host & Guest Structures
 * ========================================================================== */

typedef struct MockPosition {
    float x;
    float y;
} MockPosition;

typedef struct MockPositionVTable {
    BamVTableHeader header;
    float (*get_x)(const MockPosition* self_ptr);
} MockPositionVTable;

static float mock_pos_get_x(const MockPosition* self_ptr) {
    return self_ptr->x;
}

struct BamHostCtx {
    char         last_err[128];
    const void*  vtable_symbol;
    BamComponent health_handle;
    BamComponent pos_handle;
    BamComponent dt_res_handle;
};

struct BamLoadCtx {
    BamHostCtx*    host_ctx;
    uint32_t       registered_components;
    uint32_t       registered_systems;
    BamSystemDesc  last_system;
};

struct BamIter {
    uint32_t      chunk_step;
    BamEntity     entities[2];
    float         health_dense[2];
    MockPosition  sparse_rows_storage[2];
    uint8_t*      sparse_row_ptrs[2];
    float         dt_resource;
    uint32_t      marked_start;
    uint32_t      marked_count;
    BamComponentUuid health_uuid;
};

struct BamCommands {
    uint32_t  spawned_count;
    uint32_t  batch_component_count;
    uint32_t  inserted_count;
    uint32_t  despawned_count;
    BamEntity next_entity;
};

struct BamSystemCtx {
    BamHostCtx*  host_ctx;
    BamIter*     iter0;
    BamCommands* commands;
};

struct BamInstance {
    const BamHostCtx* host_ctx;
    BamComponent      health_comp;
    BamComponent      pos_comp;
    BamComponent      dt_res;
    uint32_t          ticks_executed;
    BamBool           enabled;
};

static MockPositionVTable g_mock_pos_vtable;
static uint32_t g_shutdown_calls = 0u;
static uint32_t g_drop_calls = 0u;

static void mock_health_drop(void* ptr) {
    (void)ptr;
    g_drop_calls += 1u;
}

/* ==========================================================================
 * Mock Host Implementation of All 27 `BamHostApi` Slots
 * ========================================================================== */

static const void* host_get_proc_address(const BamHostCtx* ctx, const char* name) {
    if (ctx == NULL || name == NULL) {
        return NULL;
    }
    if (strcmp(name, "bam.v0.game::MockPosition.vtable") == 0) {
        return ctx->vtable_symbol;
    }
    return NULL;
}

static size_t host_last_error(const BamHostCtx* ctx, char* buf, size_t cap) {
    size_t msg_len;
    size_t copy_n;
    if (ctx == NULL) {
        return 0u;
    }
    msg_len = strlen(ctx->last_err);
    if (buf != NULL && cap > 0u) {
        copy_n = (msg_len < (cap - 1u)) ? msg_len : (cap - 1u);
        memcpy(buf, ctx->last_err, copy_n);
        buf[copy_n] = '\0';
    }
    return msg_len;
}

static const BamHostCtx* host_load_host_ctx(const BamLoadCtx* load_ctx) {
    return (load_ctx != NULL) ? load_ctx->host_ctx : NULL;
}

static BamResult host_register_component(
    BamLoadCtx* load_ctx,
    const BamComponentDesc* desc,
    BamComponent* out_component
) {
    if (load_ctx == NULL || desc == NULL || out_component == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (desc->struct_size < BAM_COMPONENT_DESC_MIN_V0_2_SIZE) {
        return BAM_RESULT_INVALID_VERSION;
    }
    if ((desc->flags & ~BAM_COMPONENT_KNOWN_FLAGS) != 0u) {
        return BAM_RESULT_UNSUPPORTED;
    }
    load_ctx->registered_components += 1u;
    *out_component = load_ctx->host_ctx->health_handle;
    return BAM_RESULT_OK;
}

static BamResult host_resolve_component(
    const BamHostCtx* ctx,
    const char* name,
    const BamComponentUuid* uuid,
    BamComponent* out_component
) {
    if (ctx == NULL || out_component == NULL || (name == NULL && uuid == NULL)) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (name != NULL && strcmp(name, "game::MockPosition") == 0) {
        *out_component = ctx->pos_handle;
        return BAM_RESULT_OK;
    }
    if (name != NULL && strcmp(name, "game::DeltaTime") == 0) {
        *out_component = ctx->dt_res_handle;
        return BAM_RESULT_OK;
    }
    strcpy(((BamHostCtx*)ctx)->last_err, "component not registered in mock world");
    return BAM_RESULT_NOT_FOUND;
}

static BamResult host_register_system(BamLoadCtx* load_ctx, const BamSystemDesc* desc) {
    if (load_ctx == NULL || desc == NULL || desc->name == NULL || desc->callback == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (desc->struct_size < BAM_SYSTEM_DESC_MIN_V0_2_SIZE) {
        return BAM_RESULT_INVALID_VERSION;
    }
    if ((desc->flags & ~BAM_SYSTEM_KNOWN_FLAGS) != 0u) {
        return BAM_RESULT_UNSUPPORTED;
    }
    if (desc->reserved0 != 0u) {
        return BAM_RESULT_INVALID_ARGUMENT;
    }
    load_ctx->registered_systems += 1u;
    load_ctx->last_system = *desc;
    return BAM_RESULT_OK;
}

static const BamHostCtx* host_sys_host_ctx(const BamSystemCtx* sys_ctx) {
    return (sys_ctx != NULL) ? sys_ctx->host_ctx : NULL;
}

static BamResult host_sys_query(BamSystemCtx* sys_ctx, size_t query_index, BamIter** out_iter) {
    if (sys_ctx == NULL || out_iter == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (query_index != 0u) {
        return BAM_RESULT_OUT_OF_RANGE;
    }
    *out_iter = sys_ctx->iter0;
    return BAM_RESULT_OK;
}

static BamResult host_sys_commands(BamSystemCtx* sys_ctx, BamCommands** out_commands) {
    if (sys_ctx == NULL || out_commands == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *out_commands = sys_ctx->commands;
    return BAM_RESULT_OK;
}

static BamBool host_iter_next_chunk(BamIter* iter, size_t* out_len) {
    if (iter == NULL || out_len == NULL) {
        return BAM_FALSE;
    }
    if (iter->chunk_step == 0u) {
        iter->chunk_step = 1u;
        *out_len = 2u;
        return BAM_TRUE;
    }
    *out_len = 0u;
    return BAM_FALSE;
}

static BamResult host_iter_reset(BamIter* iter) {
    if (iter == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    iter->chunk_step = 0u;
    return BAM_RESULT_OK;
}

static const BamEntity* host_iter_entities(const BamIter* iter) {
    return (iter != NULL) ? iter->entities : NULL;
}

static const uint8_t* host_iter_column_readonly(const BamIter* iter, size_t term_index) {
    if (iter == NULL || term_index != 0u) {
        return NULL;
    }
    return (const uint8_t*)iter->health_dense;
}

static uint8_t* host_iter_column_mutable(BamIter* iter, size_t term_index) {
    if (iter == NULL || term_index != 0u) {
        return NULL;
    }
    return (uint8_t*)iter->health_dense;
}

static uint8_t* host_iter_column_mutable_untracked(BamIter* iter, size_t term_index) {
    if (iter == NULL || term_index != 0u) {
        return NULL;
    }
    return (uint8_t*)iter->health_dense;
}

static BamResult host_iter_mark_changed_range(
    BamIter* iter,
    size_t term_index,
    size_t start_row,
    size_t row_count
) {
    if (iter == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (term_index != 0u || start_row + row_count > 2u) {
        return BAM_RESULT_OUT_OF_RANGE;
    }
    iter->marked_start = (uint32_t)start_row;
    iter->marked_count = (uint32_t)row_count;
    return BAM_RESULT_OK;
}

static const uint8_t* const* host_iter_column_rows_readonly(BamIter* iter, size_t term_index) {
    if (iter == NULL || term_index != 1u) {
        return NULL;
    }
    return (const uint8_t* const*)iter->sparse_row_ptrs;
}

static uint8_t* const* host_iter_column_rows_mutable(BamIter* iter, size_t term_index) {
    if (iter == NULL || term_index != 1u) {
        return NULL;
    }
    return iter->sparse_row_ptrs;
}

static BamResult host_iter_column_info(
    const BamIter* iter,
    size_t term_index,
    BamColumnInfo* out_info
) {
    if (iter == NULL || out_info == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (term_index != 0u) {
        return BAM_RESULT_OUT_OF_RANGE;
    }
    out_info->component = 1u;
    out_info->mode = BAM_ACCESS_WRITE;
    out_info->uuid = iter->health_uuid;
    out_info->size = sizeof(float);
    out_info->align = BAM_ALIGNOF(float);
    out_info->storage_flags = BAM_STORAGE_FLAG_SPARSE_SET;
    out_info->reserved0 = 0u;
    return BAM_RESULT_OK;
}

static const uint8_t* host_res_ptr(const BamIter* iter, size_t term_index) {
    if (iter == NULL || term_index != 2u) {
        return NULL;
    }
    return (const uint8_t*)&iter->dt_resource;
}

static uint8_t* host_res_mut_ptr(BamIter* iter, size_t term_index) {
    if (iter == NULL || term_index != 2u) {
        return NULL;
    }
    return (uint8_t*)&iter->dt_resource;
}

static BamResult host_cmd_spawn_empty(BamCommands* commands, BamEntity* out_entity) {
    if (commands == NULL || out_entity == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    commands->next_entity += 1ull;
    commands->spawned_count += 1u;
    *out_entity = commands->next_entity;
    return BAM_RESULT_OK;
}

static BamResult host_cmd_spawn(
    BamCommands* commands,
    const BamComponent* components,
    const uint8_t* const* data_ptrs,
    size_t count,
    BamEntity* out_entity
) {
    if (commands == NULL || out_entity == NULL || (count > 0u && (components == NULL || data_ptrs == NULL))) {
        return BAM_RESULT_NULL_POINTER;
    }
    commands->next_entity += 1ull;
    commands->spawned_count += 1u;
    commands->batch_component_count += (uint32_t)count;
    *out_entity = commands->next_entity;
    return BAM_RESULT_OK;
}

static BamResult host_cmd_despawn(BamCommands* commands, BamEntity entity) {
    if (commands == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (bam_entity_is_null(entity)) {
        return BAM_RESULT_INVALID_ENTITY;
    }
    commands->despawned_count += 1u;
    return BAM_RESULT_OK;
}

static BamResult host_cmd_insert(
    BamCommands* commands,
    BamEntity entity,
    BamComponent component,
    const uint8_t* data_ptr,
    size_t data_len
) {
    if (commands == NULL || (data_len > 0u && data_ptr == NULL)) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (bam_entity_is_null(entity)) {
        return BAM_RESULT_INVALID_ENTITY;
    }
    if (!bam_component_is_valid(component)) {
        return BAM_RESULT_INVALID_COMPONENT;
    }
    commands->inserted_count += 1u;
    return BAM_RESULT_OK;
}

static BamResult host_cmd_insert_default(
    BamCommands* commands,
    BamEntity entity,
    BamComponent component
) {
    if (commands == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (bam_entity_is_null(entity)) {
        return BAM_RESULT_INVALID_ENTITY;
    }
    if (!bam_component_is_valid(component)) {
        return BAM_RESULT_INVALID_COMPONENT;
    }
    commands->inserted_count += 1u;
    return BAM_RESULT_OK;
}

static BamResult host_cmd_remove(
    BamCommands* commands,
    BamEntity entity,
    BamComponent component
) {
    if (commands == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (bam_entity_is_null(entity)) {
        return BAM_RESULT_INVALID_ENTITY;
    }
    if (!bam_component_is_valid(component)) {
        return BAM_RESULT_INVALID_COMPONENT;
    }
    return BAM_RESULT_OK;
}

static BamHostApi build_mock_host_api(void) {
    BamHostApi api;
    memset(&api, 0, sizeof(api));
    api.struct_size = (uint32_t)sizeof(BamHostApi);
    api.abi_version = BAM_ABI_VERSION;
    api.get_proc_address = host_get_proc_address;
    api.last_error = host_last_error;
    api.load_host_ctx = host_load_host_ctx;
    api.register_component = host_register_component;
    api.resolve_component = host_resolve_component;
    api.register_system = host_register_system;
    api.sys_host_ctx = host_sys_host_ctx;
    api.sys_query = host_sys_query;
    api.sys_commands = host_sys_commands;
    api.iter_next_chunk = host_iter_next_chunk;
    api.iter_reset = host_iter_reset;
    api.iter_entities = host_iter_entities;
    api.iter_column_readonly = host_iter_column_readonly;
    api.iter_column_mutable = host_iter_column_mutable;
    api.iter_column_mutable_untracked = host_iter_column_mutable_untracked;
    api.iter_mark_changed_range = host_iter_mark_changed_range;
    api.iter_column_rows_readonly = host_iter_column_rows_readonly;
    api.iter_column_rows_mutable = host_iter_column_rows_mutable;
    api.iter_column_info = host_iter_column_info;
    api.res_ptr = host_res_ptr;
    api.res_mut_ptr = host_res_mut_ptr;
    api.cmd_spawn_empty = host_cmd_spawn_empty;
    api.cmd_spawn = host_cmd_spawn;
    api.cmd_despawn = host_cmd_despawn;
    api.cmd_insert = host_cmd_insert;
    api.cmd_insert_default = host_cmd_insert_default;
    api.cmd_remove = host_cmd_remove;
    return api;
}

/* ==========================================================================
 * Mock Guest Mod Entry Points & Unified System Callback
 * ========================================================================== */

static BamHostApi g_guest_api;

static const BamManifest g_guest_manifest = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "combat_mod",
    .version = "0.2.0",
    .flags = 0u,
    .reserved0 = 0u
};

static const BamManifest* mock_guest_manifest(void) {
    return &g_guest_manifest;
}

static BamBool mock_guest_run_if(void* user_data) {
    const BamInstance* inst = (const BamInstance*)user_data;
    return (inst != NULL) ? inst->enabled : BAM_FALSE;
}

static BamResult mock_guest_system(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* inst = (BamInstance*)user_data;
    BamIter* iter = NULL;
    BamCommands* cmds = NULL;
    BamColumnInfo col_info;
    size_t chunk_len = 0u;
    BamResult rc;

    rc = g_guest_api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = g_guest_api.sys_commands(sys_ctx, &cmds);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    rc = g_guest_api.iter_column_info(iter, 0u, &col_info);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    assert(col_info.size == sizeof(float));
    assert((col_info.storage_flags & BAM_STORAGE_FLAG_SPARSE_SET) != 0u);

    while (g_guest_api.iter_next_chunk(iter, &chunk_len) != BAM_FALSE) {
        const BamEntity* entities = g_guest_api.iter_entities(iter);
        float* health = (float*)g_guest_api.iter_column_mutable_untracked(iter, 0u);
        uint8_t* const* pos_rows = g_guest_api.iter_column_rows_mutable(iter, 1u);
        const float* dt = (const float*)g_guest_api.res_ptr(iter, 2u);
        size_t row;

        assert(entities != NULL && health != NULL && pos_rows != NULL && dt != NULL);
        for (row = 0u; row < chunk_len; ++row) {
            MockPosition* pos = (MockPosition*)pos_rows[row];
            pos->x += *dt;
            if (row == 1u) {
                health[row] += 5.0f;
            }
        }
        rc = g_guest_api.iter_mark_changed_range(iter, 0u, 1u, 1u);
        assert(rc == BAM_RESULT_OK);
    }

    /* Reset iterator for re-traversal */
    rc = g_guest_api.iter_reset(iter);
    assert(rc == BAM_RESULT_OK);

    /* Batched spawn + handle-based insert */
    {
        float initial_hp = 100.0f;
        MockPosition initial_pos = { 4.0f, 8.0f };
        BamComponent batch_comps[2];
        const uint8_t* batch_ptrs[2];
        BamEntity spawned = BAM_ENTITY_NULL;

        batch_comps[0] = inst->health_comp;
        batch_comps[1] = inst->pos_comp;
        batch_ptrs[0] = (const uint8_t*)&initial_hp;
        batch_ptrs[1] = (const uint8_t*)&initial_pos;

        rc = g_guest_api.cmd_spawn(cmds, batch_comps, batch_ptrs, 2u, &spawned);
        assert(rc == BAM_RESULT_OK);
        assert(!bam_entity_is_null(spawned));

        rc = g_guest_api.cmd_insert(
            cmds,
            spawned,
            inst->health_comp,
            (const uint8_t*)&initial_hp,
            sizeof(initial_hp)
        );
        assert(rc == BAM_RESULT_OK);
    }

    inst->ticks_executed += 1u;
    return BAM_RESULT_OK;
}

static BamResult mock_guest_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    BamResult rc;
    const BamHostCtx* host_ctx;
    BamInstance* inst;
    BamComponentDesc comp_desc;
    BamTermDesc terms[3];
    BamQueryDesc query_desc;
    BamSystemDesc sys_desc;
    const char* after_sets[1] = { "physics_prepare" };

    if (instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    rc = bam_host_api_copy_bounded(&g_guest_api, sizeof(g_guest_api), api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    host_ctx = g_guest_api.load_host_ctx(load_ctx);
    if (host_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }

    inst = (BamInstance*)calloc(1u, sizeof(BamInstance));
    if (inst == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    inst->host_ctx = host_ctx;
    inst->enabled = BAM_TRUE;

    memset(&comp_desc, 0, sizeof(comp_desc));
    comp_desc.struct_size = (uint32_t)sizeof(BamComponentDesc);
    comp_desc.flags = BAM_COMPONENT_FLAG_SPARSE_SET;
    comp_desc.name = "combat_mod::Health";
    comp_desc.uuid = bam_uuid_from_parts("combat_mod::Health", sizeof(float), BAM_ALIGNOF(float), 0ull);
    comp_desc.size = sizeof(float);
    comp_desc.align = BAM_ALIGNOF(float);
    comp_desc.drop_fn = mock_health_drop;
    comp_desc.vtable_ptr = NULL;

    rc = g_guest_api.register_component(load_ctx, &comp_desc, &inst->health_comp);
    if (rc != BAM_RESULT_OK) {
        free(inst);
        return rc;
    }

    rc = g_guest_api.resolve_component(host_ctx, "game::MockPosition", NULL, &inst->pos_comp);
    if (rc != BAM_RESULT_OK) {
        free(inst);
        return rc;
    }

    rc = g_guest_api.resolve_component(host_ctx, "game::DeltaTime", NULL, &inst->dt_res);
    if (rc != BAM_RESULT_OK) {
        free(inst);
        return rc;
    }

    terms[0] = bam_term_desc(inst->health_comp, BAM_ACCESS_WRITE, 0u);
    terms[1] = bam_term_desc(inst->pos_comp, BAM_ACCESS_WRITE, 0u);
    terms[2] = bam_term_desc(inst->dt_res, BAM_ACCESS_RESOURCE_READ, 0u);

    memset(&query_desc, 0, sizeof(query_desc));
    query_desc.struct_size = (uint32_t)sizeof(BamQueryDesc);
    query_desc.flags = 0u;
    query_desc.terms = terms;
    query_desc.term_count = 3u;

    memset(&sys_desc, 0, sizeof(sys_desc));
    sys_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys_desc.flags = BAM_SYSTEM_FLAG_MAIN_THREAD;
    sys_desc.name = "combat_mod::regen_and_step";
    sys_desc.queries = &query_desc;
    sys_desc.query_count = 1u;
    sys_desc.callback = mock_guest_system;
    sys_desc.user_data = inst;
    sys_desc.schedule = BAM_SCHEDULE_CUSTOM_BASE + 1u;
    sys_desc.reserved0 = 0u;
    sys_desc.after = after_sets;
    sys_desc.after_count = 1u;
    sys_desc.before = NULL;
    sys_desc.before_count = 0u;
    sys_desc.run_if = mock_guest_run_if;
    sys_desc.run_if_user_data = inst;

    rc = g_guest_api.register_system(load_ctx, &sys_desc);
    if (rc != BAM_RESULT_OK) {
        free(inst);
        return rc;
    }

    *instance_out = inst;
    return BAM_RESULT_OK;
}

static void mock_guest_shutdown(BamInstance* instance) {
    g_shutdown_calls += 1u;
    if (instance != NULL) {
        free(instance);
    }
}

/* ==========================================================================
 * Test Suites
 * ========================================================================== */

static void test_versions_and_result_names(void) {
    assert(BAM_ABI_VERSION == 0x00020000u);
    assert(bam_version_major(BAM_ABI_VERSION) == 0u);
    assert(bam_version_minor(BAM_ABI_VERSION) == 2u);
    assert(bam_version_patch(BAM_ABI_VERSION) == 0u);

    assert(bam_version_is_compatible(bam_pack_abi_version(0u, 2u, 0u), bam_pack_abi_version(0u, 2u, 7u)));
    assert(!bam_version_is_compatible(bam_pack_abi_version(0u, 2u, 0u), bam_pack_abi_version(0u, 1u, 0u)));
    assert(!bam_version_is_compatible(bam_pack_abi_version(0u, 3u, 0u), bam_pack_abi_version(0u, 2u, 0u)));
    assert(bam_version_is_compatible(bam_pack_abi_version(1u, 2u, 0u), bam_pack_abi_version(1u, 1u, 0u)));
    assert(!bam_version_is_compatible(bam_pack_abi_version(1u, 1u, 0u), bam_pack_abi_version(1u, 2u, 0u)));

    assert(strcmp(bam_result_name(BAM_RESULT_OK), "OK") == 0);
    assert(strcmp(bam_result_name(BAM_RESULT_UNSUPPORTED), "UNSUPPORTED") == 0);
    assert(strcmp(bam_result_name(BAM_RESULT_TYPE_MISMATCH), "TYPE_MISMATCH") == 0);
    assert(strcmp(bam_result_name(BAM_RESULT_INTERNAL_ERROR), "INTERNAL_ERROR") == 0);
    assert(strcmp(bam_result_name((BamResult)777u), "UNKNOWN") == 0);

    {
        BamStrView view = bam_str_view("bevy_abi_mod");
        BamStrView empty_view = bam_str_view(NULL);
        assert(view.len == 12u);
        assert(memcmp(view.ptr, "bevy_abi_mod", 12u) == 0);
        assert(empty_view.ptr == NULL && empty_view.len == 0u);
    }
}

static void test_cross_language_siphash_uuids(void) {
    /* Exact 16-byte golden digests produced by `bevy_abi_mod_sys::ComponentUuid::from_parts` */
    static const uint8_t expected_health_uuid[16] = {
        0x45u, 0x1cu, 0x42u, 0x67u, 0x54u, 0xb2u, 0xcdu, 0x60u,
        0xc6u, 0x72u, 0x1cu, 0x1fu, 0xf8u, 0x89u, 0x07u, 0xbdu
    };
    static const uint8_t expected_inventory_uuid[16] = {
        0xe0u, 0x21u, 0x62u, 0xedu, 0x29u, 0xacu, 0xc1u, 0x48u,
        0x32u, 0xa1u, 0x71u, 0x6cu, 0x5bu, 0x7bu, 0x9cu, 0xcbu
    };

    BamComponentUuid c_health = bam_uuid_from_parts("combat_mod::Health", 4u, 4u, 0ull);
    BamComponentUuid c_other_ns = bam_uuid_from_parts("ui_mod::Health", 4u, 4u, 0ull);
    BamComponentUuid c_inventory = bam_uuid_from_canonical_name("my_game::Inventory");

    assert(memcmp(c_health.bytes, expected_health_uuid, 16u) == 0);
    assert(memcmp(c_inventory.bytes, expected_inventory_uuid, 16u) == 0);
    assert(!bam_uuid_equals(c_health, c_other_ns));
    assert(bam_uuid_equals(c_health, c_health));
}

static void test_extensible_host_api_copy_bounded(void) {
    typedef struct FutureGuestHostApiV03 {
        BamHostApi v0_2;
        void*      future_slot_a;
        void*      future_slot_b;
    } FutureGuestHostApiV03;

    BamHostApi host_v02 = build_mock_host_api();
    FutureGuestHostApiV03 guest_v03;
    BamHostApi tiny_bad;

    memset(&guest_v03, 0xAA, sizeof(guest_v03));
    assert(
        bam_host_api_copy_bounded(
            &guest_v03.v0_2,
            sizeof(guest_v03),
            &host_v02
        ) == BAM_RESULT_OK
    );
    assert(guest_v03.v0_2.struct_size == sizeof(BamHostApi));
    assert(guest_v03.v0_2.cmd_remove == host_cmd_remove);
    assert(guest_v03.future_slot_a == NULL);
    assert(guest_v03.future_slot_b == NULL);

    memset(&tiny_bad, 0, sizeof(tiny_bad));
    tiny_bad.struct_size = 8u;
    tiny_bad.abi_version = BAM_ABI_VERSION;
    assert(
        bam_host_api_copy_bounded(&guest_v03.v0_2, sizeof(guest_v03), &tiny_bad)
        == BAM_RESULT_INVALID_VERSION
    );
}

static void test_end_to_end_c99_lifecycle_and_vtable(void) {
    BamManifestFn manifest_fn = mock_guest_manifest;
    BamInitFn init_fn = mock_guest_init;
    BamShutdownFn shutdown_fn = mock_guest_shutdown;

    BamHostApi host_api = build_mock_host_api();
    BamHostCtx host_ctx;
    BamLoadCtx load_ctx;
    BamIter iter0;
    BamCommands cmds;
    BamSystemCtx sys_ctx;
    BamInstance* instance = NULL;
    const BamManifest* manifest;
    const MockPositionVTable* resolved_vt;
    MockPosition sample_pos = { 42.5f, -1.0f };
    BamComponent missing = BAM_COMPONENT_INVALID;
    char err_buf[64];
    size_t err_len;

    /* Populate VTable symbol */
    g_mock_pos_vtable.header.struct_size = (uint32_t)sizeof(MockPositionVTable);
    g_mock_pos_vtable.header.abi_version = BAM_ABI_VERSION;
    g_mock_pos_vtable.header.uuid = bam_uuid_from_parts(
        "game::MockPosition",
        sizeof(MockPosition),
        BAM_ALIGNOF(MockPosition),
        0ull
    );
    g_mock_pos_vtable.header.size = sizeof(MockPosition);
    g_mock_pos_vtable.header.align = BAM_ALIGNOF(MockPosition);
    g_mock_pos_vtable.get_x = mock_pos_get_x;

    memset(&host_ctx, 0, sizeof(host_ctx));
    host_ctx.vtable_symbol = &g_mock_pos_vtable;
    host_ctx.health_handle = 1u;
    host_ctx.pos_handle = 2u;
    host_ctx.dt_res_handle = 3u;

    memset(&load_ctx, 0, sizeof(load_ctx));
    load_ctx.host_ctx = &host_ctx;

    /* 1. Manifest verification */
    manifest = manifest_fn();
    assert(manifest != NULL);
    assert(manifest->struct_size >= BAM_MANIFEST_MIN_V0_2_SIZE);
    assert(bam_version_is_compatible(BAM_ABI_VERSION, manifest->abi_version));
    assert(strcmp(manifest->name, "combat_mod") == 0);
    assert(strcmp(manifest->version, "0.2.0") == 0);
    assert(manifest->flags == 0u && manifest->reserved0 == 0u);

    /* 2. Init & registration */
    assert(init_fn(&host_api, &load_ctx, &instance) == BAM_RESULT_OK);
    assert(instance != NULL);
    assert(load_ctx.registered_components == 1u);
    assert(load_ctx.registered_systems == 1u);
    assert((load_ctx.last_system.flags & BAM_SYSTEM_FLAG_MAIN_THREAD) != 0u);
    assert(load_ctx.last_system.schedule == BAM_SCHEDULE_CUSTOM_BASE + 1u);

    /* 3. VTable discovery & `last_error` diagnostic check */
    resolved_vt = (const MockPositionVTable*)host_api.get_proc_address(
        &host_ctx,
        "bam.v0.game::MockPosition.vtable"
    );
    assert(resolved_vt != NULL);
    assert(resolved_vt->header.struct_size >= BAM_VTABLE_HEADER_MIN_V0_2_SIZE);
    assert(resolved_vt->get_x(&sample_pos) == 42.5f);

    assert(
        host_api.resolve_component(&host_ctx, "game::Unknown", NULL, &missing)
        == BAM_RESULT_NOT_FOUND
    );
    err_len = host_api.last_error(&host_ctx, err_buf, sizeof(err_buf));
    assert(err_len > 0u);
    assert(strcmp(err_buf, "component not registered in mock world") == 0);

    /* 4. Run predicate + system execution */
    memset(&iter0, 0, sizeof(iter0));
    iter0.entities[0] = 101ull;
    iter0.entities[1] = 102ull;
    iter0.health_dense[0] = 20.0f;
    iter0.health_dense[1] = 30.0f;
    iter0.sparse_rows_storage[0].x = 1.0f;
    iter0.sparse_rows_storage[0].y = 2.0f;
    iter0.sparse_rows_storage[1].x = 10.0f;
    iter0.sparse_rows_storage[1].y = 20.0f;
    iter0.sparse_row_ptrs[0] = (uint8_t*)&iter0.sparse_rows_storage[0];
    iter0.sparse_row_ptrs[1] = (uint8_t*)&iter0.sparse_rows_storage[1];
    iter0.dt_resource = 0.5f;
    iter0.health_uuid = bam_uuid_from_parts("combat_mod::Health", sizeof(float), BAM_ALIGNOF(float), 0ull);

    memset(&cmds, 0, sizeof(cmds));
    cmds.next_entity = 1000ull;

    sys_ctx.host_ctx = &host_ctx;
    sys_ctx.iter0 = &iter0;
    sys_ctx.commands = &cmds;

    assert(load_ctx.last_system.run_if(load_ctx.last_system.run_if_user_data) == BAM_TRUE);
    assert(
        load_ctx.last_system.callback(&sys_ctx, load_ctx.last_system.user_data)
        == BAM_RESULT_OK
    );
    assert(instance->ticks_executed == 1u);
    assert(iter0.health_dense[0] == 20.0f);
    assert(iter0.health_dense[1] == 35.0f);
    assert(iter0.sparse_rows_storage[0].x == 1.5f);
    assert(iter0.sparse_rows_storage[1].x == 10.5f);
    assert(iter0.marked_start == 1u && iter0.marked_count == 1u);
    assert(cmds.spawned_count == 1u);
    assert(cmds.batch_component_count == 2u);
    assert(cmds.inserted_count == 1u);

    mock_health_drop(&iter0.health_dense[0]);
    assert(g_drop_calls == 1u);

    /* 5. Per-instance shutdown */
    shutdown_fn(instance);
    assert(g_shutdown_calls == 1u);
}

static BamResult mock_observer_cb(BamEntity target_entity, const void* event_data, BamCommands* commands, void* user_data) {
    (void)target_entity;
    (void)event_data;
    (void)commands;
    (void)user_data;
    return BAM_RESULT_OK;
}

static void test_hierarchy_ticks_layout_and_observer_descriptors(void) {
    BamChildOf child_of;
    BamChangeTicks ticks;
    BamComponentDesc dyn_desc;
    BamComponentUuid expected_dyn_uuid;
    BamObserverDesc obs_desc;
    BamSystemDesc sys_desc;
    const char* sets[1] = { "host::SimulationSet" };
    BamHostApi api;

    /* 1. ChildOf wire struct & UUID */
    child_of.parent = 42ull;
    assert(sizeof(child_of) == 8u);
    assert(child_of.parent == 42ull);
    assert(strcmp(BAM_CHILD_OF_NAME, "bevy_ecs::hierarchy::ChildOf") == 0);
    {
        BamComponentUuid u1 = bam_uuid_from_parts(BAM_CHILD_OF_NAME, 8u, 8u, 0ull);
        BamComponentUuid u2 = bam_uuid_from_parts("bevy_ecs::hierarchy::ChildOf", 8u, 8u, 0ull);
        assert(memcmp(&u1, &u2, sizeof(BamComponentUuid)) == 0);
    }

    /* 2. Change-detection ticks & wraparound */
    assert(bam_ticks_is_added(NULL) == 0);
    assert(bam_ticks_is_changed(NULL) == 0);
    ticks.added = 10u;
    ticks.changed = 12u;
    ticks.last_run = 10u;
    ticks.this_run = 12u;
    assert(bam_ticks_is_added(&ticks) == 0);
    assert(bam_ticks_is_changed(&ticks) == 1);

    /* Wraparound across uint32_t boundary */
    ticks.added = 0xFFFFFFFEu;
    ticks.changed = 2u;
    ticks.last_run = 0xFFFFFFFEu;
    ticks.this_run = 2u;
    assert(bam_ticks_is_added(&ticks) == 0);
    assert(bam_ticks_is_changed(&ticks) == 1);

    /* 3. Dynamic layout registration helper */
    dyn_desc = bam_desc_component_with_layout("c_mod::RuntimeState", 16u, 8u, mock_health_drop);
    expected_dyn_uuid = bam_uuid_from_parts("c_mod::RuntimeState", 16u, 8u, 0ull);
    assert(dyn_desc.struct_size == (uint32_t)sizeof(BamComponentDesc));
    assert(dyn_desc.flags == 0u);
    assert(strcmp(dyn_desc.name, "c_mod::RuntimeState") == 0);
    assert(dyn_desc.size == 16u);
    assert(dyn_desc.align == 8u);
    assert(dyn_desc.drop_fn == mock_health_drop);
    assert(memcmp(&dyn_desc.uuid, &expected_dyn_uuid, sizeof(BamComponentUuid)) == 0);

    /* 4. Observer descriptor helper & 64-bit entity packing */
    obs_desc = bam_observer_desc(77u, 0x11223344AABBCCDDull, mock_observer_cb, (void*)0x1234);
    assert(obs_desc.struct_size == (uint32_t)sizeof(BamObserverDesc));
    assert(obs_desc.flags == 0u);
    assert(obs_desc.event == 77u);
    assert(obs_desc.entity_lo == 0xAABBCCDDu);
    assert(obs_desc.entity_hi == 0x11223344u);
    assert(obs_desc.callback == mock_observer_cb);
    assert(obs_desc.user_data == (void*)0x1234);

    /* 5. Event flags & SystemDesc.in_sets */
    assert(BAM_STORAGE_FLAG_EVENT == 0x10u);
    assert(BAM_STORAGE_FLAG_ENTITY_EVENT == 0x20u);
    memset(&sys_desc, 0, sizeof(sys_desc));
    sys_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys_desc.in_sets = sets;
    sys_desc.in_set_count = 1u;
    assert(sys_desc.in_set_count == 1u);
    assert(strcmp(sys_desc.in_sets[0], "host::SimulationSet") == 0);

    /* 6. HostApi has extended slots */
    api = build_mock_host_api();
    assert(api.struct_size >= (uint32_t)(offsetof(BamHostApi, cmd_trigger) + sizeof(BamCmdTriggerFn)));
    assert(api.msg_write_batch == NULL);
    assert(api.res_ticks == NULL);
    assert(api.sys_res_ptr == NULL);
    assert(api.sys_res_mut_ptr == NULL);
    assert(api.register_observer == NULL);
    assert(api.cmd_trigger == NULL);
}

int main(void) {
    test_versions_and_result_names();
    test_cross_language_siphash_uuids();
    test_extensible_host_api_copy_bounded();
    test_end_to_end_c99_lifecycle_and_vtable();
    test_hierarchy_ticks_layout_and_observer_descriptors();

    printf("All ABI v0.2.0 ISO C99 conformance & layout checks passed.\n");
    return 0;
}
