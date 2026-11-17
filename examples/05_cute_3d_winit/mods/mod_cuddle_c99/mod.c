/* ============================================================================
 * mod.c — Pure ISO C99 Guest Mod: Cozy Cuddle Huddle (`mod_cuddle_c99`, ABI 0.2.0)
 *
 * Demonstrates multi-language C-ABI modding in 3D with per-World `BamInstance`
 * heap state (`L-02`, `L-08`), `BamVTableHeader` (`M-05`), and `BamResult` setters (`M-04`).
 * ============================================================================ */

#include "bevy_abi_mod.h"
#include <math.h>
#include <stdlib.h>
#include <string.h>

typedef struct Vec3VTable {
    BamVTableHeader header;
    float     (*get_x)(const void* ptr);
    BamResult (*set_x)(void* ptr, float val);
    float     (*get_y)(const void* ptr);
    BamResult (*set_y)(void* ptr, float val);
    float     (*get_z)(const void* ptr);
    BamResult (*set_z)(void* ptr, float val);
} Vec3VTable;

typedef struct CuteSpiritVTable {
    BamVTableHeader header;
    float     (*get_yaw)(const void* ptr);
    BamResult (*set_yaw)(void* ptr, float val);
    float     (*get_ear_wiggle)(const void* ptr);
    BamResult (*set_ear_wiggle)(void* ptr, float val);
    float     (*get_blush)(const void* ptr);
    BamResult (*set_blush)(void* ptr, float val);
} CuteSpiritVTable;

typedef struct ModState {
    BamHostApi              api;
    const Vec3VTable*       pos_vt;
    const Vec3VTable*       vel_vt;
    const Vec3VTable*       scale_vt;
    const CuteSpiritVTable* spirit_vt;
    uint64_t                tick;
} ModState;

static const BamManifest MANIFEST = {
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "mod_cuddle_c99",
    "0.2.0",
    0u,
    0u
};

