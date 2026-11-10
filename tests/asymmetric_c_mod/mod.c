#include <stdint.h>
#include <stddef.h>

#define MOD_SUCCESS 0
#define MOD_ERR_INTERNAL -99

typedef int32_t ModStatus;
typedef const void* (*GetProcAddressFn)(const char* proc_name);

typedef struct {
    ModStatus (*register_system)(
        void* host_context,
        const void* descriptor,
        void (*callback)(void* user_data, void* chunk),
        void* user_data
    );
} HostApi;

// Exported C Mod Lifecycle Entry Points
#if defined(_WIN32)
    #define EXPORT __declspec(dllexport)
#else
    #define EXPORT __attribute__((visibility("default")))
#endif

EXPORT ModStatus bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api) {
        return MOD_ERR_INTERNAL;
    }
    return MOD_SUCCESS;
}

EXPORT ModStatus bevy_abi_mod_shutdown(void* host_context) {
    return MOD_SUCCESS;
}
