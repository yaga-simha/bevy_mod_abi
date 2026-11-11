use bevy_abi_mod_macros::AbiPod;

#[repr(C)]
pub struct CustomDroppable(pub u64);
impl Drop for CustomDroppable {
    fn drop(&mut self) {}
}

#[repr(C)]
#[derive(AbiPod)]
pub struct OuterWithDroppable {
    pub handle: CustomDroppable,
}

fn main() {}
