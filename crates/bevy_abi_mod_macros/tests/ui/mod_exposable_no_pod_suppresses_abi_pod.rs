use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, ModExposable)]
#[mod_component(no_pod)]
pub struct ExplicitNoPod {
    pub value: u64,
}

fn require_pod<T: AbiPod>() {}

fn main() {
    require_pod::<ExplicitNoPod>();
}
