//! Live `bql_lsp` telemetry streaming (`ModTelemetryEndpoint` / [`SystemTelemetryDigest64`]) and
//! Bevy Remote Protocol (`bevy_remote` BRP) inspection & hot-reload endpoints (`ModTelemetryPlugin`).
//!
//! Exposes four `bam/*` BRP JSON-RPC methods in [`RemoteMethods`]:
//!
//! - [`BRP_METHOD_BAM_LIST_MODS`] (`"bam/list_mods"`): active mods, IDs, versions, system counts,
//!   fault counts, and per-system quarantine status.
//! - [`BRP_METHOD_BAM_SYSTEM_METRICS`] (`"bam/system_metrics"`): per-system `matched_entities`,
//!   `matched_tables`, `skipped_summary_tick_tables`, rolling `p50_us` / `p99_us`, `chunks`, and
//!   `commands_queued`.
//! - [`BRP_METHOD_BAM_COMPONENT_SCHEMAS`] (`"bam/component_schemas"`): registered components,
//!   UUIDs, sizes, aligns, `schema_fingerprint`, and exact `BamFieldDesc` field names/types/offsets.
//! - [`BRP_METHOD_BAM_HOT_RELOAD`] (`"bam/hot_reload"`): schedules (or immediately triggers when
//!   safe) a mod hot-reload by `mod_id` or `mod_name`.
//!
//! In addition, [`ModTelemetryEndpoint`] provides a non-blocking local telemetry stream for
//! `bql_lsp` emitting fixed **64-byte** little-endian [`SystemTelemetryDigest64`] frames drained
//! in Bevy's [`Last`] schedule so slow or disconnected clients never stall frame execution:
//!
//! - On Unix (`#[cfg(unix)]`): non-blocking `AF_UNIX` [`std::os::unix::net::UnixListener`] with
//!   stale-socket cleanup on bind and automatic unlink on [`Drop`].
//! - On Windows (`#[cfg(windows)]`) and optionally on Unix: safe (`#![forbid(unsafe_code)]`)
//!   non-blocking loopback [`std::net::TcpListener`] (`127.0.0.1:<port>`), avoiding raw Win32
//!   named-pipe FFI while preserving identical 64-byte binary frame semantics.

use crate::savegame::format_component_uuid;
use bevy_abi_mod_host::{
    LoadOptions, LoadedModInfo, ModHost, ModId, ModQuarantine, ModRegistry, ModWorldExt,
};
use bevy_abi_mod_host_sys::{
    inspect_component_bam_fields, inspect_component_witness_fields,
    inspect_component_witness_fields_in_registry,
};
use bevy_abi_mod_sys::{BamComponent, FNV1A_64_OFFSET_BASIS, FNV1A_64_PRIME};
use bevy_app::{App, Last, Plugin, Update};
use bevy_ecs::change_detection::Tick;
use bevy_ecs::prelude::{In, Resource};
use bevy_ecs::schedule::{IntoScheduleConfigs, Schedules};
use bevy_ecs::system::{Res, ResMut};
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use bevy_remote::{
    BrpError, BrpRequest, BrpResult, RemoteMethodSystemId, RemoteMethods, RemotePlugin, error_codes,
};
use core::fmt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, ErrorKind, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// BRP method path listing active mods, versions, system counts, fault counts, and quarantine state.
pub const BRP_METHOD_BAM_LIST_MODS: &str = "bam/list_mods";
/// BRP method path reporting per-system execution, `summary_tick`, and `p50_us` / `p99_us` metrics.
pub const BRP_METHOD_BAM_SYSTEM_METRICS: &str = "bam/system_metrics";
/// BRP method path inspecting registered component schemas, fingerprints, and `BamFieldDesc` offsets.
pub const BRP_METHOD_BAM_COMPONENT_SCHEMAS: &str = "bam/component_schemas";
/// BRP method path triggering or scheduling a mod hot-reload by `mod_id` or `mod_name`.
pub const BRP_METHOD_BAM_HOT_RELOAD: &str = "bam/hot_reload";

/// 4-byte wire magic prefix for [`SystemTelemetryDigest64`] frames (`"BAMT"`).
pub const SYSTEM_TELEMETRY_MAGIC: [u8; 4] = *b"BAMT";

/// Wire schema version for [`SystemTelemetryDigest64`].
pub const SYSTEM_TELEMETRY_DIGEST_VERSION: u16 = 1;

/// Exact byte length of one serialized [`SystemTelemetryDigest64`] frame (`64` bytes).
pub const SYSTEM_TELEMETRY_DIGEST_SIZE: usize = 64;

/// Flag bit indicating the guest system is currently quarantined in [`ModQuarantine`].
pub const TELEMETRY_FLAG_QUARANTINED: u16 = 1 << 0;
/// Flag bit indicating the guest system faulted on its latest sample.
pub const TELEMETRY_FLAG_FAULTED: u16 = 1 << 1;
/// Flag bit indicating Bevy `0.20` `summary_tick` table skipping occurred (`skipped > 0`).
pub const TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE: u16 = 1 << 2;
/// Flag bit indicating a per-mod budget watchdog breach was reported for this system.
pub const TELEMETRY_FLAG_BUDGET_BREACHED: u16 = 1 << 3;

const DEFAULT_PERCENTILE_WINDOW: usize = 128;
const DEFAULT_MAX_CLIENTS: usize = 32;
const DEFAULT_MAX_PENDING_FRAMES_PER_CLIENT: usize = 256;
const MAX_ACCEPTS_PER_TICK: usize = 16;

/// Computes the deterministic 64-bit FNV-1a system identifier hash streamed in
/// [`SystemTelemetryDigest64::system_hash`].
pub fn fnv1a_64_hash_system(mod_name: &str, system_name: &str) -> u64 {
    let mut hash = FNV1A_64_OFFSET_BASIS;
    for &b in mod_name.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }
    for &b in b"::" {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }
    for &b in system_name.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }
    hash
}

/// Errors returned when decoding a 64-byte [`SystemTelemetryDigest64`] wire frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TelemetryFrameError {
    /// Leading 4-byte magic did not match [`SYSTEM_TELEMETRY_MAGIC`] (`"BAMT"`).
    InvalidMagic([u8; 4]),
    /// Frame wire version is unsupported.
    UnsupportedVersion(u16),
}

impl fmt::Display for TelemetryFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic(m) => {
                write!(
                    f,
                    "invalid telemetry frame magic {m:?} (expected b\"BAMT\")"
                )
            }
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported telemetry frame version {v} (expected {SYSTEM_TELEMETRY_DIGEST_VERSION})"
            ),
        }
    }
}

impl std::error::Error for TelemetryFrameError {}

/// Fixed **64-byte** little-endian binary telemetry digest frame streamed to `bql_lsp` clients.
///
/// Layout (64 bytes, 8-byte aligned, zero implicit padding):
/// - `0..4`: `magic` (`b"BAMT"`)
/// - `4..6`: `version` (`u16` LE, currently `1`)
/// - `6..8`: `flags` (`u16` LE bitmask of `TELEMETRY_FLAG_*`)
/// - `8..16`: `mod_id` (`u64` LE)
/// - `16..24`: `system_hash` (`u64` LE FNV-1a of `"{mod_name}::{system_name}"`)
/// - `24..32`: `frame_seq` (`u64` LE monotonic frame counter)
/// - `32..36`: `matched_entities` (`u32` LE)
/// - `36..40`: `matched_tables` (`u32` LE)
/// - `40..44`: `skipped_summary_tick_tables` (`u32` LE)
/// - `44..48`: `chunks` (`u32` LE)
/// - `48..52`: `p50_us` (`u32` LE rolling median duration in microseconds)
/// - `52..56`: `p99_us` (`u32` LE rolling 99th-percentile duration in microseconds)
/// - `56..60`: `commands_queued` (`u32` LE)
/// - `60..64`: `reserved` (`u32` LE, always `0`)
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemTelemetryDigest64 {
    /// Wire magic prefix (`b"BAMT"`).
    pub magic: [u8; 4],
    /// Wire schema version (`SYSTEM_TELEMETRY_DIGEST_VERSION`).
    pub version: u16,
    /// Status bitflags (`TELEMETRY_FLAG_*`).
    pub flags: u16,
    /// Owning `ModId.0` (`0` for unscoped / host-owned systems).
    pub mod_id: u64,
    /// Deterministic 64-bit FNV-1a hash of `"{mod_name}::{system_name}"`.
    pub system_hash: u64,
    /// Monotonic frame sequence number emitted by [`ModTelemetryPlugin`].
    pub frame_seq: u64,
    /// Number of entities matched/visited by the system on the latest sample.
    pub matched_entities: u32,
    /// Number of ECS tables matched by the system's query.
    pub matched_tables: u32,
    /// Number of matched ECS tables skipped via Bevy `0.20` `summary_tick` acceleration.
    pub skipped_summary_tick_tables: u32,
    /// Number of contiguous chunk iterations executed by the system.
    pub chunks: u32,
    /// Rolling median (`p50`) wall-clock execution duration in microseconds.
    pub p50_us: u32,
    /// Rolling 99th-percentile (`p99`) wall-clock execution duration in microseconds.
    pub p99_us: u32,
    /// Number of deferred host commands queued on the latest sample.
    pub commands_queued: u32,
    /// Reserved zero word for 64-byte wire alignment.
    pub reserved: u32,
}

