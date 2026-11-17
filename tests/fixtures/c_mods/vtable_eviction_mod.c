#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include "bevy_abi_mod.h"

static BamVTableHeader g_dummy_vtable;

static void guest_drop_hook(void* ptr) {
    (void)ptr;
}

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "vtable_eviction_mod",
    .version = "0.2.0",
    .flags = 0u,
    .reserved0 = 0u
};

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    BamHostApi local_api;
    BamComponentDesc desc;
    BamComponent out_comp = BAM_COMPONENT_INVALID;
    BamResult rc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    rc = bam_host_api_copy_bounded(&local_api, sizeof(local_api), api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    g_dummy_vtable.struct_size = (uint32_t)sizeof(BamVTableHeader);
    g_dummy_vtable.abi_version = BAM_ABI_VERSION;
    g_dummy_vtable.uuid = bam_uuid_from_parts("vtable_eviction_mod::GuestVTableComp", 8u, 8u, 0ull);
    g_dummy_vtable.size = 8u;
    g_dummy_vtable.align = 8u;

    memset(&desc, 0, sizeof(desc));
    desc.struct_size = (uint32_t)sizeof(BamComponentDesc);
    desc.flags = 0u;
    desc.name = "GuestVTableComp";
    desc.uuid = g_dummy_vtable.uuid;
    desc.size = 8u;
    desc.align = 8u;
    desc.drop_fn = guest_drop_hook;
    desc.vtable_ptr = (const void*)&g_dummy_vtable;

    return local_api.register_component(load_ctx, &desc, &out_comp);
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    (void)instance;
}
