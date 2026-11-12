use bevy_abi_mod_macros::ModComponent;

pub struct Vec<T>(pub T);

#[derive(ModComponent)]
pub struct SpoofedVecHolder {
    pub items: Vec<u32>,
}

fn main() {}
