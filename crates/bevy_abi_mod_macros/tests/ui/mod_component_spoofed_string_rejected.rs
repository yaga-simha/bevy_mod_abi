use bevy_abi_mod_macros::ModComponent;

pub struct String {
    pub not_a_std_string: u32,
}

#[derive(ModComponent)]
pub struct SpoofedStringHolder {
    pub label: String,
}

fn main() {}