const _: () = {
    assert!(core::mem::size_of::<SystemTelemetryDigest64>() == SYSTEM_TELEMETRY_DIGEST_SIZE);
    assert!(core::mem::align_of::<SystemTelemetryDigest64>() == 8);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, magic) == 0);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, version) == 4);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, flags) == 6);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, mod_id) == 8);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, system_hash) == 16);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, frame_seq) == 24);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, matched_entities) == 32);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, matched_tables) == 36);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, skipped_summary_tick_tables) == 40);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, chunks) == 44);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, p50_us) == 48);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, p99_us) == 52);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, commands_queued) == 56);
    assert!(core::mem::offset_of!(SystemTelemetryDigest64, reserved) == 60);
};

impl SystemTelemetryDigest64 {
    /// Encodes this digest into a canonical 64-byte little-endian wire buffer.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; SYSTEM_TELEMETRY_DIGEST_SIZE] {
        let mut buf = [0u8; SYSTEM_TELEMETRY_DIGEST_SIZE];
        buf[0..4].copy_from_slice(&self.magic);
        buf[4..6].copy_from_slice(&self.version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..16].copy_from_slice(&self.mod_id.to_le_bytes());
        buf[16..24].copy_from_slice(&self.system_hash.to_le_bytes());
        buf[24..32].copy_from_slice(&self.frame_seq.to_le_bytes());
        buf[32..36].copy_from_slice(&self.matched_entities.to_le_bytes());
        buf[36..40].copy_from_slice(&self.matched_tables.to_le_bytes());
        buf[40..44].copy_from_slice(&self.skipped_summary_tick_tables.to_le_bytes());
        buf[44..48].copy_from_slice(&self.chunks.to_le_bytes());
        buf[48..52].copy_from_slice(&self.p50_us.to_le_bytes());
        buf[52..56].copy_from_slice(&self.p99_us.to_le_bytes());
        buf[56..60].copy_from_slice(&self.commands_queued.to_le_bytes());
        buf[60..64].copy_from_slice(&self.reserved.to_le_bytes());
        buf
    }

    /// Decodes and validates a 64-byte little-endian wire buffer into [`SystemTelemetryDigest64`].
    pub fn from_le_bytes(
        buf: &[u8; SYSTEM_TELEMETRY_DIGEST_SIZE],
    ) -> Result<Self, TelemetryFrameError> {
        let magic = [buf[0], buf[1], buf[2], buf[3]];
        if magic != SYSTEM_TELEMETRY_MAGIC {
            return Err(TelemetryFrameError::InvalidMagic(magic));
        }
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version != SYSTEM_TELEMETRY_DIGEST_VERSION {
            return Err(TelemetryFrameError::UnsupportedVersion(version));
        }
        Ok(Self {
            magic,
            version,
            flags: u16::from_le_bytes([buf[6], buf[7]]),
            mod_id: u64::from_le_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]),
            system_hash: u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
            ]),
            frame_seq: u64::from_le_bytes([
                buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
            ]),
            matched_entities: u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
            matched_tables: u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
            skipped_summary_tick_tables: u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]),
            chunks: u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]]),
            p50_us: u32::from_le_bytes([buf[48], buf[49], buf[50], buf[51]]),
            p99_us: u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]),
            commands_queued: u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]),
            reserved: u32::from_le_bytes([buf[60], buf[61], buf[62], buf[63]]),
        })
    }
}

/// Live Bevy `0.20` `summary_tick` table-scan statistics returned by [`probe_summary_tick_tables`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SummaryTickTableStats {
    /// Total non-empty ECS tables carrying the inspected component column.
    pub matched_tables: u64,
    /// Number of matched tables skipped because their `summary_tick` was not newer than `last_run`.
    pub skipped_summary_tick_tables: u64,
    /// Number of matched tables whose rows would be visited (`matched_tables - skipped`).
    pub visited_tables: u64,
    /// Total entities inside all non-skipped matched tables.
    pub matched_entities: u64,
}

/// Inspects live Bevy `0.20` ECS tables carrying `component` and counts how many tables are
/// skipped by Bevy's per-table `summary_tick` fast path for `(last_run, this_run)`.
///
/// 100% safe (`#![forbid(unsafe_code)]`) read-only helper useful both for host instrumentation
/// and end-to-end telemetry verification.
pub fn probe_summary_tick_tables(
    world: &World,
    component: BamComponent,
    last_run: Tick,
    this_run: Tick,
) -> SummaryTickTableStats {
    let Some(entry) = world
        .get_resource::<ModRegistry>()
        .and_then(|r| r.get_by_handle(component))
    else {
        return SummaryTickTableStats::default();
    };
    let comp_id = entry.component_id;
    let mut stats = SummaryTickTableStats::default();

    for table in world.storages().tables.iter() {
        let len = table.entity_count() as u64;
        if len == 0 {
            continue;
        }
        let Some(col) = table.get_column(comp_id) else {
            continue;
        };
        stats.matched_tables += 1;
        if let Some(summary) = col.get_summary_tick()
            && !summary.get().is_newer_than(last_run, this_run)
        {
            stats.skipped_summary_tick_tables += 1;
        } else {
            stats.visited_tables += 1;
            stats.matched_entities += len;
        }
    }
    stats
}

// ============================================================================
// Telemetry Recorder & Rolling Percentile Aggregator
// ============================================================================

/// Single per-system execution sample ingested by [`ModTelemetryRecorder::record_sample`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemExecutionSample {
    /// Owning [`ModId`], if associated with a loaded mod.
    pub mod_id: Option<ModId>,
    /// Canonical mod name (`"host"` / `"unscoped"` if host-owned).
    pub mod_name: String,
    /// Registered guest/host system name.
    pub system_name: String,
    /// Wall-clock system duration for this invocation in microseconds.
    pub duration_us: u64,
    /// Entities matched or processed during this invocation.
    pub matched_entities: u64,
    /// ECS tables matched by this system's queries.
    pub matched_tables: u64,
    /// Matched tables skipped via Bevy `0.20` `summary_tick` acceleration.
    pub skipped_summary_tick_tables: u64,
    /// Chunk iterations processed during this invocation.
    pub chunks: u64,
    /// Deferred host commands queued during this invocation.
    pub commands_queued: u64,
    /// Whether this invocation faulted or panicked.
    pub faulted: bool,
    /// Whether this invocation triggered a budget watchdog breach.
    pub budget_breached: bool,
}

/// Pre-aggregated per-system metric snapshot ingested by [`ModTelemetryRecorder::record_snapshot`]
/// (designed for direct forwarding from per-mod budget watchdogs such as `ModBudgetPolicy`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemMetricSnapshot {
    /// Owning [`ModId`], if associated with a loaded mod.
    pub mod_id: Option<ModId>,
    /// Canonical mod name.
    pub mod_name: String,
    /// Registered system name.
    pub system_name: String,
    /// Precomputed rolling `p50` duration in microseconds.
    pub p50_us: u64,
    /// Precomputed rolling `p99` duration in microseconds.
    pub p99_us: u64,
    /// Entities matched on the latest frame.
    pub matched_entities: u64,
    /// ECS tables matched on the latest frame.
    pub matched_tables: u64,
    /// Matched tables skipped via `summary_tick` on the latest frame.
    pub skipped_summary_tick_tables: u64,
    /// Chunks visited on the latest frame.
    pub chunks: u64,
    /// Deferred commands queued on the latest frame.
    pub commands_queued: u64,
    /// Whether this system is quarantined.
    pub quarantined: bool,
    /// Whether this system faulted.
    pub faulted: bool,
    /// Whether this system has breached its frame budget.
    pub budget_breached: bool,
}

