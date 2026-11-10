# ==============================================================================
# bevy_abi_mod: Root Makefile
# Unified Developer Experience for Building, Testing, Benchmarking & Verifying
# ==============================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help

# Colors for terminal output
BOLD  := \033[1m
GREEN := \033[32m
CYAN  := \033[36m
RESET := \033[0m

.PHONY: help
help: ## Display this help message
	@echo -e "$(BOLD)bevy_abi_mod Development Commands$(RESET)"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  $(CYAN)%-18s$(RESET) %s\n", $$1, $$2}'
	@echo ""

.PHONY: build
build: build-mods build-bench-mods ## Build the entire workspace and all asymmetric guest mods
	@echo -e "$(GREEN)[BUILD]$(RESET) Compiling workspace..."
	cargo build --workspace

.PHONY: build-mods
build-mods: ## Build test asymmetric C99 (.so/.dylib) and Rust (.cdylib) guest mods
	@echo -e "$(GREEN)[BUILD-MODS]$(RESET) Compiling asymmetric C99 guest mod..."
	$(MAKE) -C tests/asymmetric_c_mod
	@echo -e "$(GREEN)[BUILD-MODS]$(RESET) Compiling asymmetric Rust guest mod (independent Cargo lockfile)..."
	cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release

.PHONY: build-bench-mods
build-bench-mods: ## Build benchmark guest mods (C99 debug/release & Rust cdylib debug/release)
	@echo -e "$(GREEN)[BUILD-BENCH-MODS]$(RESET) Compiling benchmark C99 mods (-O0 debug & -O3 release)..."
	$(MAKE) -C benches/c_abi_c99_mod
	@echo -e "$(GREEN)[BUILD-BENCH-MODS]$(RESET) Compiling benchmark C-ABI Rust mod (debug & release)..."
	cargo build --manifest-path benches/c_abi_rust_mod/Cargo.toml
	cargo build --manifest-path benches/c_abi_rust_mod/Cargo.toml --release

.PHONY: build-game-mods
build-game-mods: ## Build the 3 distinct guest mods for Example 4 (Gravity, Rainbow, Vortex)
	@echo -e "$(GREEN)[BUILD-GAME-MODS]$(RESET) Compiling Gravity guest mod..."
	cargo build --manifest-path examples/04_visual_game/mods/mod_gravity/Cargo.toml --release
	@echo -e "$(GREEN)[BUILD-GAME-MODS]$(RESET) Compiling Rainbow guest mod..."
	cargo build --manifest-path examples/04_visual_game/mods/mod_rainbow/Cargo.toml --release
	@echo -e "$(GREEN)[BUILD-GAME-MODS]$(RESET) Compiling Vortex guest mod..."
	cargo build --manifest-path examples/04_visual_game/mods/mod_vortex/Cargo.toml --release

.PHONY: build-example-mods
build-example-mods: build-game-mods ## Build guest mods for all progression examples (Stage 1, 2, 3, 4)
	@echo -e "$(GREEN)[BUILD-EXAMPLE-MODS]$(RESET) Compiling Example 1 guest mod..."
	cargo build --manifest-path examples/01_minimal_handshake/guest_mod/Cargo.toml --release
	@echo -e "$(GREEN)[BUILD-EXAMPLE-MODS]$(RESET) Compiling Example 2 guest mod..."
	cargo build --manifest-path examples/02_multi_system_pipeline/guest_mod/Cargo.toml --release
	@echo -e "$(GREEN)[BUILD-EXAMPLE-MODS]$(RESET) Compiling Example 3 C99 guest mod..."
	$(MAKE) -C examples/03_cross_language_interop/c_mod
	@echo -e "$(GREEN)[BUILD-EXAMPLE-MODS]$(RESET) Compiling Example 3 Rust guest mod..."
	cargo build --manifest-path examples/03_cross_language_interop/rust_mod/Cargo.toml --release

.PHONY: example-1
example-1: ## Run Stage 1 Minimal Handshake PoC (<100 LOC)
	@echo -e "$(GREEN)[EXAMPLE 1]$(RESET) Building and running Stage 1 Minimal Handshake..."
	cargo build --manifest-path examples/01_minimal_handshake/guest_mod/Cargo.toml --release
	cargo run --manifest-path examples/01_minimal_handshake/Cargo.toml

.PHONY: example-2
example-2: ## Run Stage 2 Multi-System Pipeline (Queries, Filters, Commands, Ticks)
	@echo -e "$(GREEN)[EXAMPLE 2]$(RESET) Building and running Stage 2 Multi-System Pipeline..."
	cargo build --manifest-path examples/02_multi_system_pipeline/guest_mod/Cargo.toml --release
	cargo run --manifest-path examples/02_multi_system_pipeline/Cargo.toml

.PHONY: example-3
example-3: ## Run Stage 3 Cross-Language C99 + Rust Interoperability
	@echo -e "$(GREEN)[EXAMPLE 3]$(RESET) Building and running Stage 3 Cross-Language Interop..."
	$(MAKE) -C examples/03_cross_language_interop/c_mod
	cargo build --manifest-path examples/03_cross_language_interop/rust_mod/Cargo.toml --release
	cargo run --manifest-path examples/03_cross_language_interop/Cargo.toml

.PHONY: example-4 game
example-4: build-game-mods ## Run Stage 4 2D Visual Game with Runtime Mod Reloading
	@echo -e "$(GREEN)[EXAMPLE 4]$(RESET) Launching Stage 4 Visual Kinetic Arena Game..."
	cargo run --manifest-path examples/04_visual_game/Cargo.toml

game: example-4 ## Alias for example-4

.PHONY: example-4-test
example-4-test: build-game-mods ## Run Stage 4 Visual Game in automated headless test mode
	@echo -e "$(GREEN)[EXAMPLE 4 TEST]$(RESET) Running Stage 4 in headless verification mode..."
	cargo run --manifest-path examples/04_visual_game/Cargo.toml -- --test-mode --ticks 30

.PHONY: examples
examples: example-1 example-2 example-3 example-4-test ## Run all progression examples sequentially (Stages 1-4)


.PHONY: check
check: ## Run static type checks across all workspace targets
	@echo -e "$(GREEN)[CHECK]$(RESET) Running cargo check on all targets..."
	cargo check --workspace --all-targets

.PHONY: test
test: build-mods ## Run all workspace unit and integration tests
	@echo -e "$(GREEN)[TEST]$(RESET) Running workspace tests..."
	cargo test --workspace -- --test-threads=1

.PHONY: test-e2e
test-e2e: ## Execute comprehensive 9-stage end-to-end verification pipeline
	@echo -e "$(GREEN)[TEST-E2E]$(RESET) Running full E2E verification pipeline..."
	bash scripts/run_e2e_tests.sh

.PHONY: bench
bench: build-mods build-bench-mods ## Run full cross-architecture & debug/release permutation benchmark matrix
	@echo -e "$(GREEN)[BENCH]$(RESET) Running complete permutation benchmark suite (Debug + Release x Native + Rust dylib + C-ABI Rust/C99)..."
	cargo run --release -p permutation_bench
	@if [ -f "target/benchmark_results.json" ]; then \
		echo -e "$(GREEN)[BENCH]$(RESET) Successfully generated target/benchmark_results.json"; \
	fi

.PHONY: bench-quick
bench-quick: build-mods ## Run Milestone 4 host_runtime benchmark test (1k, 10k, 100k entities)
	@echo -e "$(GREEN)[BENCH-QUICK]$(RESET) Running Milestone 4 host_runtime benchmark test..."
	cargo test --release --test benchmarks -- --nocapture

.PHONY: fmt
fmt: ## Auto-format all Rust code across the workspace
	@echo -e "$(GREEN)[FMT]$(RESET) Formatting Rust code..."
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## Check code formatting without applying changes
	@echo -e "$(GREEN)[FMT-CHECK]$(RESET) Checking code formatting..."
	cargo fmt --all -- --check

.PHONY: clippy
clippy: ## Run Clippy linter with strict warning checks
	@echo -e "$(GREEN)[CLIPPY]$(RESET) Running clippy..."
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: clean
clean: clean-mods ## Clean all cargo targets and build artifacts
	@echo -e "$(GREEN)[CLEAN]$(RESET) Cleaning workspace target..."
	cargo clean

.PHONY: clean-mods
clean-mods: ## Clean guest mod build artifacts
	@echo -e "$(GREEN)[CLEAN-MODS]$(RESET) Cleaning C mods..."
	$(MAKE) -C tests/asymmetric_c_mod clean
	$(MAKE) -C benches/c_abi_c99_mod clean
	@if [ -f "examples/03_cross_language_interop/c_mod/Makefile" ]; then \
		$(MAKE) -C examples/03_cross_language_interop/c_mod clean; \
	fi
	@echo -e "$(GREEN)[CLEAN-MODS]$(RESET) Cleaning Rust mods..."
	cargo clean --manifest-path tests/asymmetric_rust_mod/Cargo.toml
	cargo clean --manifest-path benches/c_abi_rust_mod/Cargo.toml
	@if [ -f "examples/01_minimal_handshake/guest_mod/Cargo.toml" ]; then \
		cargo clean --manifest-path examples/01_minimal_handshake/guest_mod/Cargo.toml; \
	fi
	@if [ -f "examples/02_multi_system_pipeline/guest_mod/Cargo.toml" ]; then \
		cargo clean --manifest-path examples/02_multi_system_pipeline/guest_mod/Cargo.toml; \
	fi
	@if [ -f "examples/03_cross_language_interop/rust_mod/Cargo.toml" ]; then \
		cargo clean --manifest-path examples/03_cross_language_interop/rust_mod/Cargo.toml; \
	fi
	@if [ -f "examples/04_visual_game/mods/mod_gravity/Cargo.toml" ]; then \
		cargo clean --manifest-path examples/04_visual_game/mods/mod_gravity/Cargo.toml; \
	fi
	@if [ -f "examples/04_visual_game/mods/mod_rainbow/Cargo.toml" ]; then \
		cargo clean --manifest-path examples/04_visual_game/mods/mod_rainbow/Cargo.toml; \
	fi
	@if [ -f "examples/04_visual_game/mods/mod_vortex/Cargo.toml" ]; then \
		cargo clean --manifest-path examples/04_visual_game/mods/mod_vortex/Cargo.toml; \
	fi
