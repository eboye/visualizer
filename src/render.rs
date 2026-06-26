//! wgpu rendering: a 3D scrolling wireframe terrain.
//!
//! Frequency spans the width (bass → treble), magnitude is height, and time
//! scrolls into the distance. The grid geometry is implicit (computed from
//! `vertex_index` in the shader); per-vertex height comes from a scrolling
//! heightmap storage buffer fed one row per frame.

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

/// Terrain grid: columns across the width, rows of depth/history.
const COLS: usize = SPECTRUM_COLS;
const ROWS: usize = 96;

/// World-space depth of the terrain and how tall peaks get. The width is
/// computed per frame from the camera frustum so the grid always fills the
/// viewport (see `render`).
const DEPTH: f32 = 24.0;
const HEIGHT_SCALE: f32 = 5.0;
const VFOV_DEG: f32 = 55.0;
/// Slight horizontal overshoot so the terrain reaches just past the viewport
/// edges (the shader fades the last sliver to zero — see the edge taper).
const WIDTH_MARGIN: f32 = 1.02;
/// Cap the aspect used for terrain width. Screens up to this aspect fill edge to
/// edge; wider (ultrawide) screens keep the terrain centered at this width with
/// faded/empty sides instead of stretching everything across the viewport.
const MAX_FILL_ASPECT: f32 = 1.85;

/// Globe mode ("G" key): camera distance, spin speed (radians/sec), and axial
/// tilt. The tilt leans the "newest data" pole (where fresh, spiky spectrum
/// piles up) up and away from the camera so it isn't staring at the viewer.
const GLOBE_EYE_DIST: f32 = 4.9;
const GLOBE_SPIN_SPEED: f32 = 0.3;
const GLOBE_TILT: f32 = 1.0;

