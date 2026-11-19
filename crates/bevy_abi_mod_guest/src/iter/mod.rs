//! Safe chunk iterators, dense column slices, and indirect sparse-set row views.

pub mod chunk;
pub mod chunk_iter;
pub mod filters;
pub mod slice;
pub mod typed_query;

pub use bevy_abi_mod_guest_sys::{SparseRows, SparseRowsIter, SparseRowsMut};
pub use chunk::Chunk;
pub use chunk_iter::ChunkIter;
pub use slice::{ChunkSlice, ChunkSliceMut};
pub use typed_query::{
    Added, Allow, Changed, Has, Or, Or2, Or3, OrFilterTerm, OrFilterTuple, QueryTerm, QueryTuple,
    TypedChunkIter, With, Without,
};
