use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
pub struct U128InPod {
    pub high_precision: u128,
}

fn main() {}
