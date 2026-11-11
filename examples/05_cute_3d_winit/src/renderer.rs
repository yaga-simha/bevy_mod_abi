//! Cute Toy-Diorama 3D Renderer (`wgpu` + `winit`) with procedural kawaii meshes
//! and crisp pixel-art HUD overlay.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Quat, Vec3};
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::components::{ColorRgba, CuteSpirit, Position, Scale3D};
use crate::mod_manager::{ActiveMod, ModStatusResource};

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const MAX_INSTANCES: usize = 8192;
const MAX_UI_VERTICES: usize = 32768;

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct MeshVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct ModelInstance {
    pub model_col0: [f32; 4],
    pub model_col1: [f32; 4],
    pub model_col2: [f32; 4],
    pub model_col3: [f32; 4],
    pub color: [f32; 4],
}

impl ModelInstance {
    pub fn from_mat4_color(m: Mat4, color: [f32; 4]) -> Self {
        let cols = m.to_cols_array_2d();
        Self {
            model_col0: cols[0],
            model_col1: cols[1],
            model_col2: cols[2],
            model_col3: cols[3],
            color,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct CameraUniform {
    view_proj: [[f32; 4]; 4],
    camera_pos: [f32; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct UiVertex {
    pub pos_ndc: [f32; 2],
    pub color: [f32; 4],
}

/// Snapshot of a slime spirit for rendering a frame.
#[derive(Clone, Copy)]
pub struct SlimeRenderData {
    pub pos: Position,
    pub scale: Scale3D,
    pub color: ColorRgba,
    pub spirit: CuteSpirit,
}

/// Interactive 3D orbital camera state.
pub struct OrbitCamera {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub target: Vec3,
    pub auto_orbit: bool,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            yaw: 0.55,
            pitch: 0.44,
            distance: 18.0,
            target: Vec3::new(0.0, 1.1, 0.0),
            auto_orbit: true,
        }
    }
}

impl OrbitCamera {
    pub fn eye(&self) -> Vec3 {
        let cp = self.pitch.cos();
        let sp = self.pitch.sin();
        let sy = self.yaw.sin();
        let cy = self.yaw.cos();
        self.target
            + Vec3::new(
                self.distance * cp * sy,
                self.distance * sp,
                self.distance * cp * cy,
            )
    }
}

pub struct CuteRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    depth_view: wgpu::TextureView,
    pipeline_3d: wgpu::RenderPipeline,
    pipeline_ui: wgpu::RenderPipeline,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    vertex_buffer_sphere: wgpu::Buffer,
    index_buffer_sphere: wgpu::Buffer,
    sphere_index_count: u32,
    instance_buffer: wgpu::Buffer,
    ui_vertex_buffer: wgpu::Buffer,
    static_scenery: Vec<ModelInstance>,
}

const SHADER_3D: &str = r#"
struct CameraUniform {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: CameraUniform;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

struct InstanceInput {
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) color: vec4<f32>,
};

@vertex
fn vs_main(v: VertexInput, inst: InstanceInput) -> VertexOutput {
    let model = mat4x4<f32>(inst.m0, inst.m1, inst.m2, inst.m3);
    let world_pos4 = model * vec4<f32>(v.position, 1.0);

    // Transform normal smoothly even under squash-and-stretch non-uniform scale
    let c0 = inst.m0.xyz;
    let c1 = inst.m1.xyz;
    let c2 = inst.m2.xyz;
    let inv_s0 = 1.0 / max(dot(c0, c0), 0.0001);
    let inv_s1 = 1.0 / max(dot(c1, c1), 0.0001);
    let inv_s2 = 1.0 / max(dot(c2, c2), 0.0001);
    let world_n = normalize(c0 * (v.normal.x * inv_s0) + c1 * (v.normal.y * inv_s1) + c2 * (v.normal.z * inv_s2));

    var out: VertexOutput;
    out.clip_position = camera.view_proj * world_pos4;
    out.world_pos = world_pos4.xyz;
    out.world_normal = world_n;
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Unlit flag stored when alpha < 0.995 (used for soft drop shadows & glowing sparkles)
    if (in.color.a < 0.995) {
        return vec4<f32>(in.color.rgb, 1.0);
    }

    let n = normalize(in.world_normal);
    let v = normalize(camera.camera_pos.xyz - in.world_pos);
    let sun_dir = normalize(vec3<f32>(0.48, 0.82, 0.34));

    // Cozy Nintendo-style pastel hemisphere + soft toon wrap lighting
    let ndotl = dot(n, sun_dir);
    let wrap = clamp(ndotl * 0.5 + 0.5, 0.0, 1.0);
    let toon_band = smoothstep(0.32, 0.68, wrap);

    let sky_tint = vec3<f32>(0.96, 0.92, 1.00);
    let warm_bounce = vec3<f32>(0.92, 0.80, 0.86);
    let hemi = mix(warm_bounce, sky_tint, clamp(n.y * 0.5 + 0.5, 0.0, 1.0));

    // Cute glossy jelly specular highlight + velvety Fresnel rim glow
    let h = normalize(sun_dir + v);
    let spec = pow(max(dot(n, h), 0.0), 36.0);
    let spec_pop = smoothstep(0.45, 0.82, spec) * 0.28;
    let rim = pow(1.0 - max(dot(n, v), 0.0), 3.0) * 0.24;

    var lit = in.color.rgb * (hemi * 0.62 + vec3<f32>(1.0, 0.98, 0.95) * (0.44 * toon_band + 0.12));
    lit = lit + vec3<f32>(1.0, 0.98, 1.0) * (spec_pop + rim);

    return vec4<f32>(clamp(lit, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

const SHADER_UI: &str = r#"
struct UiIn {
    @location(0) pos_ndc: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct UiOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_ui(in: UiIn) -> UiOut {
    var out: UiOut;
    out.clip_pos = vec4<f32>(in.pos_ndc, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_ui(in: UiOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

impl CuteRenderer {
    pub fn new(window: Arc<Window>) -> Result<Self, String> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("Failed to create wgpu surface: {e}"))?;

        let adapter =
            futures_executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
            .map_err(|e| format!("No suitable GPU adapter found: {e}"))?;

        let (device, queue) =
            futures_executor::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("Cute3DModDevice"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            }))
            .map_err(|e| format!("Failed to request GPU device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let depth_view = Self::create_depth_view(&device, width, height);

        // Camera uniform
        let cam_uniform = CameraUniform {
            view_proj: Mat4::IDENTITY.to_cols_array_2d(),
            camera_pos: [0.0, 10.0, 18.0, 1.0],
        };
        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("CameraUniformBuffer"),
            contents: bytemuck::bytes_of(&cam_uniform),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let camera_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("CameraBGL"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("CameraBG"),
            layout: &camera_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

        let shader_3d = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Shader3D"),
            source: wgpu::ShaderSource::Wgsl(SHADER_3D.into()),
        });

        let pipeline_layout_3d = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("PipelineLayout3D"),
            bind_group_layouts: &[Some(&camera_bgl)],
            immediate_size: 0,
        });

        let pipeline_3d = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Pipeline3D"),
            layout: Some(&pipeline_layout_3d),
            vertex: wgpu::VertexState {
                module: &shader_3d,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: core::mem::size_of::<MeshVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[
                            wgpu::VertexAttribute {
                                offset: 0,
                                shader_location: 0,
                                format: wgpu::VertexFormat::Float32x3,
                            },
                            wgpu::VertexAttribute {
                                offset: 12,
                                shader_location: 1,
                                format: wgpu::VertexFormat::Float32x3,
                            },
                        ],
                    },
                    wgpu::VertexBufferLayout {
                        array_stride: core::mem::size_of::<ModelInstance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                offset: 0,
                                shader_location: 2,
                                format: wgpu::VertexFormat::Float32x4,
                            },
                            wgpu::VertexAttribute {
                                offset: 16,
                                shader_location: 3,
                                format: wgpu::VertexFormat::Float32x4,
                            },
                            wgpu::VertexAttribute {
                                offset: 32,
                                shader_location: 4,
                                format: wgpu::VertexFormat::Float32x4,
                            },
                            wgpu::VertexAttribute {
                                offset: 48,
                                shader_location: 5,
                                format: wgpu::VertexFormat::Float32x4,
                            },
                            wgpu::VertexAttribute {
                                offset: 64,
                                shader_location: 6,
                                format: wgpu::VertexFormat::Float32x4,
                            },
                        ],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader_3d,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        // 2D HUD Overlay pipeline
        let shader_ui = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ShaderUI"),
            source: wgpu::ShaderSource::Wgsl(SHADER_UI.into()),
        });
        let pipeline_layout_ui = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("PipelineLayoutUI"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let pipeline_ui = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("PipelineUI"),
            layout: Some(&pipeline_layout_ui),
            vertex: wgpu::VertexState {
                module: &shader_ui,
                entry_point: Some("vs_ui"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: core::mem::size_of::<UiVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x4,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader_ui,
                entry_point: Some("fs_ui"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let (sphere_verts, sphere_indices) = build_uv_sphere(26, 18);
        let sphere_index_count = sphere_indices.len() as u32;

        let vertex_buffer_sphere = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("SphereVertices"),
            contents: bytemuck::cast_slice(&sphere_verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer_sphere = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("SphereIndices"),
            contents: bytemuck::cast_slice(&sphere_indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ModelInstanceBuffer"),
            size: (MAX_INSTANCES * core::mem::size_of::<ModelInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let ui_vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("UiVertexBuffer"),
            size: (MAX_UI_VERTICES * core::mem::size_of::<UiVertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let static_scenery = build_cute_diorama_scenery();

        Ok(Self {
            surface,
            device,
            queue,
            config,
            depth_view,
            pipeline_3d,
            pipeline_ui,
            camera_buffer,
            camera_bind_group,
            vertex_buffer_sphere,
            index_buffer_sphere,
            sphere_index_count,
            instance_buffer,
            ui_vertex_buffer,
            static_scenery,
        })
    }

    fn create_depth_view(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DepthTexture"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.depth_view = Self::create_depth_view(&self.device, width, height);
    }

    pub fn render_frame(
        &mut self,
        camera: &OrbitCamera,
        slimes: &[SlimeRenderData],
        mod_status: &ModStatusResource,
        fps: f64,
        paused: bool,
        time_secs: f32,
    ) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // 1. Update Camera Uniform
        let aspect = self.config.width as f32 / (self.config.height.max(1) as f32);
        let eye = camera.eye();
        let view_mat = glam::camera::rh::view::look_at_mat4(eye, camera.target, Vec3::Y);
        let proj_mat =
            glam::camera::rh::proj::directx::perspective(42.0_f32.to_radians(), aspect, 0.1, 120.0);
        let cam_uniform = CameraUniform {
            view_proj: (proj_mat * view_mat).to_cols_array_2d(),
            camera_pos: [eye.x, eye.y, eye.z, 1.0],
        };
        self.queue
            .write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&cam_uniform));

        // 2. Assemble 3D instances: static diorama + animated clouds/stars + cute slimes
        let mut instances = Vec::with_capacity(self.static_scenery.len() + slimes.len() * 11 + 32);
        instances.extend_from_slice(&self.static_scenery);

        // Drifting fluffy clouds around the pastel sky
        append_fluffy_clouds(&mut instances, time_secs);

        // Floating magical starlight motes when a mod is active!
        if mod_status.active_mod != ActiveMod::None {
            let accent = mod_status.active_mod.accent_rgb();
            for i in 0..14 {
                let a = (i as f32) * 0.4488 + time_secs * 0.65;
                let r = 3.2 + 4.4 * ((i as f32 * 1.7).sin() * 0.5 + 0.5);
                let y = 1.6 + 1.8 * ((time_secs * 1.8 + i as f32).sin() * 0.5 + 0.5);
                let pulse = 0.07 + 0.04 * (time_secs * 5.0 + i as f32).sin();
                let m = Mat4::from_scale_rotation_translation(
                    Vec3::splat(pulse),
                    Quat::IDENTITY,
                    Vec3::new(r * a.cos(), y, r * a.sin()),
                );
                // alpha = 0.99 marks unlit glowing mote
                instances.push(ModelInstance::from_mat4_color(
                    m,
                    [accent[0], accent[1], accent[2], 0.99],
                ));
            }
        }

        // Cute Slime Spirits (Body + Bunny Ears + Glossy Eyes + Eye Sparkles + Rosy Blush + Ground Shadow)
        for s in slimes {
            if instances.len() + 12 >= MAX_INSTANCES {
                break;
            }
            append_cute_slime(&mut instances, s);
        }

        let instance_count = instances.len().min(MAX_INSTANCES);
        self.queue.write_buffer(
            &self.instance_buffer,
            0,
            bytemuck::cast_slice(&instances[..instance_count]),
        );

        // 3. Build crisp pixel-font HUD overlay banner
        let ui_verts = build_hud_overlay(
            self.config.width as f32,
            self.config.height as f32,
            mod_status,
            slimes.len(),
            fps,
            paused,
        );
        let ui_vert_count = ui_verts.len().min(MAX_UI_VERTICES);
        if ui_vert_count > 0 {
            self.queue.write_buffer(
                &self.ui_vertex_buffer,
                0,
                bytemuck::cast_slice(&ui_verts[..ui_vert_count]),
            );
        }

        // Soft dreamy pastel sky background tinted subtly by the active mod
        let sky_color = match mod_status.active_mod {
            ActiveMod::None => wgpu::Color {
                r: 0.85,
                g: 0.93,
                b: 0.99,
                a: 1.0,
            },
            ActiveMod::Boing => wgpu::Color {
                r: 0.97,
                g: 0.89,
                b: 0.95,
                a: 1.0,
            },
            ActiveMod::Carousel => wgpu::Color {
                r: 0.89,
                g: 0.87,
                b: 0.99,
                a: 1.0,
            },
            ActiveMod::CuddleC99 => wgpu::Color {
                r: 0.99,
                g: 0.92,
                b: 0.85,
                a: 1.0,
            },
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("CuteRenderEncoder"),
            });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Pass3D"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(sky_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            rpass.set_pipeline(&self.pipeline_3d);
            rpass.set_bind_group(0, &self.camera_bind_group, &[]);
            rpass.set_vertex_buffer(0, self.vertex_buffer_sphere.slice(..));
            rpass.set_vertex_buffer(1, self.instance_buffer.slice(..));
            rpass.set_index_buffer(
                self.index_buffer_sphere.slice(..),
                wgpu::IndexFormat::Uint16,
            );
            rpass.draw_indexed(0..self.sphere_index_count, 0, 0..(instance_count as u32));
        }

        if ui_vert_count > 0 {
            let mut ui_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("PassUI"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            ui_pass.set_pipeline(&self.pipeline_ui);
            ui_pass.set_vertex_buffer(0, self.ui_vertex_buffer.slice(..));
            ui_pass.draw(0..(ui_vert_count as u32), 0..1);
        }

        self.queue.submit(core::iter::once(encoder.finish()));
        frame.present();
    }
}

/// Assembles a cute kawaii bunny-slime from smooth instanced ellipsoids!
fn append_cute_slime(out: &mut Vec<ModelInstance>, s: &SlimeRenderData) {
    let base_radius = 0.52_f32;
    let body_scale = Vec3::new(
        base_radius * s.scale.x,
        base_radius * 0.88 * s.scale.y,
        base_radius * s.scale.z,
    );

    let ground_y = s.pos.y.max(0.0);
    let body_center = Vec3::new(s.pos.x, ground_y + body_scale.y * 0.92, s.pos.z);
    let rot_y = Quat::from_rotation_y(s.spirit.yaw);

    // 1. Soft dark-sage drop shadow on the grass (shrinks slightly as the slime hops high)
    let shadow_factor = (1.0 / (1.0 + ground_y * 0.35)).clamp(0.35, 1.0);
    let shadow_mat = Mat4::from_scale_rotation_translation(
        Vec3::new(
            base_radius * 0.98 * shadow_factor * s.scale.x,
            0.018,
            base_radius * 0.98 * shadow_factor * s.scale.z,
        ),
        Quat::IDENTITY,
        Vec3::new(s.pos.x, 0.015, s.pos.z),
    );
    out.push(ModelInstance::from_mat4_color(
        shadow_mat,
        [0.42, 0.66, 0.50, 0.99], // alpha 0.99 = flat unlit shadow tint on meadow
    ));

    // 2. Plump jelly body
    let body_mat = Mat4::from_scale_rotation_translation(body_scale, rot_y, body_center);
    let body_col = [s.color.r, s.color.g, s.color.b, 1.0];
    out.push(ModelInstance::from_mat4_color(body_mat, body_col));

    // Parent frame that follows the slime's facing direction & squash-stretch silhouette
    let local_to_world = |local_pos: Vec3, local_rot: Quat, local_scale: Vec3| -> Mat4 {
        let scaled_offset = local_pos * body_scale;
        let world_pos = body_center + rot_y * scaled_offset;
        Mat4::from_scale_rotation_translation(local_scale, rot_y * local_rot, world_pos)
    };

    // 3. Cute floppy bunny ears on top!
    let ear_col = [
        (s.color.r * 0.95 + 0.05).min(1.0),
        (s.color.g * 0.90 + 0.04).min(1.0),
        (s.color.b * 0.94 + 0.05).min(1.0),
        1.0,
    ];
    let ear_size = Vec3::new(
        0.12 * s.scale.x,
        0.30 * s.scale.y.clamp(0.8, 1.25),
        0.09 * s.scale.z,
    );
    let left_ear_rot = Quat::from_euler(
        glam::EulerRot::XYZ,
        s.spirit.ear_wiggle,
        0.0,
        0.32 + s.spirit.ear_wiggle * 0.35,
    );
    let right_ear_rot = Quat::from_euler(
        glam::EulerRot::XYZ,
        s.spirit.ear_wiggle,
        0.0,
        -0.32 - s.spirit.ear_wiggle * 0.35,
    );
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(-0.42, 0.92, -0.05), left_ear_rot, ear_size),
        ear_col,
    ));
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(0.42, 0.92, -0.05), right_ear_rot, ear_size),
        ear_col,
    ));

    // 4. Glossy dark espresso eyes (+Z is forward)
    let eye_col = [0.11, 0.08, 0.14, 1.0];
    let eye_scale = Vec3::new(0.085, 0.105, 0.055);
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(-0.34, 0.16, 0.90), Quat::IDENTITY, eye_scale),
        eye_col,
    ));
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(0.34, 0.16, 0.90), Quat::IDENTITY, eye_scale),
        eye_col,
    ));

    // 5. Tiny white kawaii eye-catchlight sparkles!
    let spark_col = [1.0, 1.0, 1.0, 0.99];
    let spark_scale = Vec3::splat(0.032);
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(-0.30, 0.24, 0.99), Quat::IDENTITY, spark_scale),
        spark_col,
    ));
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(0.38, 0.24, 0.99), Quat::IDENTITY, spark_scale),
        spark_col,
    ));

    // 6. Rosy pink blush cheeks (grow & brighten when blushing!)
    let b = s.spirit.blush.clamp(0.15, 1.0);
    let blush_col = [1.0, 0.42 + 0.15 * (1.0 - b), 0.60, 0.99];
    let blush_scale = Vec3::new(0.095 * (0.75 + 0.45 * b), 0.058 * (0.75 + 0.45 * b), 0.045);
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(-0.56, -0.02, 0.78), Quat::IDENTITY, blush_scale),
        blush_col,
    ));
    out.push(ModelInstance::from_mat4_color(
        local_to_world(Vec3::new(0.56, -0.02, 0.78), Quat::IDENTITY, blush_scale),
        blush_col,
    ));
}

