//! Fullscreen audio visualizer for Linux/PipeWire.
//!
//! Captures audio (system output by default, or a mic), runs an FFT to extract
//! bass/mid/treble + beat, and drives a reactive fullscreen shader via wgpu.
//!
//! Keys:  F / F11 = fullscreen   Tab = cycle audio source
//!        C = cycle accent color   1-6 = pick accent directly   Esc = quit
//! Flags: --list-sources   --mic (capture default input instead of output)
//!        --accent <name> (neon-red|amber|green|cyan|violet|white)

mod audio;
mod dsp;
mod nowplaying;
mod render;

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use audio::AudioEngine;
use dsp::{Analyzer, AudioFeatures};
use nowplaying::NowPlaying;
use render::Renderer;

// Now-playing overlay timing (seconds).
const NP_FADE_IN: f32 = 0.5;
const NP_HOLD: f32 = 5.0;
const NP_FADE_OUT: f32 = 1.2;
/// How often the overlay re-appears on its own when the track hasn't changed.
const NP_CYCLE: f32 = 30.0;

/// How the now-playing overlay behaves. Cycled with Space.
#[derive(Clone, Copy, PartialEq)]
enum OverlayMode {
    /// Fades in on track change and every `NP_CYCLE` seconds (default).
    Occasional,
    /// Always visible.
    Permanent,
    /// Never shown.
    Never,
}

impl OverlayMode {
    fn next(self) -> Self {
        match self {
            OverlayMode::Occasional => OverlayMode::Permanent,
            OverlayMode::Permanent => OverlayMode::Never,
            OverlayMode::Never => OverlayMode::Occasional,
        }
    }
    fn label(self) -> &'static str {
        match self {
            OverlayMode::Occasional => "occasional",
            OverlayMode::Permanent => "permanent",
            OverlayMode::Never => "never",
        }
    }
}

/// Fade envelope for the overlay: in → hold → out → hidden.
fn np_alpha(t: f32) -> f32 {
    if t < NP_FADE_IN {
        t / NP_FADE_IN
    } else if t < NP_FADE_IN + NP_HOLD {
        1.0
    } else if t < NP_FADE_IN + NP_HOLD + NP_FADE_OUT {
        1.0 - (t - NP_FADE_IN - NP_HOLD) / NP_FADE_OUT
    } else {
        0.0
    }
}

/// Accent color palette. The scene is grayscale; one of these tints the bass /
/// beat. Default (index 0) is a neon red with a slight rose lean.
const ACCENTS: &[(&str, [f32; 3])] = &[
    ("neon-red", [1.0, 0.10, 0.22]),
    ("amber", [1.0, 0.45, 0.05]),
    ("green", [0.25, 1.0, 0.30]),
    ("cyan", [0.0, 0.85, 1.0]),
    ("violet", [0.55, 0.20, 1.0]),
    ("white", [1.0, 1.0, 1.0]),
];

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    engine: AudioEngine,
    analyzer: Analyzer,
    last_rate: u32,
    samples: Vec<f32>,
    features: AudioFeatures,
    start: Instant,
    fullscreen: bool,
    sel: usize,
    accent: usize,
    // Now-playing overlay.
    nowplaying: NowPlaying,
    track: Option<String>,
    track_shown_at: Instant,
    overlay_mode: OverlayMode,
}

impl App {
    fn new(engine: AudioEngine, accent: usize) -> Self {
        let rate = engine.sample_rate();
        Self {
            window: None,
            renderer: None,
            analyzer: Analyzer::new(rate as f32),
            last_rate: rate,
            engine,
            samples: Vec::new(),
            features: AudioFeatures::default(),
            start: Instant::now(),
            fullscreen: false,
            sel: 0,
            accent,
            nowplaying: NowPlaying::start(),
            track: None,
            track_shown_at: Instant::now(),
            overlay_mode: OverlayMode::Occasional,
        }
    }

    fn cycle_overlay_mode(&mut self) {
        self.overlay_mode = self.overlay_mode.next();
        self.track_shown_at = Instant::now(); // re-trigger the fade in Occasional mode
        println!("→ now-playing overlay: {}", self.overlay_mode.label());
    }

    fn set_accent(&mut self, idx: usize) {
        self.accent = idx % ACCENTS.len();
        println!("→ accent: {}", ACCENTS[self.accent].0);
    }

    fn toggle_fullscreen(&mut self) {
        if let Some(window) = &self.window {
            self.fullscreen = !self.fullscreen;
            window.set_fullscreen(if self.fullscreen {
                Some(Fullscreen::Borderless(None))
            } else {
                None
            });
        }
    }

