use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, ModExposable)]
pub struct BoolComp {
    pub flag: bool,
    pub count: u32,
}

fn require_pod<T: AbiPod>() {}

fn main() {
    require_pod::<BoolComp>();
}
