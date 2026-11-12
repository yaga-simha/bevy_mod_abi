#include <string.h>
#include "bevy_abi_mod.h"

typedef struct OrderTraceRes {
    uint32_t steps[4];
    uint32_t len;
} OrderTraceRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult produce_cb(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) { r->steps[r->len++] = 1u; }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    g_res_mut = (IterResourceMutableFn)get_proc_address("iter_resource_mutable");
    ComponentAccessDesc term = bam_access_desc("OrderTraceRes", COMPONENT_ACCESS_RESOURCE_WRITE);

    SystemDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.struct_size = sizeof(SystemDescriptor);
    desc.name = "produce";
    desc.queries = &term;
    desc.query_count = 1;
    desc.callback = produce_cb;
    desc.user_data = NULL;
    return api->register_system(ctx, &desc);
}
