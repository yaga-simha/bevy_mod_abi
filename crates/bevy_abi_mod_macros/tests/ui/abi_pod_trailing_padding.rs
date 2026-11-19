use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
struct PaddedTail {
    id: u64,
    flag: u8,
}

fn main() {}
