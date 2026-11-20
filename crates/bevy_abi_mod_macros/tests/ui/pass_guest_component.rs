use bevy_abi_mod_macros::{AbiPod, GuestComponent, ModComponent};
use bevy_abi_mod_sys::{
    AbiPod as AbiPodTrait, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_COMPONENT_FLAG_SUMMARY_TICK, ModComponent as ModComponentTrait,
};

mod mock_guest {
    use bevy_abi_mod_sys::{AbiPod, ComponentUuid};
    use core::ffi::CStr;

    pub trait GuestComponent: AbiPod {
        const NAME: &'static CStr;
        const FIELD_SCHEMA_HASH: u64 = 0;
        const FLAGS: u32 = 0;
        const UUID: ComponentUuid = match Self::NAME.to_str() {
            Ok(name_str) => ComponentUuid::from_parts(
                name_str,
                core::mem::size_of::<Self>(),
                core::mem::align_of::<Self>(),
                Self::FIELD_SCHEMA_HASH,
            ),
            Err(_) => panic!("GuestComponent::NAME must be valid UTF-8"),
        };
    }
}

use mock_guest::GuestComponent as GuestComponentTrait;

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
#[mod_component(name = "shared::Position", summary_tick)]
pub struct HostPosition {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Copy, Clone, AbiPod, GuestComponent)]
#[guest_component(name = "shared::Position", summary_tick, guest_crate = mock_guest)]
pub struct GuestPosition {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Copy, Clone, ModComponent, GuestComponent)]
#[mod_component(name = "shared::DualDerived", immutable, guest_crate = mock_guest)]
pub struct DualDerived {
    pub id: u64,
    pub hp: f32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
#[mod_component(name = "shared::ShorthandGuest", sparse_set, guest, guest_crate = mock_guest)]
pub struct ShorthandGuest {
    pub score: u32,
}

const _: () = {
    // Bit-for-bit UUID parity between host `#[derive(ModComponent)]` and guest `#[derive(AbiPod, GuestComponent)]`.
    assert!(
        <HostPosition as ModComponentTrait>::UUID.to_u128()
            == <GuestPosition as GuestComponentTrait>::UUID.to_u128()
    );
    assert!(<GuestPosition as GuestComponentTrait>::FLAGS == BAM_COMPONENT_FLAG_SUMMARY_TICK);

    // `#[derive(ModComponent, GuestComponent)]` fallback to `#[mod_component(...)]`.
    assert!(
        <DualDerived as ModComponentTrait>::UUID.to_u128()
            == <DualDerived as GuestComponentTrait>::UUID.to_u128()
    );
    assert!(<DualDerived as GuestComponentTrait>::FLAGS == BAM_COMPONENT_FLAG_IMMUTABLE);

    // `#[mod_component(..., guest)]` shorthand.
    assert!(
        <ShorthandGuest as ModComponentTrait>::UUID.to_u128()
            == <ShorthandGuest as GuestComponentTrait>::UUID.to_u128()
    );
    assert!(<ShorthandGuest as GuestComponentTrait>::FLAGS == BAM_COMPONENT_FLAG_SPARSE_SET);
};

fn assert_guest<T: AbiPodTrait + GuestComponentTrait>() {}

fn main() {
    assert_guest::<GuestPosition>();
    assert_guest::<DualDerived>();
    assert_guest::<ShorthandGuest>();
}
