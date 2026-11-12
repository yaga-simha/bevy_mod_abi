use bevy_abi_mod_macros::ModComponent;

#[derive(Clone, ModComponent)]
pub struct InnerLabel {
    pub text: String,
}

#[repr(C)]
#[derive(Clone, ModComponent)]
pub struct NonOpaqueOuter {
    pub label: InnerLabel,
}

fn main() {}
