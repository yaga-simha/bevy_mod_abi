//! ECS lifecycle components and buffered `Message`s for native mods (`B-05`, `B-09`, `L-08`).
//!
//! Per `B-09`, lifecycle and fault notifications (`ModLoaded`, `ModUnloaded`, `ModSystemFaulted`)
//! emit once via Bevy [`Message`](bevy_ecs::message::Message) queues (`MessageReader<M>`) without
//! duplicate `Observer`/`Event` double-writes.

pub use bevy_abi_mod_host_sys::{
    LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
};
