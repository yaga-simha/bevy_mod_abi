//! Synchronous and `World`-backed `FilteredAccessSet` validation and `QueryState` builder
//!.
//!
//! # Intent
//! Dynamic C-ABI systems have no Rust type-system `Query<(&mut A, &B)>` signature for Bevy
//! to inspect at compile time. To enforce Bevy's aliasing rules (multiple readers OR one
//! writer per component/resource) and enable Bevy's multi-threaded scheduler to run
//! non-conflicting mod and host systems in parallel, every guest system's declared terms
//! must be translated into Bevy's [`FilteredAccessSet`].
//!
//! # How It Works
//! 1. **Registration-time validation:** [`validate_system_queries_against_snapshot`] checks
//!    handle validity, immutability flags, and intra/inter-query `FilteredAccessSet`
//!    compatibility synchronously during `host_register_system`.
//! 2. **World initialization:** During [`GuestSystem::initialize`](crate::system::GuestSystem),
//!    `build_query_groups_in_world` constructs the final `FilteredAccessSet` (returned to Bevy's
//!    scheduler) alongside pre-planned `QueryPlan`s and `QueryState`s.

use crate::ffi::iter::{QueryPlan, QueryTermMeta};
use crate::registry::RegistrySnapshot;
use crate::system::error_policy::SystemInitError;
use crate::system::query_group::{
    BamQuerySpecOwned, CachedQueryGroup, PersistentCursorKey, PersistentMessageCursors,
};
use bevy_abi_mod_sys::{
    AccessMode, BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_SPARSE_SET, FfiResult,
    MAX_TERMS_PER_QUERY,
};
use bevy_ecs::component::{ComponentId, StorageType};
use bevy_ecs::lifecycle::RemovedComponentEntity;
use bevy_ecs::message::MessageCursor;
use bevy_ecs::query::{FilteredAccess, FilteredAccessSet, QueryBuilder};
use bevy_ecs::world::World;
use bevy_platform::collections::{HashMap, HashSet};
use std::sync::Arc;

fn find_conflicting_name(
    id_to_name: &HashMap<ComponentId, Arc<str>>,
    existing: &FilteredAccessSet,
    candidate: &FilteredAccess,
) -> String {
    for (&cid, name) in id_to_name {
        let cw = candidate.access().has_write(cid);
        let cr = candidate.access().has_read(cid);
        if !cr && !cw {
            continue;
        }
        let ew = existing.combined_access().has_write(cid);
        let er = existing.combined_access().has_read(cid);
        if (cw && (er || ew)) || (ew && cr) {
            return name.to_string();
        }
    }
    "unknown".to_string()
}

