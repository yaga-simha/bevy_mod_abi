//! Synchronous and `World`-backed `FilteredAccessSet` validation and `QueryState` builder
//! (`C-01`, `C-05`, `P-01..P-03`, `P-08`, `B-08`).

use crate::ffi::iter::{QueryPlan, QueryTermMeta};
use crate::registry::RegistrySnapshot;
use crate::system::error_policy::SystemInitError;
use crate::system::query_group::{BamQuerySpecOwned, CachedQueryGroup};
use bevy_abi_mod_sys::{
    AccessMode, BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_SPARSE_SET, FfiResult,
    MAX_TERMS_PER_QUERY,
};
use bevy_ecs::component::{ComponentId, StorageType};
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
        let mut resolved: Vec<(ComponentId, AccessMode)> = Vec::with_capacity(group.terms.len());

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
            if term.mode.is_resource() != reg.is_resource() {
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
                | AccessMode::ADDED => {
                    has_entity_terms = true;
                }
                AccessMode::RESOURCE_READ => {
                    if system_res_writes.contains(&comp_id) {
                        return Err((
                            FfiResult::ACCESS_CONFLICT,
                            format!("conflicting resource read/write on '{}'", reg.name),
                        ));
                    }
                    if system_res_reads.insert(comp_id) {
                        system_res_order.push((comp_id, Arc::clone(&reg.name)));
                    }
                }
                AccessMode::RESOURCE_WRITE => {
                    if system_res_reads.contains(&comp_id) || system_res_writes.contains(&comp_id) {
                        return Err((
                            FfiResult::ACCESS_CONFLICT,
                            format!("conflicting resource write on '{}'", reg.name),
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

            resolved.push((comp_id, term.mode));
        }

        if group.terms.is_empty() || has_entity_terms {
            let mut fa = FilteredAccess::matches_everything();
            for &(comp_id, mode) in &resolved {
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
                    _ => {}
                }
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

    for group in query_groups.iter_mut() {
        group.query_state = None;
        group.reset_cache();

        if group.spec.terms.len() > MAX_TERMS_PER_QUERY {
            return Err(SystemInitError::TooManyTerms);
        }

        let mut read_components: HashSet<ComponentId> = HashSet::new();
        let mut write_components: HashSet<ComponentId> = HashSet::new();
        let mut has_entity_terms = false;
        let mut term_metas: Vec<QueryTermMeta> = Vec::with_capacity(group.spec.terms.len());

        for (col_idx, term) in group.spec.terms.iter().enumerate() {
            if !term.mode.is_valid() {
                return Err(SystemInitError::InvalidMode { index: col_idx });
            }
            let Some(reg) = snapshot.get_by_handle(term.component) else {
                return Err(SystemInitError::UnknownComponent(format!(
                    "BamComponent({})",
                    term.component.0
                )));
            };
            if term.mode.is_resource() != reg.is_resource() {
                return Err(SystemInitError::KindMismatch(reg.name.to_string()));
            }

            let comp_id = reg.component_id;
            let mut storage_flags = reg.storage_flags;
            if !reg.is_resource()
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
                | AccessMode::ADDED => {
                    has_entity_terms = true;
                }
                AccessMode::RESOURCE_READ => {
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
                AccessMode::RESOURCE_WRITE => {
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

            term_metas.push(QueryTermMeta {
                component: reg.handle,
                component_id: comp_id,
                mode: term.mode,
                uuid: reg.uuid,
                size: reg.size,
                align: reg.align,
                storage_flags,
            });
        }

        let plan = QueryPlan::from_terms(term_metas);
        group.sparse_scratch.ensure_terms(plan.terms.len());

        if group.spec.terms.is_empty() || has_entity_terms {
            let mut builder = QueryBuilder::<()>::new(world);
            for t in &plan.terms {
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
                    _ => {}
                }
            }

            let state = builder.build();
            let is_comp_dense = |id| {
                world
                    .components()
                    .get_info(id)
                    .is_some_and(|info| info.storage_type() == StorageType::Table)
            };
            group.is_dense = plan.sparse_data_mask == 0
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
