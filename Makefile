# ==============================================================================
# bevy_abi_mod: Thin Wrapper Makefile Delegating to `cargo xtask`
# ==============================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help

BOLD  := \033[1m
CYAN  := \033[36m
RESET := \033[0m

.PHONY: help
help: ## Display this help message
	@echo -e "$(BOLD)bevy_abi_mod Development Commands (via cargo xtask)$(RESET)"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  $(CYAN)%-18s$(RESET) %s\n", $$1, $$2}'
	@echo ""

.PHONY: ci
ci: ## Run full CI pipeline via cargo xtask
	cargo xtask ci

.PHONY: fixtures build-mods
fixtures: ## Build core asymmetric Rust & C99 guest mod fixtures into target/fixtures/
	cargo xtask fixtures

build-mods: fixtures ## Alias for fixtures

.PHONY: build-bench-mods
build-bench-mods: ## Build benchmark guest mod fixtures
	cargo xtask fixtures --benches

.PHONY: build-example-mods build-game-mods build-3d-mods
build-example-mods: ## Build guest mod fixtures for all progression examples
	cargo xtask fixtures --examples

build-game-mods: build-example-mods ## Alias for build-example-mods
build-3d-mods: build-example-mods ## Alias for build-example-mods

.PHONY: build
build: ## Build all guest fixtures and workspace crates
	cargo xtask fixtures --all
	cargo build --workspace

.PHONY: header
header: ## Validate include/bevy_abi_mod.h as strict ISO C99
	cargo xtask header

.PHONY: test-c99
test-c99: ## Compile & run strict ISO C99 header and standalone ABI harness
	cargo xtask header --test-c99

.PHONY: check
check: ## Run static type checks across all workspace targets
	cargo check --workspace --all-targets

.PHONY: test
test: fixtures ## Run unit and integration tests across default workspace members
	BAM_FIXTURE_DIR="$(CURDIR)/target/fixtures" cargo test -- --test-threads=1

.PHONY: test-e2e
test-e2e: ## Run end-to-end verification pipeline
	bash scripts/run_e2e_tests.sh

.PHONY: miri
miri: ## Run Miri (-Zmiri-tree-borrows) via cargo xtask
	cargo xtask miri --all

.PHONY: bench
bench: ## Run full cross-language permutation benchmark matrix
	cargo xtask bench --permutation

.PHONY: bench-quick
bench-quick: ## Run Criterion / host chunk throughput benchmark
	cargo xtask bench

.PHONY: fmt
fmt: ## Auto-format all Rust code across the workspace
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## Check code formatting without applying changes
	cargo fmt --all -- --check

.PHONY: clippy
clippy: ## Run Clippy linter with strict warning checks
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: example-1 example-2 example-3 example-4 game example-4-test example-5 game-3d example-5-test examples
example-1: build-example-mods ## Run Example 1: Minimal Handshake
	cargo run --manifest-path examples/01_minimal_handshake/Cargo.toml

example-2: build-example-mods ## Run Example 2: Multi-System Pipeline
	cargo run --manifest-path examples/02_multi_system_pipeline/Cargo.toml

example-3: build-example-mods ## Run Example 3: Cross-Language C99 + Rust Interop
	cargo run --manifest-path examples/03_cross_language_interop/Cargo.toml

example-4: build-example-mods ## Run Example 4: 2D Visual Game
	cargo run --manifest-path examples/04_visual_game/Cargo.toml

game: example-4 ## Alias for example-4

example-4-test: build-example-mods ## Run Example 4: Visual Game in headless test mode
	cargo run --manifest-path examples/04_visual_game/Cargo.toml -- --test-mode --ticks 30

example-5: build-example-mods ## Run Example 5: Cute 3D Slime Meadow (winit + wgpu)
	cargo run --manifest-path examples/05_cute_3d_winit/Cargo.toml --release

game-3d: example-5 ## Alias for example-5

example-5-test: build-example-mods ## Run Example 5: Cute 3D Slime Meadow in headless test mode
	cargo run --manifest-path examples/05_cute_3d_winit/Cargo.toml --release -- --test-mode --ticks 45

examples: example-1 example-2 example-3 example-4-test example-5-test ## Run progression examples sequentially

.PHONY: clean clean-mods
clean: clean-mods ## Clean workspace target and guest mod artifacts
	cargo clean

clean-mods: ## Clean guest mod build artifacts
	$(MAKE) -C tests/asymmetric_c_mod clean
	$(MAKE) -C benches/c_abi_c99_mod clean
	cargo clean --manifest-path tests/asymmetric_rust_mod/Cargo.toml
	cargo clean --manifest-path benches/c_abi_rust_mod/Cargo.toml