/// Builds the static cute 3D floating island meadow, giant plush spotted mushroom,
/// pastel flowers, and cozy puffball trees.
fn build_cute_diorama_scenery() -> Vec<ModelInstance> {
    let mut s = Vec::with_capacity(140);

    // 1. Lush mint-green grass top disc + warm biscuit cliff base
    s.push(ModelInstance::from_mat4_color(
        Mat4::from_scale_rotation_translation(
            Vec3::new(9.6, 0.55, 9.6),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.52, 0.0),
        ),
        [0.58, 0.88, 0.64, 1.0],
    ));
    s.push(ModelInstance::from_mat4_color(
        Mat4::from_scale_rotation_translation(
            Vec3::new(8.9, 1.65, 8.9),
            Quat::IDENTITY,
            Vec3::new(0.0, -1.55, 0.0),
        ),
        [0.86, 0.72, 0.58, 1.0],
    ));

    // 2. Centerpiece Giant Spotted Plush Mushroom!
    // Cream stem
    s.push(ModelInstance::from_mat4_color(
        Mat4::from_scale_rotation_translation(
            Vec3::new(0.72, 1.25, 0.72),
            Quat::IDENTITY,
            Vec3::new(0.0, 0.95, 0.0),
        ),
        [0.99, 0.95, 0.88, 1.0],
    ));
    // Strawberry-coral mushroom cap
    s.push(ModelInstance::from_mat4_color(
        Mat4::from_scale_rotation_translation(
            Vec3::new(2.05, 0.95, 2.05),
            Quat::IDENTITY,
            Vec3::new(0.0, 2.15, 0.0),
        ),
        [0.98, 0.45, 0.56, 1.0],
    ));
    // Cute cream polka-dots on the mushroom cap
    let dot_col = [1.0, 0.98, 0.94, 1.0];
    s.push(ModelInstance::from_mat4_color(
        Mat4::from_scale_rotation_translation(
            Vec3::new(0.38, 0.18, 0.38),
            Quat::IDENTITY,
            Vec3::new(0.0, 3.05, 0.0),
        ),
        dot_col,
    ));
    for i in 0..6 {
        let a = (i as f32) * core::f32::consts::TAU / 6.0;
        let r = 1.25;
        let tilt = Quat::from_rotation_y(-a) * Quat::from_rotation_z(-0.42);
        s.push(ModelInstance::from_mat4_color(
            Mat4::from_scale_rotation_translation(
                Vec3::new(0.34, 0.14, 0.34),
                tilt,
                Vec3::new(r * a.cos(), 2.62, r * a.sin()),
            ),
            dot_col,
        ));
    }

    // 3. Ring of pastel puffball trees & mini toadstools around the outer rim
    let tree_colors = [
        [1.00, 0.74, 0.84, 1.0], // Sakura pink tree
        [0.66, 0.92, 0.74, 1.0], // Mint puff tree
        [0.98, 0.88, 0.62, 1.0], // Honey-lemon tree
        [0.78, 0.84, 1.00, 1.0], // Periwinkle puff tree
    ];
    for i in 0..8 {
        let a = (i as f32) * core::f32::consts::TAU / 8.0 + 0.22;
        let r = 8.55;
        let x = r * a.cos();
        let z = r * a.sin();

        // Trunk
        s.push(ModelInstance::from_mat4_color(
            Mat4::from_scale_rotation_translation(
                Vec3::new(0.22, 0.65, 0.22),
                Quat::IDENTITY,
                Vec3::new(x, 0.45, z),
            ),
            [0.78, 0.60, 0.48, 1.0],
        ));
        // Puffball canopy trio
        let col = tree_colors[i % tree_colors.len()];
        s.push(ModelInstance::from_mat4_color(
            Mat4::from_scale_rotation_translation(
                Vec3::splat(0.78),
                Quat::IDENTITY,
                Vec3::new(x, 1.35, z),
            ),
            col,
        ));
        s.push(ModelInstance::from_mat4_color(
            Mat4::from_scale_rotation_translation(
                Vec3::splat(0.52),
                Quat::IDENTITY,
                Vec3::new(x + 0.42 * a.sin(), 1.12, z - 0.42 * a.cos()),
            ),
            col,
        ));
    }

    // 4. Tiny meadow flowers scattered on the grass
    let petal_colors = [
        [1.0, 0.72, 0.84, 1.0],
        [1.0, 0.95, 0.65, 1.0],
        [0.82, 0.78, 1.0, 1.0],
        [1.0, 0.82, 0.66, 1.0],
    ];
    for i in 0..18 {
        let a = (i as f32) * 2.39996;
        let r = 2.0 + ((i * 7) % 13) as f32 * 0.48;
        let fx = r * a.cos();
        let fz = r * a.sin();
        let pcol = petal_colors[i % petal_colors.len()];

        s.push(ModelInstance::from_mat4_color(
            Mat4::from_scale_rotation_translation(
                Vec3::new(0.11, 0.04, 0.11),
                Quat::IDENTITY,
                Vec3::new(fx, 0.04, fz),
            ),
            [1.0, 0.88, 0.35, 1.0],
        ));
        for p in 0..5 {
            let pa = (p as f32) * core::f32::consts::TAU / 5.0;
            s.push(ModelInstance::from_mat4_color(
                Mat4::from_scale_rotation_translation(
                    Vec3::new(0.10, 0.03, 0.10),
                    Quat::IDENTITY,
                    Vec3::new(fx + 0.14 * pa.cos(), 0.03, fz + 0.14 * pa.sin()),
                ),
                pcol,
            ));
        }
    }

    s
}

