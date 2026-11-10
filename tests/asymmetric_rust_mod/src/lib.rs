use core::ffi::c_void;
use guest_sdk::{
    export_mod, GetProcAddressFn, HostApi, ModStatus,
};

fn mod_init(
    _get_proc_address: GetProcAddressFn,
    _host_api: *const HostApi,
    _host_context: *mut c_void,
) -> ModStatus {
    ModStatus::Success
}

fn mod_shutdown(_host_context: *mut c_void) -> ModStatus {
    ModStatus::Success
}

export_mod!(mod_init, mod_shutdown);
