# CLAUDE.md

Guidance for AI agents working in this repo. User-facing usage lives in [`README.md`](./README.md).

## What this is

A fullscreen Linux/PipeWire audio visualizer in Rust. Captures audio → FFT → log-spaced
spectrum + beat → **3D scrolling wireframe terrain** (wgpu). Frequency spans the width
(bass→treble), magnitude is height, time scrolls into the distance. Lines drawn in one
selectable accent color on black. A now-playing **artist/title overlay** (glyphon text) fades
in periodically, read from the active media player over **MPRIS/D-Bus** (`nowplaying.rs`).

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
   consumer → `Analyzer::feed` → `Analyzer::analyze()` (`dsp.rs`) → `Renderer::render`. The
   renderer writes the newest `Analyzer::spectrum()` row into a scrolling heightmap storage
   buffer, updates the camera/uniforms, and draws the implicit grid as a `LineList` wireframe.

### Terrain rendering (`render.rs` + `terrain.wgsl`)

- **No vertex buffer.** The grid is `COLS×ROWS` implicit vertices; the vertex shader derives
  `(x, z)` from `@builtin(vertex_index)` and reads `y` from a read-only **storage buffer**
  `heights[]` (binding 1, vertex stage). A `LineList` **index buffer** (across + depth segments)
  is built once on the CPU for the cross-hatch; draw via `draw_indexed`.
- **Scrolling = a ring, not a memmove.** Each frame writes only the newest spectrum row at
  `head*COLS*4` in `heights`, then `head = (head+1) % ROWS`. The shader maps logical depth `d`
  (0 = front) to physical row `(head + ROWS - 1 - d) % ROWS`. `cols/rows/head` are passed in the
  uniform, so WGSL has **no hardcoded grid size** — only the storage-buffer length depends on
  `COLS*ROWS` at creation.
- **`COLS` must equal `dsp::SPECTRUM_COLS`** (the spectrum row fills one terrain row).
- Camera is `glam`: `perspective_rh (wgpu 0..1 depth, NOT _gl) * look_at_rh`, recomputed per
  frame for free resize/aspect + a beat/bass bob. No depth buffer (x-ray wireframe; distance
  fade in the fragment shader gives the depth cue).
- Terrain **width is frustum-matched per frame** (`2·view-depth·tan(hfov/2)·margin`, front &
  back interpolated by depth) so it fills the viewport at every depth. Aspect is capped at
  `MAX_FILL_ASPECT` so ultrawide screens keep it centered instead of stretching. Shader tapers
  the left/right columns to zero (`edge`) for a borderless, infinite feel.
- **Globe mode** (`G` key, `misc.y` flag): same grid + heightmap + index buffer, but the
  vertex shader branches to wrap the grid onto a sphere (longitude = col/frequency, latitude =
  row/time, height = radial bump) and `render` builds a spinning `proj·view·model` matrix.
  Terrain-only effects (frustum width, edge taper, distance fade) are skipped (x-ray sphere).
  No extra pipeline/buffers — just the mode flag and per-mode `view_proj`.

### Now-playing overlay (`nowplaying.rs` + glyphon in `render.rs`)

- `NowPlaying::start()` spawns a thread polling **MPRIS** (`mpris` crate, libdbus) every ~1.5 s
  for the active player's artist/title → `Arc<Mutex<Option<String>>>`. Read cheaply via
  `current()`. Fully optional — if D-Bus/MPRIS is absent the thread logs once and exits.
- Text is rendered with **glyphon 0.11** (which pins `wgpu 29.0.3` — matches ours, no conflict).
  `Renderer` owns the `FontSystem`/`SwashCache`/`Viewport`/`TextAtlas`/`TextRenderer`/`Buffer`.
  Re-shape only when the string changes (`text_cache`); `prepare` before the pass, `render`
  inside the pass after the terrain, `atlas.trim()` after present.
- `main.rs` drives timing: show on track change + every `NP_CYCLE`s; `np_alpha()` is the
  fade-in/hold/fade-out envelope; alpha → text color alpha (skipped entirely when ~0).
  `Space` cycles `OverlayMode` (occasional → permanent → never). Text is monospace, centered
  (line `Align::Center` + buffer width = screen), near the top.

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
- The shader (`terrain.wgsl`) `Uniforms` struct layout must stay in sync with the `Uniforms`
  `#[repr(C)]` struct in `render.rs`: `mat4x4 view_proj`, then three `vec4`s — `accent` (rgb +
  level in `.w`), `grid` (cols, rows, head, beat), `world` (width, depth, height_scale, time).
- Tunable constants surfaced for easy iteration (see README §Customization): FFT size +
  spectrum range/tilt + beat threshold in `dsp.rs`; grid `COLS`/`ROWS`, `WIDTH`/`DEPTH`/
  `HEIGHT_SCALE` + camera in `render.rs`; accent palette in `main.rs`.
