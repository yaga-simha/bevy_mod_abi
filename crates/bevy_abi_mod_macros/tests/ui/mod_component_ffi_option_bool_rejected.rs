use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::FfiOption;

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
pub struct BadFfiOptionBool {
    pub bad: FfiOption<bool>,
}

fn main() {}
