#include <string.h>
#include "bevy_abi_mod.h"

static uint64_t g_bad_vtable = 0x99887766ULL;
static void bad_drop_hook(void* ptr) { (void)ptr; }

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    (void)get_proc_address;
    ModComponentDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.struct_size = sizeof(ModComponentDescriptor);
    desc.name = "S06RollbackComp";
    desc.uuid = bevy_abi_uuid_from_name("S06RollbackComp");
    desc.size = 8;
    desc.align = 8;
    desc.drop_fn = bad_drop_hook;
    desc.vtable_ptr = (const void*)&g_bad_vtable;
    FfiResult r = api->register_component(ctx, &desc);
    if (r != FFI_RESULT_OK) { return r; }
    return FFI_RESULT_INVALID_ARGUMENT;
}
