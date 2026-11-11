use bevy_abi_mod_macros::AbiPod;

#[derive(Copy, Clone, AbiPod)]
pub struct MissingReprC {
    pub x: f32,
    pub y: f32,
}

fn main() {}
