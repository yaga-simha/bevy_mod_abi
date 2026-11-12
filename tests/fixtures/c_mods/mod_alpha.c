#include <string.h>
#include "bevy_abi_mod.h"

typedef struct OrderTraceRes {
    uint32_t steps[4];
    uint32_t len;
} OrderTraceRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult sys_step(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) { r->steps[r->len++] = 10u; }
    return FFI_RESULT_OK;
}

static FfiResult sys_finish(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) { r->steps[r->len++] = 11u; }
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

    ScheduledSystemDescriptor d_step;
    memset(&d_step, 0, sizeof(d_step));
    d_step.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d_step.base.name = "step";
    d_step.base.queries = &term;
    d_step.base.query_count = 1;
    d_step.base.callback = sys_step;
    d_step.schedule = SCHEDULE_LABEL_UPDATE;
    if (reg(ctx, &d_step) != FFI_RESULT_OK) { return FFI_RESULT_INTERNAL_ERROR; }

    const char* after_dep = "step";
    ScheduledSystemDescriptor d_fin;
    memset(&d_fin, 0, sizeof(d_fin));
    d_fin.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d_fin.base.name = "finish";
    d_fin.base.queries = &term;
    d_fin.base.query_count = 1;
    d_fin.base.callback = sys_finish;
    d_fin.schedule = SCHEDULE_LABEL_UPDATE;
    d_fin.after = &after_dep;
    d_fin.after_count = 1;
    return reg(ctx, &d_fin);
}