/// Materialized per-system metrics returned by [`ModTelemetryRecorder::system_metrics`] and the
/// `bam/system_metrics` BRP endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemTelemetryMetrics {
    /// Owning `ModId.0` (`0` for unscoped systems).
    pub mod_id: u64,
    /// Canonical mod name.
    pub mod_name: String,
    /// Registered system name.
    pub system_name: String,
    /// Deterministic 64-bit FNV-1a system hash (`fnv1a_64_hash_system`).
    pub system_hash: u64,
    /// Entities matched on the latest frame.
    pub matched_entities: u64,
    /// ECS tables matched on the latest frame.
    pub matched_tables: u64,
    /// Matched tables skipped via `summary_tick` on the latest frame.
    pub skipped_summary_tick_tables: u64,
    /// Rolling 50th-percentile duration in microseconds.
    pub p50_us: u64,
    /// Rolling 99th-percentile duration in microseconds.
    pub p99_us: u64,
    /// Most recent single-invocation duration in microseconds.
    pub last_duration_us: u64,
    /// Chunks processed on the latest frame.
    pub chunks: u64,
    /// Deferred commands queued on the latest frame.
    pub commands_queued: u64,
    /// Total samples observed for this system.
    pub sample_count: u64,
    /// `true` if this system is currently quarantined.
    pub quarantined: bool,
    /// `true` if the latest invocation faulted.
    pub faulted: bool,
    /// `true` if a budget breach is active on this system.
    pub budget_breached: bool,
}

impl SystemTelemetryMetrics {
    /// Converts this metric record into a fixed 64-byte [`SystemTelemetryDigest64`] frame stamped
    /// with `frame_seq`.
    #[must_use]
    pub fn to_digest64(&self, frame_seq: u64) -> SystemTelemetryDigest64 {
        let mut flags = 0u16;
        if self.quarantined {
            flags |= TELEMETRY_FLAG_QUARANTINED;
        }
        if self.faulted {
            flags |= TELEMETRY_FLAG_FAULTED;
        }
        if self.skipped_summary_tick_tables > 0 {
            flags |= TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE;
        }
        if self.budget_breached {
            flags |= TELEMETRY_FLAG_BUDGET_BREACHED;
        }
        SystemTelemetryDigest64 {
            magic: SYSTEM_TELEMETRY_MAGIC,
            version: SYSTEM_TELEMETRY_DIGEST_VERSION,
            flags,
            mod_id: self.mod_id,
            system_hash: self.system_hash,
            frame_seq,
            matched_entities: u32::try_from(self.matched_entities).unwrap_or(u32::MAX),
            matched_tables: u32::try_from(self.matched_tables).unwrap_or(u32::MAX),
            skipped_summary_tick_tables: u32::try_from(self.skipped_summary_tick_tables)
                .unwrap_or(u32::MAX),
            chunks: u32::try_from(self.chunks).unwrap_or(u32::MAX),
            p50_us: u32::try_from(self.p50_us).unwrap_or(u32::MAX),
            p99_us: u32::try_from(self.p99_us).unwrap_or(u32::MAX),
            commands_queued: u32::try_from(self.commands_queued).unwrap_or(u32::MAX),
            reserved: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct SystemMetricState {
    mod_id: u64,
    mod_name: String,
    system_name: String,
    system_hash: u64,
    durations_us: VecDeque<u64>,
    override_percentiles: Option<(u64, u64)>,
    last_duration_us: u64,
    matched_entities: u64,
    matched_tables: u64,
    skipped_summary_tick_tables: u64,
    chunks: u64,
    commands_queued: u64,
    sample_count: u64,
    quarantined: bool,
    faulted: bool,
    budget_breached: bool,
}

impl SystemMetricState {
    fn compute_percentiles(&self) -> (u64, u64) {
        if let Some(pair) = self.override_percentiles {
            return pair;
        }
        if self.durations_us.is_empty() {
            return (0, 0);
        }
        let mut sorted: Vec<u64> = self.durations_us.iter().copied().collect();
        sorted.sort_unstable();
        (
            nearest_rank_percentile(&sorted, 50),
            nearest_rank_percentile(&sorted, 99),
        )
    }

    fn to_metrics(&self) -> SystemTelemetryMetrics {
        let (p50_us, p99_us) = self.compute_percentiles();
        SystemTelemetryMetrics {
            mod_id: self.mod_id,
            mod_name: self.mod_name.clone(),
            system_name: self.system_name.clone(),
            system_hash: self.system_hash,
            matched_entities: self.matched_entities,
            matched_tables: self.matched_tables,
            skipped_summary_tick_tables: self.skipped_summary_tick_tables,
            p50_us,
            p99_us,
            last_duration_us: self.last_duration_us,
            chunks: self.chunks,
            commands_queued: self.commands_queued,
            sample_count: self.sample_count,
            quarantined: self.quarantined,
            faulted: self.faulted,
            budget_breached: self.budget_breached,
        }
    }
}

fn nearest_rank_percentile(sorted: &[u64], pct: usize) -> u64 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    let rank = (pct.saturating_mul(n)).div_ceil(100);
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// Host [`Resource`] accumulating per-system execution metrics, rolling `p50_us`/`p99_us`
/// latency windows, and `summary_tick` skip counters for `bam/system_metrics` and live
/// [`SystemTelemetryDigest64`] streaming.
#[derive(Resource, Clone, Debug)]
pub struct ModTelemetryRecorder {
    window_size: usize,
    frame_seq: u64,
    systems: BTreeMap<(u64, String, String), SystemMetricState>,
}

impl Default for ModTelemetryRecorder {
    fn default() -> Self {
        Self::new(DEFAULT_PERCENTILE_WINDOW)
    }
}

impl ModTelemetryRecorder {
    /// Creates a new [`ModTelemetryRecorder`] with a sliding percentile window of `window_size`
    /// samples (clamped to at least `1`).
    #[must_use]
    pub fn new(window_size: usize) -> Self {
        Self {
            window_size: window_size.max(1),
            frame_seq: 0,
            systems: BTreeMap::new(),
        }
    }

    /// Returns the configured sliding percentile window length.
    #[inline]
    pub const fn window_size(&self) -> usize {
        self.window_size
    }

    /// Returns the monotonic frame sequence number incremented once per `Last` flush.
    #[inline]
    pub const fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    /// Records a single system execution sample, updating rolling `p50_us` and `p99_us`.
    pub fn record_sample(&mut self, sample: SystemExecutionSample) {
        let mod_id = sample.mod_id.map_or(0, |id| id.0);
        let mod_name = if sample.mod_name.is_empty() {
            "unscoped".to_owned()
        } else {
            sample.mod_name
        };
        let key = (mod_id, mod_name.clone(), sample.system_name.clone());
        let window_size = self.window_size;
        let state = self.systems.entry(key).or_insert_with(|| {
            let system_hash = fnv1a_64_hash_system(&mod_name, &sample.system_name);
            SystemMetricState {
                mod_id,
                mod_name,
                system_name: sample.system_name,
                system_hash,
                durations_us: VecDeque::with_capacity(window_size),
                override_percentiles: None,
                last_duration_us: 0,
                matched_entities: 0,
                matched_tables: 0,
                skipped_summary_tick_tables: 0,
                chunks: 0,
                commands_queued: 0,
                sample_count: 0,
                quarantined: false,
                faulted: false,
                budget_breached: false,
            }
        });
        if state.durations_us.len() >= window_size {
            state.durations_us.pop_front();
        }
        state.durations_us.push_back(sample.duration_us);
        state.override_percentiles = None;
        state.last_duration_us = sample.duration_us;
        state.matched_entities = sample.matched_entities;
        state.matched_tables = sample.matched_tables;
        state.skipped_summary_tick_tables = sample.skipped_summary_tick_tables;
        state.chunks = sample.chunks;
        state.commands_queued = sample.commands_queued;
        state.sample_count = state.sample_count.saturating_add(1);
        state.faulted = sample.faulted;
        state.budget_breached = sample.budget_breached;
    }

    /// Ingests a pre-aggregated [`SystemMetricSnapshot`] (e.g. synchronized from a per-mod budget
    /// watchdog).
    pub fn record_snapshot(&mut self, snapshot: SystemMetricSnapshot) {
        let mod_id = snapshot.mod_id.map_or(0, |id| id.0);
        let mod_name = if snapshot.mod_name.is_empty() {
            "unscoped".to_owned()
        } else {
            snapshot.mod_name
        };
        let key = (mod_id, mod_name.clone(), snapshot.system_name.clone());
        let window_size = self.window_size;
        let state = self.systems.entry(key).or_insert_with(|| {
            let system_hash = fnv1a_64_hash_system(&mod_name, &snapshot.system_name);
            SystemMetricState {
                mod_id,
                mod_name,
                system_name: snapshot.system_name,
                system_hash,
                durations_us: VecDeque::with_capacity(window_size),
                override_percentiles: None,
                last_duration_us: 0,
                matched_entities: 0,
                matched_tables: 0,
                skipped_summary_tick_tables: 0,
                chunks: 0,
                commands_queued: 0,
                sample_count: 0,
                quarantined: false,
                faulted: false,
                budget_breached: false,
            }
        });
        state.override_percentiles = Some((snapshot.p50_us, snapshot.p99_us));
        state.last_duration_us = snapshot.p50_us;
        state.matched_entities = snapshot.matched_entities;
        state.matched_tables = snapshot.matched_tables;
        state.skipped_summary_tick_tables = snapshot.skipped_summary_tick_tables;
        state.chunks = snapshot.chunks;
        state.commands_queued = snapshot.commands_queued;
        state.sample_count = state.sample_count.saturating_add(1);
        state.quarantined = snapshot.quarantined;
        state.faulted = snapshot.faulted;
        state.budget_breached = snapshot.budget_breached;
    }

    /// Synchronizes quarantine status from [`ModQuarantine`] across all tracked systems.
    pub fn sync_quarantine(&mut self, quarantine: &ModQuarantine) {
        for state in self.systems.values_mut() {
            if state.mod_id != 0 {
                state.quarantined =
                    quarantine.is_quarantined(ModId(state.mod_id), &state.system_name);
            }
        }
    }

    /// Removes all tracked systems owned by `mod_id`.
    pub fn clear_mod(&mut self, mod_id: ModId) {
        self.systems.retain(|(id, _, _), _| *id != mod_id.0);
    }

    /// Returns a snapshot vector of all recorded [`SystemTelemetryMetrics`].
    pub fn system_metrics(&self) -> Vec<SystemTelemetryMetrics> {
        self.systems
            .values()
            .map(SystemMetricState::to_metrics)
            .collect()
    }

    /// Materializes fixed 64-byte [`SystemTelemetryDigest64`] frames for all tracked systems at
    /// `frame_seq`.
    pub fn build_digests(&self, frame_seq: u64) -> Vec<SystemTelemetryDigest64> {
        self.systems
            .values()
            .map(|state| state.to_metrics().to_digest64(frame_seq))
            .collect()
    }

    fn advance_frame_seq(&mut self) -> u64 {
        self.frame_seq = self.frame_seq.saturating_add(1);
        self.frame_seq
    }
}

// ============================================================================
// Deferred Hot-Reload Request Queue (`bam/hot_reload`)
// ============================================================================

/// Queued mod hot-reload request submitted via `bam/hot_reload` BRP or host code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotReloadRequest {
    /// Target [`ModId`] raw value.
    pub mod_id: u64,
    /// Target mod canonical name.
    pub mod_name: String,
    /// Optional replacement library path (`None` reloads from the original source path).
    pub override_path: Option<PathBuf>,
    /// Frame sequence at which the request was enqueued.
    pub requested_at_frame: u64,
}

/// Recorded result of processing a [`HotReloadRequest`] in `Last`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HotReloadOutcome {
    /// Target mod reloaded cleanly onto `new_mod_id`.
    Reloaded {
        /// Previous [`ModId`] raw value.
        old_mod_id: u64,
        /// Newly assigned [`ModId`] raw value.
        new_mod_id: u64,
        /// Canonical mod name.
        mod_name: String,
    },
    /// Target mod reload failed with a structured error message.
    Failed {
        /// Target [`ModId`] raw value.
        mod_id: u64,
        /// Canonical mod name.
        mod_name: String,
        /// Error detail.
        error: String,
    },
}

/// Optional host callback overriding or handling deferred [`HotReloadRequest`] execution (for
/// custom orchestration or unit/integration testing without on-disk shared libraries).
pub type CustomHotReloadHandler =
    Arc<dyn Fn(&mut World, &HotReloadRequest) -> Result<ModId, String> + Send + Sync>;

/// Host [`Resource`] buffering `bam/hot_reload` requests until Bevy's [`Last`] schedule runs
/// outside any leased `Update` schedule.
#[derive(Resource, Default, Clone)]
pub struct ModHotReloadQueue {
    pending: VecDeque<HotReloadRequest>,
    history: Vec<HotReloadOutcome>,
    custom_handler: Option<CustomHotReloadHandler>,
}

impl ModHotReloadQueue {
    /// Creates an empty [`ModHotReloadQueue`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues a [`HotReloadRequest`].
    pub fn push(&mut self, req: HotReloadRequest) {
        self.pending.push_back(req);
    }

