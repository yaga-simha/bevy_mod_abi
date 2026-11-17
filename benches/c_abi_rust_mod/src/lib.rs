#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]
#![allow(clippy::cast_ptr_alignment, unreachable_pub)]

//! Asymmetric C-ABI Rust Benchmark Mod (`cdylib`, independent `Cargo.lock`, zero `bevy` dep, `ABI 0.2.0`).
//!
//! Implements the Multi-Archetype Nested Query Scenario in 3 progressive patterns
//! selectable via `c_abi_rust_mod_set_variant(u32)` prior to `bevy_abi_mod_init`:
//! - `0` (`ChunkOptimized`): Unpacks emitter chunks once (`O(U + E)` VTable calls), runs inner loop in registers.
//! - `1` (`EntityNestedVTable`): Re-traverses emitter chunks per unit via `iter_reset`, invoking VTable getters on every `(u, e)` pair (`O(U * E)` cross-ABI calls).
//! - `2` (`NaiveChattyFfi`): Re-reads both unit and emitter via VTable on every pair and performs immediate VTable writes + `iter_mark_changed_range` inside the inner loop.

use bevy_abi_mod_guest::prelude::*;
use bevy_abi_mod_guest::raw::BamColumnInfo;
use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicU32, Ordering};

const SIM_DT: f32 = 0.016_f32;
const MAX_CACHED_EMITTERS: usize = 512;

// ============================================================================
// 1. VTable Mirrors Matching Host `#[derive(ModComponent)]` Layouts (`M-04`, `M-05`)
// ============================================================================

/// C-ABI VTable mirror for `shared_scenario::Vec3f`.
#[repr(C)]
pub struct Vec3fVTable {
    /// Standard ABI 0.2 VTable header.
    pub header: VTableHeader,
    /// Reads `x`.
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `x`.
    pub set_x: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `y`.
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `y`.
    pub set_y: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `z`.
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `z`.
    pub set_z: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
}

/// C-ABI VTable mirror for `shared_scenario::Transform3D`.
#[repr(C)]
pub struct Transform3DVTable {
    /// Standard ABI 0.2 VTable header.
    pub header: VTableHeader,
    /// Interior const pointer to `position`.
    pub ptr_position: unsafe extern "C" fn(*const c_void) -> *const c_void,
    /// Interior mut pointer to `position`.
    pub ptr_mut_position: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    /// Interior const pointer to `forward`.
    pub ptr_forward: unsafe extern "C" fn(*const c_void) -> *const c_void,
    /// Interior mut pointer to `forward`.
    pub ptr_mut_forward: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    /// Reads `scale`.
    pub get_scale: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `scale`.
    pub set_scale: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
}

/// C-ABI VTable mirror for `shared_scenario::Kinematics`.
#[repr(C)]
pub struct KinematicsVTable {
    /// Standard ABI 0.2 VTable header.
    pub header: VTableHeader,
    /// Interior const pointer to `velocity`.
    pub ptr_velocity: unsafe extern "C" fn(*const c_void) -> *const c_void,
    /// Interior mut pointer to `velocity`.
    pub ptr_mut_velocity: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    /// Reads `drag`.
    pub get_drag: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `drag`.
    pub set_drag: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `max_speed`.
    pub get_max_speed: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `max_speed`.
    pub set_max_speed: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
}

/// C-ABI VTable mirror for `shared_scenario::Vitality`.
#[repr(C)]
pub struct VitalityVTable {
    /// Standard ABI 0.2 VTable header.
    pub header: VTableHeader,
    /// Reads `health`.
    pub get_health: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `health`.
    pub set_health: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `max_health`.
    pub get_max_health: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `max_health`.
    pub set_max_health: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `shield`.
    pub get_shield: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `shield`.
    pub set_shield: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `energy`.
    pub get_energy: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `energy`.
    pub set_energy: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
}

/// C-ABI VTable mirror for `shared_scenario::Resistance`.
#[repr(C)]
pub struct ResistanceVTable {
    /// Standard ABI 0.2 VTable header.
    pub header: VTableHeader,
    /// Reads `kinetic`.
    pub get_kinetic: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `kinetic`.
    pub set_kinetic: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `thermal`.
    pub get_thermal: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `thermal`.
    pub set_thermal: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `ionic`.
    pub get_ionic: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `ionic`.
    pub set_ionic: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
}

/// C-ABI VTable mirror for `shared_scenario::AuraEmitter`.
#[repr(C)]
pub struct AuraEmitterVTable {
    /// Standard ABI 0.2 VTable header.
    pub header: VTableHeader,
    /// Reads `radius`.
    pub get_radius: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `radius`.
    pub set_radius: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `force`.
    pub get_force: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `force`.
    pub set_force: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `thermal_dps`.
    pub get_thermal_dps: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `thermal_dps`.
    pub set_thermal_dps: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `heal_hps`.
    pub get_heal_hps: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `heal_hps`.
    pub set_heal_hps: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
    /// Reads `falloff`.
    pub get_falloff: unsafe extern "C" fn(*const c_void) -> f32,
    /// Writes `falloff`.
    pub set_falloff: unsafe extern "C" fn(*mut c_void, f32) -> FfiResult,
}

