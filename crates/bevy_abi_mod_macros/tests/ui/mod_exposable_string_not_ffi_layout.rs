use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::FfiLayout;

#[repr(C)]
#[derive(ModExposable)]
pub struct NamedComp {
    pub title: String,
}

fn require_layout<T: FfiLayout>() {}

fn main() {
    require_layout::<NamedComp>();
}
