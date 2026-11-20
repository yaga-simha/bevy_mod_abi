//! Synchronous and `World`-backed `FilteredAccessSet` validation and `QueryState` builder.
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

enum AccessPlanError {
    TooManyTerms {
        q_idx: usize,
    },
    TermValidation {
        q_idx: usize,
        t_idx: usize,
        code: FfiResult,
    },
    InvalidMode {
        q_idx: usize,
        t_idx: usize,
        mode: AccessMode,
        or_group: u8,
    },
    UnknownComponent {
        q_idx: usize,
        t_idx: usize,
        handle: u32,
    },
    KindMismatch {
        q_idx: usize,
        t_idx: usize,
        name: String,
        mode: AccessMode,
    },
    ImmutableWrite {
        q_idx: usize,
        t_idx: usize,
        name: String,
    },
    IntraConflict {
        name: String,
        detail: &'static str,
    },
    InterConflict {
        name: String,
    },
    ResourceConflict {
        name: String,
    },
}

impl AccessPlanError {
    fn into_ffi(self) -> (FfiResult, String) {
        match self {
            Self::TooManyTerms { q_idx } => (
                FfiResult::CAPACITY_EXCEEDED,
                format!("query {q_idx} exceeds MAX_TERMS_PER_QUERY (32)"),
            ),
            Self::TermValidation { q_idx, t_idx, code } => (
                code,
                format!("query {q_idx} term {t_idx} failed validation ({code})"),
            ),
            Self::InvalidMode {
                q_idx,
                t_idx,
                mode,
                or_group,
            } if or_group != 0 => (
                FfiResult::INVALID_ARGUMENT,
                format!(
                    "query {q_idx} term {t_idx} uses invalid mode {mode:?} inside or_group={or_group}"
                ),
            ),
            Self::InvalidMode { mode, .. } => (
                FfiResult::INVALID_ARGUMENT,
                format!("invalid AccessMode({})", mode.0),
            ),
            Self::UnknownComponent {
                q_idx,
                t_idx,
                handle,
            } => (
                FfiResult::INVALID_COMPONENT,
                format!("query {q_idx} term {t_idx} references unknown BamComponent({handle})"),
            ),
            Self::KindMismatch {
                q_idx,
                t_idx,
                name,
                mode,
            } => (
                FfiResult::INVALID_COMPONENT,
                format!(
                    "query {q_idx} term {t_idx} ('{name}') kind mismatch between mode {mode:?} and registration"
                ),
            ),
            Self::ImmutableWrite { q_idx, t_idx, name } => (
                FfiResult::PERMISSION_DENIED,
                format!(
                    "query {q_idx} term {t_idx} requests mutable access on immutable component '{name}'"
                ),
            ),
            Self::IntraConflict { name, detail } => {
                (FfiResult::ACCESS_CONFLICT, format!("{detail} '{name}'"))
            }
            Self::InterConflict { name } => (
                FfiResult::ACCESS_CONFLICT,
                format!("inter-query borrow conflict on '{name}'"),
            ),
            Self::ResourceConflict { name } => (
                FfiResult::ACCESS_CONFLICT,
                format!("resource access conflict on '{name}'"),
            ),
        }
    }

    fn into_init_error(self) -> SystemInitError {
        match self {
            Self::TooManyTerms { .. } => SystemInitError::TooManyTerms,
            Self::TermValidation { t_idx, .. } | Self::InvalidMode { t_idx, .. } => {
                SystemInitError::InvalidMode { index: t_idx }
            }
            Self::UnknownComponent { handle, .. } => {
                SystemInitError::UnknownComponent(format!("BamComponent({handle})"))
            }
            Self::KindMismatch { name, .. } => SystemInitError::KindMismatch(name),
            Self::ImmutableWrite { name, .. } => SystemInitError::ImmutableWrite(name),
            Self::IntraConflict { name, .. }
            | Self::InterConflict { name }
            | Self::ResourceConflict { name } => SystemInitError::SelfConflict {
                a: name.clone(),
                b: name,
            },
        }
    }
}

