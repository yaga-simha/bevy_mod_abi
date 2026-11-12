use bevy_abi_mod_macros::ModComponent;

#[derive(Copy, Clone)]
pub struct ReprRustPos {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, ModComponent)]
pub struct PathWaypoints {
    pub points: Vec<ReprRustPos>,
}

fn main() {}
