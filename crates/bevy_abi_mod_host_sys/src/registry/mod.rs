//! Component, resource, asset, VTable, and `"bam.v0.*"` symbol registration catalog (`ModRegistry`).

pub mod bridge;
pub mod catalog;
pub mod collision;

pub use bridge::{ExtractRelationshipTargetsFn, HostStringBridge, HostWireBridge};
pub use catalog::{
    ComponentOptions, CreateMsgCursorFn, ExtractEventTargetFn, InsertDefaultFn, InsertRawFn,
    ModRegistry, MsgReadBatchFn, MsgSendFn, MsgWriteBatchFn, RegisteredComponent, RegistrySnapshot,
    TargetEntity, TriggerEventFn,
};
pub(crate) use catalog::{SymbolRecord, component_flags_to_storage_flags};
pub use collision::ComponentCollisionError;
