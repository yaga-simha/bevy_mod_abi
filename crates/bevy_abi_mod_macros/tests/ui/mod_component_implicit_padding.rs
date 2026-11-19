use bevy_abi_mod_macros::ModComponent;

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
struct PaddedModComponent {
    a: u8,
    b: u64,
}

fn main() {}