    fn cycle_source(&mut self) {
        let sources = self.engine.sources();
        if sources.is_empty() {
            println!("No audio sources discovered yet.");
            return;
        }
        self.sel = (self.sel + 1) % sources.len();
        let src = &sources[self.sel];
        println!(
            "→ capturing [{}] {} ({})",
            src.id,
            src.name,
            if src.is_sink { "output/monitor" } else { "input" }
        );
        self.engine.connect_to(src);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_title(
                    "Audio Visualizer — F: fullscreen, Tab: source, Esc: quit",
                ))
                .expect("create window"),
        );
        self.renderer = Some(Renderer::new(window.clone()));
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size);
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => match logical_key {
                Key::Named(NamedKey::Escape) => event_loop.exit(),
                Key::Named(NamedKey::Tab) => self.cycle_source(),
                Key::Named(NamedKey::Space) => self.cycle_overlay_mode(),
                Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
                Key::Character(ref c) if c.eq_ignore_ascii_case("f") => self.toggle_fullscreen(),
                Key::Character(ref c) if c.eq_ignore_ascii_case("c") => {
                    self.set_accent(self.accent + 1)
                }
                Key::Character(ref c) => {
                    if let Some(d) = c.chars().next().and_then(|ch| ch.to_digit(10))
                        && d >= 1
                        && (d as usize) <= ACCENTS.len()
                    {
                        self.set_accent(d as usize - 1);
                    }
                }
                _ => {}
            },
            WindowEvent::RedrawRequested => {
                // Rebuild the analyzer if the negotiated rate changed.
                let rate = self.engine.sample_rate();
                if rate != self.last_rate {
                    self.analyzer = Analyzer::new(rate as f32);
                    self.last_rate = rate;
                }

                self.engine.drain_samples(&mut self.samples);
                self.analyzer.feed(&self.samples);
                self.features = self.analyzer.analyze();

                // Now-playing overlay: show on track change, then periodically.
                let np = self.nowplaying.current();
                if np != self.track {
                    self.track = np;
                    if self.track.is_some() {
                        self.track_shown_at = Instant::now();
                    }
                } else if self.track.is_some()
                    && self.overlay_mode == OverlayMode::Occasional
                    && self.track_shown_at.elapsed().as_secs_f32() >= NP_CYCLE
                {
                    self.track_shown_at = Instant::now();
                }
                let text_alpha = match self.overlay_mode {
                    _ if self.track.is_none() => 0.0,
                    OverlayMode::Never => 0.0,
                    OverlayMode::Permanent => 1.0,
                    OverlayMode::Occasional => np_alpha(self.track_shown_at.elapsed().as_secs_f32()),
                };

                let accent = ACCENTS[self.accent].1;
                if let Some(r) = &mut self.renderer {
                    let t = self.start.elapsed().as_secs_f32();
                    let spectrum = self.analyzer.spectrum();
                    let track = self.track.as_deref();
                    let presented = r.render(t, &self.features, accent, spectrum, track, text_alpha);
                    // If the surface couldn't present (occluded/minimized), there
                    // is no vsync block pacing us — back off to avoid a busy loop.
                    if !presented {
                        std::thread::sleep(std::time::Duration::from_millis(8));
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list-sources") {
        println!("Discovering audio sources…\n");
        let sources = audio::enumerate_sources();
        if sources.is_empty() {
            println!("(none found — is PipeWire running?)");
        }
        for s in &sources {
            println!(
                "  id={:<5} {:<14} {}",
                s.id,
                if s.is_sink { "output/monitor" } else { "input" },
                s.name
            );
        }
        return;
    }

    // Resolve the starting accent from --accent <name> (default: neon-red).
    let accent = args
        .iter()
        .position(|a| a == "--accent")
        .and_then(|i| args.get(i + 1))
        .and_then(|name| ACCENTS.iter().position(|(n, _)| n == name))
        .unwrap_or(0);

    // Default to capturing system output; --mic captures the default input.
    let capture_system_output = !args.iter().any(|a| a == "--mic");
    println!(
        "Capturing {}.  Accent: {}",
        if capture_system_output {
            "system output (default sink monitor)"
        } else {
            "default microphone/input"
        },
        ACCENTS[accent].0
    );
    println!(
        "Keys: F/F11 fullscreen · Tab source · C / 1-6 accent · Space song overlay · Esc quit"
    );

    let engine = AudioEngine::start(capture_system_output);

    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::new(engine, accent);
    event_loop.run_app(&mut app).expect("run app");
}
