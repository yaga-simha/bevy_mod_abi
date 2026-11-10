/* ============================================================================
 * mod.c — ISO C99 Dynamic Mod for bevy_abi_mod Example 3
 * ============================================================================ */

#include "bevy_abi_mod.h"
#include <string.h>
#include <stdbool.h>

/* C mirrors of host PositionVTable and VelocityVTable */
typedef struct PositionVTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
} PositionVTable;

typedef struct VelocityVTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
} VelocityVTable;

static const HostApi* g_host_api = NULL;
static const PositionVTable* g_pos_vt = NULL;
static const VelocityVTable* g_vel_vt = NULL;

/* Kinematics system: updates position based on velocity */
static FfiResult c99_kinematics_system(
    RawChunkIter* iter,
    RawCommandQueue* commands,
    void* user_data
) {
    (void)commands;
    (void)user_data;

    if (!g_host_api || !g_pos_vt || !g_vel_vt) {
        return FFI_RESULT_INTERNAL_ERROR;
    }

    while (1) {
        size_t count = g_host_api->iter_next_chunk(iter);
        if (count == 0) {
            break;
        }

        uint8_t* pos_base = g_host_api->iter_column_mutable(iter, 0);
        const uint8_t* vel_base = g_host_api->iter_column_readonly(iter, 1);
        size_t pos_stride = g_host_api->iter_column_stride(iter, 0);
        size_t vel_stride = g_host_api->iter_column_stride(iter, 1);

        if (!pos_base || !vel_base) {
            return FFI_RESULT_NULL_POINTER;
        }

        for (size_t i = 0; i < count; ++i) {
            void* p_ptr = (void*)(pos_base + (i * pos_stride));
            const void* v_ptr = (const void*)(vel_base + (i * vel_stride));

            float px = g_pos_vt->get_x(p_ptr);
            float py = g_pos_vt->get_y(p_ptr);
            float pz = g_pos_vt->get_z(p_ptr);

            float vx = g_vel_vt->get_x(v_ptr);
            float vy = g_vel_vt->get_y(v_ptr);
            float vz = g_vel_vt->get_z(v_ptr);

            // Integrate 3D position by velocity
            g_pos_vt->set_x(p_ptr, px + vx * 0.1f);
            g_pos_vt->set_y(p_ptr, py + vy * 0.1f);
            g_pos_vt->set_z(p_ptr, pz + vz * 0.1f);

            // Flag row mutation for Changed<Position> detection
            g_host_api->iter_mark_changed(iter, 0, i);
        }
    }

    return FFI_RESULT_OK;
}

/* Lifecycle dynamic entry points */

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api || host_api->struct_size < sizeof(HostApi) ||
        host_api->abi_version != BEVY_ABI_VERSION_PACKED) {
        return FFI_RESULT_INVALID_VERSION;
    }
    if (!get_proc_address) {
        return FFI_RESULT_NULL_POINTER;
    }

    g_host_api = host_api;
    g_pos_vt = (const PositionVTable*)get_proc_address("Position_vtable");
    g_vel_vt = (const VelocityVTable*)get_proc_address("Velocity_vtable");

    if (!g_pos_vt || !g_vel_vt) {
        return FFI_RESULT_INVALID_COMPONENT;
    }

    if (!host_context || !host_api->register_system) {
        return FFI_RESULT_OK;
    }

    static ComponentAccessDesc queries[2];
    queries[0].name = "Position";
    queries[0].mode = COMPONENT_ACCESS_WRITE;
    queries[1].name = "Velocity";
    queries[1].mode = COMPONENT_ACCESS_READ;

    SystemDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.struct_size = (uint32_t)sizeof(SystemDescriptor);
    desc.name = "c99_kinematics";
    desc.queries = queries;
    desc.query_count = 2;
    desc.callback = c99_kinematics_system;
    desc.user_data = NULL;

    return host_api->register_system(host_context, &desc);
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    g_host_api = NULL;
    g_pos_vt = NULL;
    g_vel_vt = NULL;
    return FFI_RESULT_OK;
}
