use bevy_abi_mod_macros::GuestComponent;

#[repr(u8)]
#[derive(Copy, Clone, GuestComponent)]
pub enum NotAStruct {
    A = 0,
    B = 1,
}

fn main() {}