fn append_fluffy_clouds(out: &mut Vec<ModelInstance>, time_secs: f32) {
    let cloud_col = [1.0, 0.99, 1.0, 1.0];
    for i in 0..5 {
        let angle = (i as f32) * core::f32::consts::TAU / 5.0 + time_secs * 0.06;
        let radius = 13.5 + (i % 2) as f32 * 2.2;
        let cx = radius * angle.cos();
        let cz = radius * angle.sin();
        let cy = 5.2 + 0.35 * (time_secs * 0.7 + i as f32).sin();

        let rot = Quat::from_rotation_y(-angle);
        let puffs = [
            (Vec3::ZERO, Vec3::new(1.15, 0.72, 0.85)),
            (Vec3::new(-0.85, -0.12, 0.0), Vec3::new(0.80, 0.56, 0.68)),
            (Vec3::new(0.85, -0.10, 0.0), Vec3::new(0.82, 0.58, 0.68)),
        ];
        for (offset, scale) in puffs {
            out.push(ModelInstance::from_mat4_color(
                Mat4::from_scale_rotation_translation(
                    scale,
                    rot,
                    Vec3::new(cx, cy, cz) + rot * offset,
                ),
                cloud_col,
            ));
        }
    }
}

fn build_uv_sphere(sectors: u16, stacks: u16) -> (Vec<MeshVertex>, Vec<u16>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();

    for i in 0..=stacks {
        let v = i as f32 / stacks as f32;
        let phi = core::f32::consts::PI * v;
        let sin_phi = phi.sin();
        let cos_phi = phi.cos();

        for j in 0..=sectors {
            let u = j as f32 / sectors as f32;
            let theta = core::f32::consts::TAU * u;
            let x = sin_phi * theta.cos();
            let y = cos_phi;
            let z = sin_phi * theta.sin();

            vertices.push(MeshVertex {
                position: [x, y, z],
                normal: [x, y, z],
            });
        }
    }

    let ring = sectors + 1;
    for i in 0..stacks {
        for j in 0..sectors {
            let k1 = i * ring + j;
            let k2 = k1 + ring;
            if i != 0 {
                indices.extend_from_slice(&[k1, k2, k1 + 1]);
            }
            if i != stacks - 1 {
                indices.extend_from_slice(&[k1 + 1, k2, k2 + 1]);
            }
        }
    }

    (vertices, indices)
}

