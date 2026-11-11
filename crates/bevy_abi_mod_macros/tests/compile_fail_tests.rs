#![allow(unreachable_pub)]

//! `trybuild` UI compile-fail and positive-control test suite for `#[derive(AbiPod)]`
//! and `#[derive(ModExposable)]`.

use bevy_abi_mod_macros::{AbiPod, ModExposable};
use bevy_abi_mod_sys::ExposableComponent as _;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
struct LinkAnchorPod {
    x: f32,
}

#[repr(C)]
#[derive(Copy, Clone, ModExposable)]
struct LinkAnchorComp {
    val: LinkAnchorPod,
}

#[test]
fn link_anchor_traits_are_live() {
    fn assert_pod<T: bevy_abi_mod_sys::AbiPod>() {}
    fn assert_layout<T: bevy_abi_mod_sys::FfiLayout>() {}
    assert_pod::<LinkAnchorPod>();
    assert_pod::<LinkAnchorComp>();
    assert_layout::<LinkAnchorComp>();
    assert_eq!(LinkAnchorComp::COMPONENT_NAME, "LinkAnchorComp");
}

#[test]
#[cfg_attr(miri, ignore)]
fn ui_compile_fail_and_pass_suite() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_*.rs");
    t.compile_fail("tests/ui/abi_pod_*.rs");
    t.compile_fail("tests/ui/mod_exposable_*.rs");
}
