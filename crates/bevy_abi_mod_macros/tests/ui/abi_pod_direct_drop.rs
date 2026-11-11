use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
#[derive(AbiPod)]
pub struct DirectDropPod {
    pub fd: i32,
}

impl Drop for DirectDropPod {
    fn drop(&mut self) {}
}

fn main() {}