/// C-ABI VTable mirror for `shared_scenario::Faction`.
#[repr(C)]
pub struct FactionVTable {
    /// Standard ABI 0.2 VTable header.
    pub header: VTableHeader,
    /// Reads `team_id`.
    pub get_team_id: unsafe extern "C" fn(*const c_void) -> u32,
    /// Writes `team_id`.
    pub set_team_id: unsafe extern "C" fn(*mut c_void, u32) -> FfiResult,
    /// Reads `serial_id`.
    pub get_serial_id: unsafe extern "C" fn(*const c_void) -> u32,
    /// Writes `serial_id`.
    pub set_serial_id: unsafe extern "C" fn(*mut c_void, u32) -> FfiResult,
}

// ============================================================================
// 2. Per-Instance VTable Bundle (`L-02`) & Pre-Load Variant Selector
// ============================================================================

static BENCH_VARIANT: AtomicU32 = AtomicU32::new(0);

/// Selects the benchmark execution pattern (`0`, `1`, or `2`) for the next `bevy_abi_mod_init`.
#[unsafe(no_mangle)]
pub extern "C" fn c_abi_rust_mod_set_variant(variant: u32) {
    BENCH_VARIANT.store(variant, Ordering::SeqCst);
}

#[derive(Clone, Copy)]
struct VTableBundle {
    v3: *const Vec3fVTable,
    tf: *const Transform3DVTable,
    kin: *const KinematicsVTable,
    vit: *const VitalityVTable,
    res: *const ResistanceVTable,
    aura: *const AuraEmitterVTable,
    fac: *const FactionVTable,
}

// SAFETY: Host static VTable pointers reside in host `.rodata` for the entire lifetime of the
// owning Bevy `World` and contain only read-only metadata and thread-safe `extern "C"` fn pointers.
unsafe impl Send for VTableBundle {}
// SAFETY: Immutable static function-table pointers are safe to share across worker threads.
unsafe impl Sync for VTableBundle {}

impl VTableBundle {
    fn resolve(app: &GuestApp<'_>) -> Result<Self, FfiResult> {
        unsafe fn cast_vt<T>(app: &GuestApp<'_>, sym: &CStr) -> Result<*const T, FfiResult> {
            let raw = app.resolve_custom_symbol(sym)?;
            if raw.is_null() {
                return Err(FfiResult::INVALID_COMPONENT);
            }
            let header_ptr = raw.cast::<VTableHeader>();
            if !header_ptr.is_aligned() {
                return Err(FfiResult::INVALID_COMPONENT);
            }
            // SAFETY: `raw` is non-null, aligned to `VTableHeader`, and points to a host static VTable.
            let header = unsafe { &*header_ptr };
            if (header.struct_size as usize) < core::mem::size_of::<T>() {
                return Err(FfiResult::INVALID_VERSION);
            }
            Ok(raw.cast::<T>())
        }

        // SAFETY: Each symbol resolves a host-generated `<Struct>VTable` whose layout matches `T`.
        unsafe {
            Ok(Self {
                v3: cast_vt::<Vec3fVTable>(app, c"bam.v0.shared_scenario::Vec3f.vtable")?,
                tf: cast_vt::<Transform3DVTable>(
                    app,
                    c"bam.v0.shared_scenario::Transform3D.vtable",
                )?,
                kin: cast_vt::<KinematicsVTable>(
                    app,
                    c"bam.v0.shared_scenario::Kinematics.vtable",
                )?,
                vit: cast_vt::<VitalityVTable>(app, c"bam.v0.shared_scenario::Vitality.vtable")?,
                res: cast_vt::<ResistanceVTable>(
                    app,
                    c"bam.v0.shared_scenario::Resistance.vtable",
                )?,
                aura: cast_vt::<AuraEmitterVTable>(
                    app,
                    c"bam.v0.shared_scenario::AuraEmitter.vtable",
                )?,
                fac: cast_vt::<FactionVTable>(app, c"bam.v0.shared_scenario::Faction.vtable")?,
            })
        }
    }
}

#[derive(Clone, Copy)]
struct UnpackedEmitter {
    px: f32,
    py: f32,
    pz: f32,
    fx: f32,
    fy: f32,
    fz: f32,
    radius: f32,
    rad_sq: f32,
    force: f32,
    thermal_dps: f32,
    heal_hps: f32,
    falloff: f32,
    team_id: u32,
}

impl UnpackedEmitter {
    const ZERO: Self = Self {
        px: 0.0,
        py: 0.0,
        pz: 0.0,
        fx: 0.0,
        fy: 0.0,
        fz: 0.0,
        radius: 0.0,
        rad_sq: 0.0,
        force: 0.0,
        thermal_dps: 0.0,
        heal_hps: 0.0,
        falloff: 0.0,
        team_id: 0,
    };
}

// ============================================================================
// Variant 0: Chunk-Optimized Nested Query (`O(U + E)` C-ABI boundary calls)
// ============================================================================

