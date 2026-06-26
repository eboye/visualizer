# Audio Visualizer

A fullscreen, Winamp/Milkdrop-style music visualizer for **Linux + PipeWire**, written
in Rust. It captures audio (system output or a microphone), runs an FFT, and renders a
**3D scrolling wireframe terrain**: frequency spans the width (bass left → treble right),
magnitude is height, and each frame adds a new row at the front so the landscape flows
into the distance.

Lines are drawn in a single **accent color** on black — brighter on peaks and beats,
dimmer with distance. The accent is selectable at runtime.

```
PipeWire capture thread ──(lock-free ring buffer)──► render thread
                                                        │
                                    DSP: window → FFT → log-spaced spectrum + beat
                                                        │
                                    spectrum row[128] + AudioFeatures { bass, level, beat }
                                                        │
                              wgpu: scrolling heightmap + camera → wireframe line mesh
```

## Requirements

- **Linux with PipeWire** running (the capture backend is PipeWire-native)
- **A Vulkan-capable GPU + driver** (wgpu selects the Vulkan backend on Linux). Verify with
  `vulkaninfo | head`.
- **Rust** (edition 2024; built/tested on rustc 1.96)
- Build-time: `clang` and the PipeWire dev headers (`libpipewire-0.3`) — `pipewire-rs` uses
  bindgen. Most desktop distros already have these.

## Build & run

```bash
cargo run --release                    # capture system output (default sink monitor)
cargo run --release -- --mic           # capture the default microphone/input instead
cargo run --release -- --list-sources  # print all capture targets (id, type, name) and exit
cargo run --release -- --accent cyan   # start with a specific accent color
```

`cargo build --release` produces the optimized binary at `target/release/visualizer`.

## Controls

| Key | Action |
|-----|--------|
| `F` / `F11` | Toggle fullscreen |
| `Tab` | Cycle through audio sources (mics + output monitors) |
| `C` | Cycle accent color |
| `1`–`6` | Pick accent color directly |
| `Esc` | Quit |

## Audio source selection

- **Default** captures **system output** — the default sink's *monitor*, so it reacts to
  whatever is playing (Spotify, browser, any app).
- `--mic` captures the **default input** instead.
- At runtime, **`Tab`** cycles through every discovered source. Run `--list-sources` to see
  them; "output/monitor" entries are system-output captures, "input" entries are mics/line-ins.

## Customization

Three independent layers, easiest → deepest:

### 1. Accent colors — `ACCENTS` table in `src/main.rs`
A `&[(&str, [f32; 3])]` list of `(name, RGB)`. Default (index 0) is `neon-red`
`[1.0, 0.10, 0.22]`. Add/edit entries freely; names become valid `--accent` values and the
first six map to keys `1`–`6`.

### 2. Terrain shape & reactivity — top of `src/dsp.rs`
- `FFT_SIZE` (2048) — window size. Power of two; 2048 @ 48 kHz ≈ 43 ms.
- `SPECTRUM_COLS` (128) — terrain width / number of frequency columns.
- `SPECTRUM_LO_HZ` / `SPECTRUM_HI_HZ` (30 Hz – 16 kHz) — frequency span across the width
  (log-spaced). The per-column `tilt` gain in `analyze()` boosts treble; raise/lower if the
  high end looks weak or clipped.
- Per-column decay (`* 0.80`) — ridge smoothing; lower = snappier, higher = smoother.
- Beat threshold `instant > avg * 1.4` and the `avg > 0.02` noise gate — tune sensitivity.

### 3. Terrain size & camera — top of `src/render.rs`
- `COLS` / `ROWS` (128 × 96) — grid resolution (width × depth/history). `COLS` must equal
  `SPECTRUM_COLS`.
- `WIDTH` / `DEPTH` / `HEIGHT_SCALE` — world dimensions and peak height.
- Camera eye/target/fov in `view_proj()` — viewpoint and the beat/bass-driven bob.

### 4. The look — `src/shaders/terrain.wgsl`
WGSL vertex + fragment shader; iterate without recompiling the engine logic. The vertex stage
places the grid and samples height; the fragment stage colors lines in the accent, brighter on
peaks/beats and faded with distance.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | winit event loop, keybinds, CLI flags, accent selector, frame loop |
| `src/audio.rs` | PipeWire capture thread, node enumeration, runtime source switching, ring buffer |
| `src/dsp.rs` | Hann window → FFT → log-spaced spectrum + beat detection (`AudioFeatures`) |
| `src/render.rs` | wgpu 3D terrain renderer: camera, line mesh, scrolling heightmap (`Renderer`) |
| `src/shaders/terrain.wgsl` | 3D vertex (grid + heightmap) + fragment (accent lines) shader |

## Tech stack

`wgpu` (Vulkan backend) · `winit` · `pipewire-rs` · `rustfft` · `ringbuf` · `glam` · `bytemuck` ·
`pollster`. See [`CLAUDE.md`](./CLAUDE.md) for architecture details and version-specific notes.
