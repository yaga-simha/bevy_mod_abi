use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
pub struct BoolInPod {
    pub active: bool,
    pub _pad: [u8; 3],
    pub score: u32,
}

fn main() {}