// =============================================================================
// Crisp Built-in 5x7 Pixel Bitmap Font HUD Overlay (Zero External Font Deps)
// =============================================================================

fn build_hud_overlay(
    screen_w: f32,
    screen_h: f32,
    status: &ModStatusResource,
    slime_count: usize,
    fps: f64,
    paused: bool,
) -> Vec<UiVertex> {
    let mut v = Vec::with_capacity(4096);
    if screen_w < 100.0 || screen_h < 100.0 {
        return v;
    }

    // Scale UI cleanly for HiDPI / Retina windows
    let ui_scale = (screen_w / 1280.0).clamp(1.0, 2.2);
    let px = (2.0 * ui_scale).round().max(2.0);

    // Top-left pastel card background
    let card_x = 18.0 * ui_scale;
    let card_y = 16.0 * ui_scale;
    let card_w = 510.0 * ui_scale;
    let card_h = 92.0 * ui_scale;

    let accent = status.active_mod.accent_rgb();
    push_rect(
        &mut v,
        screen_w,
        screen_h,
        card_x - 3.0 * ui_scale,
        card_y - 3.0 * ui_scale,
        card_w + 6.0 * ui_scale,
        card_h + 6.0 * ui_scale,
        [accent[0], accent[1], accent[2], 0.92],
    );
    push_rect(
        &mut v,
        screen_w,
        screen_h,
        card_x,
        card_y,
        card_w,
        card_h,
        [0.16, 0.14, 0.22, 0.84],
    );

    let state_tag = if paused { " [PAUSED]" } else { "" };
    let line1 = format!(
        "{}{}  |  SLIMES: {}  |  {:.0} FPS",
        status.active_mod.short_badge(),
        state_tag,
        slime_count,
        fps
    );
    let line2 = "[1] BOING (RUST)  [2] CAROUSEL (RUST)  [3] CUDDLE (C99)";
    let line3 = format!(
        "[R] HOT-RELOAD (#{})  [U] UNLOAD  [SPACE] PAUSE  [DRAG] ORBIT",
        status.reload_count
    );

    draw_text(
        &mut v,
        screen_w,
        screen_h,
        card_x + 14.0 * ui_scale,
        card_y + 12.0 * ui_scale,
        px,
        &line1,
        [accent[0], accent[1], accent[2], 1.0],
    );
    draw_text(
        &mut v,
        screen_w,
        screen_h,
        card_x + 14.0 * ui_scale,
        card_y + 40.0 * ui_scale,
        px,
        line2,
        [0.98, 0.96, 1.0, 0.96],
    );
    draw_text(
        &mut v,
        screen_w,
        screen_h,
        card_x + 14.0 * ui_scale,
        card_y + 66.0 * ui_scale,
        px,
        &line3,
        [0.80, 0.88, 0.98, 0.92],
    );

    v
}