/// Mirrors the `Uniforms` struct in `shaders/terrain.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    accent: [f32; 4], // rgb, level
    grid: [f32; 4],   // cols, rows, head, beat
    shape: [f32; 4],  // depth, width_front, width_back, height_scale
    misc: [f32; 4],   // time, _, _, _
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    heights_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    index_count: u32,
    bind_group: wgpu::BindGroup,
    pub size: winit::dpi::PhysicalSize<u32>,
    head: usize, // ring write position into the heightmap
    // Globe-mode mouse orbit/zoom state.
    globe_yaw: f32,
    globe_pitch: f32,
    globe_zoom: f32,
    // Now-playing text overlay (glyphon).
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    text_buffer: TextBuffer,
    text_cache: Option<String>, // last shaped string, to avoid re-shaping each frame
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let mut idesc = wgpu::InstanceDescriptor::new_without_display_handle();
        idesc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(idesc);

        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");

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

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terrain"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/terrain.wgsl").into()),
        });

        // --- Line index buffer (built once): cross-hatch of across + depth lines.
        let mut indices: Vec<u32> = Vec::with_capacity(COLS * ROWS * 4);
        for d in 0..ROWS {
            for col in 0..COLS {
                let i = (d * COLS + col) as u32;
                if col + 1 < COLS {
                    indices.push(i);
                    indices.push(i + 1);
                }
                if d + 1 < ROWS {
                    indices.push(i);
                    indices.push(i + COLS as u32);
                }
            }
        }
        let index_count = indices.len() as u32;
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("indices"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        // --- Heightmap storage buffer, zero-initialized.
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

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("bgl"),
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

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg"),
            layout: &bind_group_layout,
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // --- Text overlay (glyphon). The glyph cache is only needed during setup.
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
            pipeline,
            uniform_buffer,
            heights_buffer,
            index_buffer,
            index_count,
            bind_group,
            size,
            head: 0,
            globe_yaw: 0.0,
            globe_pitch: 0.0,
            globe_zoom: 1.0,
            font_system,
            swash_cache,
            viewport,
            text_atlas,
            text_renderer,
            text_buffer,
            text_cache: None,
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
    }

    /// Globe mode: orbit by a mouse drag delta (pixels).
    pub fn globe_rotate(&mut self, dx: f32, dy: f32) {
        self.globe_yaw += dx * 0.005;
        self.globe_pitch = (self.globe_pitch + dy * 0.005).clamp(-1.3, 1.3);
    }

    /// Globe mode: zoom by a scroll amount (positive = closer).
    pub fn globe_zoom(&mut self, scroll: f32) {
        self.globe_zoom = (self.globe_zoom * (1.0 + scroll * 0.1)).clamp(0.4, 4.0);
    }

    /// Camera: raised behind the front edge, looking down the terrain. A subtle
    /// beat/bass-driven bob adds life.
    fn camera(&self, beat: f32, bass: f32) -> (Vec3, Vec3) {
        let eye = Vec3::new(0.0, 6.0 + beat * 0.6 + bass * 0.5, -7.0);
        let target = Vec3::new(0.0, 0.8, DEPTH * 0.45);
        (eye, target)
    }

    /// Push the newest spectrum row, update uniforms, draw. Returns `true` if a
    /// frame was presented (see the busy-loop backoff in `main.rs`).
    // A render entry point with several distinct per-frame inputs; grouping them
    // into a struct would add indirection without real benefit.
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
        // Write the newest row at the current ring position, then advance so the
        // shader's `head - 1` points at it (the front edge).
        let n = spectrum.len().min(COLS);
        let offset = (self.head * COLS * std::mem::size_of::<f32>()) as u64;
        self.queue
            .write_buffer(&self.heights_buffer, offset, bytemuck::cast_slice(&spectrum[..n]));
        self.head = (self.head + 1) % ROWS;

        let aspect = self.size.width as f32 / self.size.height.max(1) as f32;
        let proj = Mat4::perspective_rh(VFOV_DEG.to_radians(), aspect, 0.1, 100.0);

        // Terrain width is frustum-matched per frame so the grid fills the
        // viewport at every depth (capped at MAX_FILL_ASPECT for ultrawide).
        // Unused in globe mode but cheap to compute.
        let (eye, target) = self.camera(features.beat, features.bass);
        let dir = (target - eye).normalize();
        let fill_aspect = aspect.min(MAX_FILL_ASPECT);
        let tan_h = fill_aspect * (VFOV_DEG.to_radians() * 0.5).tan();
        let z_front = (Vec3::ZERO - eye).dot(dir);
        let z_back = (Vec3::new(0.0, 0.0, DEPTH) - eye).dot(dir);
        let width_front = 2.0 * z_front * tan_h * WIDTH_MARGIN;
        let width_back = 2.0 * z_back * tan_h * WIDTH_MARGIN;

        // Mouse-controlled orbit distance (scroll zoom).
        let globe_dist = GLOBE_EYE_DIST / self.globe_zoom;
        let view_proj = if globe {
            // Spinning, tilted globe at the origin; mouse drag adds yaw/pitch.
            let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, -globe_dist), Vec3::ZERO, Vec3::Y);
            let model = Mat4::from_rotation_x(GLOBE_TILT + self.globe_pitch)
                * Mat4::from_rotation_y(time * GLOBE_SPIN_SPEED + self.globe_yaw);
            proj * view * model
        } else {
            proj * Mat4::look_at_rh(eye, target, Vec3::Y)
        };

        // Globe far-side fade bounds (camera-space distance): the back hemisphere
        // fades toward transparent so it doesn't clutter the near side.
        let fade_near = globe_dist - 0.3;
        let fade_far = globe_dist + 1.6;

        let uniforms = Uniforms {
            view_proj: view_proj.to_cols_array_2d(),
            accent: [accent[0], accent[1], accent[2], features.level],
            grid: [COLS as f32, ROWS as f32, self.head as f32, features.beat],
            shape: [DEPTH, width_front, width_back, HEIGHT_SCALE],
            misc: [time, if globe { 1.0 } else { 0.0 }, fade_near, fade_far],
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        // Prepare the now-playing overlay (re-shape only when the text changes).
        let show_text = text_alpha > 0.01 && now_playing.is_some();
        if show_text {
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
                // Center each line across the full buffer width.
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
                (accent[0] * 255.0) as u8,
                (accent[1] * 255.0) as u8,
                (accent[2] * 255.0) as u8,
                a,
            );
            // Centered (buffer width = screen, lines center-aligned), near the top.
            let left = 0.0;
            let top = 40.0;
            let _ = self.text_renderer.prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.text_atlas,
                &self.viewport,
                [TextArea {
                    buffer: &self.text_buffer,
                    left,
                    top,
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

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("encoder"),
            });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rpass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
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
            });
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &self.bind_group, &[]);
            rpass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            rpass.draw_indexed(0..self.index_count, 0, 0..1);

            // Overlay the now-playing text on top of the terrain.
            if show_text {
                let _ = self
                    .text_renderer
                    .render(&self.text_atlas, &self.viewport, &mut rpass);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.text_atlas.trim();
        true
    }
}
