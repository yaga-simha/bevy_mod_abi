#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include "bevy_abi_mod.h"

static BamVTableHeader g_strict_vt;

static void strict_drop(void* ptr) {
    (void)ptr;
}

static BamResult noop_sys(BamSystemCtx* sys_ctx, void* user_data) {
    (void)sys_ctx;
    (void)user_data;
    return BAM_RESULT_OK;
}

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "s06_strict_fail",
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
    static const char* const MISSING_AFTER[] = { "nonexistent_mod::missing_stage" };
    BamHostApi local_api;
    BamComponentDesc cdesc;
    BamComponent out_comp = BAM_COMPONENT_INVALID;
    BamSystemDesc sdesc;
    BamResult rc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    rc = bam_host_api_copy_bounded(&local_api, sizeof(local_api), api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    g_strict_vt.struct_size = (uint32_t)sizeof(BamVTableHeader);
    g_strict_vt.abi_version = BAM_ABI_VERSION;
    g_strict_vt.uuid = bam_uuid_from_parts("s06_strict_fail::S06StrictTempComp", 8u, 8u, 0ull);
    g_strict_vt.size = 8u;
    g_strict_vt.align = 8u;

    memset(&cdesc, 0, sizeof(cdesc));
    cdesc.struct_size = (uint32_t)sizeof(BamComponentDesc);
    cdesc.flags = 0u;
    cdesc.name = "S06StrictTempComp";
    cdesc.uuid = g_strict_vt.uuid;
    cdesc.size = 8u;
    cdesc.align = 8u;
    cdesc.drop_fn = strict_drop;
    cdesc.vtable_ptr = (const void*)&g_strict_vt;

    rc = local_api.register_component(load_ctx, &cdesc, &out_comp);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    /*
     * Register a system whose `after` target is syntactically valid (`init` succeeds),
     * so `LoadOptions::new().with_strict(true)` rejects it post-`init` with
     * `ModLoadError::UnresolvedOrderingTarget` and exercises post-init rollback (`S-06`).
     */
    memset(&sdesc, 0, sizeof(sdesc));
    sdesc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sdesc.flags = 0u;
    sdesc.name = "strict_bad_sys";
    sdesc.queries = NULL;
    sdesc.query_count = 0u;
    sdesc.callback = noop_sys;
    sdesc.user_data = NULL;
    sdesc.schedule = BAM_SCHEDULE_UPDATE;
    sdesc.reserved0 = 0u;
    sdesc.after = MISSING_AFTER;
    sdesc.after_count = 1u;

    return local_api.register_system(load_ctx, &sdesc);
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    (void)instance;
}
