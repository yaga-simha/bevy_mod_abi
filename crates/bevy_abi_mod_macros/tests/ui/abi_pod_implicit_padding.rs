use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
struct PaddedMiddle {
    tag: u8,
    value: u32,
}

fn main() {}