fn run_chunk_optimized(ctx: &mut SystemContext<'_>, vts: VTableBundle) -> FfiResult {
    let api = *ctx.host_api();
    let Some(iter_next_chunk) = api.iter_next_chunk else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_col_ro) = api.iter_column_readonly else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_col_mut) = api.iter_column_mutable else {
        return FfiResult::NULL_POINTER;
    };

    let Some((u_iter, e_iter)) = ctx.queries_2_mut(0, 1) else {
        return FfiResult::INTERNAL_ERROR;
    };
    let unit_iter = u_iter.as_raw_ptr();
    let emit_iter = e_iter.as_raw_ptr();

    // SAFETY: VTable pointers in `vts` were validated during `bevy_abi_mod_init` and remain static.
    let (v3, tf_vt, kin_vt, vit_vt, res_vt, aura_vt, fac_vt) = unsafe {
        (
            &*vts.v3, &*vts.tf, &*vts.kin, &*vts.vit, &*vts.res, &*vts.aura, &*vts.fac,
        )
    };

    let mut stack_emitters = [UnpackedEmitter::ZERO; MAX_CACHED_EMITTERS];
    let mut heap_emitters: Vec<UnpackedEmitter> = Vec::new();
    let mut total_emitters = 0_usize;

    loop {
        let mut e_count = 0_usize;
        // SAFETY: `emit_iter` is an active `BamIter` for this `SystemContext` invocation.
        if !unsafe { iter_next_chunk(emit_iter, &raw mut e_count) }.get() || e_count == 0 {
            break;
        }
        // SAFETY: Columns 0..3 match `Transform3D`, `AuraEmitter`, `Faction` read terms.
        let (e_tf_col, e_aura_col, e_fac_col) = unsafe {
            (
                iter_col_ro(emit_iter, 0),
                iter_col_ro(emit_iter, 1),
                iter_col_ro(emit_iter, 2),
            )
        };

        for row in 0..e_count {
            // SAFETY: `row < e_count`; column strides match VTable header sizes.
            let item = unsafe {
                let tf_p = e_tf_col.add(row * tf_vt.header.size).cast::<c_void>();
                let aura_p = e_aura_col.add(row * aura_vt.header.size).cast::<c_void>();
                let fac_p = e_fac_col.add(row * fac_vt.header.size).cast::<c_void>();

                let pos_p = (tf_vt.ptr_position)(tf_p);
                let fwd_p = (tf_vt.ptr_forward)(tf_p);
                let radius = (aura_vt.get_radius)(aura_p);

                UnpackedEmitter {
                    px: (v3.get_x)(pos_p),
                    py: (v3.get_y)(pos_p),
                    pz: (v3.get_z)(pos_p),
                    fx: (v3.get_x)(fwd_p),
                    fy: (v3.get_y)(fwd_p),
                    fz: (v3.get_z)(fwd_p),
                    radius,
                    rad_sq: radius * radius,
                    force: (aura_vt.get_force)(aura_p),
                    thermal_dps: (aura_vt.get_thermal_dps)(aura_p),
                    heal_hps: (aura_vt.get_heal_hps)(aura_p),
                    falloff: (aura_vt.get_falloff)(aura_p),
                    team_id: (fac_vt.get_team_id)(fac_p),
                }
            };

            if total_emitters < MAX_CACHED_EMITTERS && heap_emitters.is_empty() {
                stack_emitters[total_emitters] = item;
            } else {
                if heap_emitters.is_empty() {
                    heap_emitters.extend_from_slice(&stack_emitters[..total_emitters]);
                }
                heap_emitters.push(item);
            }
            total_emitters += 1;
        }
    }

    let emit_slice: &[UnpackedEmitter] = if heap_emitters.is_empty() {
        &stack_emitters[..total_emitters]
    } else {
        heap_emitters.as_slice()
    };

    loop {
        let mut u_count = 0_usize;
        // SAFETY: `unit_iter` is an active `BamIter` for query 0.
        if !unsafe { iter_next_chunk(unit_iter, &raw mut u_count) }.get() || u_count == 0 {
            break;
        }
        // SAFETY: Columns 0..5 match `Transform3D`, `Kinematics`, `Vitality`, `Resistance`, `Faction`.
        let (u_tf_col, u_kin_col, u_vit_col, u_res_col, u_fac_col) = unsafe {
            (
                iter_col_mut(unit_iter, 0),
                iter_col_mut(unit_iter, 1),
                iter_col_mut(unit_iter, 2),
                iter_col_ro(unit_iter, 3),
                iter_col_ro(unit_iter, 4),
            )
        };

        for row in 0..u_count {
            // SAFETY: `row < u_count`; pointers address valid unit components in this chunk.
            unsafe {
                let tf_p = u_tf_col.add(row * tf_vt.header.size).cast::<c_void>();
                let kin_p = u_kin_col.add(row * kin_vt.header.size).cast::<c_void>();
                let vit_p = u_vit_col.add(row * vit_vt.header.size).cast::<c_void>();
                let res_p = u_res_col.add(row * res_vt.header.size).cast::<c_void>();
                let fac_p = u_fac_col.add(row * fac_vt.header.size).cast::<c_void>();

                let pos_p = (tf_vt.ptr_mut_position)(tf_p);
                let ux = (v3.get_x)(pos_p);
                let uy = (v3.get_y)(pos_p);
                let uz = (v3.get_z)(pos_p);

                let vel_p = (kin_vt.ptr_mut_velocity)(kin_p);
                let mut vx = (v3.get_x)(vel_p);
                let mut vy = (v3.get_y)(vel_p);
                let mut vz = (v3.get_z)(vel_p);
                let drag = (kin_vt.get_drag)(kin_p);
                let max_speed = (kin_vt.get_max_speed)(kin_p);

                let mut health = (vit_vt.get_health)(vit_p);
                let max_health = (vit_vt.get_max_health)(vit_p);
                let mut shield = (vit_vt.get_shield)(vit_p);
                let mut energy = (vit_vt.get_energy)(vit_p);

                let r_kin = (res_vt.get_kinetic)(res_p);
                let r_therm = (res_vt.get_thermal)(res_p);
                let r_ion = (res_vt.get_ionic)(res_p);
                let u_team = (fac_vt.get_team_id)(fac_p);

                for em in emit_slice {
                    let dx = ux - em.px;
                    let dy = uy - em.py;
                    let dz = uz - em.pz;
                    let dist_sq = dx * dx + dy * dy + dz * dz + 0.25_f32;

                    if dist_sq < em.rad_sq {
                        let dist = dist_sq.sqrt();
                        let inv_dist = 1.0_f32 / dist;
                        let norm = 1.0_f32 - (dist / em.radius);
                        let atten = norm / (1.0_f32 + em.falloff * dist_sq);

                        if u_team == em.team_id {
                            let boost = em.force * 0.25_f32 * atten * SIM_DT;
                            vx += em.fx * boost;
                            vy += em.fy * boost;
                            vz += em.fz * boost;

                            let heal = em.heal_hps * atten * SIM_DT * (1.0_f32 + 0.1_f32 * r_ion);
                            let nh = health + heal;
                            health = if nh > max_health { max_health } else { nh };

                            let ne = energy + 2.5_f32 * atten * SIM_DT;
                            energy = if ne > 100.0_f32 { 100.0_f32 } else { ne };
                        } else {
                            let imp =
                                em.force * atten * SIM_DT * (1.0_f32 - 0.5_f32 * r_kin) * inv_dist;
                            vx += dx * imp;
                            vy += dy * imp;
                            vz += dz * imp;

                            let raw_dmg =
                                em.thermal_dps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * r_therm);
                            let absorbed = if shield >= raw_dmg { raw_dmg } else { shield };
                            shield -= absorbed;
                            let nh = health - (raw_dmg - absorbed);
                            health = if nh < 0.0_f32 { 0.0_f32 } else { nh };

                            let drain = 1.5_f32 * atten * SIM_DT * (1.0_f32 - 0.4_f32 * r_ion);
                            let ne = energy - drain;
                            energy = if ne < 0.0_f32 { 0.0_f32 } else { ne };
                        }
                    }
                }

                let damp = 1.0_f32 / (1.0_f32 + drag * SIM_DT);
                vx *= damp;
                vy *= damp;
                vz *= damp;

                let speed_sq = vx * vx + vy * vy + vz * vz;
                let max_sq = max_speed * max_speed;
                if speed_sq > max_sq && speed_sq > 1.0e-12_f32 {
                    let scale = max_speed / speed_sq.sqrt();
                    vx *= scale;
                    vy *= scale;
                    vz *= scale;
                }

                let _ = (v3.set_x)(vel_p, vx);
                let _ = (v3.set_y)(vel_p, vy);
                let _ = (v3.set_z)(vel_p, vz);

                let _ = (v3.set_x)(pos_p, ux + vx * SIM_DT);
                let _ = (v3.set_y)(pos_p, uy + vy * SIM_DT);
                let _ = (v3.set_z)(pos_p, uz + vz * SIM_DT);

                let final_speed_sq = vx * vx + vy * vy + vz * vz;
                if final_speed_sq > 1.0e-6_f32 {
                    let inv_s = 1.0_f32 / final_speed_sq.sqrt();
                    let fwd_p = (tf_vt.ptr_mut_forward)(tf_p);
                    let _ = (v3.set_x)(fwd_p, vx * inv_s);
                    let _ = (v3.set_y)(fwd_p, vy * inv_s);
                    let _ = (v3.set_z)(fwd_p, vz * inv_s);
                }

                let _ = (vit_vt.set_health)(vit_p, health);
                let _ = (vit_vt.set_shield)(vit_p, shield);
                let _ = (vit_vt.set_energy)(vit_p, energy);
            }
        }
    }

    FfiResult::OK
}

