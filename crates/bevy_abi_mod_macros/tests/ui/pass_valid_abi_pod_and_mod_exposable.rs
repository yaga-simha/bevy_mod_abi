use bevy_abi_mod_macros::{AbiPod, ModExposable};
use bevy_abi_mod_sys::{AbiPod as AbiPodTrait, ExposableComponent, FfiEntity, FfiLayout};

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
pub struct ValidC {
    pub entity: FfiEntity,
    pub coords: [f32; 3],
    pub flags: u32,
}

#[repr(transparent)]
#[derive(Copy, Clone, AbiPod)]
pub struct ValidTransparent(pub u64);

#[repr(C)]
#[derive(Copy, Clone, ModExposable)]
pub struct ValidPodComp {
    pub c: ValidC,
    pub id: ValidTransparent,
}

#[derive(ModExposable)]
pub struct ValidVecConsumer {
    pub list: Vec<ValidPodComp>,
    pub name: String,
    pub enabled: bool,
}

const _: () = {
    assert!(<ValidPodComp as ExposableComponent>::IS_POD);
    assert!(<ValidPodComp as ExposableComponent>::IS_RAW_INSERTABLE);
    assert!(!<ValidVecConsumer as ExposableComponent>::IS_POD);
    assert!(!<ValidVecConsumer as ExposableComponent>::IS_RAW_INSERTABLE);
};

fn require_all<T: AbiPodTrait + FfiLayout>() {}

fn main() {
    require_all::<ValidC>();
    require_all::<ValidTransparent>();
    require_all::<ValidPodComp>();
}
