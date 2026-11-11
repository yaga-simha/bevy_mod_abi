//! Safe archetype chunk iteration, disjoint split borrows, and typed [`AbiPod`](crate::AbiPod) column views.

mod chunk;
mod chunk_iter;
mod slice;

pub use self::chunk::Chunk;
pub use self::chunk_iter::GuestChunkIter;
pub use self::slice::{ChunkSlice, ChunkSliceMut};
