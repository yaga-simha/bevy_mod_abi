//! Zero-copy hierarchy (`Children`) traversal, child command helpers, and custom dynamic
//! `Relationship` / `RelationshipTarget` pair registration (`#![forbid(unsafe_code)]`).

use core::ffi::CStr;

use bevy_abi_mod_guest_sys::{SpawnEntry, resolve_component};
use bevy_abi_mod_sys::{
    AccessMode, BAM_CHILDREN_NAME, BamComponent, ComponentUuid, FfiChildOf, FfiEntity, FfiResult,
};

use crate::{
    app::builder::{GuestApp, QueryBuilder},
    app::context::SystemContext,
    commands::GuestCommands,
    component::GuestComponent,
};

const CHILDREN_NAME_CSTR: &CStr = c"bevy_ecs::hierarchy::Children";
const CHILDREN_ALIAS_CSTR: &CStr = c"Children";
const CHILD_OF_NAME_CSTR: &CStr = c"bevy_ecs::hierarchy::ChildOf";

impl<'a> SystemContext<'a> {
    /// Borrows the contiguous zero-copy `&[FfiEntity]` slice of Bevy `Children` attached to
    /// `entity`.
    ///
    /// Returns [`FfiResult::PERMISSION_DENIED`] unless a query in this system declared read access
    /// to `Children` (for example via [`QueryBuilder::read_children`]). Returns `Ok(&[])` when
    /// `entity` is alive and has no `Children` component.
    #[inline]
    pub fn children(&self, entity: FfiEntity) -> Result<&[FfiEntity], FfiResult> {
        self.sys_ref.entity_children(entity, BamComponent::INVALID)
    }

    /// Borrows the contiguous zero-copy `&[FfiEntity]` slice of incoming source entities recorded
    /// under `target_handle` (either a bridged `RelationshipTarget` or the target half of a
    /// dynamic pair registered via [`GuestApp::register_relationship_pair`]).
    ///
    /// Returns [`FfiResult::PERMISSION_DENIED`] unless a query in this system declared read access
    /// to `target_handle` (for example via [`QueryBuilder::read_handle`] or
    /// [`QueryBuilder::optional_read_handle`]).
    #[inline]
    pub fn relationship_targets(
        &self,
        entity: FfiEntity,
        target_handle: BamComponent,
    ) -> Result<&[FfiEntity], FfiResult> {
        if !target_handle.is_valid() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        self.sys_ref.entity_children(entity, target_handle)
    }
}

impl<'b> QueryBuilder<'b> {
    /// Resolves Bevy's built-in `Children` component (`"bevy_ecs::hierarchy::Children"` /
    /// `"Children"`) and appends an [`AccessMode::READ`] term so [`SystemContext::children`] has
    /// scheduler-verified shared read permission.
    pub fn read_children(mut self) -> Self {
        let uuid = ComponentUuid::from_canonical_name(BAM_CHILDREN_NAME);
        match resolve_component(self.host, Some(CHILDREN_NAME_CSTR), Some(uuid))
            .or_else(|_| resolve_component(self.host, Some(CHILDREN_ALIAS_CSTR), None))
        {
            Ok(comp) => self.push_term(comp, AccessMode::READ),
            Err(err) => self.record_error(err),
        }
        self
    }
}

impl<'a> GuestCommands<'a> {
    /// Attaches `FfiChildOf::new(parent)` onto `child`, adding `child` to `parent`'s `Children`
    /// slice when deferred commands flush.
    #[inline]
    pub fn add_child(&mut self, parent: FfiEntity, child: FfiEntity) -> FfiResult {
        if parent.is_null() || child.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        self.insert_component(child, &FfiChildOf::new(parent))
    }

    /// Removes `ChildOf` (`FfiChildOf`) from `child`, detaching `child` from its current parent's
    /// `Children` slice when deferred commands flush.
    #[inline]
    pub fn remove_parent(&mut self, child: FfiEntity) -> FfiResult {
        if child.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        self.remove_component::<FfiChildOf>(child)
    }