/// Validates a set of [`BamQuerySpecOwned`] clauses synchronously during `host_register_system`
/// against `snapshot`, checking handle existence, immutability, component/resource kind match,
/// and intra/inter-query `FilteredAccessSet` borrow compatibility.
pub(crate) fn validate_system_queries_against_snapshot(
    snapshot: &RegistrySnapshot,
    groups: &[BamQuerySpecOwned],
) -> Result<(), (FfiResult, String)> {
    let mut access_set = FilteredAccessSet::new();
    let mut system_res_reads: HashSet<ComponentId> = HashSet::new();
    let mut system_res_writes: HashSet<ComponentId> = HashSet::new();
    let mut system_res_order: Vec<(ComponentId, Arc<str>)> = Vec::new();
    let mut id_to_name: HashMap<ComponentId, Arc<str>> = HashMap::new();

    for (q_idx, group) in groups.iter().enumerate() {
        if group.terms.len() > MAX_TERMS_PER_QUERY {
            return Err((
                FfiResult::CAPACITY_EXCEEDED,
                format!("query {q_idx} exceeds MAX_TERMS_PER_QUERY (32)"),
            ));
        }

        let mut read_components: HashSet<ComponentId> = HashSet::new();
        let mut write_components: HashSet<ComponentId> = HashSet::new();
        let mut has_entity_terms = false;
        let mut resolved: Vec<(ComponentId, AccessMode, u8)> =
            Vec::with_capacity(group.terms.len());
        let mut or_group_order: Vec<u8> = Vec::new();

        for (t_idx, term) in group.terms.iter().enumerate() {
            let v = term.validate();
            if v.is_err() {
                return Err((
                    v,
                    format!("query {q_idx} term {t_idx} failed validation ({v})"),
                ));
            }
            let Some(reg) = snapshot.get_by_handle(term.component) else {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    format!(
                        "query {q_idx} term {t_idx} references unknown BamComponent({})",
                        term.component.0
                    ),
                ));
            };
            if term.mode.is_resource() != reg.is_resource()
                || term.mode.is_message() != reg.is_message()
            {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    format!(
                        "query {q_idx} term {t_idx} ('{}') kind mismatch between mode {:?} and registration",
                        reg.name, term.mode
                    ),
                ));
            }
            if reg.is_immutable() && term.mode.is_mutable() {
                return Err((
                    FfiResult::PERMISSION_DENIED,
                    format!(
                        "query {q_idx} term {t_idx} requests mutable access on immutable component '{}'",
                        reg.name
                    ),
                ));
            }

            if term.or_group != 0 {
                if !matches!(
                    term.mode,
                    AccessMode::FILTER_ONLY
                        | AccessMode::WITHOUT
                        | AccessMode::CHANGED
                        | AccessMode::ADDED
                ) {
                    return Err((
                        FfiResult::INVALID_ARGUMENT,
                        format!(
                            "query {q_idx} term {t_idx} uses invalid mode {:?} inside or_group={}",
                            term.mode, term.or_group
                        ),
                    ));
                }
                if !or_group_order.contains(&term.or_group) {
                    or_group_order.push(term.or_group);
                }
            }

            let comp_id = reg.component_id;
            id_to_name
                .entry(comp_id)
                .or_insert_with(|| Arc::clone(&reg.name));

            match term.mode {
                AccessMode::READ | AccessMode::OPTIONAL_READ => {
                    has_entity_terms = true;
                    if write_components.contains(&comp_id) {
                        return Err((
                            FfiResult::ACCESS_CONFLICT,
                            format!(
                                "intra-query read/write conflict on component '{}'",
                                reg.name
                            ),
                        ));
                    }
                    read_components.insert(comp_id);
                }
                AccessMode::WRITE | AccessMode::OPTIONAL_WRITE => {
                    has_entity_terms = true;
                    if read_components.contains(&comp_id) || write_components.contains(&comp_id) {
                        return Err((
                            FfiResult::ACCESS_CONFLICT,
                            format!("intra-query write conflict on component '{}'", reg.name),
                        ));
                    }
                    write_components.insert(comp_id);
                }
                AccessMode::FILTER_ONLY
                | AccessMode::WITHOUT
                | AccessMode::CHANGED
                | AccessMode::ADDED
                | AccessMode::ALLOW
                | AccessMode::HAS => {
                    has_entity_terms = true;
                }
                AccessMode::REMOVED_READ => {}
                AccessMode::RESOURCE_READ | AccessMode::MESSAGE_READ => {
                    if system_res_writes.contains(&comp_id) {
                        return Err((
                            FfiResult::ACCESS_CONFLICT,
                            format!("conflicting resource/message read/write on '{}'", reg.name),
                        ));
                    }
                    if system_res_reads.insert(comp_id) {
                        system_res_order.push((comp_id, Arc::clone(&reg.name)));
                    }
                }
                AccessMode::RESOURCE_WRITE | AccessMode::MESSAGE_WRITE => {
                    if system_res_reads.contains(&comp_id) || system_res_writes.contains(&comp_id) {
                        return Err((
                            FfiResult::ACCESS_CONFLICT,
                            format!("conflicting resource/message write on '{}'", reg.name),
                        ));
                    }
                    system_res_writes.insert(comp_id);
                    system_res_order.push((comp_id, Arc::clone(&reg.name)));
                }
                _ => {
                    return Err((
                        FfiResult::INVALID_ARGUMENT,
                        format!("invalid AccessMode({})", term.mode.0),
                    ));
                }
            }

            resolved.push((comp_id, term.mode, term.or_group));
        }

        if group.terms.is_empty() || has_entity_terms {
            let mut fa = FilteredAccess::matches_everything();
            for &(comp_id, mode, or_group) in &resolved {
                if or_group != 0 {
                    continue;
                }
                match mode {
                    AccessMode::READ => {
                        fa.add_read(comp_id);
                    }
                    AccessMode::WRITE => {
                        fa.add_write(comp_id);
                    }
                    AccessMode::OPTIONAL_READ => {
                        fa.access_mut().add_read(comp_id);
                    }
                    AccessMode::OPTIONAL_WRITE => {
                        fa.access_mut().add_write(comp_id);
                    }
                    AccessMode::FILTER_ONLY => {
                        fa.and_with(comp_id);
                    }
                    AccessMode::WITHOUT => {
                        fa.and_without(comp_id);
                    }
                    AccessMode::CHANGED | AccessMode::ADDED => {
                        if write_components.contains(&comp_id) {
                            fa.and_with(comp_id);
                        } else {
                            fa.add_read(comp_id);
                        }
                    }
                    AccessMode::ALLOW | AccessMode::HAS => {
                        fa.access_mut().add_archetypal(comp_id);
                    }
                    _ => {}
                }
            }
            for &group_id in &or_group_order {
                let mut or_access = FilteredAccess::matches_nothing();
                for &(comp_id, mode, og) in &resolved {
                    if og != group_id {
                        continue;
                    }
                    let mut branch = FilteredAccess::default();
                    match mode {
                        AccessMode::FILTER_ONLY => {
                            branch.and_with(comp_id);
                        }
                        AccessMode::WITHOUT => {
                            branch.and_without(comp_id);
                        }
                        AccessMode::CHANGED | AccessMode::ADDED => {
                            if write_components.contains(&comp_id) {
                                branch.and_with(comp_id);
                            } else {
                                branch.add_read(comp_id);
                            }
                        }
                        _ => {}
                    }
                    or_access.append_or(&branch);
                    or_access.extend_access(&branch);
                }
                fa.extend(&or_access);
            }

            let mut candidate_set = FilteredAccessSet::new();
            candidate_set.add(fa.clone());
            if !access_set.is_compatible(&candidate_set) {
                let conflict = find_conflicting_name(&id_to_name, &access_set, &fa);
                return Err((
                    FfiResult::ACCESS_CONFLICT,
                    format!("inter-query borrow conflict on '{conflict}'"),
                ));
            }
            access_set.add(fa);
        }
    }

    for (res_id, res_name) in system_res_order {
        let writable = system_res_writes.contains(&res_id);
        let mut candidate_set = FilteredAccessSet::new();
        if writable {
            candidate_set.add_resource_write(res_id);
        } else {
            candidate_set.add_resource_read(res_id);
        }
        if !access_set.is_compatible(&candidate_set) {
            return Err((
                FfiResult::ACCESS_CONFLICT,
                format!("resource access conflict on '{res_name}'"),
            ));
        }
        if writable {
            access_set.add_resource_write(res_id);
        } else {
            access_set.add_resource_read(res_id);
        }
    }

    Ok(())
}

