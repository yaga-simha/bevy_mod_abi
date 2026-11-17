#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "bevy_abi_mod.h"

/* Host-registered `w4::StateProbe` POD resource mirrored in C99 (`F-01`, `F-05`). */
typedef struct W4StateProbe {
    uint64_t reload_epoch;
    uint64_t accumulated_ticks;
    uint32_t fail_next_save;
    uint32_t fail_next_restore;
} W4StateProbe;

/* Dynamic POD component `"w4_ext_mod::HookedAura"` with `on_add` / `on_remove` hooks (`F-04`). */
typedef struct HookedAura {
    uint32_t strength;
    uint32_t tag;
} HookedAura;

typedef void (*W4RecordHookFn)(BamEntity entity, uint32_t is_remove, uint32_t strength);

typedef struct W4Instance {
    BamHostApi     api;
    const BamHostCtx* host_ctx;
    uint64_t       reload_epoch;
    uint64_t       accumulated_ticks;
    uint32_t       fail_next_save;
    uint32_t       fail_next_restore;
    BamComponent   probe_res;
    BamComponent   aura_comp;
    W4RecordHookFn record_hook;
} W4Instance;

static void w4_aura_on_add(BamEntity entity, const void* component_ptr, void* user_data) {
    W4Instance* inst = (W4Instance*)user_data;
    const HookedAura* aura = (const HookedAura*)component_ptr;
    if (inst != NULL && inst->record_hook != NULL && aura != NULL) {
        inst->record_hook(entity, 0u, aura->strength);
    }
}

static void w4_aura_on_remove(BamEntity entity, const void* component_ptr, void* user_data) {
    W4Instance* inst = (W4Instance*)user_data;
    const HookedAura* aura = (const HookedAura*)component_ptr;
    if (inst != NULL && inst->record_hook != NULL && aura != NULL) {
        inst->record_hook(entity, 1u, aura->strength);
    }
}

static BamResult w4_tick_cb(BamSystemCtx* sys_ctx, void* user_data) {
    W4Instance* inst = (W4Instance*)user_data;
    BamIter* iter = NULL;
    W4StateProbe* probe;
    BamResult rc;

    if (inst == NULL || sys_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    inst->accumulated_ticks += 1u;
    rc = inst->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK || iter == NULL) {
        return rc;
    }
    probe = (W4StateProbe*)inst->api.res_mut_ptr(iter, 0u);
    if (probe != NULL) {
        inst->fail_next_save = probe->fail_next_save;
        inst->fail_next_restore = probe->fail_next_restore;
        probe->reload_epoch = inst->reload_epoch;
        probe->accumulated_ticks = inst->accumulated_ticks;
    }
    return BAM_RESULT_OK;
}

static BamResult w4_save_state(
    BamInstance* raw_inst,
    BamStateWriterFn writer,
    void* writer_ctx
) {
    W4Instance* inst = (W4Instance*)raw_inst;
    uint8_t buf[24];

    if (inst == NULL || writer == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (inst->fail_next_save != 0u) {
        return BAM_RESULT_INVALID_STATE;
    }
    bam_write_le64(buf, inst->reload_epoch);
    bam_write_le64(buf + 8u, inst->accumulated_ticks);
    bam_write_le64(buf + 16u, (uint64_t)inst->fail_next_restore);
    return writer(writer_ctx, buf, sizeof(buf));
}

static BamResult w4_restore_state(
    BamInstance* raw_inst,
    const uint8_t* bytes,
    size_t len
) {
    W4Instance* inst = (W4Instance*)raw_inst;
    uint64_t fail_flag;

    if (inst == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (len != 24u || bytes == NULL) {
        return BAM_RESULT_INVALID_ARGUMENT;
    }
    fail_flag = bam_read_le64(bytes + 16u);
    if (fail_flag != 0u) {
        return BAM_RESULT_INVALID_STATE;
    }
    inst->reload_epoch = bam_read_le64(bytes) + 1u;
    inst->accumulated_ticks = bam_read_le64(bytes + 8u);
    return BAM_RESULT_OK;
}

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "w4_ext_mod",
    .version = "0.2.0",
    .flags = 0u,
    .reserved0 = 0u,
    .save_state = w4_save_state,
    .restore_state = w4_restore_state
};

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    static const char* const AFTER_PHYSICS[] = { "host::physics" };
    static const char* const BEFORE_RENDER[] = { "host::render" };
    W4Instance* inst;
    BamComponentDesc aura_desc;
    BamTermDesc res_term;
    BamQueryDesc res_query;
    BamSystemDesc sys_desc;
    BamComponent step_res = BAM_COMPONENT_INVALID;
    BamResult rc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    inst = (W4Instance*)calloc(1u, sizeof(W4Instance));
    if (inst == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }

    rc = bam_host_api_copy_bounded(&inst->api, sizeof(inst->api), api);
    if (rc != BAM_RESULT_OK) {
        free(inst);
        return rc;
    }
    inst->host_ctx = inst->api.load_host_ctx(load_ctx);
    if (inst->host_ctx == NULL) {
        free(inst);
        return BAM_RESULT_NULL_POINTER;
    }

    inst->record_hook = (W4RecordHookFn)inst->api.get_proc_address(
        inst->host_ctx,
        "bam.v0.w4.record_hook"
    );

    memset(&aura_desc, 0, sizeof(aura_desc));
    aura_desc.struct_size = (uint32_t)sizeof(BamComponentDesc);
    aura_desc.name = "w4_ext_mod::HookedAura";
    aura_desc.uuid = bam_uuid_from_parts(
        "w4_ext_mod::HookedAura",
        sizeof(HookedAura),
        sizeof(uint32_t),
        0ull
    );
    aura_desc.size = sizeof(HookedAura);
    aura_desc.align = sizeof(uint32_t);
    aura_desc.on_add = w4_aura_on_add;
    aura_desc.on_remove = w4_aura_on_remove;
    aura_desc.hook_user_data = inst;

    rc = inst->api.register_component(load_ctx, &aura_desc, &inst->aura_comp);
    if (rc != BAM_RESULT_OK) {
        free(inst);
        return rc;
    }

    rc = inst->api.resolve_component(inst->host_ctx, "w4::StateProbe", NULL, &inst->probe_res);
    if (rc == BAM_RESULT_OK && bam_component_is_valid(inst->probe_res)) {
        res_term = bam_term_desc(inst->probe_res, BAM_ACCESS_RESOURCE_WRITE, 0u);
        memset(&res_query, 0, sizeof(res_query));
        res_query.struct_size = (uint32_t)sizeof(BamQueryDesc);
        res_query.terms = &res_term;
        res_query.term_count = 1u;

        memset(&sys_desc, 0, sizeof(sys_desc));
        sys_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
        sys_desc.name = "w4_state_tick";
        sys_desc.queries = &res_query;
        sys_desc.query_count = 1u;
        sys_desc.callback = w4_tick_cb;
        sys_desc.user_data = inst;
        sys_desc.schedule = BAM_SCHEDULE_UPDATE;

        if (inst->api.resolve_component(inst->host_ctx, "w4::OrderHostSets", NULL, &step_res) == BAM_RESULT_OK) {
            sys_desc.after = AFTER_PHYSICS;
            sys_desc.after_count = 1u;
            sys_desc.before = BEFORE_RENDER;
            sys_desc.before_count = 1u;
        }

        rc = inst->api.register_system(load_ctx, &sys_desc);
        if (rc != BAM_RESULT_OK) {
            free(inst);
            return rc;
        }
    }

    *instance_out = (BamInstance*)inst;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    free(instance);
}
