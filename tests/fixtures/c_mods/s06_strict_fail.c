#include <string.h>
#include "bevy_abi_mod.h"

static uint64_t g_strict_vt = 0x55443322ULL;
static void strict_drop(void* ptr) { (void)ptr; }
static FfiResult noop_sys(RawChunkIter* it, RawCommandQueue* q, void* ud) {
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
    (void)get_proc_address;
    ModComponentDescriptor cdesc;
    memset(&cdesc, 0, sizeof(cdesc));
    cdesc.struct_size = sizeof(ModComponentDescriptor);
    cdesc.name = "S06StrictTempComp";
    cdesc.uuid = bevy_abi_uuid_from_name("S06StrictTempComp");
    cdesc.size = 8;
    cdesc.align = 8;
    cdesc.drop_fn = strict_drop;
    cdesc.vtable_ptr = (const void*)&g_strict_vt;
    FfiResult r = api->register_component(ctx, &cdesc);
    if (r != FFI_RESULT_OK) { return r; }

    ComponentAccessDesc term = bam_access_desc("TotallyMissingHostComp", COMPONENT_ACCESS_READ);
    SystemDescriptor sdesc;
    memset(&sdesc, 0, sizeof(sdesc));
    sdesc.struct_size = sizeof(SystemDescriptor);
    sdesc.name = "strict_bad_sys";
    sdesc.queries = &term;
    sdesc.query_count = 1;
    sdesc.callback = noop_sys;
    sdesc.user_data = NULL;
    return api->register_system(ctx, &sdesc);
}
