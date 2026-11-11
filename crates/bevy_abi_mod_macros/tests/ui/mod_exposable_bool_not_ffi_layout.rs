use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::FfiLayout;

#[repr(C)]
#[derive(Copy, Clone, ModExposable)]
pub struct BoolComp {
    pub flag: bool,
    pub count: u32,
}

fn require_layout<T: FfiLayout>() {}

fn main() {
    require_layout::<BoolComp>();
}
