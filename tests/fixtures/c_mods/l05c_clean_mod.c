#include <string.h>
#include "bevy_abi_mod.h"

static FfiResult noop_cb(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)it; (void)q; (void)ud;
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
    RegisterScheduledSystemFn reg =
        (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");

    ScheduledSystemDescriptor d1;
    memset(&d1, 0, sizeof(d1));
    d1.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d1.base.name = "stage_one";
    d1.base.callback = noop_cb;
    d1.schedule = SCHEDULE_LABEL_UPDATE;
    if (reg(ctx, &d1) != FFI_RESULT_OK) { return FFI_RESULT_INTERNAL_ERROR; }

    const char* dep = "stage_one";
    ScheduledSystemDescriptor d2;
    memset(&d2, 0, sizeof(d2));
    d2.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d2.base.name = "stage_two";
    d2.base.callback = noop_cb;
    d2.schedule = SCHEDULE_LABEL_UPDATE;
    d2.after = &dep;
    d2.after_count = 1;
    return reg(ctx, &d2);
}
