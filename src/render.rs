//! wgpu rendering: a 3D wireframe terrain / globe with a modern post stack.
//!
//! Pass chain per frame: (1) scene → HDR target (MSAA-resolved) drawing a graded
//! backdrop + thick line quads; (2) bright-pass at half-res; (3,4) separable
//! Gaussian blur (H then V) for bloom; (5) composite → surface with scene +
//! additive bloom, ACES tone-map + grade, then the now-playing text.

use std::sync::Arc;

use glam::{Mat4, Vec3};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache as GlyphCache, Color as TextColor, Family, FontSystem,
    Metrics, Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer,
    Viewport,
};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::dsp::{AudioFeatures, SPECTRUM_COLS};

const COLS: usize = SPECTRUM_COLS;
const ROWS: usize = 96;
const DEPTH: f32 = 24.0;
const HEIGHT_SCALE: f32 = 5.0;
const VFOV_DEG: f32 = 55.0;
const WIDTH_MARGIN: f32 = 1.02;
const MAX_FILL_ASPECT: f32 = 1.85;
const LINE_THICKNESS_PX: f32 = 2.2;

const GLOBE_EYE_DIST: f32 = 4.9;
const GLOBE_SPIN_SPEED: f32 = 0.3;
const GLOBE_TILT: f32 = 1.0;

const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    accent: [f32; 4], // rgb, level
    grid: [f32; 4],   // cols, rows, head, beat
    shape: [f32; 4],  // depth, width_front, width_back, height_scale
    misc: [f32; 4],   // time, globe?, fade_near, fade_far
    res: [f32; 4],    // width, height, thickness_px, _
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurDir {
    texel: [f32; 4],
}

/// Size-dependent render targets + their post bind groups, rebuilt on resize.
struct Targets {
    scene_msaa: Option<wgpu::TextureView>,
    scene: wgpu::TextureView,
    bloom_a: wgpu::TextureView,
    bloom_b: wgpu::TextureView,
    bright_bg: wgpu::BindGroup,
    blur_h_bg: wgpu::BindGroup,
    blur_v_bg: wgpu::BindGroup,
    composite_bg: wgpu::BindGroup,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    sample_count: u32,

    line_pipeline: wgpu::RenderPipeline,
    backdrop_pipeline: wgpu::RenderPipeline,
    bright_pipeline: wgpu::RenderPipeline,
    blur_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline,

    bright_bgl: wgpu::BindGroupLayout,
    blur_bgl: wgpu::BindGroupLayout,
    composite_bgl: wgpu::BindGroupLayout,

    uniform_buffer: wgpu::Buffer,
    heights_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_count: u32,
    scene_bind_group: wgpu::BindGroup,

    sampler: wgpu::Sampler,
    blur_h_buf: wgpu::Buffer,
    blur_v_buf: wgpu::Buffer,
    targets: Targets,

    // Now-playing text (glyphon), drawn in the composite pass.
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    text_buffer: TextBuffer,
    text_cache: Option<String>,

    globe_yaw: f32,
    globe_pitch: f32,
    globe_zoom: f32,
    bob: f32,             // eased beat for the camera bob
    accent_cur: [f32; 3], // eased accent color

