#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! Asymmetric C-ABI Rust Benchmark Mod (`cdylib`, independent `Cargo.lock`, zero `bevy` dep).
//!
//! Implements the Complex Multi-Archetype Nested Query Scenario in 3 progressive patterns
//! selectable via `c_abi_rust_mod_set_variant(u32)` prior to `bevy_abi_mod_init`:
//! - `0` (`ChunkOptimized`): Unpacks emitter chunks once ($O(U + E)$ VTable calls), runs inner loop in registers.
//! - `1` (`EntityNestedVTable`): Re-traverses emitter chunks per unit via `iter_reset`, invoking VTable getters on every `(u, e)` pair ($O(U \times E)$ cross-ABI calls).
//! - `2` (`NaiveChattyFfi`): Re-reads both unit & emitter via VTable on every pair and performs immediate VTable writes + `iter_mark_changed` inside the inner loop.

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use guest_sdk::prelude::*;

const SIM_DT: f32 = 0.016_f32;
const MAX_CACHED_EMITTERS: usize = 512;

// ============================================================================
// 1. VTable Mirrors Matching Host `#[derive(ModExposable)]` Layouts
// ============================================================================

#[repr(C)]
pub struct Vec3fVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct Transform3DVTable {
    pub size: usize,
    pub align: usize,
    pub ptr_position: unsafe extern "C" fn(*const c_void) -> *const c_void,
    pub ptr_mut_position: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub ptr_forward: unsafe extern "C" fn(*const c_void) -> *const c_void,
    pub ptr_mut_forward: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub get_scale: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_scale: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct KinematicsVTable {
    pub size: usize,
    pub align: usize,
    pub ptr_velocity: unsafe extern "C" fn(*const c_void) -> *const c_void,
    pub ptr_mut_velocity: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub get_drag: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_drag: unsafe extern "C" fn(*mut c_void, f32),
    pub get_max_speed: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_max_speed: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct VitalityVTable {
    pub size: usize,
    pub align: usize,
    pub get_health: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_health: unsafe extern "C" fn(*mut c_void, f32),
    pub get_max_health: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_max_health: unsafe extern "C" fn(*mut c_void, f32),
    pub get_shield: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_shield: unsafe extern "C" fn(*mut c_void, f32),
    pub get_energy: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_energy: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct ResistanceVTable {
    pub size: usize,
    pub align: usize,
    pub get_kinetic: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_kinetic: unsafe extern "C" fn(*mut c_void, f32),
    pub get_thermal: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_thermal: unsafe extern "C" fn(*mut c_void, f32),
    pub get_ionic: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_ionic: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct AuraEmitterVTable {
    pub size: usize,
    pub align: usize,
    pub get_radius: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_radius: unsafe extern "C" fn(*mut c_void, f32),
    pub get_force: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_force: unsafe extern "C" fn(*mut c_void, f32),
    pub get_thermal_dps: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_thermal_dps: unsafe extern "C" fn(*mut c_void, f32),
    pub get_heal_hps: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_heal_hps: unsafe extern "C" fn(*mut c_void, f32),
    pub get_falloff: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_falloff: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct FactionVTable {
    pub size: usize,
    pub align: usize,
    pub get_team_id: unsafe extern "C" fn(*const c_void) -> u32,
    pub set_team_id: unsafe extern "C" fn(*mut c_void, u32),
    pub get_serial_id: unsafe extern "C" fn(*const c_void) -> u32,
    pub set_serial_id: unsafe extern "C" fn(*mut c_void, u32),
}

// ============================================================================
// 2. Resolved Host Handles & Variant Selector
// ============================================================================

static BENCH_VARIANT: AtomicU32 = AtomicU32::new(0);
static HOST_API_PTR: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static ITER_RESET_FN: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

static VEC3_VT: AtomicPtr<Vec3fVTable> = AtomicPtr::new(core::ptr::null_mut());
static TF_VT: AtomicPtr<Transform3DVTable> = AtomicPtr::new(core::ptr::null_mut());
static KIN_VT: AtomicPtr<KinematicsVTable> = AtomicPtr::new(core::ptr::null_mut());
static VIT_VT: AtomicPtr<VitalityVTable> = AtomicPtr::new(core::ptr::null_mut());
static RES_VT: AtomicPtr<ResistanceVTable> = AtomicPtr::new(core::ptr::null_mut());
static AURA_VT: AtomicPtr<AuraEmitterVTable> = AtomicPtr::new(core::ptr::null_mut());
static FAC_VT: AtomicPtr<FactionVTable> = AtomicPtr::new(core::ptr::null_mut());

#[no_mangle]
pub extern "C" fn c_abi_rust_mod_set_variant(variant: u32) {
    BENCH_VARIANT.store(variant, Ordering::SeqCst);
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
// Variant 0: Chunk-Optimized Nested Query ($O(U + E)$ C-ABI boundary calls)
// ============================================================================

unsafe fn sys_chunk_optimized(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    if iter_count < 2 || iters.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*HOST_API_PTR.load(Ordering::Acquire) };
    let v3 = unsafe { &*VEC3_VT.load(Ordering::Acquire) };
    let tf_vt = unsafe { &*TF_VT.load(Ordering::Acquire) };
    let kin_vt = unsafe { &*KIN_VT.load(Ordering::Acquire) };
    let vit_vt = unsafe { &*VIT_VT.load(Ordering::Acquire) };
    let res_vt = unsafe { &*RES_VT.load(Ordering::Acquire) };
    let aura_vt = unsafe { &*AURA_VT.load(Ordering::Acquire) };
    let fac_vt = unsafe { &*FAC_VT.load(Ordering::Acquire) };

    let unit_iter = unsafe { *iters.add(0) };
    let emit_iter = unsafe { *iters.add(1) };

    // Step 1: Unpack all active emitter chunks once onto stack/buffer
    let mut stack_emitters = [UnpackedEmitter::ZERO; MAX_CACHED_EMITTERS];
    let mut heap_emitters: Vec<UnpackedEmitter> = Vec::new();
    let mut total_emitters = 0_usize;

    loop {
        let e_count = unsafe { (api.iter_next_chunk)(emit_iter) };
        if e_count == 0 {
            break;
        }
        let e_tf_col = unsafe { (api.iter_column_readonly)(emit_iter, 0) };
        let e_aura_col = unsafe { (api.iter_column_readonly)(emit_iter, 1) };
        let e_fac_col = unsafe { (api.iter_column_readonly)(emit_iter, 2) };

        for row in 0..e_count {
            let tf_p = unsafe { e_tf_col.add(row * tf_vt.size) as *const c_void };
            let aura_p = unsafe { e_aura_col.add(row * aura_vt.size) as *const c_void };
            let fac_p = unsafe { e_fac_col.add(row * fac_vt.size) as *const c_void };

            let pos_p = unsafe { (tf_vt.ptr_position)(tf_p) };
            let fwd_p = unsafe { (tf_vt.ptr_forward)(tf_p) };
            let radius = unsafe { (aura_vt.get_radius)(aura_p) };

            let item = UnpackedEmitter {
                px: unsafe { (v3.get_x)(pos_p) },
                py: unsafe { (v3.get_y)(pos_p) },
                pz: unsafe { (v3.get_z)(pos_p) },
                fx: unsafe { (v3.get_x)(fwd_p) },
                fy: unsafe { (v3.get_y)(fwd_p) },
                fz: unsafe { (v3.get_z)(fwd_p) },
                radius,
                rad_sq: radius * radius,
                force: unsafe { (aura_vt.get_force)(aura_p) },
                thermal_dps: unsafe { (aura_vt.get_thermal_dps)(aura_p) },
                heal_hps: unsafe { (aura_vt.get_heal_hps)(aura_p) },
                falloff: unsafe { (aura_vt.get_falloff)(aura_p) },
                team_id: unsafe { (fac_vt.get_team_id)(fac_p) },
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

    // Step 2: Traverse 16 active unit chunks and evaluate cached emitter interactions in registers
    loop {
        let u_count = unsafe { (api.iter_next_chunk)(unit_iter) };
        if u_count == 0 {
            break;
        }
        let u_tf_col = unsafe { (api.iter_column_mutable)(unit_iter, 0) };
        let u_kin_col = unsafe { (api.iter_column_mutable)(unit_iter, 1) };
        let u_vit_col = unsafe { (api.iter_column_mutable)(unit_iter, 2) };
        let u_res_col = unsafe { (api.iter_column_readonly)(unit_iter, 3) };
        let u_fac_col = unsafe { (api.iter_column_readonly)(unit_iter, 4) };

        for row in 0..u_count {
            let tf_p = unsafe { u_tf_col.add(row * tf_vt.size) as *mut c_void };
            let kin_p = unsafe { u_kin_col.add(row * kin_vt.size) as *mut c_void };
            let vit_p = unsafe { u_vit_col.add(row * vit_vt.size) as *mut c_void };
            let res_p = unsafe { u_res_col.add(row * res_vt.size) as *const c_void };
            let fac_p = unsafe { u_fac_col.add(row * fac_vt.size) as *const c_void };

            let pos_p = unsafe { (tf_vt.ptr_mut_position)(tf_p) };
            let ux = unsafe { (v3.get_x)(pos_p) };
            let uy = unsafe { (v3.get_y)(pos_p) };
            let uz = unsafe { (v3.get_z)(pos_p) };

            let vel_p = unsafe { (kin_vt.ptr_mut_velocity)(kin_p) };
            let mut vx = unsafe { (v3.get_x)(vel_p) };
            let mut vy = unsafe { (v3.get_y)(vel_p) };
            let mut vz = unsafe { (v3.get_z)(vel_p) };
            let drag = unsafe { (kin_vt.get_drag)(kin_p) };
            let max_speed = unsafe { (kin_vt.get_max_speed)(kin_p) };

            let mut health = unsafe { (vit_vt.get_health)(vit_p) };
            let max_health = unsafe { (vit_vt.get_max_health)(vit_p) };
            let mut shield = unsafe { (vit_vt.get_shield)(vit_p) };
            let mut energy = unsafe { (vit_vt.get_energy)(vit_p) };

            let r_kin = unsafe { (res_vt.get_kinetic)(res_p) };
            let r_thm = unsafe { (res_vt.get_thermal)(res_p) };
            let r_ion = unsafe { (res_vt.get_ionic)(res_p) };
            let u_team = unsafe { (fac_vt.get_team_id)(fac_p) };

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

                        let raw_dmg = em.thermal_dps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * r_thm);
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

            unsafe {
                (v3.set_x)(vel_p, vx);
                (v3.set_y)(vel_p, vy);
                (v3.set_z)(vel_p, vz);

                (v3.set_x)(pos_p, ux + vx * SIM_DT);
                (v3.set_y)(pos_p, uy + vy * SIM_DT);
                (v3.set_z)(pos_p, uz + vz * SIM_DT);

                let final_speed_sq = vx * vx + vy * vy + vz * vz;
                if final_speed_sq > 1.0e-6_f32 {
                    let inv_s = 1.0_f32 / final_speed_sq.sqrt();
                    let fwd_p = (tf_vt.ptr_mut_forward)(tf_p);
                    (v3.set_x)(fwd_p, vx * inv_s);
                    (v3.set_y)(fwd_p, vy * inv_s);
                    (v3.set_z)(fwd_p, vz * inv_s);
                }

                (vit_vt.set_health)(vit_p, health);
                (vit_vt.set_shield)(vit_p, shield);
                (vit_vt.set_energy)(vit_p, energy);
            }
        }
    }

    FfiResult::OK
}

// ============================================================================
// Variant 1: Entity-Nested VTable-per-Pair ($O(U \times E)$ C-ABI getter calls)
// ============================================================================

unsafe fn sys_entity_nested_vtable(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    if iter_count < 2 || iters.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*HOST_API_PTR.load(Ordering::Acquire) };
    let iter_reset: IterResetFn =
        unsafe { core::mem::transmute(ITER_RESET_FN.load(Ordering::Acquire)) };

    let v3 = unsafe { &*VEC3_VT.load(Ordering::Acquire) };
    let tf_vt = unsafe { &*TF_VT.load(Ordering::Acquire) };
    let kin_vt = unsafe { &*KIN_VT.load(Ordering::Acquire) };
    let vit_vt = unsafe { &*VIT_VT.load(Ordering::Acquire) };
    let res_vt = unsafe { &*RES_VT.load(Ordering::Acquire) };
    let aura_vt = unsafe { &*AURA_VT.load(Ordering::Acquire) };
    let fac_vt = unsafe { &*FAC_VT.load(Ordering::Acquire) };

    let unit_iter = unsafe { *iters.add(0) };
    let emit_iter = unsafe { *iters.add(1) };

    loop {
        let u_count = unsafe { (api.iter_next_chunk)(unit_iter) };
        if u_count == 0 {
            break;
        }
        let u_tf_col = unsafe { (api.iter_column_mutable)(unit_iter, 0) };
        let u_kin_col = unsafe { (api.iter_column_mutable)(unit_iter, 1) };
        let u_vit_col = unsafe { (api.iter_column_mutable)(unit_iter, 2) };
        let u_res_col = unsafe { (api.iter_column_readonly)(unit_iter, 3) };
        let u_fac_col = unsafe { (api.iter_column_readonly)(unit_iter, 4) };

        for u_row in 0..u_count {
            let tf_p = unsafe { u_tf_col.add(u_row * tf_vt.size) as *mut c_void };
            let kin_p = unsafe { u_kin_col.add(u_row * kin_vt.size) as *mut c_void };
            let vit_p = unsafe { u_vit_col.add(u_row * vit_vt.size) as *mut c_void };
            let res_p = unsafe { u_res_col.add(u_row * res_vt.size) as *const c_void };
            let fac_p = unsafe { u_fac_col.add(u_row * fac_vt.size) as *const c_void };

            let pos_p = unsafe { (tf_vt.ptr_mut_position)(tf_p) };
            let ux = unsafe { (v3.get_x)(pos_p) };
            let uy = unsafe { (v3.get_y)(pos_p) };
            let uz = unsafe { (v3.get_z)(pos_p) };

            let vel_p = unsafe { (kin_vt.ptr_mut_velocity)(kin_p) };
            let mut vx = unsafe { (v3.get_x)(vel_p) };
            let mut vy = unsafe { (v3.get_y)(vel_p) };
            let mut vz = unsafe { (v3.get_z)(vel_p) };
            let drag = unsafe { (kin_vt.get_drag)(kin_p) };
            let max_speed = unsafe { (kin_vt.get_max_speed)(kin_p) };

            let mut health = unsafe { (vit_vt.get_health)(vit_p) };
            let max_health = unsafe { (vit_vt.get_max_health)(vit_p) };
            let mut shield = unsafe { (vit_vt.get_shield)(vit_p) };
            let mut energy = unsafe { (vit_vt.get_energy)(vit_p) };

            let r_kin = unsafe { (res_vt.get_kinetic)(res_p) };
            let r_thm = unsafe { (res_vt.get_thermal)(res_p) };
            let r_ion = unsafe { (res_vt.get_ionic)(res_p) };
            let u_team = unsafe { (fac_vt.get_team_id)(fac_p) };

            // Reset inner emitter iterator and walk all 6 emitter chunks for this unit
            let _ = unsafe { iter_reset(emit_iter) };
            loop {
                let e_count = unsafe { (api.iter_next_chunk)(emit_iter) };
                if e_count == 0 {
                    break;
                }
                let e_tf_col = unsafe { (api.iter_column_readonly)(emit_iter, 0) };
                let e_aura_col = unsafe { (api.iter_column_readonly)(emit_iter, 1) };
                let e_fac_col = unsafe { (api.iter_column_readonly)(emit_iter, 2) };

                for e_row in 0..e_count {
                    let e_tf_p = unsafe { e_tf_col.add(e_row * tf_vt.size) as *const c_void };
                    let e_aura_p = unsafe { e_aura_col.add(e_row * aura_vt.size) as *const c_void };
                    let e_fac_p = unsafe { e_fac_col.add(e_row * fac_vt.size) as *const c_void };

                    let e_pos_p = unsafe { (tf_vt.ptr_position)(e_tf_p) };
                    let dx = ux - unsafe { (v3.get_x)(e_pos_p) };
                    let dy = uy - unsafe { (v3.get_y)(e_pos_p) };
                    let dz = uz - unsafe { (v3.get_z)(e_pos_p) };
                    let dist_sq = dx * dx + dy * dy + dz * dz + 0.25_f32;
                    let radius = unsafe { (aura_vt.get_radius)(e_aura_p) };

                    if dist_sq < radius * radius {
                        let dist = dist_sq.sqrt();
                        let inv_dist = 1.0_f32 / dist;
                        let falloff = unsafe { (aura_vt.get_falloff)(e_aura_p) };
                        let norm = 1.0_f32 - (dist / radius);
                        let atten = norm / (1.0_f32 + falloff * dist_sq);
                        let force = unsafe { (aura_vt.get_force)(e_aura_p) };
                        let e_team = unsafe { (fac_vt.get_team_id)(e_fac_p) };

                        if u_team == e_team {
                            let e_fwd_p = unsafe { (tf_vt.ptr_forward)(e_tf_p) };
                            let boost = force * 0.25_f32 * atten * SIM_DT;
                            vx += unsafe { (v3.get_x)(e_fwd_p) } * boost;
                            vy += unsafe { (v3.get_y)(e_fwd_p) } * boost;
                            vz += unsafe { (v3.get_z)(e_fwd_p) } * boost;

                            let heal_hps = unsafe { (aura_vt.get_heal_hps)(e_aura_p) };
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

                            let tdps = unsafe { (aura_vt.get_thermal_dps)(e_aura_p) };
                            let raw_dmg = tdps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * r_thm);
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

            unsafe {
                (v3.set_x)(vel_p, vx);
                (v3.set_y)(vel_p, vy);
                (v3.set_z)(vel_p, vz);

                (v3.set_x)(pos_p, ux + vx * SIM_DT);
                (v3.set_y)(pos_p, uy + vy * SIM_DT);
                (v3.set_z)(pos_p, uz + vz * SIM_DT);

                let final_speed_sq = vx * vx + vy * vy + vz * vz;
                if final_speed_sq > 1.0e-6_f32 {
                    let inv_s = 1.0_f32 / final_speed_sq.sqrt();
                    let fwd_p = (tf_vt.ptr_mut_forward)(tf_p);
                    (v3.set_x)(fwd_p, vx * inv_s);
                    (v3.set_y)(fwd_p, vy * inv_s);
                    (v3.set_z)(fwd_p, vz * inv_s);
                }

                (vit_vt.set_health)(vit_p, health);
                (vit_vt.set_shield)(vit_p, shield);
                (vit_vt.set_energy)(vit_p, energy);
            }
        }
    }

    FfiResult::OK
}

// ============================================================================
// Variant 2: Naive Chatty FFI Anti-Pattern (Per-Pair VTable Read/Write + Tick)
// ============================================================================

unsafe fn sys_naive_chatty_ffi(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    if iter_count < 2 || iters.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*HOST_API_PTR.load(Ordering::Acquire) };
    let iter_reset: IterResetFn =
        unsafe { core::mem::transmute(ITER_RESET_FN.load(Ordering::Acquire)) };

    let v3 = unsafe { &*VEC3_VT.load(Ordering::Acquire) };
    let tf_vt = unsafe { &*TF_VT.load(Ordering::Acquire) };
    let kin_vt = unsafe { &*KIN_VT.load(Ordering::Acquire) };
    let vit_vt = unsafe { &*VIT_VT.load(Ordering::Acquire) };
    let res_vt = unsafe { &*RES_VT.load(Ordering::Acquire) };
    let aura_vt = unsafe { &*AURA_VT.load(Ordering::Acquire) };
    let fac_vt = unsafe { &*FAC_VT.load(Ordering::Acquire) };

    let unit_iter = unsafe { *iters.add(0) };
    let emit_iter = unsafe { *iters.add(1) };

    loop {
        let u_count = unsafe { (api.iter_next_chunk)(unit_iter) };
        if u_count == 0 {
            break;
        }
        let u_tf_col = unsafe { (api.iter_column_mutable)(unit_iter, 0) };
        let u_kin_col = unsafe { (api.iter_column_mutable)(unit_iter, 1) };
        let u_vit_col = unsafe { (api.iter_column_mutable)(unit_iter, 2) };
        let u_res_col = unsafe { (api.iter_column_readonly)(unit_iter, 3) };
        let u_fac_col = unsafe { (api.iter_column_readonly)(unit_iter, 4) };

        let u_tf_stride = unsafe { (api.iter_column_stride)(unit_iter, 0) };
        let u_kin_stride = unsafe { (api.iter_column_stride)(unit_iter, 1) };
        let u_vit_stride = unsafe { (api.iter_column_stride)(unit_iter, 2) };
        let u_res_stride = unsafe { (api.iter_column_stride)(unit_iter, 3) };
        let u_fac_stride = unsafe { (api.iter_column_stride)(unit_iter, 4) };

        for u_row in 0..u_count {
            let tf_p = unsafe { u_tf_col.add(u_row * u_tf_stride) as *mut c_void };
            let kin_p = unsafe { u_kin_col.add(u_row * u_kin_stride) as *mut c_void };
            let vit_p = unsafe { u_vit_col.add(u_row * u_vit_stride) as *mut c_void };
            let res_p = unsafe { u_res_col.add(u_row * u_res_stride) as *const c_void };
            let fac_p = unsafe { u_fac_col.add(u_row * u_fac_stride) as *const c_void };

            let _ = unsafe { iter_reset(emit_iter) };
            loop {
                let e_count = unsafe { (api.iter_next_chunk)(emit_iter) };
                if e_count == 0 {
                    break;
                }
                let e_tf_col = unsafe { (api.iter_column_readonly)(emit_iter, 0) };
                let e_aura_col = unsafe { (api.iter_column_readonly)(emit_iter, 1) };
                let e_fac_col = unsafe { (api.iter_column_readonly)(emit_iter, 2) };

                for e_row in 0..e_count {
                    let e_tf_p = unsafe { e_tf_col.add(e_row * tf_vt.size) as *const c_void };
                    let e_aura_p = unsafe { e_aura_col.add(e_row * aura_vt.size) as *const c_void };
                    let e_fac_p = unsafe { e_fac_col.add(e_row * fac_vt.size) as *const c_void };

                    // Naive pattern: unhoisted VTable reads on BOTH unit and emitter every pair
                    let u_pos_ro = unsafe { (tf_vt.ptr_position)(tf_p) };
                    let e_pos_ro = unsafe { (tf_vt.ptr_position)(e_tf_p) };

                    let dx = unsafe { (v3.get_x)(u_pos_ro) - (v3.get_x)(e_pos_ro) };
                    let dy = unsafe { (v3.get_y)(u_pos_ro) - (v3.get_y)(e_pos_ro) };
                    let dz = unsafe { (v3.get_z)(u_pos_ro) - (v3.get_z)(e_pos_ro) };
                    let dist_sq = dx * dx + dy * dy + dz * dz + 0.25_f32;
                    let radius = unsafe { (aura_vt.get_radius)(e_aura_p) };

                    if dist_sq < radius * radius {
                        let dist = dist_sq.sqrt();
                        let inv_dist = 1.0_f32 / dist;
                        let falloff = unsafe { (aura_vt.get_falloff)(e_aura_p) };
                        let norm = 1.0_f32 - (dist / radius);
                        let atten = norm / (1.0_f32 + falloff * dist_sq);
                        let force = unsafe { (aura_vt.get_force)(e_aura_p) };

                        let u_team = unsafe { (fac_vt.get_team_id)(fac_p) };
                        let e_team = unsafe { (fac_vt.get_team_id)(e_fac_p) };

                        let vel_mut = unsafe { (kin_vt.ptr_mut_velocity)(kin_p) };

                        if u_team == e_team {
                            let e_fwd_ro = unsafe { (tf_vt.ptr_forward)(e_tf_p) };
                            let boost = force * 0.25_f32 * atten * SIM_DT;
                            unsafe {
                                (v3.set_x)(
                                    vel_mut,
                                    (v3.get_x)(vel_mut) + (v3.get_x)(e_fwd_ro) * boost,
                                );
                                (v3.set_y)(
                                    vel_mut,
                                    (v3.get_y)(vel_mut) + (v3.get_y)(e_fwd_ro) * boost,
                                );
                                (v3.set_z)(
                                    vel_mut,
                                    (v3.get_z)(vel_mut) + (v3.get_z)(e_fwd_ro) * boost,
                                );

                                let heal_hps = (aura_vt.get_heal_hps)(e_aura_p);
                                let r_ion = (res_vt.get_ionic)(res_p);
                                let heal = heal_hps * atten * SIM_DT * (1.0_f32 + 0.1_f32 * r_ion);
                                let max_h = (vit_vt.get_max_health)(vit_p);
                                let nh = (vit_vt.get_health)(vit_p) + heal;
                                (vit_vt.set_health)(vit_p, if nh > max_h { max_h } else { nh });

                                let ne = (vit_vt.get_energy)(vit_p) + 2.5_f32 * atten * SIM_DT;
                                (vit_vt.set_energy)(
                                    vit_p,
                                    if ne > 100.0_f32 { 100.0_f32 } else { ne },
                                );
                            }
                        } else {
                            unsafe {
                                let r_kin = (res_vt.get_kinetic)(res_p);
                                let imp =
                                    force * atten * SIM_DT * (1.0_f32 - 0.5_f32 * r_kin) * inv_dist;
                                (v3.set_x)(vel_mut, (v3.get_x)(vel_mut) + dx * imp);
                                (v3.set_y)(vel_mut, (v3.get_y)(vel_mut) + dy * imp);
                                (v3.set_z)(vel_mut, (v3.get_z)(vel_mut) + dz * imp);

                                let tdps = (aura_vt.get_thermal_dps)(e_aura_p);
                                let r_thm = (res_vt.get_thermal)(res_p);
                                let raw_dmg = tdps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * r_thm);
                                let cur_sh = (vit_vt.get_shield)(vit_p);
                                let absorbed = if cur_sh >= raw_dmg { raw_dmg } else { cur_sh };
                                (vit_vt.set_shield)(vit_p, cur_sh - absorbed);
                                let nh = (vit_vt.get_health)(vit_p) - (raw_dmg - absorbed);
                                (vit_vt.set_health)(vit_p, if nh < 0.0_f32 { 0.0_f32 } else { nh });

                                let r_ion = (res_vt.get_ionic)(res_p);
                                let drain = 1.5_f32 * atten * SIM_DT * (1.0_f32 - 0.4_f32 * r_ion);
                                let ne = (vit_vt.get_energy)(vit_p) - drain;
                                (vit_vt.set_energy)(vit_p, if ne < 0.0_f32 { 0.0_f32 } else { ne });
                            }
                        }

                        // Naive per-pair explicit change-tick FFI calls!
                        unsafe {
                            let _ = (api.iter_mark_changed)(unit_iter, 1, u_row);
                            let _ = (api.iter_mark_changed)(unit_iter, 2, u_row);
                        }
                    }
                }
            }

            // Finalize drag, speed cap, and position integration
            unsafe {
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

                (v3.set_x)(vel_mut, vx);
                (v3.set_y)(vel_mut, vy);
                (v3.set_z)(vel_mut, vz);

                let pos_mut = (tf_vt.ptr_mut_position)(tf_p);
                (v3.set_x)(pos_mut, (v3.get_x)(pos_mut) + vx * SIM_DT);
                (v3.set_y)(pos_mut, (v3.get_y)(pos_mut) + vy * SIM_DT);
                (v3.set_z)(pos_mut, (v3.get_z)(pos_mut) + vz * SIM_DT);

                let final_speed_sq = vx * vx + vy * vy + vz * vz;
                if final_speed_sq > 1.0e-6_f32 {
                    let inv_s = 1.0_f32 / final_speed_sq.sqrt();
                    let fwd_mut = (tf_vt.ptr_mut_forward)(tf_p);
                    (v3.set_x)(fwd_mut, vx * inv_s);
                    (v3.set_y)(fwd_mut, vy * inv_s);
                    (v3.set_z)(fwd_mut, vz * inv_s);
                }

                let _ = (api.iter_mark_changed)(unit_iter, 0, u_row);
            }
        }
    }

    FfiResult::OK
}

wrap_multi_query_callback!(fn c_chunk_optimized calls sys_chunk_optimized);
wrap_multi_query_callback!(fn c_entity_nested_vtable calls sys_entity_nested_vtable);
wrap_multi_query_callback!(fn c_naive_chatty_ffi calls sys_naive_chatty_ffi);

fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    if host_api.is_null() || host_context.is_null() {
        return FfiResult::NULL_POINTER;
    }
    HOST_API_PTR.store(host_api as *mut HostApi, Ordering::Release);

    let resolve = |sym: &[u8]| -> *const c_void {
        unsafe { get_proc_address(sym.as_ptr() as *const core::ffi::c_char) }
    };

    let reg_mq_ptr = resolve(b"register_multi_query_system\0");
    let reset_ptr = resolve(b"iter_reset\0");
    let v3_ptr = resolve(b"Vec3f_vtable\0");
    let tf_ptr = resolve(b"Transform3D_vtable\0");
    let kin_ptr = resolve(b"Kinematics_vtable\0");
    let vit_ptr = resolve(b"Vitality_vtable\0");
    let res_ptr = resolve(b"Resistance_vtable\0");
    let aura_ptr = resolve(b"AuraEmitter_vtable\0");
    let fac_ptr = resolve(b"Faction_vtable\0");

    if reg_mq_ptr.is_null()
        || reset_ptr.is_null()
        || v3_ptr.is_null()
        || tf_ptr.is_null()
        || kin_ptr.is_null()
        || vit_ptr.is_null()
        || res_ptr.is_null()
        || aura_ptr.is_null()
        || fac_ptr.is_null()
    {
        return FfiResult::INVALID_COMPONENT;
    }

    ITER_RESET_FN.store(reset_ptr as *mut c_void, Ordering::Release);
    VEC3_VT.store(v3_ptr as *mut Vec3fVTable, Ordering::Release);
    TF_VT.store(tf_ptr as *mut Transform3DVTable, Ordering::Release);
    KIN_VT.store(kin_ptr as *mut KinematicsVTable, Ordering::Release);
    VIT_VT.store(vit_ptr as *mut VitalityVTable, Ordering::Release);
    RES_VT.store(res_ptr as *mut ResistanceVTable, Ordering::Release);
    AURA_VT.store(aura_ptr as *mut AuraEmitterVTable, Ordering::Release);
    FAC_VT.store(fac_ptr as *mut FactionVTable, Ordering::Release);

    let unit_clause = [
        ComponentAccessDesc::write(c"Transform3D".as_ptr()),
        ComponentAccessDesc::write(c"Kinematics".as_ptr()),
        ComponentAccessDesc::write(c"Vitality".as_ptr()),
        ComponentAccessDesc::read(c"Resistance".as_ptr()),
        ComponentAccessDesc::read(c"Faction".as_ptr()),
        ComponentAccessDesc::filter_only(c"CombatUnitTag".as_ptr()),
        ComponentAccessDesc::without(c"EmitterBeaconTag".as_ptr()),
        ComponentAccessDesc::without(c"FrozenFlag".as_ptr()),
    ];

    let emitter_clause = [
        ComponentAccessDesc::read(c"Transform3D".as_ptr()),
        ComponentAccessDesc::read(c"AuraEmitter".as_ptr()),
        ComponentAccessDesc::read(c"Faction".as_ptr()),
        ComponentAccessDesc::filter_only(c"EmitterBeaconTag".as_ptr()),
        ComponentAccessDesc::without(c"CombatUnitTag".as_ptr()),
        ComponentAccessDesc::without(c"FrozenFlag".as_ptr()),
    ];

    let specs = [
        QuerySpec {
            queries: unit_clause.as_ptr(),
            query_count: unit_clause.len(),
        },
        QuerySpec {
            queries: emitter_clause.as_ptr(),
            query_count: emitter_clause.len(),
        },
    ];

    let variant = BENCH_VARIANT.load(Ordering::SeqCst);
    let (sys_name, cb): (*const core::ffi::c_char, MultiQuerySystemCallback) = match variant {
        0 => (
            c"rust_cabi_chunk_optimized".as_ptr(),
            c_chunk_optimized,
        ),
        1 => (
            c"rust_cabi_entity_nested_vtable".as_ptr(),
            c_entity_nested_vtable,
        ),
        _ => (
            c"rust_cabi_naive_chatty_ffi".as_ptr(),
            c_naive_chatty_ffi,
        ),
    };

    let desc = MultiQuerySystemDescriptor {
        struct_size: core::mem::size_of::<MultiQuerySystemDescriptor>() as u32,
        name: sys_name,
        query_specs: specs.as_ptr(),
        query_spec_count: specs.len(),
        callback: cb,
        user_data: core::ptr::null_mut(),
    };

    let register_multi_query: RegisterMultiQuerySystemFn =
        unsafe { core::mem::transmute(reg_mq_ptr) };
    unsafe { register_multi_query(host_context, &desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
