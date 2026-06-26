# CLAUDE.md

Guidance for AI agents working in this repo. User-facing usage lives in [`README.md`](./README.md).

## What this is

A fullscreen Linux/PipeWire audio visualizer in Rust (~1100 LOC). Captures audio → FFT →
bass/mid/treble + beat → reactive wgpu shader. Grayscale scene with one selectable accent color.

## Commands

```bash
cargo build                       # debug build
cargo build --release             # optimized binary → target/release/visualizer
cargo clippy                      # lint (keep this clean — currently zero warnings)
cargo run --release -- --list-sources   # non-GUI smoke test: verifies PipeWire connection
```

There is no test suite. Verification is manual:
- **Non-GUI check:** `--list-sources` exercises the full PipeWire connect + registry path.
- **GUI smoke test:** the dev shell is headless, but a compositor socket exists. Run with a
  short timeout against it (exit 124 = ran clean, no panic):
  ```bash
  WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 timeout 3 cargo run --quiet
  ```
  This confirms window creation + wgpu/Vulkan init + the render loop. It does **not** verify
  the visuals react correctly — that needs a human watching with audio playing.

## Architecture & data flow

Two threads, joined by one lock-free ring buffer:

1. **PipeWire thread** (`audio.rs`, `run_audio_loop`): owns the PW main loop. A registry
   listener keeps `Arc<Mutex<Vec<SourceInfo>>>` of capture targets live. A capture stream's
   `process` callback downmixes samples to mono and `push_slice`s them into the ring producer.
   Runtime re-targeting comes in over a `pw::channel` (`Command::Connect`) which rebuilds the
   stream. Negotiated sample rate is published via `Arc<AtomicU32>`.
2. **Render thread** (`main.rs`): winit event loop. Each `RedrawRequested`, drains the ring
   consumer → `Analyzer::feed` → `Analyzer::analyze()` (`dsp.rs`) → `Renderer::render` writes
   `AudioFeatures` + accent into a uniform buffer and draws a fullscreen triangle.

### Critical invariants — do not break

- **The capture stream uses NO `RT_PROCESS` flag** (`StreamFlags::AUTOCONNECT | MAP_BUFFERS`
  only). This is deliberate: it makes `process` run on the PW loop thread, so the
  `Rc<RefCell<HeapProd>>` producer and the channel callback are all single-threaded and safe.
  Adding `RT_PROCESS` would move `process` to a separate realtime thread and make the `Rc`
  unsound. If you ever need RT, switch the producer to a `Send`+`Sync` handoff.
- The **ring buffer is the only cross-thread boundary** for samples. Keep PW-side state (`Rc`,
  listeners, streams) on the PW thread; keep the consumer on the render thread.
- `enumerate_sources()` must **clone out** `found.borrow().clone()` — the registry listener
  still holds the `Rc`, so `Rc::try_unwrap` would fail and silently return empty (this was a bug).

## Version-specific gotchas (these cost real time to rediscover)

- **`CARGO_HOME=/home/eboye/.local/share/cargo`**, not `~/.cargo`. Extracted crate sources for
  reading exact signatures: `$CARGO_HOME/registry/src/index.crates.io-*/<crate>-<ver>/`. The
  `pipewire-0.10.0/examples/` dir (audio-capture.rs, streams.rs, pw-mon.rs, roundtrip.rs) is the
  canonical API reference — read it before guessing.
- **wgpu 29** has breaking changes vs older docs/training:
  - `Instance::new(desc)` takes the descriptor **by value**; `InstanceDescriptor` has **no
    `Default`** — use `InstanceDescriptor::new_without_display_handle()`.
  - `get_current_texture()` returns an **enum `CurrentSurfaceTexture`** (`Success`/`Suboptimal`/
    `Timeout`/`Occluded`/`Outdated`/`Lost`/`Validation`), **not** `Result<_, SurfaceError>`.
  - `PipelineLayoutDescriptor`: no `push_constant_ranges`; has `immediate_size: u32`.
    `bind_group_layouts` is `&[Option<&BindGroupLayout>]`.
  - `RenderPipelineDescriptor` / `RenderPassDescriptor`: `multiview` → `multiview_mask`.
  - `RenderPassColorAttachment` needs `depth_slice: None`.
  - Pipeline `entry_point` is `Option<&str>` (`Some("vs_main")`).
- **`rustfft::Fft::process()` allocates a scratch `Vec` on every call.** Always use
  `process_with_scratch` with a persistent buffer of `get_inplace_scratch_len()` in hot paths.
  `dsp.rs` does this.
- **pipewire-rs 0.10**: use the `*Rc` owning variants (`MainLoopRc`, `ContextRc`, `connect_rc`,
  `StreamRc`, `get_registry_rc`). `StreamRc::new(core: CoreRc, ...)` takes `CoreRc` by value
  (clone it) and derefs to `Stream`; `StreamListener<D>` owns its state (no lifetime), so a
  `(StreamRc, StreamListener)` pair can be stored together and swapped on re-target.
- **Edition 2024** is set — let-chains (`if let Some(x) = .. && cond`) are available and clippy
  prefers them.

## Conventions

- Keep `cargo clippy` clean.
- The shader (`fullscreen.wgsl`) `Uniforms` struct layout must stay in sync with the `Uniforms`
  `#[repr(C)]` struct in `render.rs` (three `vec4`s: time/res/beat, bands, accent).
- Tunable constants (FFT size, band ranges, gains, decay, beat threshold, accent palette) are
  intentionally surfaced at the top of `dsp.rs` / `main.rs` for easy iteration — see README §Customization.
