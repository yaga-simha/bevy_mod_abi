/* ============================================================================
 * mod.c — ISO C99 Dynamic Mod for bevy_abi_mod Example 3 (ABI 0.2.0)
 * ============================================================================ */

#include "../../../include/bevy_abi_mod.h"
#include <stdlib.h>

/* C99 VTable mirrors prefixed at byte offset 0 by BamVTableHeader (M-04, M-05). */
typedef struct PositionVTable {
    BamVTableHeader header;
    float     (*get_x)(const void* ptr);
    BamResult (*set_x)(void* ptr, float val);
    float     (*get_y)(const void* ptr);
    BamResult (*set_y)(void* ptr, float val);
    float     (*get_z)(const void* ptr);
    BamResult (*set_z)(void* ptr, float val);
} PositionVTable;

typedef struct VelocityVTable {
    BamVTableHeader header;
    float     (*get_x)(const void* ptr);
    BamResult (*set_x)(void* ptr, float val);
    float     (*get_y)(const void* ptr);
    BamResult (*set_y)(void* ptr, float val);
    float     (*get_z)(const void* ptr);
    BamResult (*set_z)(void* ptr, float val);
} VelocityVTable;

/* Per-World instance state owned by the guest heap; zero process globals (L-02). */
typedef struct ModState {
    BamHostApi            api;
    BamComponent          pos_comp;
    BamComponent          vel_comp;
    const PositionVTable* pos_vt;
    const VelocityVTable* vel_vt;
} ModState;

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "example_03_c_mod",
    .version = "0.2.0",
    .flags = 0u,
    .reserved0 = 0u
};

static int validate_pos_vtable(const PositionVTable* vt) {
    if (vt == NULL) {
        return 0;
    }
    if ((size_t)vt->header.struct_size < sizeof(PositionVTable)) {
        return 0;
    }
    if (!bam_version_is_compatible(vt->header.abi_version, BAM_ABI_VERSION)) {
        return 0;
    }
    return (vt->get_x != NULL) && (vt->set_x != NULL) &&
           (vt->get_y != NULL) && (vt->set_y != NULL) &&
           (vt->get_z != NULL) && (vt->set_z != NULL);
}

static int validate_vel_vtable(const VelocityVTable* vt) {
    if (vt == NULL) {
        return 0;
    }
    if ((size_t)vt->header.struct_size < sizeof(VelocityVTable)) {
        return 0;
    }
    if (!bam_version_is_compatible(vt->header.abi_version, BAM_ABI_VERSION)) {
        return 0;
    }
    return (vt->get_x != NULL) && (vt->set_x != NULL) &&
           (vt->get_y != NULL) && (vt->set_y != NULL) &&
           (vt->get_z != NULL) && (vt->set_z != NULL);
}

