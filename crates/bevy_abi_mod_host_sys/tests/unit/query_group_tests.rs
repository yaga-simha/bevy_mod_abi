use super::*;
use bevy_ecs::world::World;

#[test]
fn cached_query_group_drop_guards_and_empty_update_archetypes() {
    let world = World::new();
    let store = PersistentMessageCursors::default().shared_store();

    // 1. `persistent_store` present, `owner_system_key == None` -> drop skips persistence.
    {
        let mut g = CachedQueryGroup::new(BamQuerySpecOwned::default());
        g.persistent_store = Some((Arc::clone(&store), 0));
        g.msg_cursors.push(Some(Box::new(1u32)));
        g.update_archetypes(world.as_unsafe_world_cell_readonly());
        g.reset_cache();
    }
    assert!(store.lock().unwrap().is_empty());

    // 2. `owner_system_key` present, `cursor_uuids` has `None` -> drop skips `None` UUID entry.
    {
        let mut g = CachedQueryGroup::new(BamQuerySpecOwned::default());
        g.set_system_identity(Some(Arc::from("mod_qg")), "sys_qg");
        g.persistent_store = Some((Arc::clone(&store), 0));
        g.msg_cursors.push(None);
        g.msg_cursors.push(Some(Box::new(2u32)));
        g.cursor_uuids = vec![None, None];
    }
    assert!(store.lock().unwrap().is_empty());
}
