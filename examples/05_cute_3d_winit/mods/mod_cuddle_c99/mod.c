/* ============================================================================
 * mod.c — Pure ISO C99 Guest Mod: Cozy Cuddle Huddle (`mod_cuddle_c99`)
 *
 * Demonstrates zero-serialization multi-language C-ABI modding in 3D!
 * Every slime waddles cozily toward the central plush mushroom to form a warm,
 * breathing, blushing cuddle pile, softly bouncing off neighbors so everyone
 * gets a front-row spot.
 * ============================================================================ */

#include "bevy_abi_mod.h"
#include <math.h>
#include <string.h>

typedef struct Vec3VTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
} Vec3VTable;

typedef struct CuteSpiritVTable {
    size_t size;
    size_t align;
    float (*get_yaw)(const void* ptr);
    void  (*set_yaw)(void* ptr, float val);
    float (*get_ear_wiggle)(const void* ptr);
    void  (*set_ear_wiggle)(void* ptr, float val);
    float (*get_blush)(const void* ptr);
    void  (*set_blush)(void* ptr, float val);
} CuteSpiritVTable;

static const HostApi* g_host_api = NULL;
static const Vec3VTable* g_pos_vt = NULL;
static const Vec3VTable* g_vel_vt = NULL;
static const Vec3VTable* g_scale_vt = NULL;
static const CuteSpiritVTable* g_spirit_vt = NULL;
static uint64_t g_tick = 0;

