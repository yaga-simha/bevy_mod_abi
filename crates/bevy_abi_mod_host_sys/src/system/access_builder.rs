//! Synchronous and deferred `FilteredAccessSet` validation and `QueryState` builder.

use crate::ffi::iter::{MAX_QUERY_COMPONENTS, QueryColumnDesc};
use crate::registry::ModRegistry;
use crate::system::error_policy::SystemInitError;
use crate::system::query_group::{CachedQueryGroup, ComponentAccessDescOwned};
use bevy_abi_mod_sys::ComponentAccessMode;
use bevy_ecs::component::{ComponentId, StorageType};
use bevy_ecs::query::{FilteredAccess, FilteredAccessSet, QueryBuilder};
use bevy_ecs::world::World;
use std::collections::{HashMap, HashSet};

fn find_conflicting_component_name(
    names_by_id: &[String],
    existing: &FilteredAccessSet,
    candidate: &FilteredAccess,
) -> String {
    for (idx, name) in names_by_id.iter().enumerate() {
        let cid = ComponentId::new(idx);
        let cand_writes = candidate.access().has_write(cid);
        let cand_reads = candidate.access().has_read(cid);
        if !cand_reads && !cand_writes {
            continue;
        }
        let ex_writes = existing.combined_access().has_write(cid);
        let ex_reads = existing.combined_access().has_read(cid);
        if (cand_writes && (ex_reads || ex_writes)) || (ex_writes && cand_reads) {
            return name.clone();
        }
    }
    names_by_id
        .first()
        .cloned()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Validates all registry-independent query invariants synchronously during `register_system`:
/// term count bounds, valid `ComponentAccessMode` discriminants, consistent component-vs-resource
/// usage per name, and intra/inter-query `FilteredAccessSet` borrow compatibility.
pub(crate) fn validate_registry_independent_groups(
    groups: &[Vec<ComponentAccessDescOwned>],
) -> Result<(), SystemInitError> {
    for group in groups {
        if group.len() > MAX_QUERY_COMPONENTS {
            return Err(SystemInitError::TooManyTerms);
        }
        for (idx, desc) in group.iter().enumerate() {
            if !desc.mode.is_valid() {
                return Err(SystemInitError::InvalidMode { index: idx });
            }
        }
    }

    let mut id_map: HashMap<&str, (ComponentId, bool)> = HashMap::new();
    let mut names_by_id: Vec<String> = Vec::new();
    let mut system_res_reads: HashSet<ComponentId> = HashSet::new();
    let mut system_res_writes: HashSet<ComponentId> = HashSet::new();
    let mut system_res_order: Vec<ComponentId> = Vec::new();
    let mut access_set = FilteredAccessSet::new();

    for group in groups {
        let mut read_components: HashSet<ComponentId> = HashSet::new();
        let mut write_components: HashSet<ComponentId> = HashSet::new();
        let mut has_entity_terms = false;
        let mut resolved: Vec<(ComponentId, ComponentAccessMode)> = Vec::with_capacity(group.len());

        for desc in group {
            let is_res = desc.mode.is_resource();
            let comp_id = if let Some(&(existing_id, prev_is_res)) = id_map.get(desc.name.as_str())
            {
                if prev_is_res != is_res {
                    return Err(SystemInitError::KindMismatch(desc.name.clone()));
                }
                existing_id
            } else {
                let new_id = ComponentId::new(names_by_id.len());
                names_by_id.push(desc.name.clone());
                id_map.insert(desc.name.as_str(), (new_id, is_res));
                new_id
            };

            match desc.mode {
                ComponentAccessMode::READ | ComponentAccessMode::OPTIONAL_READ => {
                    has_entity_terms = true;
                    if write_components.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    read_components.insert(comp_id);
                }
                ComponentAccessMode::WRITE | ComponentAccessMode::OPTIONAL_WRITE => {
                    has_entity_terms = true;
                    if read_components.contains(&comp_id) || write_components.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    write_components.insert(comp_id);
                }
                ComponentAccessMode::FILTER_ONLY
                | ComponentAccessMode::WITHOUT
                | ComponentAccessMode::CHANGED
                | ComponentAccessMode::ADDED => {
                    has_entity_terms = true;
                }
                ComponentAccessMode::RESOURCE_READ => {
                    if system_res_writes.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    if system_res_reads.insert(comp_id) {
                        system_res_order.push(comp_id);
                    }
                }
                ComponentAccessMode::RESOURCE_WRITE => {
                    if system_res_reads.contains(&comp_id) || system_res_writes.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    system_res_writes.insert(comp_id);
                    system_res_order.push(comp_id);
                }
                _ => unreachable!("mode.is_valid() checked in pass 1"),
            }

            resolved.push((comp_id, desc.mode));
        }

        if group.is_empty() || has_entity_terms {
            let mut fa = FilteredAccess::matches_everything();
            for &(comp_id, mode) in &resolved {
                match mode {
                    ComponentAccessMode::READ => {
                        fa.add_read(comp_id);
                    }
                    ComponentAccessMode::WRITE => {
                        fa.add_write(comp_id);
                    }
                    ComponentAccessMode::OPTIONAL_READ => {
                        fa.access_mut().add_read(comp_id);
                    }
                    ComponentAccessMode::OPTIONAL_WRITE => {
                        fa.access_mut().add_write(comp_id);
                    }
                    ComponentAccessMode::FILTER_ONLY => {
                        fa.and_with(comp_id);
                    }
                    ComponentAccessMode::WITHOUT => {
                        fa.and_without(comp_id);
                    }
                    ComponentAccessMode::CHANGED | ComponentAccessMode::ADDED => {
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
                let name = find_conflicting_component_name(&names_by_id, &access_set, &fa);
                return Err(SystemInitError::SelfConflict {
                    a: name.clone(),
                    b: name,
                });
            }
            access_set.add(fa);
        }
    }

    for res_id in system_res_order {
        let writable = system_res_writes.contains(&res_id);
        let mut candidate_set = FilteredAccessSet::new();
        if writable {
            candidate_set.add_resource_write(res_id);
        } else {
            candidate_set.add_resource_read(res_id);
        }
        if !access_set.is_compatible(&candidate_set) {
            let name = names_by_id
                .get(SparseSetIndexHelper::index(res_id))
                .cloned()
                .unwrap_or_else(|| "resource".to_string());
            return Err(SystemInitError::SelfConflict {
                a: name.clone(),
                b: name,
            });
        }
        if writable {
            access_set.add_resource_write(res_id);
        } else {
            access_set.add_resource_read(res_id);
        }
    }

    Ok(())
}

struct SparseSetIndexHelper;
impl SparseSetIndexHelper {
    #[inline]
    fn index(id: ComponentId) -> usize {
        id.index()
    }
}

/// Resolves all query groups against `registry` and `world`, building `QueryState`s and the
/// combined Bevy [`FilteredAccessSet`] or returning a structured [`SystemInitError`].
pub(crate) fn initialize_query_groups_in_world(
    world: &mut World,
    registry: &ModRegistry,
    query_groups: &mut [CachedQueryGroup],
) -> Result<FilteredAccessSet, SystemInitError> {
    let mut access_set = FilteredAccessSet::new();
    let mut system_res_reads: HashSet<ComponentId> = HashSet::new();
    let mut system_res_writes: HashSet<ComponentId> = HashSet::new();
    let mut system_res_order: Vec<(ComponentId, String)> = Vec::new();
    let mut id_to_name: HashMap<ComponentId, String> = HashMap::new();

    for group in query_groups.iter_mut() {
        group.columns.clear();
        group.declared_resources.clear();
        group.query_state = None;
        group.sparse_column_mask = 0;
        group.reset_cache();

        if group.descs.len() > MAX_QUERY_COMPONENTS {
            return Err(SystemInitError::TooManyTerms);
        }
        for (col_idx, desc) in group.descs.iter().enumerate() {
            if !desc.mode.is_valid() {
                return Err(SystemInitError::InvalidMode { index: col_idx });
            }
        }

        let mut read_components: HashSet<ComponentId> = HashSet::new();
        let mut write_components: HashSet<ComponentId> = HashSet::new();
        let mut has_entity_terms = false;
        let mut resolved = Vec::with_capacity(group.descs.len());

        for (col_idx, desc) in group.descs.iter().enumerate() {
            let Some(reg) = registry.get_by_name(&desc.name) else {
                return Err(SystemInitError::UnknownComponent(desc.name.clone()));
            };

            if desc.mode.is_resource() != reg.is_resource {
                return Err(SystemInitError::KindMismatch(desc.name.clone()));
            }

            let comp_id = reg.component_id;
            let stride = reg.size;
            id_to_name
                .entry(comp_id)
                .or_insert_with(|| desc.name.clone());

            match desc.mode {
                ComponentAccessMode::READ | ComponentAccessMode::OPTIONAL_READ => {
                    has_entity_terms = true;
                    if write_components.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    read_components.insert(comp_id);
                }
                ComponentAccessMode::WRITE | ComponentAccessMode::OPTIONAL_WRITE => {
                    has_entity_terms = true;
                    if read_components.contains(&comp_id) || write_components.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    write_components.insert(comp_id);
                }
                ComponentAccessMode::FILTER_ONLY
                | ComponentAccessMode::WITHOUT
                | ComponentAccessMode::CHANGED
                | ComponentAccessMode::ADDED => {
                    has_entity_terms = true;
                }
                ComponentAccessMode::RESOURCE_READ => {
                    if system_res_writes.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    if system_res_reads.insert(comp_id) {
                        system_res_order.push((comp_id, desc.name.clone()));
                    }
                    if !group
                        .declared_resources
                        .iter()
                        .any(|&(cid, _)| cid == comp_id)
                    {
                        group.declared_resources.push((comp_id, false));
                    }
                }
                ComponentAccessMode::RESOURCE_WRITE => {
                    if system_res_reads.contains(&comp_id) || system_res_writes.contains(&comp_id) {
                        return Err(SystemInitError::SelfConflict {
                            a: desc.name.clone(),
                            b: desc.name.clone(),
                        });
                    }
                    system_res_writes.insert(comp_id);
                    system_res_order.push((comp_id, desc.name.clone()));
                    group.declared_resources.push((comp_id, true));
                }
                _ => {
                    return Err(SystemInitError::InvalidMode { index: col_idx });
                }
            }

            let is_sparse_storage = !desc.mode.is_resource()
                && world
                    .components()
                    .get_info(comp_id)
                    .is_none_or(|info| info.storage_type() == StorageType::SparseSet);

            if is_sparse_storage
                && matches!(
                    desc.mode,
                    ComponentAccessMode::READ
                        | ComponentAccessMode::WRITE
                        | ComponentAccessMode::OPTIONAL_READ
                        | ComponentAccessMode::OPTIONAL_WRITE
                        | ComponentAccessMode::CHANGED
                        | ComponentAccessMode::ADDED
                )
            {
                group.sparse_column_mask |= 1u32 << col_idx;
            }

            resolved.push((comp_id, desc.mode, stride));
        }

        for &(comp_id, mode, stride) in &resolved {
            group.columns.push(QueryColumnDesc {
                component_id: comp_id,
                mode,
                stride,
            });
        }

        if group.descs.is_empty() || has_entity_terms {
            let mut builder = QueryBuilder::<()>::new(world);
            for &(comp_id, mode, _stride) in &resolved {
                match mode {
                    ComponentAccessMode::READ => {
                        builder.ref_id(comp_id);
                    }
                    ComponentAccessMode::WRITE => {
                        builder.mut_id(comp_id);
                    }
                    ComponentAccessMode::OPTIONAL_READ => {
                        builder.optional(|b| {
                            b.ref_id(comp_id);
                        });
                    }
                    ComponentAccessMode::OPTIONAL_WRITE => {
                        builder.optional(|b| {
                            b.mut_id(comp_id);
                        });
                    }
                    ComponentAccessMode::FILTER_ONLY => {
                        builder.with_id(comp_id);
                    }
                    ComponentAccessMode::WITHOUT => {
                        builder.without_id(comp_id);
                    }
                    ComponentAccessMode::CHANGED | ComponentAccessMode::ADDED => {
                        if write_components.contains(&comp_id) {
                            builder.with_id(comp_id);
                        } else {
                            builder.ref_id(comp_id);
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
            group.is_dense = group.sparse_column_mask == 0
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
                let conflict_name = id_to_name
                    .iter()
                    .find(|&(&cid, _)| {
                        let cw = filtered_access.access().has_write(cid);
                        let cr = filtered_access.access().has_read(cid);
                        let ew = access_set.combined_access().has_write(cid);
                        let er = access_set.combined_access().has_read(cid);
                        (cw && (er || ew)) || (ew && cr)
                    })
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                return Err(SystemInitError::SelfConflict {
                    a: conflict_name.clone(),
                    b: conflict_name,
                });
            }

            access_set.add(filtered_access);
        } else {
            group.is_dense = true;
            group.query_state = None;
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
            return Err(SystemInitError::SelfConflict {
                a: res_name.clone(),
                b: res_name,
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
