#include <string.h>
#include "bevy_abi_mod.h"

typedef struct OrderTraceRes {
    uint32_t steps[4];
    uint32_t len;
} OrderTraceRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult consume_cb(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) { r->steps[r->len++] = 2u; }
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
    (void)api;
    g_res_mut = (IterResourceMutableFn)get_proc_address("iter_resource_mutable");
    RegisterScheduledSystemFn reg =
        (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");

    ComponentAccessDesc term = bam_access_desc("OrderTraceRes", COMPONENT_ACCESS_RESOURCE_WRITE);
    const char* after_dep = "mod_producer::produce";

    ScheduledSystemDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.base.struct_size = sizeof(ScheduledSystemDescriptor);
    desc.base.name = "consume";
    desc.base.queries = &term;
    desc.base.query_count = 1;
    desc.base.callback = consume_cb;
    desc.schedule = SCHEDULE_LABEL_UPDATE;
    desc.after = &after_dep;
    desc.after_count = 1;
    return reg(ctx, &desc);
}
