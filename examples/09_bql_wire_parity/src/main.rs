#![forbid(unsafe_code)]

//! Example 9: `bevy_bql` ABI `0.3.0` ISO C99 vs. Safe Rust Guest Differential Wire Parity Runner.
//!
//! Compiles dual strict ISO C99 (`v1` & `v2`) and Safe Rust (`v1` & `v2`) native DSOs and drives
//! two independent deterministic Bevy `0.20` simulations across 500 `FixedUpdate` frames with a
//! live frame-250 `v1 -> v2` `migrate_component_column` reload, verifying bit-identical archetype
//! component column byte hashes on every single frame.

use example_09_bql_wire_parity::{
    DEFAULT_MIGRATE_FRAME, DEFAULT_TOTAL_FRAMES, build_parity_dsos, run_differential_parity,
};

fn main() {
    println!("========================================================================");
    println!("  bevy_abi_mod: Example 9 — bevy_bql C99 vs. Safe Rust Wire Parity      ");
    println!("  500 Deterministic FixedUpdate Frames + Live Frame-250 v1->v2 Reload   ");
    println!("========================================================================\n");

    println!("[HOST] Building strict ISO C99 (v1/v2) & Safe Rust (v1/v2) guest DSOs...");
    let dsos = build_parity_dsos();
    println!("  C99  v1 : {}", dsos.c99_v1.display());
    println!("  C99  v2 : {}", dsos.c99_v2.display());
    println!("  Rust v1 : {}", dsos.rust_v1.display());
    println!("  Rust v2 : {}\n", dsos.rust_v2.display());

    println!(
        "[HOST] Running dual deterministic worlds for {DEFAULT_TOTAL_FRAMES} FixedUpdate frames \
         (live v1 -> v2 migrate_component_column at frame {DEFAULT_MIGRATE_FRAME})..."
    );
    let report = run_differential_parity(&dsos, DEFAULT_TOTAL_FRAMES, DEFAULT_MIGRATE_FRAME);

    println!("\n--- Differential Wire Parity Summary ---");
    println!(
        "  Verified frames       : {}/{} bit-identical",
        report.verified_frames, report.total_frames
    );
    println!(
        "  Migrated rows (v1->v2): {} live entities (12B -> 16B)",
        report.migrated_rows
    );
    println!(
        "  Interned symbols      : {} (monotonic across reload)",
        report.symbol_count
    );
    println!(
        "  ATOMIC transactions   : {} committed, {} rolled back",
        report.final_accumulator.committed_tx_count, report.final_accumulator.rolled_back_tx_count
    );
    println!(
        "  Parallel RO reduction : sum=0x{:016x}, iter_count={}, order_trace=0x{:x}",
        report.final_accumulator.readonly_sum,
        report.final_accumulator.readonly_count,
        report.final_accumulator.order_trace
    );
    println!(
        "  Frame   0 SHA-256     : {} (FNV-1a=0x{:016x}, {} bytes)",
        report.frame_0_digest.sha256_hex(),
        report.frame_0_digest.fnv1a64,
        report.frame_0_digest.pod_bytes_hashed
    );
    println!(
        "  Frame 249 SHA-256     : {} (FNV-1a=0x{:016x}, {} bytes)",
        report.pre_reload_digest.sha256_hex(),
        report.pre_reload_digest.fnv1a64,
        report.pre_reload_digest.pod_bytes_hashed
    );
    println!(
        "  Migrated  SHA-256     : {} (FNV-1a=0x{:016x}, {} bytes)",
        report.post_migration_digest.sha256_hex(),
        report.post_migration_digest.fnv1a64,
        report.post_migration_digest.pod_bytes_hashed
    );
    println!(
        "  Frame 499 SHA-256     : {} (FNV-1a=0x{:016x}, {} bytes)",
        report.final_digest.sha256_hex(),
        report.final_digest.fnv1a64,
        report.final_digest.pod_bytes_hashed
    );
    println!("\n========================================================================");
    println!("  ALL 500 FRAMES BIT-IDENTICAL ACROSS ISO C99 AND SAFE RUST GUESTS.     ");
    println!("========================================================================");
}