    /// Returns the number of pending unexecuted hot-reload requests.
    #[inline]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Returns a slice of completed [`HotReloadOutcome`] records in execution order.
    #[inline]
    pub fn history(&self) -> &[HotReloadOutcome] {
        &self.history
    }

    /// Installs an optional custom reload handler invoked when `ModHost` does not own a disk
    /// library for `req.mod_id` (or to intercept hot-reload execution).
    pub fn set_custom_handler<F>(&mut self, handler: F)
    where
        F: Fn(&mut World, &HotReloadRequest) -> Result<ModId, String> + Send + Sync + 'static,
    {
        self.custom_handler = Some(Arc::new(handler));
    }

    fn drain_pending(&mut self) -> Vec<HotReloadRequest> {
        self.pending.drain(..).collect()
    }
}

fn execute_hot_reload_request(world: &mut World, req: &HotReloadRequest) -> HotReloadOutcome {
    let custom_handler = world
        .get_resource::<ModHotReloadQueue>()
        .and_then(|q| q.custom_handler.clone());

    let hosted = world
        .get_resource::<ModHost>()
        .is_some_and(|h| h.contains_mod(ModId(req.mod_id)));

    if hosted {
        let res = match req.override_path.as_deref() {
            Some(path) => world.reload_mod_from(
                ModId(req.mod_id),
                path,
                LoadOptions::default().with_shadow_copy(true),
            ),
            None => world.reload_mod(ModId(req.mod_id)),
        };
        return match res {
            Ok(new_id) => HotReloadOutcome::Reloaded {
                old_mod_id: req.mod_id,
                new_mod_id: new_id.0,
                mod_name: req.mod_name.clone(),
            },
            Err(err) => HotReloadOutcome::Failed {
                mod_id: req.mod_id,
                mod_name: req.mod_name.clone(),
                error: err.to_string(),
            },
        };
    }

    if let Some(handler) = custom_handler {
        return match handler(world, req) {
            Ok(new_id) => HotReloadOutcome::Reloaded {
                old_mod_id: req.mod_id,
                new_mod_id: new_id.0,
                mod_name: req.mod_name.clone(),
            },
            Err(error) => HotReloadOutcome::Failed {
                mod_id: req.mod_id,
                mod_name: req.mod_name.clone(),
                error,
            },
        };
    }

    HotReloadOutcome::Failed {
        mod_id: req.mod_id,
        mod_name: req.mod_name.clone(),
        error: format!(
            "mod '{}' (ModId({})) is not backed by an active ModHost library",
            req.mod_name, req.mod_id
        ),
    }
}

/// Exclusive `Last`-schedule system draining [`ModHotReloadQueue`] safely outside `Update`.
pub fn process_pending_hot_reloads(world: &mut World) {
    let requests = world
        .get_resource_mut::<ModHotReloadQueue>()
        .map(|mut q| q.drain_pending())
        .unwrap_or_default();
    if requests.is_empty() {
        return;
    }
    for req in requests {
        let outcome = execute_hot_reload_request(world, &req);
        if let Some(mut queue) = world.get_resource_mut::<ModHotReloadQueue>() {
            queue.history.push(outcome);
        }
    }
}

// ============================================================================
// Non-Blocking Cross-Platform Local Telemetry Endpoint (`ModTelemetryEndpoint`)
// ============================================================================

/// Local transport configuration for [`ModTelemetryEndpoint`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModTelemetryTransport {
    /// Non-blocking Unix domain stream socket (`AF_UNIX`) with automatic stale-socket cleanup on
    /// bind and unlink on [`Drop`].
    #[cfg(unix)]
    UnixSocket(PathBuf),
    /// Safe (`#![forbid(unsafe_code)]`) non-blocking loopback TCP stream listener
    /// (`127.0.0.1:<port>` or `[::1]:<port>`).
    ///
    /// Serves as the safe cross-platform local transport on Windows (avoiding raw Win32 named-pipe
    /// FFI) and is also available on Unix whenever a loopback TCP bridge is preferred.
    LoopbackTcp(SocketAddr),
}

