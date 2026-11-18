//! ECS lifecycle components and buffered `Message`s for native mods.
//!
//! Lifecycle and fault notifications (`ModLoaded`, `ModUnloaded`, `ModSystemFaulted`)
//! emit once via Bevy [`Message`](bevy_ecs::message::Message) queues (`MessageReader<M>`) without
//! duplicate `Observer`/`Event` double-writes.

#[cfg(feature = "signature-verification")]
pub use bevy_abi_mod_host_sys::VerifiedModSignature;
pub use bevy_abi_mod_host_sys::{
    LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
};