// ============================================================================
// Variant 1: Entity-Nested VTable-per-Pair (`O(U * E)` C-ABI getter calls)
// ============================================================================

fn run_entity_nested_vtable(ctx: &mut SystemContext<'_>, vts: VTableBundle) -> FfiResult {
    let api = *ctx.host_api();
    let Some(iter_next_chunk) = api.iter_next_chunk else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_reset) = api.iter_reset else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_col_ro) = api.iter_column_readonly else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_col_mut) = api.iter_column_mutable else {
        return FfiResult::NULL_POINTER;
    };

    let Some((u_iter, e_iter)) = ctx.queries_2_mut(0, 1) else {
        return FfiResult::INTERNAL_ERROR;
    };
    let unit_iter = u_iter.as_raw_ptr();
    let emit_iter = e_iter.as_raw_ptr();

    // SAFETY: VTable pointers in `vts` were validated during `bevy_abi_mod_init`.
    let (v3, tf_vt, kin_vt, vit_vt, res_vt, aura_vt, fac_vt) = unsafe {
        (
            &*vts.v3, &*vts.tf, &*vts.kin, &*vts.vit, &*vts.res, &*vts.aura, &*vts.fac,
        )
    };

    loop {
        let mut u_count = 0_usize;
        // SAFETY: `unit_iter` is an active `BamIter` for query 0.
        if !unsafe { iter_next_chunk(unit_iter, &raw mut u_count) }.get() || u_count == 0 {
            break;
        }
        // SAFETY: Columns 0..5 match declared unit query terms.
        let (u_tf_col, u_kin_col, u_vit_col, u_res_col, u_fac_col) = unsafe {
            (
                iter_col_mut(unit_iter, 0),
                iter_col_mut(unit_iter, 1),
                iter_col_mut(unit_iter, 2),
                iter_col_ro(unit_iter, 3),
                iter_col_ro(unit_iter, 4),
            )
        };

        for u_row in 0..u_count {
            // SAFETY: `u_row < u_count` and `e_row < e_count` within valid chunk bounds.
            unsafe {
                let tf_p = u_tf_col.add(u_row * tf_vt.header.size).cast::<c_void>();
                let kin_p = u_kin_col.add(u_row * kin_vt.header.size).cast::<c_void>();
                let vit_p = u_vit_col.add(u_row * vit_vt.header.size).cast::<c_void>();
                let res_p = u_res_col.add(u_row * res_vt.header.size).cast::<c_void>();
                let fac_p = u_fac_col.add(u_row * fac_vt.header.size).cast::<c_void>();

                let pos_p = (tf_vt.ptr_mut_position)(tf_p);
                let ux = (v3.get_x)(pos_p);
                let uy = (v3.get_y)(pos_p);
                let uz = (v3.get_z)(pos_p);

                let vel_p = (kin_vt.ptr_mut_velocity)(kin_p);
                let mut vx = (v3.get_x)(vel_p);
                let mut vy = (v3.get_y)(vel_p);
                let mut vz = (v3.get_z)(vel_p);
                let drag = (kin_vt.get_drag)(kin_p);
                let max_speed = (kin_vt.get_max_speed)(kin_p);

                let mut health = (vit_vt.get_health)(vit_p);
                let max_health = (vit_vt.get_max_health)(vit_p);
                let mut shield = (vit_vt.get_shield)(vit_p);
                let mut energy = (vit_vt.get_energy)(vit_p);

                let r_kin = (res_vt.get_kinetic)(res_p);
                let r_therm = (res_vt.get_thermal)(res_p);
                let r_ion = (res_vt.get_ionic)(res_p);
                let u_team = (fac_vt.get_team_id)(fac_p);

                let _ = iter_reset(emit_iter);
                loop {
                    let mut e_count = 0_usize;
                    if !iter_next_chunk(emit_iter, &raw mut e_count).get() || e_count == 0 {
                        break;
                    }
                    let e_tf_col = iter_col_ro(emit_iter, 0);
                    let e_aura_col = iter_col_ro(emit_iter, 1);
                    let e_fac_col = iter_col_ro(emit_iter, 2);

                    for e_row in 0..e_count {
                        let e_tf_p = e_tf_col.add(e_row * tf_vt.header.size).cast::<c_void>();
                        let e_aura_p = e_aura_col.add(e_row * aura_vt.header.size).cast::<c_void>();
                        let e_fac_p = e_fac_col.add(e_row * fac_vt.header.size).cast::<c_void>();

                        let e_pos_p = (tf_vt.ptr_position)(e_tf_p);
                        let dx = ux - (v3.get_x)(e_pos_p);
                        let dy = uy - (v3.get_y)(e_pos_p);
                        let dz = uz - (v3.get_z)(e_pos_p);
                        let dist_sq = dx * dx + dy * dy + dz * dz + 0.25_f32;
                        let radius = (aura_vt.get_radius)(e_aura_p);

                        if dist_sq < radius * radius {
                            let dist = dist_sq.sqrt();
                            let inv_dist = 1.0_f32 / dist;
                            let falloff = (aura_vt.get_falloff)(e_aura_p);
                            let norm = 1.0_f32 - (dist / radius);
                            let atten = norm / (1.0_f32 + falloff * dist_sq);
                            let force = (aura_vt.get_force)(e_aura_p);
                            let e_team = (fac_vt.get_team_id)(e_fac_p);

                            if u_team == e_team {
                                let e_fwd_p = (tf_vt.ptr_forward)(e_tf_p);
                                let boost = force * 0.25_f32 * atten * SIM_DT;
                                vx += (v3.get_x)(e_fwd_p) * boost;
                                vy += (v3.get_y)(e_fwd_p) * boost;
                                vz += (v3.get_z)(e_fwd_p) * boost;

                                let heal_hps = (aura_vt.get_heal_hps)(e_aura_p);
                                let heal = heal_hps * atten * SIM_DT * (1.0_f32 + 0.1_f32 * r_ion);
                                let nh = health + heal;
                                health = if nh > max_health { max_health } else { nh };

                                let ne = energy + 2.5_f32 * atten * SIM_DT;
                                energy = if ne > 100.0_f32 { 100.0_f32 } else { ne };
                            } else {
                                let imp =
                                    force * atten * SIM_DT * (1.0_f32 - 0.5_f32 * r_kin) * inv_dist;
                                vx += dx * imp;
                                vy += dy * imp;
                                vz += dz * imp;

                                let tdps = (aura_vt.get_thermal_dps)(e_aura_p);
                                let raw_dmg = tdps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * r_therm);
                                let absorbed = if shield >= raw_dmg { raw_dmg } else { shield };
                                shield -= absorbed;
                                let nh = health - (raw_dmg - absorbed);
                                health = if nh < 0.0_f32 { 0.0_f32 } else { nh };

                                let drain = 1.5_f32 * atten * SIM_DT * (1.0_f32 - 0.4_f32 * r_ion);
                                let ne = energy - drain;
                                energy = if ne < 0.0_f32 { 0.0_f32 } else { ne };
                            }
                        }
                    }
                }

                let damp = 1.0_f32 / (1.0_f32 + drag * SIM_DT);
                vx *= damp;
                vy *= damp;
                vz *= damp;

                let speed_sq = vx * vx + vy * vy + vz * vz;
                let max_sq = max_speed * max_speed;
                if speed_sq > max_sq && speed_sq > 1.0e-12_f32 {
                    let scale = max_speed / speed_sq.sqrt();
                    vx *= scale;
                    vy *= scale;
                    vz *= scale;
                }

                let _ = (v3.set_x)(vel_p, vx);
                let _ = (v3.set_y)(vel_p, vy);
                let _ = (v3.set_z)(vel_p, vz);

                let _ = (v3.set_x)(pos_p, ux + vx * SIM_DT);
                let _ = (v3.set_y)(pos_p, uy + vy * SIM_DT);
                let _ = (v3.set_z)(pos_p, uz + vz * SIM_DT);

                let final_speed_sq = vx * vx + vy * vy + vz * vz;
                if final_speed_sq > 1.0e-6_f32 {
                    let inv_s = 1.0_f32 / final_speed_sq.sqrt();
                    let fwd_p = (tf_vt.ptr_mut_forward)(tf_p);
                    let _ = (v3.set_x)(fwd_p, vx * inv_s);
                    let _ = (v3.set_y)(fwd_p, vy * inv_s);
                    let _ = (v3.set_z)(fwd_p, vz * inv_s);
                }

                let _ = (vit_vt.set_health)(vit_p, health);
                let _ = (vit_vt.set_shield)(vit_p, shield);
                let _ = (vit_vt.set_energy)(vit_p, energy);
            }
        }
    }

    FfiResult::OK
}

