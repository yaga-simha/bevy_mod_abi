use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::AbiPod;

#[repr(C)]
#[derive(ModExposable)]
pub struct NamedComp {
    pub title: String,
}

fn require_pod<T: AbiPod>() {}

fn main() {
    require_pod::<NamedComp>();
}
