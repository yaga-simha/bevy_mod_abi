#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include "bevy_abi_mod.h"

BEVY_ABI_MOD_EXPORT void c_uuid_from_name(const char* name, uint8_t* out_bytes) {
    ComponentUuid id = bevy_abi_uuid_from_name(name);
    memcpy(out_bytes, id.bytes, 16);
}