/// Resolves all query groups against `snapshot` and `world`, precomputing [`QueryPlan`]s,
/// `QueryState`s, and the combined Bevy [`FilteredAccessSet`].
pub(crate) fn initialize_query_groups_in_world(
    world: &mut World,
    snapshot: &RegistrySnapshot,
    query_groups: &mut [CachedQueryGroup],
) -> Result<FilteredAccessSet, SystemInitError> {
    let mut access_set = FilteredAccessSet::new();
    let mut system_res_reads: HashSet<ComponentId> = HashSet::new();
    let mut system_res_writes: HashSet<ComponentId> = HashSet::new();
    let mut system_res_order: Vec<(ComponentId, Arc<str>)> = Vec::new();
    let mut id_to_name: HashMap<ComponentId, Arc<str>> = HashMap::new();
    let mut persistent_store = None;

    for (query_idx, group) in query_groups.iter_mut().enumerate() {
        group.query_state = None;
        group.reset_cache();
        group.cursor_uuids.resize(group.spec.terms.len(), None);
        let group_store = if group.owner_system_key.is_some() {
            let store = persistent_store.get_or_insert_with(|| {
                world
                    .get_resource_or_insert_with(PersistentMessageCursors::default)
                    .shared_store()
            });
            let cloned = Arc::clone(store);
            group.persistent_store = Some((Arc::clone(&cloned), query_idx));
            Some(cloned)
        } else {
            group.persistent_store = None;
            None
        };

        if group.spec.terms.len() > MAX_TERMS_PER_QUERY {
            return Err(SystemInitError::TooManyTerms);
        }

        let mut read_components: HashSet<ComponentId> = HashSet::new();
        let mut write_components: HashSet<ComponentId> = HashSet::new();
        let mut has_entity_terms = false;
        let mut term_metas: Vec<QueryTermMeta> = Vec::with_capacity(group.spec.terms.len());
        let mut term_or_groups: Vec<u8> = Vec::with_capacity(group.spec.terms.len());

        for (col_idx, term) in group.spec.terms.iter().enumerate() {
            if !term.mode.is_valid() {
                return Err(SystemInitError::InvalidMode { index: col_idx });
            }
            if term.or_group != 0
                && !matches!(
                    term.mode,
                    AccessMode::FILTER_ONLY
                        | AccessMode::WITHOUT
                        | AccessMode::CHANGED
                        | AccessMode::ADDED
                )
            {
                return Err(SystemInitError::InvalidMode { index: col_idx });
            }
            let Some(reg) = snapshot.get_by_handle(term.component) else {
                return Err(SystemInitError::UnknownComponent(format!(
                    "BamComponent({})",
                    term.component.0
                )));
            };
            if term.mode.is_resource() != reg.is_resource()
                || term.mode.is_message() != reg.is_message()
            {
                return Err(SystemInitError::KindMismatch(reg.name.to_string()));
            }

            let comp_id = reg.component_id;
            let mut storage_flags = reg.storage_flags;
            if !reg.is_resource()
                && !reg.is_message()
                && let Some(info) = world.components().get_info(comp_id)
            {
                if info.storage_type() == StorageType::SparseSet {
                    storage_flags |= BAM_STORAGE_FLAG_SPARSE_SET;
                }
                if !info.mutable() {
                    storage_flags |= BAM_STORAGE_FLAG_IMMUTABLE;
                }
            }
            if (storage_flags & BAM_STORAGE_FLAG_IMMUTABLE) != 0 && term.mode.is_mutable() {
                return Err(SystemInitError::ImmutableWrite(reg.name.to_string()));
            }

            id_to_name
                .entry(comp_id)
                .or_insert_with(|| Arc::clone(&reg.name));

            match term.mode {
                AccessMode::READ | AccessMode::OPTIONAL_READ => {
                    has_entity_terms = true;
                    if write_components.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: reg.name.to_string(),
                            b: reg.name.to_string(),
                        });
                    }
                    read_components.insert(comp_id);
                }
                AccessMode::WRITE | AccessMode::OPTIONAL_WRITE => {
                    has_entity_terms = true;
                    if read_components.contains(&comp_id) || write_components.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: reg.name.to_string(),
                            b: reg.name.to_string(),
                        });
                    }
                    write_components.insert(comp_id);
                }
                AccessMode::FILTER_ONLY
                | AccessMode::WITHOUT
                | AccessMode::CHANGED
                | AccessMode::ADDED
                | AccessMode::ALLOW
                | AccessMode::HAS => {
                    has_entity_terms = true;
                }
                AccessMode::REMOVED_READ => {}
                AccessMode::RESOURCE_READ | AccessMode::MESSAGE_READ => {
                    if system_res_writes.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: reg.name.to_string(),
                            b: reg.name.to_string(),
                        });
                    }
                    if system_res_reads.insert(comp_id) {
                        system_res_order.push((comp_id, Arc::clone(&reg.name)));
                    }
                }
                AccessMode::RESOURCE_WRITE | AccessMode::MESSAGE_WRITE => {
                    if system_res_reads.contains(&comp_id) || system_res_writes.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: reg.name.to_string(),
                            b: reg.name.to_string(),
                        });
                    }
                    system_res_writes.insert(comp_id);
                    system_res_order.push((comp_id, Arc::clone(&reg.name)));
                }
                _ => {
                    return Err(SystemInitError::InvalidMode { index: col_idx });
                }
            }

            if group.msg_cursors.len() <= col_idx {
                group.msg_cursors.resize_with(col_idx + 1, || None);
            }
            if term.mode.is_message() {
                group.cursor_uuids[col_idx] = Some(reg.uuid);
                if group.msg_cursors[col_idx].is_none()
                    && let (Some((mod_ns, sys_name)), Some(store)) =
                        (&group.owner_system_key, &group_store)
                {
                    let key = PersistentCursorKey {
                        mod_namespace: mod_ns.clone(),
                        system_name: Arc::clone(sys_name),
                        query_idx,
                        term_idx: col_idx,
                        component_uuid: reg.uuid,
                    };
                    group.msg_cursors[col_idx] =
                        PersistentMessageCursors::take_from_store(store, &key);
                }
                if group.msg_cursors[col_idx].is_none()
                    && let Some(create_fn) = reg.create_msg_cursor_fn
                {
                    group.msg_cursors[col_idx] = Some(create_fn());
                }
            } else if term.mode == AccessMode::REMOVED_READ {
                group.cursor_uuids[col_idx] = Some(reg.uuid);
                if group.msg_cursors[col_idx].is_none()
                    && let (Some((mod_ns, sys_name)), Some(store)) =
                        (&group.owner_system_key, &group_store)
                {
                    let key = PersistentCursorKey {
                        mod_namespace: mod_ns.clone(),
                        system_name: Arc::clone(sys_name),
                        query_idx,
                        term_idx: col_idx,
                        component_uuid: reg.uuid,
                    };
                    group.msg_cursors[col_idx] =
                        PersistentMessageCursors::take_from_store(store, &key);
                }
                if group.msg_cursors[col_idx].is_none() {
                    group.msg_cursors[col_idx] =
                        Some(Box::new(MessageCursor::<RemovedComponentEntity>::default()));
                }
            } else {
                group.cursor_uuids[col_idx] = None;
                group.msg_cursors[col_idx] = None;
            }

            term_or_groups.push(term.or_group);
            term_metas.push(QueryTermMeta {
                component: reg.handle,
                component_id: comp_id,
                mode: term.mode,
                uuid: reg.uuid,
                size: reg.size,
                align: reg.align,
                storage_flags,
                msg_read_batch_fn: reg.msg_read_batch_fn,
                msg_send_fn: reg.msg_send_fn,
                msg_write_batch_fn: reg.msg_write_batch_fn,
            });
        }

        let plan = QueryPlan::from_terms_with_or_groups(term_metas, &term_or_groups);
        group.sparse_scratch.ensure_terms(plan.terms.len());

        if group.spec.terms.is_empty() || has_entity_terms {
            let mut builder = QueryBuilder::<()>::new(world);
            for (idx, t) in plan.terms.iter().enumerate() {
                if (plan.or_term_mask & (1u32 << idx)) != 0 {
                    continue;
                }
                match t.mode {
                    AccessMode::READ => {
                        builder.ref_id(t.component_id);
                    }
                    AccessMode::WRITE => {
                        builder.mut_id(t.component_id);
                    }
                    AccessMode::OPTIONAL_READ => {
                        builder.optional(|b| {
                            b.ref_id(t.component_id);
                        });
                    }
                    AccessMode::OPTIONAL_WRITE => {
                        builder.optional(|b| {
                            b.mut_id(t.component_id);
                        });
                    }
                    AccessMode::FILTER_ONLY => {
                        builder.with_id(t.component_id);
                    }
                    AccessMode::WITHOUT => {
                        builder.without_id(t.component_id);
                    }
                    AccessMode::CHANGED | AccessMode::ADDED => {
                        if write_components.contains(&t.component_id) {
                            builder.with_id(t.component_id);
                        } else {
                            builder.ref_id(t.component_id);
                        }
                    }
                    AccessMode::ALLOW | AccessMode::HAS => {
                        let mut access = FilteredAccess::default();
                        access.access_mut().add_archetypal(t.component_id);
                        builder.extend_access(access);
                    }
                    _ => {}
                }
            }
            for or_group in &plan.or_groups {
                let mut or_access = FilteredAccess::matches_nothing();
                for clause in &or_group.terms {
                    let mut branch = FilteredAccess::default();
                    match clause.mode {
                        AccessMode::FILTER_ONLY => {
                            branch.and_with(clause.component_id);
                        }
                        AccessMode::WITHOUT => {
                            branch.and_without(clause.component_id);
                        }
                        AccessMode::CHANGED | AccessMode::ADDED => {
                            if write_components.contains(&clause.component_id) {
                                branch.and_with(clause.component_id);
                            } else {
                                branch.add_read(clause.component_id);
                            }
                        }
                        _ => {}
                    }
                    or_access.append_or(&branch);
                    or_access.extend_access(&branch);
                }
                builder.extend_access(or_access);
            }

            let state = builder.build();
            let is_comp_dense = |id| {
                world
                    .components()
                    .get_info(id)
                    .is_some_and(|info| info.storage_type() == StorageType::Table)
            };
            group.is_dense = plan.sparse_data_mask == 0
                && !plan.has_sparse_term
                && state.component_access().with_filters().all(is_comp_dense)
                && state
                    .component_access()
                    .without_filters()
                    .all(is_comp_dense);

            let filtered_access = state.component_access().clone();
            group.query_state = Some(state);

            let mut candidate_set = FilteredAccessSet::new();
            candidate_set.add(filtered_access.clone());
            if !access_set.is_compatible(&candidate_set) {
                let conflict = find_conflicting_name(&id_to_name, &access_set, &filtered_access);
                return Err(SystemInitError::SelfConflict {
                    a: conflict.clone(),
                    b: conflict,
                });
            }
            access_set.add(filtered_access);
        } else {
            group.is_dense = true;
            group.query_state = None;
        }

        group.plan = plan;
    }

    for (res_id, res_name) in system_res_order {
        let writable = system_res_writes.contains(&res_id);
        let mut candidate_set = FilteredAccessSet::new();
        if writable {
            candidate_set.add_resource_write(res_id);
        } else {
            candidate_set.add_resource_read(res_id);
        }
        if !access_set.is_compatible(&candidate_set) {
            return Err(SystemInitError::SelfConflict {
                a: res_name.to_string(),
                b: res_name.to_string(),
            });
        }
        if writable {
            access_set.add_resource_write(res_id);
        } else {
            access_set.add_resource_read(res_id);
        }
    }

    Ok(access_set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ModRegistry;
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::BamTermDesc;
    use bevy_ecs::message::{Message, Messages};

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
    struct ReloadPing {
        seq: u32,
    }

    impl Message for ReloadPing {}

    #[test]
    fn persistent_message_cursor_survives_query_group_reload() {
        let mut world = World::new();
        world.init_resource::<Messages<ReloadPing>>();
        let res_id = world
            .components()
            .component_id::<Messages<ReloadPing>>()
            .expect("Messages<ReloadPing> resource id");

        let mut registry = ModRegistry::new();
        let handle = registry
            .register_message::<ReloadPing>(res_id)
            .expect("register ReloadPing message");
        let snapshot = registry.snapshot();

        let term = BamTermDesc::new(handle, AccessMode::MESSAGE_READ);
        let spec = BamQuerySpecOwned::new(vec![term]);

        // Write 3 messages before initial load.
        {
            let mut msgs = world.resource_mut::<Messages<ReloadPing>>();
            msgs.write(ReloadPing { seq: 1 });
            msgs.write(ReloadPing { seq: 2 });
            msgs.write(ReloadPing { seq: 3 });
        }

        // First mod generation reads 2 of 3 messages (or all 3) and drops on unload.
        {
            let mut group = CachedQueryGroup::new(spec.clone());
            group.set_system_identity(Some(Arc::from("mod_a")), "tick_pings");
            let mut groups = [group];
            initialize_query_groups_in_world(&mut world, snapshot, &mut groups)
                .expect("init gen 1");

            let cursor = groups[0].msg_cursors[0]
                .as_mut()
                .expect("cursor initialized")
                .downcast_mut::<MessageCursor<ReloadPing>>()
                .expect("MessageCursor<ReloadPing>");
            let messages = world.resource::<Messages<ReloadPing>>();
            let batch: Vec<u32> = cursor.read(messages).take(2).map(|m| m.seq).collect();
            assert_eq!(batch, vec![1, 2]);
        }

        // Write one more message while unloaded.
        world
            .resource_mut::<Messages<ReloadPing>>()
            .write(ReloadPing { seq: 4 });

        // Second mod generation with identical (mod_namespace, system_name, query_idx, term_idx, uuid)
        // resumes cleanly at seq=3 without re-reading seq=1,2!
        {
            let mut group = CachedQueryGroup::new(spec.clone());
            group.set_system_identity(Some(Arc::from("mod_a")), "tick_pings");
            let mut groups = [group];
            initialize_query_groups_in_world(&mut world, snapshot, &mut groups)
                .expect("init gen 2");

            let cursor = groups[0].msg_cursors[0]
                .as_mut()
                .expect("cursor restored")
                .downcast_mut::<MessageCursor<ReloadPing>>()
                .expect("MessageCursor<ReloadPing>");
            let messages = world.resource::<Messages<ReloadPing>>();
            let batch: Vec<u32> = cursor.read(messages).map(|m| m.seq).collect();
            assert_eq!(batch, vec![3, 4]);
        }

        // Different system_name gets an independent fresh cursor starting from seq=1.
        {
            let mut other = CachedQueryGroup::new(spec);
            other.set_system_identity(Some(Arc::from("mod_a")), "other_system");
            let mut groups = [other];
            initialize_query_groups_in_world(&mut world, snapshot, &mut groups)
                .expect("init other");

            let cursor = groups[0].msg_cursors[0]
                .as_mut()
                .expect("fresh cursor")
                .downcast_mut::<MessageCursor<ReloadPing>>()
                .expect("MessageCursor<ReloadPing>");
            let messages = world.resource::<Messages<ReloadPing>>();
            let batch: Vec<u32> = cursor.read(messages).map(|m| m.seq).collect();
            assert_eq!(batch, vec![1, 2, 3, 4]);
        }
    }

    #[test]
    fn persistent_removed_cursor_survives_query_group_reload() {
        #[repr(C)]
        #[derive(bevy_ecs::component::Component, Clone, Copy, ModComponent)]
        struct EphemeralTag;

        let mut world = World::new();
        let cid = world.register_component::<EphemeralTag>();
        let mut registry = ModRegistry::new();
        let handle = registry
            .register_component::<EphemeralTag>(cid, Default::default())
            .expect("register EphemeralTag");
        let snapshot = registry.snapshot();

        let e1 = world.spawn(EphemeralTag).id();
        let e2 = world.spawn(EphemeralTag).id();
        let e3 = world.spawn(EphemeralTag).id();
        world.despawn(e1);
        world.despawn(e2);

        let term = BamTermDesc::new(handle, AccessMode::REMOVED_READ);
        let spec = BamQuerySpecOwned::new(vec![term]);

        {
            let mut group = CachedQueryGroup::new(spec.clone());
            group.set_system_identity(Some(Arc::from("mod_b")), "watch_removals");
            let mut groups = [group];
            initialize_query_groups_in_world(&mut world, snapshot, &mut groups)
                .expect("init removed gen 1");

            let cursor = groups[0].msg_cursors[0]
                .as_mut()
                .expect("removed cursor initialized")
                .downcast_mut::<MessageCursor<RemovedComponentEntity>>()
                .expect("MessageCursor<RemovedComponentEntity>");
            let removed = world.removed_components().get(cid).expect("removed stream");
            let first: Vec<bevy_ecs::entity::Entity> = cursor
                .read(removed)
                .take(1)
                .map(|ev| ev.clone().into())
                .collect();
            assert_eq!(first, vec![e1]);
        }

        world.despawn(e3);

        {
            let mut group = CachedQueryGroup::new(spec);
            group.set_system_identity(Some(Arc::from("mod_b")), "watch_removals");
            let mut groups = [group];
            initialize_query_groups_in_world(&mut world, snapshot, &mut groups)
                .expect("init removed gen 2");

            let cursor = groups[0].msg_cursors[0]
                .as_mut()
                .expect("removed cursor restored")
                .downcast_mut::<MessageCursor<RemovedComponentEntity>>()
                .expect("MessageCursor<RemovedComponentEntity>");
            let removed = world.removed_components().get(cid).expect("removed stream");
            let rest: Vec<bevy_ecs::entity::Entity> =
                cursor.read(removed).map(|ev| ev.clone().into()).collect();
            assert_eq!(rest, vec![e2, e3]);
        }
    }
}