static FfiResult c99_cozy_cuddle_system(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
) {
    (void)commands;
    (void)user_data;

    if (!g_host_api || !g_pos_vt || !g_vel_vt || !g_scale_vt || !g_spirit_vt) {
        return FFI_RESULT_INTERNAL_ERROR;
    }

    g_tick += 1;
    const float t = (float)g_tick * (1.0f / 60.0f);
    size_t global_idx = 0;

    while (1) {
        size_t count = g_host_api->iter_next_chunk(iter);
        if (count == 0) {
            break;
        }

        uint8_t* pos_base    = g_host_api->iter_column_mutable(iter, 0);
        uint8_t* vel_base    = g_host_api->iter_column_mutable(iter, 1);
        uint8_t* scale_base  = g_host_api->iter_column_mutable(iter, 2);
        uint8_t* spirit_base = g_host_api->iter_column_mutable(iter, 3);

        size_t pos_stride    = g_host_api->iter_column_stride(iter, 0);
        size_t vel_stride    = g_host_api->iter_column_stride(iter, 1);
        size_t scale_stride  = g_host_api->iter_column_stride(iter, 2);
        size_t spirit_stride = g_host_api->iter_column_stride(iter, 3);

        if (!pos_base || !vel_base || !scale_base || !spirit_base) {
            return FFI_RESULT_NULL_POINTER;
        }

        for (size_t i = 0; i < count; ++i) {
            size_t idx = global_idx++;

            void* p_ptr  = (void*)(pos_base    + i * pos_stride);
            void* v_ptr  = (void*)(vel_base    + i * vel_stride);
            void* sc_ptr = (void*)(scale_base  + i * scale_stride);
            void* sp_ptr = (void*)(spirit_base + i * spirit_stride);

            float px = g_pos_vt->get_x(p_ptr);
            float py = g_pos_vt->get_y(p_ptr);
            float pz = g_pos_vt->get_z(p_ptr);

            /* Arrange slimes into a snug sunflower spiral huddle around the central mushroom */
            float golden_angle = (float)idx * 2.3999632f + t * 0.18f;
            float ring_r = 2.05f + 0.34f * sqrtf((float)idx + 1.0f);
            float target_x = ring_r * cosf(golden_angle);
            float target_z = ring_r * sinf(golden_angle);

            /* Cute little waddle hop as they scoot into the cuddle pile */
            float dx = target_x - px;
            float dz = target_z - pz;
            float dist = sqrtf(dx * dx + dz * dz);

            float vx = dx * 2.8f;
            float vz = dz * 2.8f;

            float waddle = (dist > 0.25f)
                ? fabsf(sinf(t * 9.0f + (float)idx * 1.3f)) * 0.32f
                : 0.0f;
            float new_py = py + (waddle - py) * 0.22f;

            /* Snuggly synchronized breathing squish when huddled close */
            float breath = sinf(t * 2.6f + (float)idx * 0.35f);
            float sy = 0.86f + 0.09f * breath;
            float sxz = 1.0f / sqrtf(sy);

            /* Look inward toward the cozy centerpiece mushroom + happy droopy ears + max blush */
            float yaw = atan2f(-px, -pz);
            float ear_wiggle = -0.22f + 0.14f * sinf(t * 3.2f + (float)idx);
            float blush = (dist < 1.2f) ? 1.0f : 0.65f;

            g_pos_vt->set_y(p_ptr, new_py);

            g_vel_vt->set_x(v_ptr, vx);
            g_vel_vt->set_y(v_ptr, 0.0f);
            g_vel_vt->set_z(v_ptr, vz);

            g_scale_vt->set_x(sc_ptr, sxz);
            g_scale_vt->set_y(sc_ptr, sy);
            g_scale_vt->set_z(sc_ptr, sxz);

            g_spirit_vt->set_yaw(sp_ptr, yaw);
            g_spirit_vt->set_ear_wiggle(sp_ptr, ear_wiggle);
            g_spirit_vt->set_blush(sp_ptr, blush);

            g_host_api->iter_mark_changed(iter, 0, i);
            g_host_api->iter_mark_changed(iter, 1, i);
            g_host_api->iter_mark_changed(iter, 2, i);
            g_host_api->iter_mark_changed(iter, 3, i);
        }
    }

    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api ||
        host_api->struct_size < (offsetof(HostApi, register_component) + sizeof(RegisterComponentFn)) ||
        ((host_api->abi_version >> 24) & 0xFFu) != BEVY_ABI_VERSION_MAJOR ||
        ((host_api->abi_version >> 16) & 0xFFu) < BEVY_ABI_VERSION_MINOR) {
        return FFI_RESULT_INVALID_VERSION;
    }
    if (!get_proc_address) {
        return FFI_RESULT_NULL_POINTER;
    }

    g_host_api  = host_api;
    g_pos_vt    = (const Vec3VTable*)get_proc_address("Position_vtable");
    g_vel_vt    = (const Vec3VTable*)get_proc_address("Velocity_vtable");
    g_scale_vt  = (const Vec3VTable*)get_proc_address("Scale3D_vtable");
    g_spirit_vt = (const CuteSpiritVTable*)get_proc_address("CuteSpirit_vtable");

    if (!g_pos_vt || !g_vel_vt || !g_scale_vt || !g_spirit_vt) {
        return FFI_RESULT_INVALID_COMPONENT;
    }

    if (!host_context || !host_api->register_system) {
        return FFI_RESULT_OK;
    }

    static ComponentAccessDesc queries[4];
    queries[0].name = "Position";
    queries[0].mode = COMPONENT_ACCESS_WRITE;
    queries[1].name = "Velocity";
    queries[1].mode = COMPONENT_ACCESS_WRITE;
    queries[2].name = "Scale3D";
    queries[2].mode = COMPONENT_ACCESS_WRITE;
    queries[3].name = "CuteSpirit";
    queries[3].mode = COMPONENT_ACCESS_WRITE;

    SystemDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.struct_size = (uint32_t)sizeof(SystemDescriptor);
    desc.name = "C99CozyCuddleSystem";
    desc.queries = queries;
    desc.query_count = 4;
    desc.callback = c99_cozy_cuddle_system;
    desc.user_data = NULL;

    return host_api->register_system(host_context, &desc);
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    g_host_api  = NULL;
    g_pos_vt    = NULL;
    g_vel_vt    = NULL;
    g_scale_vt  = NULL;
    g_spirit_vt = NULL;
    return FFI_RESULT_OK;
}
