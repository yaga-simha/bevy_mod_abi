//! Host FFI boundary, chunk iteration, columns, column info, resources, commands,
//! alignment/padding, C99 VTable interop, and SipHash-2-4-128 UUID parity integration tests.

#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    clippy::default_constructed_unit_structs,
    clippy::clone_on_copy,
    unreachable_pub
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

#[path = "../support/helpers.rs"]
mod support;

mod batch_cmds_and_messages;
mod boundary_validation;
mod c_vtable_interop;
mod chunk_and_commands;
mod engine_bridge;
mod events_hierarchy_and_parallel;
mod query_filters;
mod sparse_and_resources;
mod strides_and_alignment;
mod uuid_parity;
