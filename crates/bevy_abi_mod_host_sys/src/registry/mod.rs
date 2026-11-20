//! Component, resource, asset, VTable, and `"bam.v0.*"` symbol registration catalog (`ModRegistry`).

pub mod bridge;
pub mod catalog;
pub mod collision;

pub(crate) use bridge::MigratedComponentSpec;
pub use bridge::{ExtractRelationshipTargetsFn, HostStringBridge, HostWireBridge};
pub use catalog::{
    ComponentOptions, CreateMsgCursorFn, EntityEventTriggerFn, ExtractEventTargetFn,
    InsertDefaultFn, InsertRawFn, ModRegistry, MsgReadBatchFn, MsgSendFn, MsgWriteBatchFn,
    RegisteredComponent, RegistrySnapshot, TargetEntity, TriggerEventFn,
};
pub(crate) use catalog::{
    DynamicComponentReg, NewComponentBase, RelationRegSpec, SymbolRecord,
    component_flags_to_storage_flags, extract_owned_dynamic_fields,
};
pub use collision::ComponentCollisionError;
