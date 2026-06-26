# Audio Visualizer

A fullscreen, Winamp/Milkdrop-style music visualizer for **Linux + PipeWire**, written
in Rust. It captures audio (system output or a microphone), runs an FFT to extract
**bass / mid / treble** energy and detect **beats**, and drives a reactive GPU shader.

The scene is **grayscale with a single accent color** that washes in with the bass and
flares on each beat. The accent is selectable at runtime.

```
PipeWire capture thread ──(lock-free ring buffer)──► render thread
                                                        │
                                              DSP: window → FFT → bands → beat
                                                        │
                                          AudioFeatures { bass, mid, treble, beat }
                                                        │
                                              wgpu: uniforms → fullscreen shader
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

### 2. Reactivity / DSP tuning — top of `src/dsp.rs`
- `FFT_SIZE` (2048) — window size. Power of two; 2048 @ 48 kHz ≈ 43 ms.
- Band ranges in `Analyzer::new`: bass 20–250 Hz, mid 250–4000 Hz, treble 4000–16000 Hz.
- Per-band gains in `analyze()` (`* 0.05`, `* 0.12`, `* 0.25`) — raise/lower if a band feels
  weak or clipped.
- `DECAY` (0.90) — visual smoothing; lower = snappier, higher = smoother.
- Beat threshold `instant > avg * 1.4` and the `avg > 0.02` noise gate — tune sensitivity.

### 3. The visuals — `src/shaders/fullscreen.wgsl`
Pure WGSL fragment shader; iterate without recompiling the engine logic. Bass drives a radial
ring, mids rotate colored petals, treble adds sparkle, beat triggers a flash. The grayscale →
accent blend lives at the bottom: `accentAmt = 0.18 + bass*0.5 + beat*0.9`.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | winit event loop, keybinds, CLI flags, accent selector, frame loop |
| `src/audio.rs` | PipeWire capture thread, node enumeration, runtime source switching, ring buffer |
| `src/dsp.rs` | Hann window → FFT → bass/mid/treble bands + beat detection → `AudioFeatures` |
| `src/render.rs` | wgpu device/surface/pipeline + uniform buffer (`Renderer`) |
| `src/shaders/fullscreen.wgsl` | Fullscreen-triangle vertex + reactive fragment shader |

## Tech stack

`wgpu` (Vulkan backend) · `winit` · `pipewire-rs` · `rustfft` · `ringbuf` · `bytemuck` ·
`pollster`. See [`CLAUDE.md`](./CLAUDE.md) for architecture details and version-specific notes.