#[allow(clippy::too_many_arguments)]
fn push_rect(
    out: &mut Vec<UiVertex>,
    sw: f32,
    sh: f32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [f32; 4],
) {
    let x0 = (x / sw) * 2.0 - 1.0;
    let x1 = ((x + w) / sw) * 2.0 - 1.0;
    let y0 = 1.0 - (y / sh) * 2.0;
    let y1 = 1.0 - ((y + h) / sh) * 2.0;

    out.extend_from_slice(&[
        UiVertex {
            pos_ndc: [x0, y0],
            color,
        },
        UiVertex {
            pos_ndc: [x1, y0],
            color,
        },
        UiVertex {
            pos_ndc: [x1, y1],
            color,
        },
        UiVertex {
            pos_ndc: [x0, y0],
            color,
        },
        UiVertex {
            pos_ndc: [x1, y1],
            color,
        },
        UiVertex {
            pos_ndc: [x0, y1],
            color,
        },
    ]);
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    out: &mut Vec<UiVertex>,
    sw: f32,
    sh: f32,
    start_x: f32,
    start_y: f32,
    pixel: f32,
    text: &str,
    color: [f32; 4],
) {
    let mut cursor_x = start_x;
    for ch in text.chars() {
        let rows = glyph_5x7(ch.to_ascii_uppercase());
        for (ry, row_bits) in rows.iter().enumerate() {
            for rx in 0..5 {
                if (row_bits >> (4 - rx)) & 1 == 1 {
                    push_rect(
                        out,
                        sw,
                        sh,
                        cursor_x + (rx as f32) * pixel,
                        start_y + (ry as f32) * pixel,
                        pixel,
                        pixel,
                        color,
                    );
                }
            }
        }
        cursor_x += 6.0 * pixel;
    }
}

fn glyph_5x7(c: char) -> [u8; 7] {
    match c {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10011, 0b10101, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        ':' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000,
        ],
        '|' => [
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        '[' => [
            0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110,
        ],
        ']' => [
            0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110,
        ],
        '(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '#' => [
            0b01010, 0b11111, 0b01010, 0b01010, 0b11111, 0b01010, 0b00000,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100, 0b00100,
        ],
        _ => [0; 7],
    }
}
