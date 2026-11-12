use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
pub struct UsizeInPod {
    pub len: usize,
}

fn main() {}
