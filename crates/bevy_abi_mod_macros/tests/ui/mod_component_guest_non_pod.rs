use bevy_abi_mod_macros::ModComponent;

#[repr(C)]
#[derive(ModComponent)]
#[mod_component(name = "bad::NonPodGuest", guest)]
pub struct NonPodGuest {
    pub label: String,
}

fn main() {}