    /// Spawns a new entity carrying `FfiChildOf::new(parent)` together with `extra_entries` in a
    /// single batched archetype transition, returning the reserved child [`FfiEntity`].
    pub fn spawn_child(
        &mut self,
        parent: FfiEntity,
        extra_entries: &[SpawnEntry<'_>],
    ) -> Result<FfiEntity, FfiResult> {
        if parent.is_null() {
            return Err(FfiResult::INVALID_ENTITY);
        }
        let child_of_handle = self
            .resolve_component(Some(CHILD_OF_NAME_CSTR), Some(FfiChildOf::UUID))
            .or_else(|_| self.resolve_component(Some(c"ChildOf"), None))?;
        let child_of = FfiChildOf::new(parent);
        let mut entries = Vec::with_capacity(1 + extra_entries.len());
        entries.push(SpawnEntry::new(child_of_handle, &child_of));
        entries.extend_from_slice(extra_entries);
        self.spawn(&entries)
    }
}

impl<'a> GuestApp<'a> {
    /// Registers a dynamic `(Rel, Target)` relationship pair backed by host `on_insert`,
    /// `on_discard`, and `on_despawn` hooks, returning `(rel_handle, target_handle)`.
    ///
    /// - `Rel` must have `size == 8` and `align == 8` (storing an `FfiEntity` target handle, e.g.
    ///   `#[repr(C)] struct Orbiting(FfiEntity);`).
    /// - `target_name` is registered as a zero-sized marker component (manifest-qualified when
    ///   unqualified) so queries (`With`, `Has`, `Changed`, `read_handle`, `optional_read_handle`)
    ///   and [`SystemContext::relationship_targets`] operate natively under Bevy's scheduler.
    pub fn register_relationship_pair<Rel: GuestComponent>(
        &mut self,
        target_name: &str,
    ) -> Result<(BamComponent, BamComponent), FfiResult> {
        if core::mem::size_of::<Rel>() != core::mem::size_of::<FfiEntity>()
            || core::mem::align_of::<Rel>() != core::mem::align_of::<FfiEntity>()
        {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        if target_name.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let qualified_target =
            if !target_name.contains("::") && !self.instance.manifest_name().is_empty() {
                format!("{}::{target_name}", self.instance.manifest_name())
            } else {
                target_name.to_string()
            };
        let rel_handle = self.register_component::<Rel>()?;
        let target_handle = self.register_component_with_layout(&qualified_target, 0, 1, None)?;
        let bind_spec = format!("__bam_rel_bind::{}::{}", rel_handle.0, target_handle.0);
        let _ = self.register_component_with_layout(&bind_spec, 0, 1, None)?;
        Ok((rel_handle, target_handle))
    }
}

#[cfg(test)]
mod tests {
    use bevy_abi_mod_guest_sys::SpawnEntry;
    use bevy_abi_mod_sys::{BamComponent, FfiEntity, FfiResult};

    use crate::{GuestApp, GuestCommands, QueryBuilder, SystemContext};

    #[allow(dead_code)]
    fn _verify_hierarchy_sdk_signatures<'a>(
        ctx: &'a SystemContext<'a>,
        qb: QueryBuilder<'a>,
        cmds: &'a mut GuestCommands<'a>,
        app: &'a mut GuestApp<'a>,
        entity: FfiEntity,
        target: BamComponent,
        entries: &[SpawnEntry<'_>],
    ) {
        let _: Result<&[FfiEntity], FfiResult> = ctx.children(entity);
        let _: Result<&[FfiEntity], FfiResult> = ctx.relationship_targets(entity, target);
        let _: QueryBuilder<'a> = qb.read_children();
        let _: FfiResult = cmds.add_child(entity, entity);
        let _: FfiResult = cmds.remove_parent(entity);
        let _: Result<FfiEntity, FfiResult> = cmds.spawn_child(entity, entries);
        let _: Result<(BamComponent, BamComponent), FfiResult> =
            app.register_relationship_pair::<bevy_abi_mod_sys::FfiChildOf>("test::Targets");
    }

    #[test]
    fn hierarchy_guest_sdk_signatures_compile_with_forbid_unsafe() {
        let _f = _verify_hierarchy_sdk_signatures;
    }
}
