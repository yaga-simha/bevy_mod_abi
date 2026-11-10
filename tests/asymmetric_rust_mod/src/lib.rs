use core::ffi::c_void;
use guest_sdk::{export_mod, FfiResult, GetProcAddressFn, HostApi};

fn mod_init(
    _get_proc_address: GetProcAddressFn,
    _host_api: *const HostApi,
    _host_context: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