enum ListenerInner {
    #[cfg(unix)]
    Unix {
        listener: UnixListener,
        path: PathBuf,
    },
    LoopbackTcp {
        listener: TcpListener,
    },
}

enum ClientStream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Write for ClientStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.write(buf),
            Self::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

struct TelemetryClient {
    stream: ClientStream,
    /// Partially transmitted 64-byte frame + current write offset (preserves frame boundaries on
    /// `WouldBlock` partial writes).
    in_flight: Option<([u8; SYSTEM_TELEMETRY_DIGEST_SIZE], usize)>,
    /// Queued full 64-byte frames awaiting transmission.
    queued: VecDeque<[u8; SYSTEM_TELEMETRY_DIGEST_SIZE]>,
}

impl TelemetryClient {
    fn new(stream: ClientStream) -> Self {
        Self {
            stream,
            in_flight: None,
            queued: VecDeque::new(),
        }
    }

    fn enqueue_frame(
        &mut self,
        frame: [u8; SYSTEM_TELEMETRY_DIGEST_SIZE],
        max_pending: usize,
    ) -> bool {
        let mut dropped = false;
        while self.queued.len() >= max_pending {
            self.queued.pop_front();
            dropped = true;
        }
        self.queued.push_back(frame);
        dropped
    }

    /// Attempts to drain queued frames non-blockingly. Returns `(frames_sent, keep_connected)`.
    fn try_flush(&mut self) -> (u64, bool) {
        let mut sent = 0u64;
        loop {
            if self.in_flight.is_none() {
                if let Some(next) = self.queued.pop_front() {
                    self.in_flight = Some((next, 0));
                } else {
                    return (sent, true);
                }
            }
            let Some((frame, offset)) = self.in_flight.as_mut() else {
                return (sent, true);
            };
            match self.stream.write(&frame[*offset..]) {
                Ok(0) => {
                    // Peer closed stream.
                    return (sent, false);
                }
                Ok(n) => {
                    *offset += n;
                    if *offset >= SYSTEM_TELEMETRY_DIGEST_SIZE {
                        self.in_flight = None;
                        sent += 1;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    // Kernel socket buffer full: preserve `in_flight` for next frame, NEVER stall!
                    return (sent, true);
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => {
                    continue;
                }
                Err(_) => {
                    // Disconnected / BrokenPipe / ConnectionReset: drop client cleanly.
                    return (sent, false);
                }
            }
        }
    }
}

struct EndpointState {
    listener: ListenerInner,
    clients: Vec<TelemetryClient>,
    max_clients: usize,
    max_pending_frames: usize,
    frames_sent: u64,
    frames_dropped: u64,
}

impl Drop for EndpointState {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let ListenerInner::Unix { path, .. } = &self.listener {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Non-blocking local telemetry streaming server (`Resource`) emitting fixed 64-byte
/// [`SystemTelemetryDigest64`] frames to connected `bql_lsp` clients in [`Last`].
#[derive(Resource)]
pub struct ModTelemetryEndpoint {
    state: Mutex<EndpointState>,
    #[cfg(unix)]
    unix_path: Option<PathBuf>,
    tcp_addr: Option<SocketAddr>,
}

impl fmt::Debug for ModTelemetryEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ds = f.debug_struct("ModTelemetryEndpoint");
        #[cfg(unix)]
        ds.field("unix_path", &self.unix_path);
        ds.field("tcp_addr", &self.tcp_addr).finish()
    }
}

impl ModTelemetryEndpoint {
    /// Binds a non-blocking local telemetry endpoint for `transport`.
    pub fn bind(transport: ModTelemetryTransport) -> io::Result<Self> {
        match transport {
            #[cfg(unix)]
            ModTelemetryTransport::UnixSocket(path) => Self::bind_unix(path),
            ModTelemetryTransport::LoopbackTcp(addr) => Self::bind_loopback_tcp(addr),
        }
    }

    /// Binds a non-blocking `AF_UNIX` stream listener at `path`, cleaning up any stale socket
    /// file left behind by an unclean exit while refusing to clobber regular files or active
    /// listeners.
    #[cfg(unix)]
    pub fn bind_unix(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if !meta.file_type().is_socket() {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!(
                        "refusing to remove non-socket file at telemetry path {}",
                        path.display()
                    ),
                ));
            }
            match UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        ErrorKind::AddrInUse,
                        format!(
                            "telemetry Unix socket {} is already served by a live process",
                            path.display()
                        ),
                    ));
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) =>
                {
                    let _ = std::fs::remove_file(&path);
                }
                Err(err) => return Err(err),
            }
        }

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            state: Mutex::new(EndpointState {
                listener: ListenerInner::Unix {
                    listener,
                    path: path.clone(),
                },
                clients: Vec::new(),
                max_clients: DEFAULT_MAX_CLIENTS,
                max_pending_frames: DEFAULT_MAX_PENDING_FRAMES_PER_CLIENT,
                frames_sent: 0,
                frames_dropped: 0,
            }),
            unix_path: Some(path),
            tcp_addr: None,
        })
    }

    /// Binds a safe (`#![forbid(unsafe_code)]`) non-blocking loopback TCP telemetry listener
    /// (`127.0.0.1:<port>` or `[::1]:<port>`).
    ///
    /// Rejects non-loopback addresses with [`ErrorKind::InvalidInput`].
    pub fn bind_loopback_tcp(addr: SocketAddr) -> io::Result<Self> {
        if !addr.ip().is_loopback() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("telemetry TCP endpoint must bind a loopback address, got {addr}"),
            ));
        }
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let bound_addr = listener.local_addr()?;
        Ok(Self {
            state: Mutex::new(EndpointState {
                listener: ListenerInner::LoopbackTcp { listener },
                clients: Vec::new(),
                max_clients: DEFAULT_MAX_CLIENTS,
                max_pending_frames: DEFAULT_MAX_PENDING_FRAMES_PER_CLIENT,
                frames_sent: 0,
                frames_dropped: 0,
            }),
            #[cfg(unix)]
            unix_path: None,
            tcp_addr: Some(bound_addr),
        })
    }

    /// Configures the per-client bounded ring depth before oldest unread frames are dropped.
    pub fn set_max_pending_frames(&self, max_pending_frames: usize) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.max_pending_frames = max_pending_frames.max(1);
    }

    /// Configures the maximum number of concurrent local telemetry clients.
    pub fn set_max_clients(&self, max_clients: usize) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.max_clients = max_clients.max(1);
    }

    /// Returns the bound Unix socket path when using [`ModTelemetryTransport::UnixSocket`].
    #[cfg(unix)]
    #[inline]
    pub fn unix_socket_path(&self) -> Option<&Path> {
        self.unix_path.as_deref()
    }

    /// Returns the bound loopback TCP socket address when using
    /// [`ModTelemetryTransport::LoopbackTcp`].
    #[inline]
    pub const fn local_tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    /// Returns the number of currently connected telemetry clients.
    pub fn connected_clients(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clients
            .len()
    }

    /// Returns cumulative 64-byte digest frames successfully written to clients.
    pub fn frames_sent(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frames_sent
    }

    /// Returns cumulative 64-byte digest frames dropped due to slow clients filling their queue.
    pub fn frames_dropped(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frames_dropped
    }

    /// Accepts waiting connections non-blockingly, enqueues `digests`, and flushes all connected
    /// clients without ever blocking the caller.
    pub fn publish_digests(&self, digests: &[SystemTelemetryDigest64]) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());

        // 1. Accept pending connections non-blockingly (bounded per tick).
        for _ in 0..MAX_ACCEPTS_PER_TICK {
            if guard.clients.len() >= guard.max_clients {
                break;
            }
            let accepted = match &guard.listener {
                #[cfg(unix)]
                ListenerInner::Unix { listener, .. } => match listener.accept() {
                    Ok((stream, _)) => {
                        if stream.set_nonblocking(true).is_ok() {
                            Some(ClientStream::Unix(stream))
                        } else {
                            None
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                },
                ListenerInner::LoopbackTcp { listener, .. } => match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        if stream.set_nonblocking(true).is_ok() {
                            Some(ClientStream::Tcp(stream))
                        } else {
                            None
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                },
            };
            if let Some(stream) = accepted {
                guard.clients.push(TelemetryClient::new(stream));
            }
        }

        if guard.clients.is_empty() {
            return;
        }

        // 2. Encode frames once and enqueue + flush non-blockingly for every connected client.
        let encoded: Vec<[u8; SYSTEM_TELEMETRY_DIGEST_SIZE]> = digests
            .iter()
            .map(SystemTelemetryDigest64::to_le_bytes)
            .collect();
        let max_pending = guard.max_pending_frames;
        let mut total_sent = 0u64;
        let mut total_dropped = 0u64;

        guard.clients.retain_mut(|client| {
            for frame in &encoded {
                if client.enqueue_frame(*frame, max_pending) {
                    total_dropped += 1;
                }
            }
            let (sent, alive) = client.try_flush();
            total_sent += sent;
            alive
        });

        guard.frames_sent = guard.frames_sent.saturating_add(total_sent);
        guard.frames_dropped = guard.frames_dropped.saturating_add(total_dropped);
    }
}

