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
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  $(CYAN)%-16s$(RESET) %s\n", $$1, $$2}'
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
	@echo -e "$(GREEN)[CLEAN-MODS]$(RESET) Cleaning Rust mods..."
	cargo clean --manifest-path tests/asymmetric_rust_mod/Cargo.toml
	cargo clean --manifest-path benches/c_abi_rust_mod/Cargo.toml
