#include "../../include/bevy_abi_mod.h"
#include <string.h>

/* C-side mirror of generated `Vec3VTable` (`#[derive(ModExposable)]` on `Vec3`) */
typedef struct Vec3VTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void (*set_z)(void* ptr, float val);
} Vec3VTable;

/* C-side mirror of generated `CInteropComponentVTable` */
typedef struct CInteropComponentVTable {
    size_t size;
    size_t align;
    int32_t (*get_health)(const void* ptr);
    void (*set_health)(void* ptr, int32_t val);
    const void* (*ptr_pos)(const void* ptr);
    void* (*ptr_mut_pos)(void* ptr);
    FfiByteSlice (*get_label)(const void* ptr);
    void (*set_label)(void* ptr, const uint8_t* utf8_ptr, size_t len);
} CInteropComponentVTable;

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    (void)get_proc_address;
    (void)host_context;
    if (!host_api || host_api->struct_size < sizeof(HostApi)) {
        return FFI_RESULT_INVALID_VERSION;
    }
    if (host_api->abi_version != BEVY_ABI_VERSION_PACKED) {
        return FFI_RESULT_INVALID_VERSION;
    }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    return FFI_RESULT_OK;
}

/*
 * Cross-language VTable verification helper:
 * Resolves `CInteropComponent_vtable` and `Vec3_vtable` via `get_proc_address`,
 * then mutates a contiguous array of `count` Rust `#[repr(Rust)]` components
 * in-place from C99 using `vtable->size` byte strides.
 */
BEVY_ABI_MOD_EXPORT FfiResult bevy_c_mod_test_vtable_mutation(
    GetProcAddressFn get_proc_address,
    void* column_base_ptr,
    size_t count
) {
    if (!get_proc_address || !column_base_ptr) {
        return FFI_RESULT_NULL_POINTER;
    }

    const CInteropComponentVTable* comp_vt =
        (const CInteropComponentVTable*)get_proc_address("CInteropComponent_vtable");
    const Vec3VTable* vec3_vt =
        (const Vec3VTable*)get_proc_address("Vec3_vtable");

    if (!comp_vt || !vec3_vt) {
        return FFI_RESULT_INVALID_COMPONENT;
    }

    uint8_t* base = (uint8_t*)column_base_ptr;
    const char* new_label = "mutated_from_c99";
    const size_t new_label_len = strlen(new_label);

    for (size_t i = 0; i < count; ++i) {
        void* elem = (void*)(base + (i * comp_vt->size));
        int32_t hp = comp_vt->get_health(elem);
        comp_vt->set_health(elem, hp + 1000);

        void* pos_ptr = comp_vt->ptr_mut_pos(elem);
        float old_x = vec3_vt->get_x(pos_ptr);
        float old_y = vec3_vt->get_y(pos_ptr);
        float old_z = vec3_vt->get_z(pos_ptr);
        vec3_vt->set_x(pos_ptr, old_x + 10.5f);
        vec3_vt->set_y(pos_ptr, old_y + 20.5f);
        vec3_vt->set_z(pos_ptr, old_z + 30.5f);

        comp_vt->set_label(elem, (const uint8_t*)new_label, new_label_len);
    }

    return FFI_RESULT_OK;
}
