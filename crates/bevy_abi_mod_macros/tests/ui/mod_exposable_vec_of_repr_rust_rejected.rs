use bevy_abi_mod_macros::ModExposable;

#[derive(Copy, Clone, ModExposable)]
pub struct ReprRustPos {
    pub x: f32,
    pub y: f32,
}

#[derive(ModExposable)]
pub struct PathWaypoints {
    pub points: Vec<ReprRustPos>,
}

fn main() {}
