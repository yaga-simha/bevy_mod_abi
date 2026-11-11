use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(AbiPod)]
pub struct VecInPod {
    pub payload: Vec<u8>,
}

fn main() {}
