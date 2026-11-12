use bevy_abi_mod_macros::{AbiPod, ModComponent};
use bevy_abi_mod_sys::{
    AbiPod as AbiPodTrait, ComponentUuid, FfiBool, FfiEntity, ModComponent as ModComponentTrait,
    RawInsertable,
};

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
pub struct ValidC {
    pub entity: FfiEntity,
    pub uuid: ComponentUuid,
    pub coords: [f32; 3],
    pub flag: FfiBool,
}

#[repr(transparent)]
#[derive(Copy, Clone, AbiPod)]
pub struct ValidTransparent(pub u64);

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
pub struct ValidPodComp {
    pub c: ValidC,
    pub id: ValidTransparent,
}

#[derive(ModComponent)]
pub struct ValidVecConsumer {
    pub list: Vec<ValidPodComp>,
    pub name: ::std::string::String,
    pub enabled: bool,
}

const _: () = {
    assert!(<ValidPodComp as ModComponentTrait>::IS_ABI_POD);
    assert!(<ValidPodComp as ModComponentTrait>::IS_RAW_INSERTABLE);
    assert!(!<ValidVecConsumer as ModComponentTrait>::IS_ABI_POD);
    assert!(!<ValidVecConsumer as ModComponentTrait>::IS_RAW_INSERTABLE);
};

fn require_all<T: AbiPodTrait + RawInsertable>() {}

fn main() {
    require_all::<ValidC>();
    require_all::<ValidTransparent>();
    require_all::<ValidPodComp>();
}
