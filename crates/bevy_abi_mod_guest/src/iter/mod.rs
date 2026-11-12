//! Safe chunk iterators, dense column slices, and indirect sparse-set row views.

pub mod chunk;
pub mod chunk_iter;
pub mod slice;

pub use bevy_abi_mod_guest_sys::{SparseRows, SparseRowsIter, SparseRowsMut};
pub use chunk::Chunk;
pub use chunk_iter::ChunkIter;
pub use slice::{ChunkSlice, ChunkSliceMut};
