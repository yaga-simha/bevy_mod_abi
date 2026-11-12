use bevy_abi_mod_macros::ModComponent;

#[derive(Clone, ModComponent)]
pub struct NamedComp {
    pub title: String,
}

fn main() {
    bevy_abi_mod_sys::assert_raw_insertable::<NamedComp>();
}
