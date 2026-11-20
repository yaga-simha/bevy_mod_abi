#![allow(unreachable_pub)]

//! `trybuild` UI compile-fail and positive-control test suite for `#[derive(AbiPod)]`
//! and `#[derive(ModComponent)]`.

use bevy_abi_mod_macros::{AbiPod, ModComponent};
use bevy_abi_mod_sys::ModComponent as _;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
struct LinkAnchorPod {
    x: f32,
}

#[repr(C)]
#[derive(Copy, Clone, ModComponent)]
struct LinkAnchorComp {
    val: LinkAnchorPod,
}

#[test]
fn link_anchor_traits_are_live() {
    bevy_abi_mod_sys::assert_abi_pod::<LinkAnchorPod>();
    bevy_abi_mod_sys::assert_abi_pod::<LinkAnchorComp>();
    bevy_abi_mod_sys::assert_raw_insertable::<LinkAnchorComp>();
    assert_eq!(
        LinkAnchorComp::CANONICAL_NAME,
        "compile_fail_tests::LinkAnchorComp"
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn ui_compile_fail_and_pass_suite() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_*.rs");
    t.compile_fail("tests/ui/abi_pod_*.rs");
    t.compile_fail("tests/ui/mod_component_*.rs");
    t.compile_fail("tests/ui/guest_component_*.rs");
}