/* Kinematics system: integrates Position by Velocity * 0.1 each tick. */
static BamResult c99_kinematics_system(BamSystemCtx* sys_ctx, void* user_data) {
    ModState* st = (ModState*)user_data;
    BamIter* iter = NULL;
    BamColumnInfo pos_info;
    BamColumnInfo vel_info;
    size_t count = 0u;
    BamResult res;

    if (st == NULL || sys_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }

    res = st->api.sys_query(sys_ctx, 0u, &iter);
    if (res != BAM_RESULT_OK) {
        return res;
    }

    res = st->api.iter_column_info(iter, 0u, &pos_info);
    if (res != BAM_RESULT_OK) {
        return res;
    }
    res = st->api.iter_column_info(iter, 1u, &vel_info);
    if (res != BAM_RESULT_OK) {
        return res;
    }

    if (pos_info.size != st->pos_vt->header.size ||
        vel_info.size != st->vel_vt->header.size ||
        !bam_uuid_equals(pos_info.uuid, st->pos_vt->header.uuid) ||
        !bam_uuid_equals(vel_info.uuid, st->vel_vt->header.uuid)) {
        return BAM_RESULT_TYPE_MISMATCH;
    }

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        uint8_t* pos_base = st->api.iter_column_mutable(iter, 0u);
        const uint8_t* vel_base = st->api.iter_column_readonly(iter, 1u);
        size_t i;

        if (pos_base == NULL || vel_base == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }

        for (i = 0u; i < count; ++i) {
            void* p_ptr = (void*)(pos_base + (i * pos_info.size));
            const void* v_ptr = (const void*)(vel_base + (i * vel_info.size));

            float px = st->pos_vt->get_x(p_ptr);
            float py = st->pos_vt->get_y(p_ptr);
            float pz = st->pos_vt->get_z(p_ptr);

            float vx = st->vel_vt->get_x(v_ptr);
            float vy = st->vel_vt->get_y(v_ptr);
            float vz = st->vel_vt->get_z(v_ptr);

            res = st->pos_vt->set_x(p_ptr, px + vx * 0.1f);
            if (res != BAM_RESULT_OK) {
                return res;
            }
            res = st->pos_vt->set_y(p_ptr, py + vy * 0.1f);
            if (res != BAM_RESULT_OK) {
                return res;
            }
            res = st->pos_vt->set_z(p_ptr, pz + vz * 0.1f);
            if (res != BAM_RESULT_OK) {
                return res;
            }
        }
    }

    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx*       load_ctx,
    BamInstance**     instance_out
) {
    BamHostApi local_api;
    const BamHostCtx* host_ctx;
    ModState* st;
    BamTermDesc terms[2];
    BamQueryDesc query;
    BamSystemDesc sys_desc;
    BamResult res;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    res = bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api);
    if (res != BAM_RESULT_OK) {
        return res;
    }

    if (local_api.load_host_ctx == NULL ||
        local_api.get_proc_address == NULL ||
        local_api.resolve_component == NULL ||
        local_api.register_system == NULL ||
        local_api.sys_query == NULL ||
        local_api.iter_next_chunk == NULL ||
        local_api.iter_column_mutable == NULL ||
        local_api.iter_column_readonly == NULL ||
        local_api.iter_column_info == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }

    st = (ModState*)calloc(1u, sizeof(ModState));
    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    st->api = local_api;

    host_ctx = st->api.load_host_ctx(load_ctx);
    if (host_ctx == NULL) {
        free(st);
        return BAM_RESULT_NULL_POINTER;
    }

    res = st->api.resolve_component(host_ctx, "example::Position", NULL, &st->pos_comp);
    if (res != BAM_RESULT_OK) {
        free(st);
        return res;
    }
    res = st->api.resolve_component(host_ctx, "example::Velocity", NULL, &st->vel_comp);
    if (res != BAM_RESULT_OK) {
        free(st);
        return res;
    }

    st->pos_vt = (const PositionVTable*)st->api.get_proc_address(
        host_ctx,
        "bam.v0.example::Position.vtable"
    );
    st->vel_vt = (const VelocityVTable*)st->api.get_proc_address(
        host_ctx,
        "bam.v0.example::Velocity.vtable"
    );
    if (!validate_pos_vtable(st->pos_vt) || !validate_vel_vtable(st->vel_vt)) {
        free(st);
        return BAM_RESULT_INVALID_COMPONENT;
    }

    terms[0] = bam_term_desc(st->pos_comp, BAM_ACCESS_WRITE, 0u);
    terms[1] = bam_term_desc(st->vel_comp, BAM_ACCESS_READ, 0u);

    memset(&query, 0, sizeof(query));
    query.struct_size = (uint32_t)sizeof(BamQueryDesc);
    query.terms       = terms;
    query.term_count  = 2u;

    memset(&sys_desc, 0, sizeof(sys_desc));
    sys_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys_desc.name        = "c99_kinematics";
    sys_desc.queries     = &query;
    sys_desc.query_count = 1u;
    sys_desc.callback    = c99_kinematics_system;
    sys_desc.user_data   = st;
    sys_desc.schedule    = BAM_SCHEDULE_UPDATE;

    res = st->api.register_system(load_ctx, &sys_desc);
    if (res != BAM_RESULT_OK) {
        free(st);
        return res;
    }

    *instance_out = (BamInstance*)st;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    if (instance != NULL) {
        free(instance);
    }
}
