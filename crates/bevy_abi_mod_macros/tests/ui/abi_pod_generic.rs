use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
pub struct GenericPod<T> {
    pub value: T,
}

fn main() {}
