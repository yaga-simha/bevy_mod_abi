use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(AbiPod)]
pub struct StringInPod {
    pub label: String,
}

fn main() {}
