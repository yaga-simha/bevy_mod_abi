#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include "bevy_abi_mod.h"

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "c_uuid_oracle",
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
    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;
    return bam_host_api_copy_bounded(&local_api, sizeof(local_api), api);
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    (void)instance;
}

BAM_EXPORT void c_uuid_from_parts(
    const char* canonical_name,
    size_t size,
    size_t align,
    uint64_t field_schema_hash,
    uint8_t* out_bytes
) {
    BamComponentUuid id = bam_uuid_from_parts(canonical_name, size, align, field_schema_hash);
    memcpy(out_bytes, id.bytes, 16u);
}

BAM_EXPORT void c_uuid_from_canonical_name(const char* canonical_name, uint8_t* out_bytes) {
    BamComponentUuid id = bam_uuid_from_canonical_name(canonical_name);
    memcpy(out_bytes, id.bytes, 16u);
}