// ============================================================================
// `Last`-Schedule Telemetry Sync & Stream Flush System
// ============================================================================

/// `Last`-schedule Bevy system synchronizing quarantine state into [`ModTelemetryRecorder`] and
/// streaming fixed 64-byte [`SystemTelemetryDigest64`] frames to any connected
/// [`ModTelemetryEndpoint`] clients non-blockingly.
pub fn flush_mod_telemetry_system(
    mut recorder: ResMut<ModTelemetryRecorder>,
    quarantine: Option<Res<ModQuarantine>>,
    endpoint: Option<Res<ModTelemetryEndpoint>>,
) {
    if let Some(ref q) = quarantine {
        recorder.sync_quarantine(q);
    }
    let frame_seq = recorder.advance_frame_seq();
    if let Some(ref ep) = endpoint {
        let digests = recorder.build_digests(frame_seq);
        ep.publish_digests(&digests);
    }
}

// ============================================================================
// `ModTelemetryPlugin` & Extension Traits
// ============================================================================

/// Bevy [`Plugin`] installing the `bam/*` BRP telemetry/hot-reload methods (`bam/list_mods`,
/// `bam/system_metrics`, `bam/component_schemas`, `bam/hot_reload`), [`ModTelemetryRecorder`],
/// [`ModHotReloadQueue`], and optional non-blocking local [`ModTelemetryEndpoint`] streaming.
#[derive(Debug, Clone)]
pub struct ModTelemetryPlugin {
    /// Optional local telemetry endpoint transport bound automatically during [`Plugin::build`].
    pub endpoint: Option<ModTelemetryTransport>,
    /// Sliding window size used by [`ModTelemetryRecorder`] for rolling `p50_us` / `p99_us`.
    pub percentile_window: usize,
}

impl Default for ModTelemetryPlugin {
    fn default() -> Self {
        Self {
            endpoint: None,
            percentile_window: DEFAULT_PERCENTILE_WINDOW,
        }
    }
}

impl ModTelemetryPlugin {
    /// Creates a [`ModTelemetryPlugin`] with BRP methods and recorder enabled (no socket bound
    /// until configured or attached via [`ModTelemetryEndpoint`]).
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures a non-blocking Unix domain socket endpoint bound during [`Plugin::build`].
    #[cfg(unix)]
    #[must_use]
    pub fn with_unix_socket(mut self, path: impl AsRef<Path>) -> Self {
        self.endpoint = Some(ModTelemetryTransport::UnixSocket(
            path.as_ref().to_path_buf(),
        ));
        self
    }

    /// Configures a safe non-blocking loopback TCP endpoint bound during [`Plugin::build`].
    #[must_use]
    pub fn with_loopback_tcp(mut self, addr: SocketAddr) -> Self {
        self.endpoint = Some(ModTelemetryTransport::LoopbackTcp(addr));
        self
    }

    /// Sets the rolling latency window size for `p50_us` / `p99_us` calculation.
    #[must_use]
    pub fn with_percentile_window(mut self, window: usize) -> Self {
        self.percentile_window = window.max(1);
        self
    }
}

impl Plugin for ModTelemetryPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<ModTelemetryRecorder>() {
            app.insert_resource(ModTelemetryRecorder::new(self.percentile_window));
        }
        app.init_resource::<ModHotReloadQueue>();
        if let Some(ref transport) = self.endpoint
            && !app.world().contains_resource::<ModTelemetryEndpoint>()
            && let Ok(endpoint) = ModTelemetryEndpoint::bind(transport.clone())
        {
            app.insert_resource(endpoint);
        }
        app.add_systems(
            Last,
            (process_pending_hot_reloads, flush_mod_telemetry_system).chain(),
        );
    }

    fn finish(&self, app: &mut App) {
        if app.world().contains_resource::<RemoteMethods>() {
            install_bam_telemetry_brp_methods(app.world_mut());
        }
    }
}

/// Extension trait on Bevy [`RemotePlugin`] attaching the four `bam/*` telemetry & hot-reload BRP
/// methods before adding [`RemotePlugin`] to an [`App`].
pub trait ModTelemetryRemoteExt {
    /// Registers `bam/list_mods`, `bam/system_metrics`, `bam/component_schemas`, and
    /// `bam/hot_reload` onto `self`.
    fn with_bam_telemetry_methods(self) -> Self;
}

impl ModTelemetryRemoteExt for RemotePlugin {
    fn with_bam_telemetry_methods(self) -> Self {
        self.with_method_main(BRP_METHOD_BAM_LIST_MODS, brp_bam_list_mods_system)
            .with_method_main(BRP_METHOD_BAM_SYSTEM_METRICS, brp_bam_system_metrics_system)
            .with_method_main(
                BRP_METHOD_BAM_COMPONENT_SCHEMAS,
                brp_bam_component_schemas_system,
            )
            .with_method_main(BRP_METHOD_BAM_HOT_RELOAD, brp_bam_hot_reload_system)
    }
}

/// Installs all four `bam/*` telemetry and hot-reload BRP handlers into `world`'s
/// [`RemoteMethods`] resource (no-op if [`RemoteMethods`] is absent).
pub fn install_bam_telemetry_brp_methods(world: &mut World) {
    if !world.contains_resource::<RemoteMethods>() {
        return;
    }
    let pairs: [(&str, RemoteMethodSystemId); 4] = [
        (
            BRP_METHOD_BAM_LIST_MODS,
            RemoteMethodSystemId::Instant(world.register_system(brp_bam_list_mods_system)),
        ),
        (
            BRP_METHOD_BAM_SYSTEM_METRICS,
            RemoteMethodSystemId::Instant(world.register_system(brp_bam_system_metrics_system)),
        ),
        (
            BRP_METHOD_BAM_COMPONENT_SCHEMAS,
            RemoteMethodSystemId::Instant(world.register_system(brp_bam_component_schemas_system)),
        ),
        (
            BRP_METHOD_BAM_HOT_RELOAD,
            RemoteMethodSystemId::Instant(world.register_system(brp_bam_hot_reload_system)),
        ),
    ];
    let mut remote = world.resource_mut::<RemoteMethods>();
    for (name, id) in pairs {
        remote.insert(name, id);
    }
}

/// Safe [`World`] extension trait for dispatching `bam/*` BRP telemetry requests, recording
/// system execution samples, and binding local [`ModTelemetryEndpoint`] streams.
pub trait ModTelemetryWorldExt {
    /// Executes a `bam/*` BRP [`BrpRequest`] synchronously against `self`.
    fn dispatch_telemetry_brp_request(&mut self, request: BrpRequest) -> BrpResult;

    /// Records a [`SystemExecutionSample`] into [`ModTelemetryRecorder`] (initializing it if
    /// absent).
    fn record_system_telemetry(&mut self, sample: SystemExecutionSample);

