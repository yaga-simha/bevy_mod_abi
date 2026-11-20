use super::*;
use std::sync::Mutex;

struct DummyBudgetHook {
    limits: CommandQuotaLimits,
    counter: Arc<AtomicUsize>,
    recorded: Mutex<Vec<GuestSystemRunMetrics>>,
    trigger_quarantine: bool,
}

impl ModBudgetHook for DummyBudgetHook {
    fn prepare_system_quotas(
        &self,
        _mod_id: Option<ModId>,
        _mod_name: &str,
    ) -> (CommandQuotaLimits, Option<Arc<AtomicUsize>>) {
        (self.limits, Some(Arc::clone(&self.counter)))
    }

    fn record_system_run(&self, metrics: GuestSystemRunMetrics) -> BudgetWatchdogAction {
        self.recorded.lock().unwrap().push(metrics);
        BudgetWatchdogAction {
            quarantine: self.trigger_quarantine,
            reason: self
                .trigger_quarantine
                .then(|| String::from("p99 budget exceeded")),
        }
    }
}

#[test]
fn mod_budget_runtime_delegates_prepare_and_record() {
    let hook = Arc::new(DummyBudgetHook {
        limits: CommandQuotaLimits {
            max_arena_bytes: Some(256),
            max_spawns_per_frame: Some(4),
        },
        counter: Arc::new(AtomicUsize::new(0)),
        recorded: Mutex::new(Vec::new()),
        trigger_quarantine: true,
    });
    let runtime = ModBudgetRuntime::new(Arc::clone(&hook) as Arc<dyn ModBudgetHook>);
    assert!(format!("{runtime:?}").contains("ModBudgetRuntime"));

    let (limits, shared) = runtime.prepare_system_quotas(Some(ModId(9)), "test_mod");
    assert_eq!(limits.max_arena_bytes, Some(256));
    assert_eq!(limits.max_spawns_per_frame, Some(4));
    assert!(shared.is_some());

    let action = runtime.record_system_run(GuestSystemRunMetrics {
        mod_id: Some(ModId(9)),
        mod_name: Arc::from("test_mod"),
        system_name: Arc::from("tick"),
        duration_us: 999,
        matched_entities: 10,
        matched_tables: 2,
        skipped_summary_tick_tables: 1,
        chunks_yielded: 1,
        commands_queued: 3,
        arena_bytes_used: 128,
        spawns_queued: 2,
        arena_breaches: 0,
        spawn_breaches: 1,
        faulted: false,
    });
    assert!(action.quarantine);
    assert_eq!(action.reason.as_deref(), Some("p99 budget exceeded"));
    assert_eq!(hook.recorded.lock().unwrap().len(), 1);
}

#[test]
fn error_policy_display_quarantine_fault_guard_drop_and_init_codes() {
    let world = World::new();

    // `SystemInitError::status_code` coverage
    assert_eq!(
        SystemInitError::TooManyTerms.status_code(),
        FfiResult::CAPACITY_EXCEEDED
    );
    assert_eq!(
        SystemInitError::InvalidMode { index: 0 }.status_code(),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(
        SystemInitError::UnknownComponent(String::from("x")).status_code(),
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        SystemInitError::KindMismatch(String::from("x")).status_code(),
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        SystemInitError::ImmutableWrite(String::from("x")).status_code(),
        FfiResult::PERMISSION_DENIED
    );
    assert_eq!(
        SystemInitError::SelfConflict {
            a: String::from("a"),
            b: String::from("b")
        }
        .status_code(),
        FfiResult::ACCESS_CONFLICT
    );

    // `ModSystemError` Display (with and without `last_error`) and `dispatch_mod_fault_to_fallback_handler`
    let err_plain = ModSystemError {
        mod_id: None,
        mod_namespace: None,
        generation: 1,
        system_name: String::from("plain_sys"),
        status: FfiResult::INVALID_STATE,
        panicked: false,
        last_error: None,
    };
    assert!(format!("{err_plain}").contains("unscoped"));
    let _ = err_plain.into_bevy_error();

    let err_detail = ModSystemError {
        mod_id: Some(ModId(3)),
        mod_namespace: Some(String::from("ns")),
        generation: 2,
        system_name: String::from("detail_sys"),
        status: FfiResult::PANIC_CAUGHT,
        panicked: true,
        last_error: Some(String::from("panic detail")),
    };
    assert!(format!("{err_detail}").contains("panic detail"));
    dispatch_mod_fault_to_fallback_handler(&world, err_detail, Tick::new(1));

    // `ModQuarantine` lifecycle
    let mut q = ModQuarantine::new();
    let flag = Arc::new(AtomicBool::new(false));
    q.register_system(ModId(1), Arc::from("q_sys"), Arc::clone(&flag));
    assert!(q.is_quarantined(ModId(1), "q_sys"));
    let list: Vec<_> = q.quarantined_systems().collect();
    assert_eq!(list, vec![(ModId(1), "q_sys")]);
    assert!(q.reenable(ModId(1), "q_sys"));
    assert!(!q.reenable(ModId(1), "q_sys"));
    assert!(!q.reenable(ModId(99), "missing"));
    q.unregister_mod(ModId(1));
    assert!(!q.is_quarantined(ModId(1), "q_sys"));

    // `ActiveGuestFaultGuard::drop` restores previous slot when dropped without `finish()`,
    // and `record_active_guest_fault` returns `None` for non-matching `host_ctx_ptr`.
    let mut ctx_a = 1u64;
    let mut ctx_b = 2u64;
    let ptr_a = (&raw mut ctx_a).cast::<BamHostCtx>();
    let ptr_b = (&raw mut ctx_b).cast::<BamHostCtx>();
    let outer = ActiveGuestFaultGuard::enter(ptr_a, Some(ModId(1)), None, 0, Arc::from("outer"));
    {
        let _inner =
            ActiveGuestFaultGuard::enter(ptr_b, Some(ModId(2)), None, 0, Arc::from("inner"));
        assert!(record_active_guest_fault(ptr_a, "wrong ctx").is_none());
        assert!(record_active_guest_fault(ptr_b, "inner msg").is_some());
    }
    assert!(record_active_guest_fault(ptr_a, "outer msg").is_some());
    assert_eq!(outer.finish().as_deref(), Some("outer msg"));
}
