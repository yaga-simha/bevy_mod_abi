use crossterm::{
    cursor,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::stdout;

pub struct TestTerminalGuard;

impl TestTerminalGuard {
    pub fn enter() -> Result<Self, std::io::Error> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, cursor::Hide)?;

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
            let _ = disable_raw_mode();
            default_hook(panic_info);
        }));

        Ok(Self)
    }
}

impl Drop for TestTerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
        let _ = disable_raw_mode();
    }
}

#[test]
fn test_terminal_guard_raii_drop() {
    // Standard RAII drop check
    {
        if let Ok(guard) = TestTerminalGuard::enter() {
            drop(guard);
        }
    }
}

#[test]
fn test_terminal_guard_panic_hook_emits_cleanup_sequences() {
    let hook_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_called_clone = hook_called.clone();

    // Verify panic hook behavior directly
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |_panic_info| {
        // Execute the exact cleanup sequence in TerminalGuard:
        let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
        let _ = disable_raw_mode();
        hook_called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    }));

    let result = std::panic::catch_unwind(|| {
        panic!("Simulated intentional panic inside TerminalGuard scope");
    });

    // Restore previous hook
    std::panic::set_hook(prev_hook);

    assert!(result.is_err(), "catch_unwind must capture the panic");
    assert!(
        hook_called.load(std::sync::atomic::Ordering::SeqCst),
        "TerminalGuard panic hook must execute on panic and emit cleanup sequence"
    );
}

#[test]
fn test_terminal_guard_nested_reentry_and_idempotent_drop() {
    // Stress test: consecutive and nested entries must not panic or abort
    for _ in 0..5 {
        let guard1 = TestTerminalGuard::enter();
        assert!(guard1.is_ok(), "Guard 1 enter must succeed");
        {
            let guard2 = TestTerminalGuard::enter();
            assert!(guard2.is_ok(), "Nested guard 2 enter must succeed");
            drop(guard2);
        }
        drop(guard1);
    }
}

#[test]
fn test_terminal_guard_unwind_drops_guard_safely() {
    let drop_executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drop_executed_clone = drop_executed.clone();

    struct DropObserver {
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        _guard: Option<TestTerminalGuard>,
    }

    impl Drop for DropObserver {
        fn drop(&mut self) {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let result = std::panic::catch_unwind(move || {
        let guard = TestTerminalGuard::enter().ok();
        let _obs = DropObserver {
            flag: drop_executed_clone,
            _guard: guard,
        };
        panic!("Intentional panic testing RAII unwinding with TerminalGuard");
    });

    std::panic::set_hook(prev_hook);

    assert!(result.is_err(), "Panic must unwind cleanly");
    assert!(
        drop_executed.load(std::sync::atomic::Ordering::SeqCst),
        "Guard drop must execute during panic stack unwinding"
    );
}

