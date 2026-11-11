//! Thread-local active registry guard and global static host symbol table.

use crate::loader::ModId;
use crate::registry::ModRegistry;
use crate::sync_ptr::SymbolAddress;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

/// Symbol table entry recording both the resolved address and its optional owning [`ModId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SymbolEntry {
    pub(crate) addr: SymbolAddress,
    pub(crate) owner: Option<ModId>,
}

thread_local! {
    /// Active registry bound to the executing thread during mod initialization and system runs.
    pub(crate) static CURRENT_REGISTRY: RefCell<Option<Arc<ModRegistry>>> = const { RefCell::new(None) };
}

/// Global fallback symbol table containing strictly host-owned `'static` symbols when no `World`
/// guard is active on the calling thread. Guest-owned VTables are never inserted here.
pub(crate) static GLOBAL_SYMBOLS: LazyLock<RwLock<HashMap<String, SymbolAddress>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// RAII guard that restores the previous thread-local `CURRENT_REGISTRY` upon drop.
#[cfg_attr(not(any(test, feature = "test-utils")), allow(unreachable_pub))]
pub struct CurrentRegistryGuard {
    pub(crate) prev: Option<Arc<ModRegistry>>,
}

impl Drop for CurrentRegistryGuard {
    fn drop(&mut self) {
        CURRENT_REGISTRY.with(|cell| {
            *cell.borrow_mut() = self.prev.take();
        });
    }
}
