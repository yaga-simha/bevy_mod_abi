//! Component, resource, VTable, and `"bam.v0.*"` symbol registration catalog (`ModRegistry`).

pub mod catalog;
pub mod collision;

pub(crate) use catalog::component_flags_to_storage_flags;
pub use catalog::{
    ComponentOptions, CreateMsgCursorFn, ExtractEventTargetFn, InsertDefaultFn, InsertRawFn,
    ModRegistry, MsgReadBatchFn, MsgSendFn, MsgWriteBatchFn, RegisteredComponent, RegistrySnapshot,
    TargetEntity, TriggerEventFn,
};
pub use collision::ComponentCollisionError;
