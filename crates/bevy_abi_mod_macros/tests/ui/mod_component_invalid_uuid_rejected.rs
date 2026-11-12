use bevy_abi_mod_macros::ModComponent;

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
#[mod_component(uuid = "not-a-valid-hex-uuid")]
pub struct BadUuidComp {
    pub x: f32,
}

fn main() {}
