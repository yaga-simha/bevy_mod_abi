use bevy_abi_mod_macros::{AbiPod, GuestComponent};

#[repr(C)]
#[derive(Copy, Clone, AbiPod, GuestComponent)]
#[guest_component(name = "bad::SparseSummary", sparse_set, summary_tick)]
pub struct SparseSummary {
    pub x: f32,
}

fn main() {}
