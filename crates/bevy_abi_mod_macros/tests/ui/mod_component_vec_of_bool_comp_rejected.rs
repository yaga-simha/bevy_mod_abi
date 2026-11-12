use bevy_abi_mod_macros::ModComponent;

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
pub struct BoolComp {
    pub flag: bool,
}

#[derive(Clone, ModComponent)]
pub struct OuterFlags {
    pub flags: Vec<BoolComp>,
}

fn main() {}