// ============================================================================
// Variant 2: Naive Chatty FFI Anti-Pattern (Per-Pair VTable Read/Write + Tick)
// ============================================================================

fn run_naive_chatty_ffi(ctx: &mut SystemContext<'_>, vts: VTableBundle) -> FfiResult {
    let api = *ctx.host_api();
    let Some(iter_next_chunk) = api.iter_next_chunk else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_reset) = api.iter_reset else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_col_ro) = api.iter_column_readonly else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_col_mut_untracked) = api.iter_column_mutable_untracked else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_mark_changed_range) = api.iter_mark_changed_range else {
        return FfiResult::NULL_POINTER;
    };
    let Some(iter_col_info) = api.iter_column_info else {
        return FfiResult::NULL_POINTER;
    };

    let Some((u_iter, e_iter)) = ctx.queries_2_mut(0, 1) else {
        return FfiResult::INTERNAL_ERROR;
    };
    let unit_iter = u_iter.as_raw_ptr();
    let emit_iter = e_iter.as_raw_ptr();

    // SAFETY: VTable pointers in `vts` were validated during `bevy_abi_mod_init`.
    let (v3, tf_vt, kin_vt, vit_vt, res_vt, aura_vt, fac_vt) = unsafe {
        (
            &*vts.v3, &*vts.tf, &*vts.kin, &*vts.vit, &*vts.res, &*vts.aura, &*vts.fac,
        )
    };

    loop {
        let mut u_count = 0_usize;
        // SAFETY: `unit_iter` is an active `BamIter` for query 0.
        if !unsafe { iter_next_chunk(unit_iter, &raw mut u_count) }.get() || u_count == 0 {
            break;
        }

        // SAFETY: Querying column base pointers and `BamColumnInfo` strides per chunk.
        let (
            u_tf_col,
            u_kin_col,
            u_vit_col,
            u_res_col,
            u_fac_col,
            u_tf_stride,
            u_kin_stride,
            u_vit_stride,
            u_res_stride,
            u_fac_stride,
        ) = unsafe {
            let mut info0 = core::mem::MaybeUninit::<BamColumnInfo>::zeroed().assume_init();
            let mut info1 = core::mem::MaybeUninit::<BamColumnInfo>::zeroed().assume_init();
            let mut info2 = core::mem::MaybeUninit::<BamColumnInfo>::zeroed().assume_init();
            let mut info3 = core::mem::MaybeUninit::<BamColumnInfo>::zeroed().assume_init();
            let mut info4 = core::mem::MaybeUninit::<BamColumnInfo>::zeroed().assume_init();
            let _ = iter_col_info(unit_iter, 0, &raw mut info0);
            let _ = iter_col_info(unit_iter, 1, &raw mut info1);
            let _ = iter_col_info(unit_iter, 2, &raw mut info2);
            let _ = iter_col_info(unit_iter, 3, &raw mut info3);
            let _ = iter_col_info(unit_iter, 4, &raw mut info4);
            (
                iter_col_mut_untracked(unit_iter, 0),
                iter_col_mut_untracked(unit_iter, 1),
                iter_col_mut_untracked(unit_iter, 2),
                iter_col_ro(unit_iter, 3),
                iter_col_ro(unit_iter, 4),
                info0.size,
                info1.size,
                info2.size,
                info3.size,
                info4.size,
            )
        };

        for u_row in 0..u_count {
            // SAFETY: `u_row < u_count` and `e_row < e_count` within valid chunk bounds.
            unsafe {
                let tf_p = u_tf_col.add(u_row * u_tf_stride).cast::<c_void>();
                let kin_p = u_kin_col.add(u_row * u_kin_stride).cast::<c_void>();
                let vit_p = u_vit_col.add(u_row * u_vit_stride).cast::<c_void>();
                let res_p = u_res_col.add(u_row * u_res_stride).cast::<c_void>();
                let fac_p = u_fac_col.add(u_row * u_fac_stride).cast::<c_void>();

                let _ = iter_reset(emit_iter);
                loop {
                    let mut e_count = 0_usize;
                    if !iter_next_chunk(emit_iter, &raw mut e_count).get() || e_count == 0 {
                        break;
                    }
                    let e_tf_col = iter_col_ro(emit_iter, 0);
                    let e_aura_col = iter_col_ro(emit_iter, 1);
                    let e_fac_col = iter_col_ro(emit_iter, 2);

                    for e_row in 0..e_count {
                        let e_tf_p = e_tf_col.add(e_row * tf_vt.header.size).cast::<c_void>();
                        let e_aura_p = e_aura_col.add(e_row * aura_vt.header.size).cast::<c_void>();
                        let e_fac_p = e_fac_col.add(e_row * fac_vt.header.size).cast::<c_void>();

                        let u_pos_ro = (tf_vt.ptr_position)(tf_p);
                        let e_pos_ro = (tf_vt.ptr_position)(e_tf_p);

                        let dx = (v3.get_x)(u_pos_ro) - (v3.get_x)(e_pos_ro);
                        let dy = (v3.get_y)(u_pos_ro) - (v3.get_y)(e_pos_ro);
                        let dz = (v3.get_z)(u_pos_ro) - (v3.get_z)(e_pos_ro);
                        let dist_sq = dx * dx + dy * dy + dz * dz + 0.25_f32;
                        let radius = (aura_vt.get_radius)(e_aura_p);

                        if dist_sq < radius * radius {
                            let dist = dist_sq.sqrt();
                            let inv_dist = 1.0_f32 / dist;
                            let falloff = (aura_vt.get_falloff)(e_aura_p);
                            let norm = 1.0_f32 - (dist / radius);
                            let atten = norm / (1.0_f32 + falloff * dist_sq);
                            let force = (aura_vt.get_force)(e_aura_p);

                            let u_team = (fac_vt.get_team_id)(fac_p);
                            let e_team = (fac_vt.get_team_id)(e_fac_p);

                            let vel_mut = (kin_vt.ptr_mut_velocity)(kin_p);

                            if u_team == e_team {
                                let e_fwd_ro = (tf_vt.ptr_forward)(e_tf_p);
                                let boost = force * 0.25_f32 * atten * SIM_DT;
                                let _ = (v3.set_x)(
                                    vel_mut,
                                    (v3.get_x)(vel_mut) + (v3.get_x)(e_fwd_ro) * boost,
                                );
                                let _ = (v3.set_y)(
                                    vel_mut,
                                    (v3.get_y)(vel_mut) + (v3.get_y)(e_fwd_ro) * boost,
                                );
                                let _ = (v3.set_z)(
                                    vel_mut,
                                    (v3.get_z)(vel_mut) + (v3.get_z)(e_fwd_ro) * boost,
                                );

                                let heal_hps = (aura_vt.get_heal_hps)(e_aura_p);
                                let r_ion = (res_vt.get_ionic)(res_p);
                                let heal = heal_hps * atten * SIM_DT * (1.0_f32 + 0.1_f32 * r_ion);
                                let max_h = (vit_vt.get_max_health)(vit_p);
                                let nh = (vit_vt.get_health)(vit_p) + heal;
                                let _ =
                                    (vit_vt.set_health)(vit_p, if nh > max_h { max_h } else { nh });

                                let ne = (vit_vt.get_energy)(vit_p) + 2.5_f32 * atten * SIM_DT;
                                let _ = (vit_vt.set_energy)(
                                    vit_p,
                                    if ne > 100.0_f32 { 100.0_f32 } else { ne },
                                );
                            } else {
                                let r_kin = (res_vt.get_kinetic)(res_p);
                                let imp =
                                    force * atten * SIM_DT * (1.0_f32 - 0.5_f32 * r_kin) * inv_dist;
                                let _ = (v3.set_x)(vel_mut, (v3.get_x)(vel_mut) + dx * imp);
                                let _ = (v3.set_y)(vel_mut, (v3.get_y)(vel_mut) + dy * imp);
                                let _ = (v3.set_z)(vel_mut, (v3.get_z)(vel_mut) + dz * imp);

                                let tdps = (aura_vt.get_thermal_dps)(e_aura_p);
                                let r_therm = (res_vt.get_thermal)(res_p);
                                let raw_dmg = tdps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * r_therm);
                                let cur_sh = (vit_vt.get_shield)(vit_p);
                                let absorbed = if cur_sh >= raw_dmg { raw_dmg } else { cur_sh };
                                let _ = (vit_vt.set_shield)(vit_p, cur_sh - absorbed);
                                let nh = (vit_vt.get_health)(vit_p) - (raw_dmg - absorbed);
                                let _ = (vit_vt.set_health)(
                                    vit_p,
                                    if nh < 0.0_f32 { 0.0_f32 } else { nh },
                                );

                                let r_ion = (res_vt.get_ionic)(res_p);
                                let drain = 1.5_f32 * atten * SIM_DT * (1.0_f32 - 0.4_f32 * r_ion);
                                let ne = (vit_vt.get_energy)(vit_p) - drain;
                                let _ = (vit_vt.set_energy)(
                                    vit_p,
                                    if ne < 0.0_f32 { 0.0_f32 } else { ne },
                                );
                            }

                            let _ = iter_mark_changed_range(unit_iter, 1, u_row, 1);
                            let _ = iter_mark_changed_range(unit_iter, 2, u_row, 1);
                        }
                    }
                }

                let vel_mut = (kin_vt.ptr_mut_velocity)(kin_p);
                let drag = (kin_vt.get_drag)(kin_p);
                let max_speed = (kin_vt.get_max_speed)(kin_p);
                let damp = 1.0_f32 / (1.0_f32 + drag * SIM_DT);

                let mut vx = (v3.get_x)(vel_mut) * damp;
                let mut vy = (v3.get_y)(vel_mut) * damp;
                let mut vz = (v3.get_z)(vel_mut) * damp;

                let speed_sq = vx * vx + vy * vy + vz * vz;
                let max_sq = max_speed * max_speed;
                if speed_sq > max_sq && speed_sq > 1.0e-12_f32 {
                    let scale = max_speed / speed_sq.sqrt();
                    vx *= scale;
                    vy *= scale;
                    vz *= scale;
                }

                let _ = (v3.set_x)(vel_mut, vx);
                let _ = (v3.set_y)(vel_mut, vy);
                let _ = (v3.set_z)(vel_mut, vz);

                let pos_mut = (tf_vt.ptr_mut_position)(tf_p);
                let _ = (v3.set_x)(pos_mut, (v3.get_x)(pos_mut) + vx * SIM_DT);
                let _ = (v3.set_y)(pos_mut, (v3.get_y)(pos_mut) + vy * SIM_DT);
                let _ = (v3.set_z)(pos_mut, (v3.get_z)(pos_mut) + vz * SIM_DT);

                let final_speed_sq = vx * vx + vy * vy + vz * vz;
                if final_speed_sq > 1.0e-6_f32 {
                    let inv_s = 1.0_f32 / final_speed_sq.sqrt();
                    let fwd_mut = (tf_vt.ptr_mut_forward)(tf_p);
                    let _ = (v3.set_x)(fwd_mut, vx * inv_s);
                    let _ = (v3.set_y)(fwd_mut, vy * inv_s);
                    let _ = (v3.set_z)(fwd_mut, vz * inv_s);
                }

                let _ = iter_mark_changed_range(unit_iter, 0, u_row, 1);
            }
        }
    }

    FfiResult::OK
}

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    let vts = match VTableBundle::resolve(app) {
        Ok(v) => v,
        Err(err) => return err,
    };

    let resolve = |name: &CStr| app.resolve_component_named(Some(name), None);
    let (
        Ok(tf_h),
        Ok(kin_h),
        Ok(vit_h),
        Ok(res_h),
        Ok(aura_h),
        Ok(fac_h),
        Ok(unit_tag_h),
        Ok(emit_tag_h),
        Ok(frozen_h),
    ) = (
        resolve(c"shared_scenario::Transform3D"),
        resolve(c"shared_scenario::Kinematics"),
        resolve(c"shared_scenario::Vitality"),
        resolve(c"shared_scenario::Resistance"),
        resolve(c"shared_scenario::AuraEmitter"),
        resolve(c"shared_scenario::Faction"),
        resolve(c"shared_scenario::CombatUnitTag"),
        resolve(c"shared_scenario::EmitterBeaconTag"),
        resolve(c"shared_scenario::FrozenFlag"),
    )
    else {
        return FfiResult::INVALID_COMPONENT;
    };

    let variant = BENCH_VARIANT.load(Ordering::SeqCst);
    let sys_name: &'static CStr = match variant {
        0 => c"rust_cabi_chunk_optimized",
        1 => c"rust_cabi_entity_nested_vtable",
        _ => c"rust_cabi_naive_chatty_ffi",
    };

    app.system(sys_name)
        .query(|q| {
            q.write_handle(tf_h)
                .write_handle(kin_h)
                .write_handle(vit_h)
                .read_handle(res_h)
                .read_handle(fac_h)
                .with_handle(unit_tag_h)
                .without_handle(emit_tag_h)
                .without_handle(frozen_h)
        })
        .query(|q| {
            q.read_handle(tf_h)
                .read_handle(aura_h)
                .read_handle(fac_h)
                .with_handle(emit_tag_h)
                .without_handle(unit_tag_h)
                .without_handle(frozen_h)
        })
        .build(move |ctx| match variant {
            0 => run_chunk_optimized(ctx, vts),
            1 => run_entity_nested_vtable(ctx, vts),
            _ => run_naive_chatty_ffi(ctx, vts),
        })
}

export_mod!(name: c"c_abi_rust_mod", version: c"0.2.0", setup);
