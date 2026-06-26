# Audio Visualizer

A fullscreen, Winamp/Milkdrop-style music visualizer for **Linux + PipeWire**, written
in Rust. It captures audio (system output or a microphone), runs an FFT, and renders a
**3D scrolling wireframe terrain**: frequency spans the width (bass left → treble right),
magnitude is height, and each frame adds a new row at the front so the landscape flows
into the distance.

Lines are drawn in a single **accent color** as thick, soft-edged strokes over a graded
backdrop, with **neon bloom**, ACES tone-mapping, and **MSAA** (4×/2× where the GPU supports
it). The accent is selectable at runtime and eases smoothly when changed. The current **song (artist —
title)** fades in periodically, read from the active media player over MPRIS/D-Bus.

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
  bindgen. Also `libdbus-1` (for the MPRIS now-playing reader). Most desktop distros have these.
- Optional: a running **D-Bus session** + an MPRIS-capable player for the now-playing overlay.
  Absent that, the overlay simply stays hidden.

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
| `G` | Toggle globe view (spectrum wrapped on a spinning sphere) |
| `Space` | Song overlay mode: occasional → permanent → never |
| `Esc` | Quit |

Press **`G`** to wrap the same spectrum onto a slowly rotating sphere — a "globe of
sound" (longitude = frequency, latitude = time, height bumps outward) — and `G` again to
return to the terrain. In globe view, **drag with the left mouse button to orbit** and
**scroll to zoom**; the far hemisphere fades out for depth.

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

### 4. The look — `src/shaders/terrain.wgsl` + `src/shaders/post.wgsl`
`terrain.wgsl` draws the graded backdrop and the thick line quads (line thickness is
`LINE_THICKNESS_PX` in `render.rs`). `post.wgsl` holds the post-processing constants —
`BLOOM_THRESHOLD`/`BLOOM_INTENSITY`, `EXPOSURE`, `SATURATION`, `CONTRAST`, `VIGNETTE` — tune
these for the overall mood. Both iterate without touching the engine code.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | winit event loop, keybinds, CLI flags, accent selector, overlay timing, frame loop |
| `src/audio.rs` | PipeWire capture thread, node enumeration, runtime source switching, ring buffer |
| `src/dsp.rs` | Hann window → FFT → log-spaced spectrum + beat detection (`AudioFeatures`) |
| `src/nowplaying.rs` | MPRIS/D-Bus thread reading the active player's artist/title |
| `src/render.rs` | wgpu multi-pass HDR renderer (scene → bloom → composite) + text (`Renderer`) |
| `src/config.rs` | persist accent / view / overlay mode / globe orbit between runs |
| `src/shaders/terrain.wgsl` | scene shader: graded backdrop + thick line-quad geometry |
| `src/shaders/post.wgsl` | post: bright-pass, Gaussian blur, composite (bloom + tone-map + grade) |

## Saved settings

The accent color, current view (terrain/globe), overlay mode, and globe orbit/zoom are saved
on exit to `~/.config/visualizer/settings.conf` (or `$XDG_CONFIG_HOME/visualizer/`) and restored
next run. A `--accent <name>` flag overrides the saved accent. Delete the file to reset.

## Now-playing overlay

The artist/title of the active media player is read over **MPRIS** (D-Bus), shown centered at
the top in a monospace font. Works with any MPRIS-capable player (Spotify, browsers, VLC, mpv,
…). If no player or D-Bus is available it stays hidden — the visualizer is unaffected.

Press **`Space`** to cycle the overlay mode:
- **occasional** (default) — fades in on track change and roughly every 30 s,
- **permanent** — always visible,
- **never** — hidden.

Timing constants (`NP_FADE_IN`, `NP_HOLD`, `NP_CYCLE`) are at the top of `src/main.rs`; the
font size is the `Metrics` in `Renderer::new` (`src/render.rs`).

## Tech stack

`wgpu` (Vulkan backend) · `winit` · `pipewire-rs` · `rustfft` · `ringbuf` · `glam` · `glyphon`
(text) · `mpris` (D-Bus) · `bytemuck` · `pollster`. See [`CLAUDE.md`](./CLAUDE.md) for
architecture details and version-specific notes.
