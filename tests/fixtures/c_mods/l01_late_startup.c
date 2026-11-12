#include <string.h>
#include "bevy_abi_mod.h"

typedef struct CounterRes {
    uint32_t startup_hits;
    uint32_t update_hits;
} CounterRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult on_startup(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    CounterRes* c = (CounterRes*)g_res_mut(it, "CounterRes");
    if (c != NULL) { c->startup_hits += 1; }
    return FFI_RESULT_OK;
}

static FfiResult on_update(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    CounterRes* c = (CounterRes*)g_res_mut(it, "CounterRes");
    if (c != NULL) { c->update_hits += 1; }
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
    RegisterScheduledSystemFn reg_sched =
        (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");
    if (g_res_mut == NULL || reg_sched == NULL) { return FFI_RESULT_NOT_FOUND; }

    ComponentAccessDesc term = bam_access_desc("CounterRes", COMPONENT_ACCESS_RESOURCE_WRITE);

    ScheduledSystemDescriptor d1;
    memset(&d1, 0, sizeof(d1));
    d1.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d1.base.name = "late_startup_sys";
    d1.base.queries = &term;
    d1.base.query_count = 1;
    d1.base.callback = on_startup;
    d1.schedule = SCHEDULE_LABEL_STARTUP;
    FfiResult r1 = reg_sched(ctx, &d1);
    if (r1 != FFI_RESULT_OK) { return r1; }

    ScheduledSystemDescriptor d2;
    memset(&d2, 0, sizeof(d2));
    d2.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d2.base.name = "regular_update_sys";
    d2.base.queries = &term;
    d2.base.query_count = 1;
    d2.base.callback = on_update;
    d2.schedule = SCHEDULE_LABEL_UPDATE;
    return reg_sched(ctx, &d2);
}