static BamResult c99_cozy_cuddle_system(BamSystemCtx* sys_ctx, void* user_data) {
    ModState* st = (ModState*)user_data;
    BamIter* iter = NULL;
    size_t count = 0u;
    size_t global_idx = 0u;
    float t;
    BamResult res;

    if (st == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }

    res = st->api.sys_query(sys_ctx, 0u, &iter);
    if (res != BAM_RESULT_OK) {
        return res;
    }

    st->tick += 1u;
    t = (float)st->tick * (1.0f / 60.0f);

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE && count > 0u) {
        uint8_t* pos_base    = st->api.iter_column_mutable(iter, 0u);
        uint8_t* vel_base    = st->api.iter_column_mutable(iter, 1u);
        uint8_t* scale_base  = st->api.iter_column_mutable(iter, 2u);
        uint8_t* spirit_base = st->api.iter_column_mutable(iter, 3u);
        size_t pos_stride    = st->pos_vt->header.size;
        size_t vel_stride    = st->vel_vt->header.size;
        size_t scale_stride  = st->scale_vt->header.size;
        size_t spirit_stride = st->spirit_vt->header.size;
        size_t i;

        if (!pos_base || !vel_base || !scale_base || !spirit_base) {
            return BAM_RESULT_NULL_POINTER;
        }

        for (i = 0u; i < count; ++i) {
            size_t idx = global_idx++;
            void* p_ptr  = (void*)(pos_base    + i * pos_stride);
            void* v_ptr  = (void*)(vel_base    + i * vel_stride);
            void* sc_ptr = (void*)(scale_base  + i * scale_stride);
            void* sp_ptr = (void*)(spirit_base + i * spirit_stride);

            float px = st->pos_vt->get_x(p_ptr);
            float py = st->pos_vt->get_y(p_ptr);
            float pz = st->pos_vt->get_z(p_ptr);

            /* Arrange slimes into a sunflower spiral huddle around the central mushroom */
            float golden_angle = (float)idx * 2.3999632f + t * 0.18f;
            float ring_r = 2.05f + 0.34f * sqrtf((float)idx + 1.0f);
            float target_x = ring_r * cosf(golden_angle);
            float target_z = ring_r * sinf(golden_angle);

            float dx = target_x - px;
            float dz = target_z - pz;
            float dist = sqrtf(dx * dx + dz * dz);

            float vx = dx * 2.8f;
            float vz = dz * 2.8f;

            float waddle = (dist > 0.25f)
                ? fabsf(sinf(t * 9.0f + (float)idx * 1.3f)) * 0.32f
                : 0.0f;
            float new_py = py + (waddle - py) * 0.22f;

            float breath = sinf(t * 2.6f + (float)idx * 0.35f);
            float sy = 0.86f + 0.09f * breath;
            float sxz = 1.0f / sqrtf(sy);

            float yaw = atan2f(-px, -pz);
            float ear_wiggle = -0.22f + 0.14f * sinf(t * 3.2f + (float)idx);
            float blush = (dist < 1.2f) ? 1.0f : 0.65f;

            (void)st->pos_vt->set_y(p_ptr, new_py);

            (void)st->vel_vt->set_x(v_ptr, vx);
            (void)st->vel_vt->set_y(v_ptr, 0.0f);
            (void)st->vel_vt->set_z(v_ptr, vz);

            (void)st->scale_vt->set_x(sc_ptr, sxz);
            (void)st->scale_vt->set_y(sc_ptr, sy);
            (void)st->scale_vt->set_z(sc_ptr, sxz);

            (void)st->spirit_vt->set_yaw(sp_ptr, yaw);
            (void)st->spirit_vt->set_ear_wiggle(sp_ptr, ear_wiggle);
            (void)st->spirit_vt->set_blush(sp_ptr, blush);
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
    BamComponent pos_h, vel_h, scale_h, spirit_h;
    BamTermDesc terms[4];
    BamQueryDesc query_desc;
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

    st->pos_vt    = (const Vec3VTable*)st->api.get_proc_address(host_ctx, "bam.v0.cute3d::Position.vtable");
    st->vel_vt    = (const Vec3VTable*)st->api.get_proc_address(host_ctx, "bam.v0.cute3d::Velocity.vtable");
    st->scale_vt  = (const Vec3VTable*)st->api.get_proc_address(host_ctx, "bam.v0.cute3d::Scale3D.vtable");
    st->spirit_vt = (const CuteSpiritVTable*)st->api.get_proc_address(host_ctx, "bam.v0.cute3d::CuteSpirit.vtable");

    if (!st->pos_vt || !st->vel_vt || !st->scale_vt || !st->spirit_vt) {
        free(st);
        return BAM_RESULT_INVALID_COMPONENT;
    }

    if (st->api.resolve_component(host_ctx, "cute3d::Position", NULL, &pos_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "cute3d::Velocity", NULL, &vel_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "cute3d::Scale3D", NULL, &scale_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "cute3d::CuteSpirit", NULL, &spirit_h) != BAM_RESULT_OK) {
        free(st);
        return BAM_RESULT_INVALID_COMPONENT;
    }

    terms[0] = bam_term_desc(pos_h,    BAM_ACCESS_WRITE, 0u);
    terms[1] = bam_term_desc(vel_h,    BAM_ACCESS_WRITE, 0u);
    terms[2] = bam_term_desc(scale_h,  BAM_ACCESS_WRITE, 0u);
    terms[3] = bam_term_desc(spirit_h, BAM_ACCESS_WRITE, 0u);

    memset(&query_desc, 0, sizeof(query_desc));
    query_desc.struct_size = (uint32_t)sizeof(BamQueryDesc);
    query_desc.terms       = terms;
    query_desc.term_count  = 4u;

    memset(&sys_desc, 0, sizeof(sys_desc));
    sys_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys_desc.name        = "C99CozyCuddleSystem";
    sys_desc.queries     = &query_desc;
    sys_desc.query_count = 1u;
    sys_desc.callback    = c99_cozy_cuddle_system;
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