    pub size: winit::dpi::PhysicalSize<u32>,
    head: usize,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let mut idesc = wgpu::InstanceDescriptor::new_without_display_handle();
        idesc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(idesc);
        let surface = instance.create_surface(window).expect("create surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter found");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        // Best MSAA level for the HDR scene format (4× → 2× → off).
        let msaa_flags = adapter.get_texture_format_features(HDR_FORMAT).flags;
        let sample_count = if msaa_flags.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X4) {
            4
        } else if msaa_flags.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X2) {
            2
        } else {
            1
        };

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

        let scene_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/terrain.wgsl").into()),
        });
        let post_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/post.wgsl").into()),
        });

        // ---- Scene geometry: line segments as instances (a,b vertex indices).
        let mut inst: Vec<u32> = Vec::with_capacity(COLS * ROWS * 4);
        for d in 0..ROWS {
            for col in 0..COLS {
                let i = (d * COLS + col) as u32;
                if col + 1 < COLS {
                    inst.push(i);
                    inst.push(i + 1);
                }
                if d + 1 < ROWS {
                    inst.push(i);
                    inst.push(i + COLS as u32);
                }
            }
        }
        let instance_count = (inst.len() / 2) as u32;
        let instance_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("segments"),
            contents: bytemuck::cast_slice(&inst),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let heights_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("heights"),
            contents: bytemuck::cast_slice(&vec![0.0f32; COLS * ROWS]),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // ---- Bind group layouts.
        let scene_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp_entry = wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let bright_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bright-bgl"),
            entries: &[samp_entry, tex_entry(1)],
        });
        let blur_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blur-bgl"),
            entries: &[
                samp_entry,
                tex_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let composite_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite-bgl"),
            entries: &[samp_entry, tex_entry(3), tex_entry(4)],
        });

        // ---- Pipelines.
        let scene_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene-layout"),
            bind_group_layouts: &[Some(&scene_bgl)],
            immediate_size: 0,
        });
        let ms = wgpu::MultisampleState {
            count: sample_count,
            mask: !0,
            alpha_to_coverage_enabled: false,
        };
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: 8,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32x2,
                offset: 0,
                shader_location: 0,
            }],
        };
        let line_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lines"),
            layout: Some(&scene_layout),
            vertex: wgpu::VertexState {
                module: &scene_shader,
                entry_point: Some("vs_main"),
                buffers: std::slice::from_ref(&instance_layout),
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &scene_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HDR_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });
        let backdrop_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("backdrop"),
            layout: Some(&scene_layout),
            vertex: wgpu::VertexState {
                module: &scene_shader,
                entry_point: Some("vs_bg"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &scene_shader,
                entry_point: Some("fs_bg"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HDR_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        let make_post = |label: &str,
                         bgl: &wgpu::BindGroupLayout,
                         entry: &str,
                         target: wgpu::TextureFormat| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &post_shader,
                    entry_point: Some("vs_fullscreen"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &post_shader,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let bright_pipeline = make_post("bright", &bright_bgl, "fs_bright", HDR_FORMAT);
        let blur_pipeline = make_post("blur", &blur_bgl, "fs_blur", HDR_FORMAT);
        let composite_pipeline = make_post("composite", &composite_bgl, "fs_composite", format);

        let scene_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene-bg"),
            layout: &scene_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: heights_buffer.as_entire_binding(),
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear-clamp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let blur_h_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blur-h"),
            size: std::mem::size_of::<BlurDir>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let blur_v_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blur-v"),
            size: std::mem::size_of::<BlurDir>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let targets = build_targets(
            &device,
            &config,
            sample_count,
            &bright_bgl,
            &blur_bgl,
            &composite_bgl,
            &sampler,
            &blur_h_buf,
            &blur_v_buf,
        );
        write_blur_dirs(&queue, &config, &blur_h_buf, &blur_v_buf);

        // ---- Text overlay.
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let glyph_cache = GlyphCache::new(&device);
        let viewport = Viewport::new(&device, &glyph_cache);
        let mut text_atlas = TextAtlas::new(&device, &queue, &glyph_cache, format);
        let text_renderer =
            TextRenderer::new(&mut text_atlas, &device, wgpu::MultisampleState::default(), None);
        let text_buffer = TextBuffer::new(&mut font_system, Metrics::new(18.0, 24.0));

        Self {
            surface,
            device,
            queue,
            config,
            sample_count,
            line_pipeline,
            backdrop_pipeline,
            bright_pipeline,
            blur_pipeline,
            composite_pipeline,
            bright_bgl,
            blur_bgl,
            composite_bgl,
            uniform_buffer,
            heights_buffer,
            instance_buffer,
            instance_count,
            scene_bind_group,
            sampler,
            blur_h_buf,
            blur_v_buf,
            targets,
            font_system,
            swash_cache,
            viewport,
            text_atlas,
            text_renderer,
            text_buffer,
            text_cache: None,
            globe_yaw: 0.0,
            globe_pitch: 0.0,
            globe_zoom: 1.0,
            bob: 0.0,
            accent_cur: [0.0; 3],
            size,
            head: 0,
        }
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.size = new_size;
        self.config.width = new_size.width;
        self.config.height = new_size.height;
        self.surface.configure(&self.device, &self.config);
        write_blur_dirs(&self.queue, &self.config, &self.blur_h_buf, &self.blur_v_buf);
        self.targets = build_targets(
            &self.device,
            &self.config,
            self.sample_count,
            &self.bright_bgl,
            &self.blur_bgl,
            &self.composite_bgl,
            &self.sampler,
            &self.blur_h_buf,
            &self.blur_v_buf,
        );
    }

    pub fn globe_rotate(&mut self, dx: f32, dy: f32) {
        self.globe_yaw += dx * 0.005;
        self.globe_pitch = (self.globe_pitch + dy * 0.005).clamp(-1.3, 1.3);
    }

    pub fn globe_zoom(&mut self, scroll: f32) {
        self.globe_zoom = (self.globe_zoom * (1.0 + scroll * 0.1)).clamp(0.4, 4.0);
    }

    pub fn globe_orbit(&self) -> (f32, f32, f32) {
        (self.globe_yaw, self.globe_pitch, self.globe_zoom)
    }

    pub fn set_globe_orbit(&mut self, yaw: f32, pitch: f32, zoom: f32) {
        self.globe_yaw = yaw;
        self.globe_pitch = pitch.clamp(-1.3, 1.3);
        self.globe_zoom = zoom.clamp(0.4, 4.0);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        time: f32,
        features: &AudioFeatures,
        accent: [f32; 3],
        spectrum: &[f32],
        globe: bool,
        now_playing: Option<&str>,
        text_alpha: f32,
    ) -> bool {
        // Newest spectrum row into the scrolling heightmap.
        let n = spectrum.len().min(COLS);
        let offset = (self.head * COLS * std::mem::size_of::<f32>()) as u64;
        self.queue
            .write_buffer(&self.heights_buffer, offset, bytemuck::cast_slice(&spectrum[..n]));
        self.head = (self.head + 1) % ROWS;

        // Eased motion: smooth the beat-driven bob and accent transitions.
        self.bob += (features.beat - self.bob) * 0.18;
        for (cur, &tgt) in self.accent_cur.iter_mut().zip(accent.iter()) {
            *cur += (tgt - *cur) * 0.12;
        }

        let aspect = self.size.width as f32 / self.size.height.max(1) as f32;
        let proj = Mat4::perspective_rh(VFOV_DEG.to_radians(), aspect, 0.1, 100.0);
        let eye = Vec3::new(0.0, 6.0 + self.bob * 0.6 + features.bass * 0.5, -7.0);
        let target = Vec3::new(0.0, 0.8, DEPTH * 0.45);
        let dir = (target - eye).normalize();
        let fill_aspect = aspect.min(MAX_FILL_ASPECT);
        let tan_h = fill_aspect * (VFOV_DEG.to_radians() * 0.5).tan();
        let z_front = (Vec3::ZERO - eye).dot(dir);
        let z_back = (Vec3::new(0.0, 0.0, DEPTH) - eye).dot(dir);
        let width_front = 2.0 * z_front * tan_h * WIDTH_MARGIN;
        let width_back = 2.0 * z_back * tan_h * WIDTH_MARGIN;

        let globe_dist = GLOBE_EYE_DIST / self.globe_zoom;
        let view_proj = if globe {
            let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, -globe_dist), Vec3::ZERO, Vec3::Y);
            let model = Mat4::from_rotation_x(GLOBE_TILT + self.globe_pitch)
                * Mat4::from_rotation_y(time * GLOBE_SPIN_SPEED + self.globe_yaw);
            proj * view * model
        } else {
            proj * Mat4::look_at_rh(eye, target, Vec3::Y)
        };
        let fade_near = globe_dist - 0.3;
        let fade_far = globe_dist + 1.6;

        let uniforms = Uniforms {
            view_proj: view_proj.to_cols_array_2d(),
            accent: [self.accent_cur[0], self.accent_cur[1], self.accent_cur[2], features.level],
            grid: [COLS as f32, ROWS as f32, self.head as f32, features.beat],
            shape: [DEPTH, width_front, width_back, HEIGHT_SCALE],
            misc: [time, if globe { 1.0 } else { 0.0 }, fade_near, fade_far],
            res: [
                self.size.width as f32,
                self.size.height as f32,
                LINE_THICKNESS_PX,
                0.0,
            ],
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        self.prepare_text(now_playing, text_alpha);

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let surface_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("encoder"),
            });

        // 1. Scene → HDR (MSAA resolved): backdrop, then thick line quads.
        {
            let (view, resolve) = match &self.targets.scene_msaa {
                Some(msaa) => (msaa, Some(&self.targets.scene)),
                None => (&self.targets.scene, None),
            };
            let mut pass = begin_color(&mut encoder, "scene", view, resolve, true);
            pass.set_bind_group(0, &self.scene_bind_group, &[]);
            pass.set_pipeline(&self.backdrop_pipeline);
            pass.draw(0..3, 0..1);
            pass.set_pipeline(&self.line_pipeline);
            pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
            pass.draw(0..6, 0..self.instance_count);
        }
        // 2. Bright-pass → bloom_a (half-res).
        {
            let mut pass = begin_color(&mut encoder, "bright", &self.targets.bloom_a, None, false);
            pass.set_pipeline(&self.bright_pipeline);
            pass.set_bind_group(0, &self.targets.bright_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        // 3. Blur H → bloom_b.
        {
            let mut pass = begin_color(&mut encoder, "blur-h", &self.targets.bloom_b, None, false);
            pass.set_pipeline(&self.blur_pipeline);
            pass.set_bind_group(0, &self.targets.blur_h_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        // 4. Blur V → bloom_a.
        {
            let mut pass = begin_color(&mut encoder, "blur-v", &self.targets.bloom_a, None, false);
            pass.set_pipeline(&self.blur_pipeline);
            pass.set_bind_group(0, &self.targets.blur_v_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        // 5. Composite → surface, then text on top.
        {
            let mut pass = begin_color(&mut encoder, "composite", &surface_view, None, false);
            pass.set_pipeline(&self.composite_pipeline);
            pass.set_bind_group(0, &self.targets.composite_bg, &[]);
            pass.draw(0..3, 0..1);
            if text_alpha > 0.01 && now_playing.is_some() {
                let _ = self
                    .text_renderer
                    .render(&self.text_atlas, &self.viewport, &mut pass);
            }
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.text_atlas.trim();
        true
    }

    fn prepare_text(&mut self, now_playing: Option<&str>, text_alpha: f32) {
        if text_alpha <= 0.01 || now_playing.is_none() {
            return;
        }
        let text = now_playing.unwrap();
        if self.text_cache.as_deref() != Some(text) {
            self.text_buffer.set_size(
                &mut self.font_system,
                Some(self.config.width as f32),
                Some(self.config.height as f32),
            );
            self.text_buffer.set_text(
                &mut self.font_system,
                text,
                &Attrs::new().family(Family::Monospace),
                Shaping::Advanced,
                None,
            );
            for line in self.text_buffer.lines.iter_mut() {
                line.set_align(Some(glyphon::cosmic_text::Align::Center));
            }
            self.text_buffer
                .shape_until_scroll(&mut self.font_system, false);
            self.text_cache = Some(text.to_string());
        }
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );
        let a = (text_alpha.clamp(0.0, 1.0) * 255.0) as u8;
        let color = TextColor::rgba(
            (self.accent_cur[0].clamp(0.0, 1.0) * 255.0) as u8,
            (self.accent_cur[1].clamp(0.0, 1.0) * 255.0) as u8,
            (self.accent_cur[2].clamp(0.0, 1.0) * 255.0) as u8,
            a,
        );
        let _ = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.text_atlas,
            &self.viewport,
            [TextArea {
                buffer: &self.text_buffer,
                left: 0.0,
                top: 40.0,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: self.config.width as i32,
                    bottom: self.config.height as i32,
                },
                default_color: color,
                custom_glyphs: &[],
            }],
            &mut self.swash_cache,
        );
    }
}

/// Begin a single-color-attachment pass, clearing to black.
fn begin_color<'a>(
    encoder: &'a mut wgpu::CommandEncoder,
    label: &str,
    view: &'a wgpu::TextureView,
    resolve: Option<&'a wgpu::TextureView>,
    _scene: bool,
) -> wgpu::RenderPass<'a> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: resolve,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

fn color_target(
    device: &wgpu::Device,
    w: u32,
    h: u32,
    sample_count: u32,
    label: &str,
) -> wgpu::TextureView {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count,
        dimension: wgpu::TextureDimension::D2,
        format: HDR_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

#[allow(clippy::too_many_arguments)]
fn build_targets(
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
    sample_count: u32,
    bright_bgl: &wgpu::BindGroupLayout,
    blur_bgl: &wgpu::BindGroupLayout,
    composite_bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    blur_h_buf: &wgpu::Buffer,
    blur_v_buf: &wgpu::Buffer,
) -> Targets {
    let (w, h) = (config.width, config.height);
    let scene_msaa = (sample_count > 1).then(|| color_target(device, w, h, sample_count, "scene-msaa"));
    let scene = color_target(device, w, h, 1, "scene");
    let (bw, bh) = ((w / 2).max(1), (h / 2).max(1));
    let bloom_a = color_target(device, bw, bh, 1, "bloom-a");
    let bloom_b = color_target(device, bw, bh, 1, "bloom-b");

    let bright_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bright-bg"),
        layout: bright_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&scene),
            },
        ],
    });
    let blur_bg = |label: &str, input: &wgpu::TextureView, dir: &wgpu::Buffer| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: blur_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(input),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: dir.as_entire_binding(),
                },
            ],
        })
    };
    let blur_h_bg = blur_bg("blur-h-bg", &bloom_a, blur_h_buf);
    let blur_v_bg = blur_bg("blur-v-bg", &bloom_b, blur_v_buf);

    let composite_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("composite-bg"),
        layout: composite_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&scene),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(&bloom_a),
            },
        ],
    });

    Targets {
        scene_msaa,
        scene,
        bloom_a,
        bloom_b,
        bright_bg,
        blur_h_bg,
        blur_v_bg,
        composite_bg,
    }
}

fn write_blur_dirs(
    queue: &wgpu::Queue,
    config: &wgpu::SurfaceConfiguration,
    blur_h_buf: &wgpu::Buffer,
    blur_v_buf: &wgpu::Buffer,
) {
    let bw = (config.width / 2).max(1) as f32;
    let bh = (config.height / 2).max(1) as f32;
    let h = BlurDir {
        texel: [1.0 / bw, 0.0, 0.0, 0.0],
    };
    let v = BlurDir {
        texel: [0.0, 1.0 / bh, 0.0, 0.0],
    };
    queue.write_buffer(blur_h_buf, 0, bytemuck::bytes_of(&h));
    queue.write_buffer(blur_v_buf, 0, bytemuck::bytes_of(&v));
}