    /// Binds and inserts a non-blocking Unix domain socket [`ModTelemetryEndpoint`] into `self`.
    #[cfg(unix)]
    fn bind_telemetry_unix_socket(&mut self, path: impl AsRef<Path>) -> io::Result<()>;

    /// Binds and inserts a safe non-blocking loopback TCP [`ModTelemetryEndpoint`] into `self`,
    /// returning the bound [`SocketAddr`].
    fn bind_telemetry_loopback_tcp(&mut self, addr: SocketAddr) -> io::Result<SocketAddr>;
}

impl ModTelemetryWorldExt for World {
    fn dispatch_telemetry_brp_request(&mut self, request: BrpRequest) -> BrpResult {
        handle_telemetry_brp_request(self, &request)
    }

    fn record_system_telemetry(&mut self, sample: SystemExecutionSample) {
        if !self.contains_resource::<ModTelemetryRecorder>() {
            self.insert_resource(ModTelemetryRecorder::default());
        }
        self.resource_mut::<ModTelemetryRecorder>()
            .record_sample(sample);
    }

    #[cfg(unix)]
    fn bind_telemetry_unix_socket(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let ep = ModTelemetryEndpoint::bind_unix(path)?;
        self.insert_resource(ep);
        Ok(())
    }

    fn bind_telemetry_loopback_tcp(&mut self, addr: SocketAddr) -> io::Result<SocketAddr> {
        let ep = ModTelemetryEndpoint::bind_loopback_tcp(addr)?;
        let bound = ep.local_tcp_addr().unwrap_or(addr);
        self.insert_resource(ep);
        Ok(bound)
    }
}

// ============================================================================
// BRP JSON-RPC Method Handlers (`bam/*`)
// ============================================================================

fn brp_err(code: i16, message: impl Into<String>) -> BrpError {
    BrpError {
        code,
        message: message.into(),
        data: None,
    }
}

/// Dispatches a Bevy Remote Protocol [`BrpRequest`] against `world` for the four `bam/*`
/// telemetry and hot-reload methods.
pub fn handle_telemetry_brp_request(world: &mut World, request: &BrpRequest) -> BrpResult {
    match request.method.as_str() {
        BRP_METHOD_BAM_LIST_MODS => brp_bam_list_mods_impl(world, request.params.as_ref()),
        BRP_METHOD_BAM_SYSTEM_METRICS => {
            brp_bam_system_metrics_impl(world, request.params.as_ref())
        }
        BRP_METHOD_BAM_COMPONENT_SCHEMAS => {
            brp_bam_component_schemas_impl(world, request.params.as_ref())
        }
        BRP_METHOD_BAM_HOT_RELOAD => brp_bam_hot_reload_impl(world, request.params.as_ref()),
        other => Err(brp_err(
            error_codes::METHOD_NOT_FOUND,
            format!("unknown bam telemetry BRP method '{other}'"),
        )),
    }
}

fn brp_bam_list_mods_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_bam_list_mods_impl(world, params.as_ref())
}

fn brp_bam_system_metrics_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_bam_system_metrics_impl(world, params.as_ref())
}

fn brp_bam_component_schemas_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_bam_component_schemas_impl(world, params.as_ref())
}

fn brp_bam_hot_reload_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_bam_hot_reload_impl(world, params.as_ref())
}

#[derive(Clone, Debug)]
struct DiscoveredModSummary {
    mod_id: u64,
    name: String,
    version: String,
    path: String,
    system_count: usize,
    fault_count: usize,
    is_active: bool,
}

fn discover_world_mods(world: &World) -> Vec<DiscoveredModSummary> {
    let mut by_id: BTreeMap<u64, DiscoveredModSummary> = BTreeMap::new();

    if let Some(info_id) = world.components().component_id::<LoadedModInfo>() {
        for arch in world.archetypes().iter() {
            if !arch.contains(info_id) {
                continue;
            }
            for arch_ent in arch.entities() {
                if let Some(info) = world.get::<LoadedModInfo>(arch_ent.id()) {
                    by_id.insert(
                        info.id.0,
                        DiscoveredModSummary {
                            mod_id: info.id.0,
                            name: info.name.clone(),
                            version: info.version.clone(),
                            path: info.path.display().to_string(),
                            system_count: info.system_count,
                            fault_count: info.fault_count,
                            is_active: true,
                        },
                    );
                }
            }
        }
    }

    if let Some(host) = world.get_resource::<ModHost>() {
        for loaded in host.loaded_mods() {
            by_id.insert(
                loaded.id().0,
                DiscoveredModSummary {
                    mod_id: loaded.id().0,
                    name: loaded.name().to_owned(),
                    version: loaded.version().to_owned(),
                    path: loaded.source_path().display().to_string(),
                    system_count: loaded.system_count(),
                    fault_count: loaded.fault_count(),
                    is_active: loaded.is_active(),
                },
            );
        }
    }

    by_id.into_values().collect()
}

fn brp_bam_list_mods_impl(world: &World, _params: Option<&Value>) -> BrpResult {
    let mods = discover_world_mods(world);
    let mut quarantined_by_mod: HashMap<u64, Vec<String>> = HashMap::default();
    let mut total_quarantined_systems = 0usize;

    if let Some(quarantine) = world.get_resource::<ModQuarantine>() {
        for (mod_id, sys_name) in quarantine.quarantined_systems() {
            quarantined_by_mod
                .entry(mod_id.0)
                .or_default()
                .push(sys_name.to_owned());
            total_quarantined_systems += 1;
        }
    }
    for list in quarantined_by_mod.values_mut() {
        list.sort();
    }

    let mut items = Vec::with_capacity(mods.len());
    for m in &mods {
        let q_systems = quarantined_by_mod
            .get(&m.mod_id)
            .cloned()
            .unwrap_or_default();
        items.push(serde_json::json!({
            "mod_id": m.mod_id,
            "name": m.name,
            "version": m.version,
            "path": m.path,
            "system_count": m.system_count,
            "fault_count": m.fault_count,
            "is_active": m.is_active,
            "quarantined": !q_systems.is_empty(),
            "quarantined_systems": q_systems,
        }));
    }

    Ok(serde_json::json!({
        "mods": items,
        "total_mods": mods.len(),
        "total_quarantined_systems": total_quarantined_systems,
    }))
}

fn brp_bam_system_metrics_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let filter_mod_id = params.and_then(|p| p.get("mod_id")).and_then(Value::as_u64);
    let filter_mod_name = params
        .and_then(|p| p.get("mod_name"))
        .and_then(Value::as_str);
    let filter_sys_name = params
        .and_then(|p| p.get("system_name"))
        .and_then(Value::as_str);

    if let (Some(q), Some(mut rec)) = (
        world.get_resource::<ModQuarantine>().cloned(),
        world.get_resource_mut::<ModTelemetryRecorder>(),
    ) {
        rec.sync_quarantine(&q);
    }

    let (frame_seq, metrics) = world
        .get_resource::<ModTelemetryRecorder>()
        .map(|rec| (rec.frame_seq(), rec.system_metrics()))
        .unwrap_or_else(|| (0, Vec::new()));

    let filtered: Vec<Value> = metrics
        .into_iter()
        .filter(|m| filter_mod_id.is_none_or(|id| m.mod_id == id))
        .filter(|m| filter_mod_name.is_none_or(|name| m.mod_name == name))
        .filter(|m| filter_sys_name.is_none_or(|sys| m.system_name == sys))
        .map(|m| {
            serde_json::json!({
                "mod_id": m.mod_id,
                "mod_name": m.mod_name,
                "system_name": m.system_name,
                "system_hash": m.system_hash,
                "matched_entities": m.matched_entities,
                "matched_tables": m.matched_tables,
                "skipped_summary_tick_tables": m.skipped_summary_tick_tables,
                "p50_us": m.p50_us,
                "p99_us": m.p99_us,
                "last_duration_us": m.last_duration_us,
                "chunks": m.chunks,
                "commands_queued": m.commands_queued,
                "sample_count": m.sample_count,
                "quarantined": m.quarantined,
                "faulted": m.faulted,
                "budget_breached": m.budget_breached,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "frame_seq": frame_seq,
        "systems": filtered,
    }))
}

