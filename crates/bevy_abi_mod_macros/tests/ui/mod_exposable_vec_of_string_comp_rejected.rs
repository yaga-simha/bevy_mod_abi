use bevy_abi_mod_macros::ModExposable;

#[repr(C)]
#[derive(ModExposable)]
pub struct NamedComp {
    pub title: String,
}

#[derive(ModExposable)]
pub struct OuterInventory {
    pub bad_items: Vec<NamedComp>,
}

fn main() {}
