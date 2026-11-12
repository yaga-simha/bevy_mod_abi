#include <string.h>
#include "bevy_abi_mod.h"

static uint64_t g_dummy_vtable = 0x1122334455667788ULL;
static void guest_drop_hook(void* ptr) { (void)ptr; }

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
    desc.name = "S05GuestComp";
    desc.uuid = bevy_abi_uuid_from_name("S05GuestComp");
    desc.size = 8;
    desc.align = 8;
    desc.drop_fn = guest_drop_hook;
    desc.vtable_ptr = (const void*)&g_dummy_vtable;
    return api->register_component(ctx, &desc);
}