fn brp_bam_component_schemas_impl(world: &World, params: Option<&Value>) -> BrpResult {
    let filter_component = params
        .and_then(|p| p.get("component"))
        .and_then(Value::as_str);
    let filter_mod_id = params.and_then(|p| p.get("mod_id")).and_then(Value::as_u64);
    let include_builtin = params
        .and_then(|p| p.get("include_builtin"))
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let Some(registry) = world.get_resource::<ModRegistry>() else {
        return Ok(serde_json::json!({ "components": [] }));
    };

    let mut schemas = Vec::new();
    for entry in registry.snapshot().entries() {
        if entry.is_message() {
            continue;
        }
        if !include_builtin
            && (entry.name.as_ref() == bevy_abi_mod_sys::BAM_CHILD_OF_NAME
                || entry.name.as_ref() == bevy_abi_mod_sys::BAM_CHILDREN_NAME)
        {
            continue;
        }
        if let Some(target_name) = filter_component
            && entry.name.as_ref() != target_name
        {
            continue;
        }
        if let Some(target_owner) = filter_mod_id
            && entry.owner.map(|id| id.0) != Some(target_owner)
        {
            continue;
        }

        let fields_json: Vec<Value> = if let Ok(Some(bam_fields)) =
            inspect_component_bam_fields(world, entry.handle)
        {
            bam_fields
                .into_iter()
                .map(|f| {
                    serde_json::json!({
                        "index": f.index,
                        "name": f.name,
                        "type_tag": f.type_tag,
                        "kind": f.kind.0,
                        "kind_name": format!("{:?}", f.kind),
                        "pod_kind": f.pod_kind.map(|pk| format!("{pk:?}")),
                        "offset": f.offset,
                        "size": f.size,
                        "align": f.align,
                        "is_inplace_reflectable": f.is_inplace_reflectable,
                    })
                })
                .collect()
        } else if let Some(witness_fields) = inspect_component_witness_fields(world, entry.handle)
            .or_else(|| inspect_component_witness_fields_in_registry(registry, entry.handle))
        {
            witness_fields
                .into_iter()
                .map(|f| {
                    serde_json::json!({
                        "index": f.index,
                        "name": f.name,
                        "type_tag": f.type_tag,
                        "offset": f.offset,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        schemas.push(serde_json::json!({
            "handle": entry.handle.0,
            "name": entry.name.as_ref(),
            "uuid": format_component_uuid(entry.uuid),
            "size": entry.size,
            "align": entry.align,
            "schema_fingerprint": entry.schema_fingerprint,
            "is_abi_pod": entry.is_abi_pod,
            "is_dynamic": entry.is_dynamic,
            "is_resource": entry.is_resource(),
            "is_immutable": entry.is_immutable(),
            "is_sparse_set": entry.is_sparse_set(),
            "is_summary_tick": entry.is_summary_tick(),
            "owner_mod_id": entry.owner.map(|id| id.0),
            "fields": fields_json,
        }));
    }

    Ok(serde_json::json!({
        "components": schemas,
    }))
}

fn brp_bam_hot_reload_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let params = params.ok_or_else(|| {
        brp_err(
            error_codes::INVALID_PARAMS,
            "missing params: expected `mod_id` or `mod_name`",
        )
    })?;
    let requested_id = params.get("mod_id").and_then(Value::as_u64);
    let requested_name = params
        .get("mod_name")
        .or_else(|| params.get("name"))
        .and_then(Value::as_str);
    if requested_id.is_none() && requested_name.is_none() {
        return Err(brp_err(
            error_codes::INVALID_PARAMS,
            "expected `mod_id` (u64) or `mod_name` / `name` (string)",
        ));
    }
    let override_path = params
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let immediate = params
        .get("immediate")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mods = discover_world_mods(world);
    let target = mods.into_iter().find(|m| {
        let id_ok = requested_id.is_none_or(|id| m.mod_id == id);
        let name_ok = requested_name.is_none_or(|name| m.name == name);
        id_ok && name_ok
    });
    let Some(target) = target else {
        return Err(brp_err(
            error_codes::RESOURCE_NOT_PRESENT,
            format!("no active mod matching mod_id={requested_id:?}, mod_name={requested_name:?}"),
        ));
    };

    let frame_seq = world
        .get_resource::<ModTelemetryRecorder>()
        .map_or(0, ModTelemetryRecorder::frame_seq);
    let request = HotReloadRequest {
        mod_id: target.mod_id,
        mod_name: target.name.clone(),
        override_path: override_path.clone(),
        requested_at_frame: frame_seq,
    };

    let update_leased_out = world
        .get_resource::<Schedules>()
        .is_some_and(|schedules| !schedules.contains(Update));

    if immediate && !update_leased_out {
        if !world.contains_resource::<ModHotReloadQueue>() {
            world.insert_resource(ModHotReloadQueue::new());
        }
        let outcome = execute_hot_reload_request(world, &request);
        world
            .resource_mut::<ModHotReloadQueue>()
            .history
            .push(outcome.clone());
        return match outcome {
            HotReloadOutcome::Reloaded {
                old_mod_id,
                new_mod_id,
                mod_name,
            } => Ok(serde_json::json!({
                "status": "reloaded",
                "old_mod_id": old_mod_id,
                "new_mod_id": new_mod_id,
                "mod_name": mod_name,
            })),
            HotReloadOutcome::Failed {
                mod_id,
                mod_name,
                error,
            } => Err(brp_err(
                error_codes::INTERNAL_ERROR,
                format!("hot-reload failed for mod '{mod_name}' (id={mod_id}): {error}"),
            )),
        };
    }

    if !world.contains_resource::<ModHotReloadQueue>() {
        world.insert_resource(ModHotReloadQueue::new());
    }
    world.resource_mut::<ModHotReloadQueue>().push(request);

    let mut out = Map::new();
    out.insert("status".to_owned(), Value::String("scheduled".to_owned()));
    out.insert("mod_id".to_owned(), Value::from(target.mod_id));
    out.insert("mod_name".to_owned(), Value::String(target.name));
    out.insert("requested_at_frame".to_owned(), Value::from(frame_seq));
    if let Some(path) = override_path {
        out.insert(
            "override_path".to_owned(),
            Value::String(path.display().to_string()),
        );
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_telemetry_digest64_exact_size_and_le_roundtrip() {
        assert_eq!(
            core::mem::size_of::<SystemTelemetryDigest64>(),
            SYSTEM_TELEMETRY_DIGEST_SIZE
        );
        let digest = SystemTelemetryDigest64 {
            magic: SYSTEM_TELEMETRY_MAGIC,
            version: SYSTEM_TELEMETRY_DIGEST_VERSION,
            flags: TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE | TELEMETRY_FLAG_QUARANTINED,
            mod_id: 7,
            system_hash: fnv1a_64_hash_system("combat_mod", "apply_damage"),
            frame_seq: 12345,
            matched_entities: 5000,
            matched_tables: 12,
            skipped_summary_tick_tables: 9,
            chunks: 3,
            p50_us: 84,
            p99_us: 295,
            commands_queued: 14,
            reserved: 0,
        };
        let wire = digest.to_le_bytes();
        assert_eq!(wire.len(), 64);
        assert_eq!(&wire[0..4], b"BAMT");
        let decoded = SystemTelemetryDigest64::from_le_bytes(&wire).unwrap();
        assert_eq!(decoded, digest);

        let mut bad_magic = wire;
        bad_magic[0] = b'X';
        assert!(matches!(
            SystemTelemetryDigest64::from_le_bytes(&bad_magic),
            Err(TelemetryFrameError::InvalidMagic(_))
        ));

        let mut bad_ver = wire;
        bad_ver[4..6].copy_from_slice(&99u16.to_le_bytes());
        assert!(matches!(
            SystemTelemetryDigest64::from_le_bytes(&bad_ver),
            Err(TelemetryFrameError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn rolling_percentiles_p50_and_p99_are_exact() {
        let mut rec = ModTelemetryRecorder::new(100);
        for d in 1u64..=100u64 {
            rec.record_sample(SystemExecutionSample {
                mod_id: Some(ModId(1)),
                mod_name: "bench_mod".to_owned(),
                system_name: "tick".to_owned(),
                duration_us: d * 10,
                matched_entities: 500,
                matched_tables: 5,
                skipped_summary_tick_tables: 4,
                chunks: 1,
                commands_queued: 2,
                faulted: false,
                budget_breached: false,
            });
        }
        let metrics = rec.system_metrics();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].p50_us, 500);
        assert_eq!(metrics[0].p99_us, 990);
        assert_eq!(metrics[0].sample_count, 100);
    }
}
