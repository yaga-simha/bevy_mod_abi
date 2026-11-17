#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include "bevy_abi_mod.h"

static const BamManifest MANIFEST = {
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "l05c_clean_mod",
    "0.2.0",
    0u,
    0u
};

static BamResult noop_cb(BamSystemCtx* sys_ctx, void* user_data) {
    (void)sys_ctx;
    (void)user_data;
    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    static const char* const AFTER_STAGE_ONE[] = { "stage_one" };
    BamHostApi local_api;
    BamSystemDesc d1;
    BamSystemDesc d2;
    BamResult rc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    rc = bam_host_api_copy_bounded(&local_api, sizeof(local_api), api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    memset(&d1, 0, sizeof(d1));
    d1.struct_size = (uint32_t)sizeof(BamSystemDesc);
    d1.name = "stage_one";
    d1.callback = noop_cb;
    d1.schedule = BAM_SCHEDULE_UPDATE;
    rc = local_api.register_system(load_ctx, &d1);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    memset(&d2, 0, sizeof(d2));
    d2.struct_size = (uint32_t)sizeof(BamSystemDesc);
    d2.name = "stage_two";
    d2.callback = noop_cb;
    d2.schedule = BAM_SCHEDULE_UPDATE;
    d2.after = AFTER_STAGE_ONE;
    d2.after_count = 1u;
    return local_api.register_system(load_ctx, &d2);
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    (void)instance;
}
