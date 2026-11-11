use bevy_abi_mod_macros::ModExposable;

#[repr(C)]
#[derive(Copy, Clone, ModExposable)]
pub struct BoolComp {
    pub flag: bool,
    pub count: u32,
}

#[derive(ModExposable)]
pub struct OuterFlags {
    pub flags: Vec<BoolComp>,
}

fn main() {}