#[derive(Default)]
struct SystemAccessAccumulator {
    access_set: FilteredAccessSet,
    res_reads: HashSet<ComponentId>,
    res_writes: HashSet<ComponentId>,
    res_order: Vec<(ComponentId, Arc<str>)>,
    id_to_name: HashMap<ComponentId, Arc<str>>,
}

struct ValidatedGroup {
    has_entity_terms: bool,
    write_components: HashSet<ComponentId>,
    term_metas: Vec<QueryTermMeta>,
    term_or_groups: Vec<u8>,
    or_group_order: Vec<u8>,
}

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

fn build_or_branch_access(
    terms: impl Iterator<Item = (ComponentId, AccessMode)>,
    write_components: &HashSet<ComponentId>,
) -> FilteredAccess {
    let mut or_access = FilteredAccess::matches_nothing();
    for (comp_id, mode) in terms {
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
    or_access
}

impl SystemAccessAccumulator {
    fn validate_group(
        &mut self,
        q_idx: usize,
        spec: &BamQuerySpecOwned,
        snapshot: &RegistrySnapshot,
        world: Option<&World>,
        strict_term_validate: bool,
    ) -> Result<ValidatedGroup, AccessPlanError> {
        if spec.terms.len() > MAX_TERMS_PER_QUERY {
            return Err(AccessPlanError::TooManyTerms { q_idx });
        }

        let mut read_components: HashSet<ComponentId> = HashSet::new();
        let mut write_components: HashSet<ComponentId> = HashSet::new();
        let mut has_entity_terms = false;
        let mut term_metas = Vec::with_capacity(spec.terms.len());
        let mut term_or_groups = Vec::with_capacity(spec.terms.len());
        let mut or_group_order = Vec::new();

        for (t_idx, term) in spec.terms.iter().enumerate() {
            if strict_term_validate {
                let v = term.validate();
                if v.is_err() {
                    return Err(AccessPlanError::TermValidation {
                        q_idx,
                        t_idx,
                        code: v,
                    });
                }
            } else if !term.mode.is_valid() {
                return Err(AccessPlanError::InvalidMode {
                    q_idx,
                    t_idx,
                    mode: term.mode,
                    or_group: 0,
                });
            }

            if term.or_group != 0 {
                if !matches!(
                    term.mode,
                    AccessMode::FILTER_ONLY
                        | AccessMode::WITHOUT
                        | AccessMode::CHANGED
                        | AccessMode::ADDED
                ) {
                    return Err(AccessPlanError::InvalidMode {
                        q_idx,
                        t_idx,
                        mode: term.mode,
                        or_group: term.or_group,
                    });
                }
                if !or_group_order.contains(&term.or_group) {
                    or_group_order.push(term.or_group);
                }
            }

            let Some(reg) = snapshot.get_by_handle(term.component) else {
                return Err(AccessPlanError::UnknownComponent {
                    q_idx,
                    t_idx,
                    handle: term.component.0,
                });
            };
            if term.mode.is_resource() != reg.is_resource()
                || term.mode.is_message() != reg.is_message()
            {
                return Err(AccessPlanError::KindMismatch {
                    q_idx,
                    t_idx,
                    name: reg.name.to_string(),
                    mode: term.mode,
                });
            }

            let comp_id = reg.component_id;
            let mut storage_flags = reg.storage_flags;
            if let Some(w) = world
                && !reg.is_resource()
                && !reg.is_message()
                && let Some(info) = w.components().get_info(comp_id)
            {
                if info.storage_type() == StorageType::SparseSet {
                    storage_flags |= BAM_STORAGE_FLAG_SPARSE_SET;
                }
                if !info.mutable() {
                    storage_flags |= BAM_STORAGE_FLAG_IMMUTABLE;
                }
            }
            if (storage_flags & BAM_STORAGE_FLAG_IMMUTABLE) != 0 && term.mode.is_mutable() {
                return Err(AccessPlanError::ImmutableWrite {
                    q_idx,
                    t_idx,
                    name: reg.name.to_string(),
                });
            }

            self.id_to_name
                .entry(comp_id)
                .or_insert_with(|| Arc::clone(&reg.name));

            match term.mode {
                AccessMode::READ | AccessMode::OPTIONAL_READ => {
                    has_entity_terms = true;
                    if write_components.contains(&comp_id) {
                        return Err(AccessPlanError::IntraConflict {
                            name: reg.name.to_string(),
                            detail: "intra-query read/write conflict on component",
                        });
                    }
                    read_components.insert(comp_id);
                }
                AccessMode::WRITE | AccessMode::OPTIONAL_WRITE => {
                    has_entity_terms = true;
                    if read_components.contains(&comp_id) || write_components.contains(&comp_id) {
                        return Err(AccessPlanError::IntraConflict {
                            name: reg.name.to_string(),
                            detail: "intra-query write conflict on component",
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
                    if self.res_writes.contains(&comp_id) {
                        return Err(AccessPlanError::IntraConflict {
                            name: reg.name.to_string(),
                            detail: "conflicting resource/message read/write on",
                        });
                    }
                    if self.res_reads.insert(comp_id) {
                        self.res_order.push((comp_id, Arc::clone(&reg.name)));
                    }
                }
                AccessMode::RESOURCE_WRITE | AccessMode::MESSAGE_WRITE => {
                    if self.res_reads.contains(&comp_id) || self.res_writes.contains(&comp_id) {
                        return Err(AccessPlanError::IntraConflict {
                            name: reg.name.to_string(),
                            detail: "conflicting resource/message write on",
                        });
                    }
                    self.res_writes.insert(comp_id);
                    self.res_order.push((comp_id, Arc::clone(&reg.name)));
                }
                _ => {
                    return Err(AccessPlanError::InvalidMode {
                        q_idx,
                        t_idx,
                        mode: term.mode,
                        or_group: 0,
                    });
                }
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

        Ok(ValidatedGroup {
            has_entity_terms,
            write_components,
            term_metas,
            term_or_groups,
            or_group_order,
        })
    }

    fn add_entity_access(&mut self, fa: FilteredAccess) -> Result<(), AccessPlanError> {
        let mut candidate_set = FilteredAccessSet::new();
        candidate_set.add(fa.clone());
        if !self.access_set.is_compatible(&candidate_set) {
            let name = find_conflicting_name(&self.id_to_name, &self.access_set, &fa);
            return Err(AccessPlanError::InterConflict { name });
        }
        self.access_set.add(fa);
        Ok(())
    }

    fn finalize_resources(mut self) -> Result<FilteredAccessSet, AccessPlanError> {
        for (res_id, res_name) in self.res_order {
            let writable = self.res_writes.contains(&res_id);
            let mut candidate_set = FilteredAccessSet::new();
            if writable {
                candidate_set.add_resource_write(res_id);
            } else {
                candidate_set.add_resource_read(res_id);
            }
            if !self.access_set.is_compatible(&candidate_set) {
                return Err(AccessPlanError::ResourceConflict {
                    name: res_name.to_string(),
                });
            }
            if writable {
                self.access_set.add_resource_write(res_id);
            } else {
                self.access_set.add_resource_read(res_id);
            }
        }
        Ok(self.access_set)
    }
}

/// Validates a set of [`BamQuerySpecOwned`] clauses synchronously during `host_register_system`
/// against `snapshot`, checking handle existence, immutability, component/resource kind match,
/// and intra/inter-query `FilteredAccessSet` borrow compatibility.
pub(crate) fn validate_system_queries_against_snapshot(
    snapshot: &RegistrySnapshot,
    groups: &[BamQuerySpecOwned],
) -> Result<(), (FfiResult, String)> {
    let mut acc = SystemAccessAccumulator::default();

    for (q_idx, group) in groups.iter().enumerate() {
        let vg = acc
            .validate_group(q_idx, group, snapshot, None, true)
            .map_err(AccessPlanError::into_ffi)?;

        if group.terms.is_empty() || vg.has_entity_terms {
            let mut fa = FilteredAccess::matches_everything();
            for (meta, &or_group) in vg.term_metas.iter().zip(&vg.term_or_groups) {
                if or_group != 0 {
                    continue;
                }
                let comp_id = meta.component_id;
                match meta.mode {
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
                        if vg.write_components.contains(&comp_id) {
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
            for &group_id in &vg.or_group_order {
                let or_access = build_or_branch_access(
                    vg.term_metas
                        .iter()
                        .zip(&vg.term_or_groups)
                        .filter(|&(_, &og)| og == group_id)
                        .map(|(m, _)| (m.component_id, m.mode)),
                    &vg.write_components,
                );
                fa.extend(&or_access);
            }
            acc.add_entity_access(fa)
                .map_err(AccessPlanError::into_ffi)?;
        }
    }

    acc.finalize_resources()
        .map(|_| ())
        .map_err(AccessPlanError::into_ffi)
}

fn restore_or_init_cursor(
    group: &mut CachedQueryGroup,
    query_idx: usize,
    col_idx: usize,
    uuid: bevy_abi_mod_sys::ComponentUuid,
    group_store: Option<&crate::system::query_group::SharedCursorStore>,
    fallback: impl FnOnce() -> Option<Box<dyn core::any::Any + Send + Sync>>,
) {
    group.cursor_uuids[col_idx] = Some(uuid);
    if group.msg_cursors[col_idx].is_none()
        && let (Some((mod_ns, sys_name)), Some(store)) = (&group.owner_system_key, group_store)
    {
        let key = PersistentCursorKey {
            mod_namespace: mod_ns.clone(),
            system_name: Arc::clone(sys_name),
            query_idx,
            term_idx: col_idx,
            component_uuid: uuid,
        };
        group.msg_cursors[col_idx] = PersistentMessageCursors::take_from_store(store, &key);
    }
    if group.msg_cursors[col_idx].is_none() {
        group.msg_cursors[col_idx] = fallback();
    }
}

/// Resolves all query groups against `snapshot` and `world`, precomputing [`QueryPlan`]s,
/// `QueryState`s, and the combined Bevy [`FilteredAccessSet`].
pub(crate) fn initialize_query_groups_in_world(
    world: &mut World,
    snapshot: &RegistrySnapshot,
    query_groups: &mut [CachedQueryGroup],
) -> Result<FilteredAccessSet, SystemInitError> {
    let mut acc = SystemAccessAccumulator::default();
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

        let vg = acc
            .validate_group(query_idx, &group.spec, snapshot, Some(world), false)
            .map_err(AccessPlanError::into_init_error)?;

        if group.msg_cursors.len() < vg.term_metas.len() {
            group.msg_cursors.resize_with(vg.term_metas.len(), || None);
        }
        for (col_idx, meta) in vg.term_metas.iter().enumerate() {
            if meta.mode.is_message() {
                let create_fn = snapshot
                    .get_by_handle(meta.component)
                    .and_then(|r| r.create_msg_cursor_fn);
                restore_or_init_cursor(
                    group,
                    query_idx,
                    col_idx,
                    meta.uuid,
                    group_store.as_ref(),
                    || create_fn.map(|f| f()),
                );
            } else if meta.mode == AccessMode::REMOVED_READ {
                restore_or_init_cursor(
                    group,
                    query_idx,
                    col_idx,
                    meta.uuid,
                    group_store.as_ref(),
                    || Some(Box::new(MessageCursor::<RemovedComponentEntity>::default())),
                );
            } else {
                group.cursor_uuids[col_idx] = None;
                group.msg_cursors[col_idx] = None;
            }
        }

        let plan = QueryPlan::from_terms_with_or_groups(vg.term_metas, &vg.term_or_groups);
        group.sparse_scratch.ensure_terms(plan.terms.len());

        if group.spec.terms.is_empty() || vg.has_entity_terms {
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
                        if vg.write_components.contains(&t.component_id) {
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
                let or_access = build_or_branch_access(
                    or_group.terms.iter().map(|c| (c.component_id, c.mode)),
                    &vg.write_components,
                );
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
            acc.add_entity_access(filtered_access)
                .map_err(AccessPlanError::into_init_error)?;
        } else {
            group.is_dense = true;
            group.query_state = None;
        }

        group.plan = plan;
    }

    acc.finalize_resources()
        .map_err(AccessPlanError::into_init_error)
}

#[cfg(test)]
#[path = "../../tests/unit/access_builder_tests.rs"]
mod tests;
