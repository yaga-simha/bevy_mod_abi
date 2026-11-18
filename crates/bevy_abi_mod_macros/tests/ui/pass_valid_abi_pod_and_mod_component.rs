use bevy_abi_mod_macros::{AbiPod, ModComponent};
use bevy_abi_mod_sys::{
    AbiPod as AbiPodTrait, BAM_COMPONENT_FLAG_SUMMARY_TICK, ComponentUuid, FfiBool, FfiEntity,
    FfiOption, ModComponent as ModComponentTrait, RawInsertable,
};

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
pub struct ValidC {
    pub entity: FfiEntity,
    pub uuid: ComponentUuid,
    pub coords: [f32; 3],
    pub flag: FfiBool,
    pub maybe_id: FfiOption<u32>,
}

#[repr(transparent)]
#[derive(Copy, Clone, AbiPod)]
pub struct ValidTransparent(pub u64);

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
#[mod_component(summary_tick)]
pub struct ValidPodComp {
    pub c: ValidC,
    pub id: ValidTransparent,
}

#[repr(C, u8)]
#[derive(ModComponent)]
#[mod_component(require(ValidPodComp))]
pub enum ValidTaggedEnum {
    Idle = 0,
    Active { target: FfiOption<FfiEntity>, label: String } = 1,
}

#[derive(ModComponent)]
pub struct ValidVecConsumer {
    pub list: Vec<ValidPodComp>,
    pub name: ::std::string::String,
    pub enabled: bool,
    pub opt: FfiOption<ValidPodComp>,
}

const _: () = {
    assert!(<ValidPodComp as ModComponentTrait>::IS_ABI_POD);
    assert!(<ValidPodComp as ModComponentTrait>::IS_RAW_INSERTABLE);
    assert!(
        <ValidPodComp as ModComponentTrait>::COMPONENT_FLAGS & BAM_COMPONENT_FLAG_SUMMARY_TICK
            != 0
    );
    assert!(!<ValidTaggedEnum as ModComponentTrait>::IS_ABI_POD);
    assert!(!<ValidTaggedEnum as ModComponentTrait>::IS_RAW_INSERTABLE);
    assert!(!<ValidVecConsumer as ModComponentTrait>::IS_ABI_POD);
    assert!(!<ValidVecConsumer as ModComponentTrait>::IS_RAW_INSERTABLE);
};

fn require_all<T: AbiPodTrait + RawInsertable>() {}

fn main() {
    require_all::<ValidC>();
    require_all::<ValidTransparent>();
    require_all::<ValidPodComp>();
}
