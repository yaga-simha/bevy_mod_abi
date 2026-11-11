use bevy_abi_mod_macros::AbiPod;

#[repr(C, packed)]
#[derive(Copy, Clone, AbiPod)]
pub struct PackedStruct {
    pub tag: u8,
    pub value: u32,
}

fn main() {}
