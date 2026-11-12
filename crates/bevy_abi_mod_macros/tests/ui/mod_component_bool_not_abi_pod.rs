use bevy_abi_mod_macros::ModComponent;

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
pub struct BoolComp {
    pub flag: bool,
    pub count: u32,
}

fn main() {
    bevy_abi_mod_sys::assert_abi_pod::<BoolComp>();
}
