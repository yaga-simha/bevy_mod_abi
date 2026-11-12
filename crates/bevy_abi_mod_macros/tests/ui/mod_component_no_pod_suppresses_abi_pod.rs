use bevy_abi_mod_macros::ModComponent;

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
#[mod_component(no_pod)]
pub struct ExplicitNoPod {
    pub value: u64,
}

fn main() {
    bevy_abi_mod_sys::assert_abi_pod::<ExplicitNoPod>();
}
