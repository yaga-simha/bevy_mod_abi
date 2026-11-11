use bevy_abi_mod_macros::ModExposable;

#[derive(ModExposable)]
pub struct InnerLabel {
    pub text: String,
}

#[derive(ModExposable)]
pub struct NonOpaqueOuter {
    pub label: InnerLabel,
}

fn main() {}
