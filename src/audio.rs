//! PipeWire audio capture.
//!
//! A dedicated thread runs the PipeWire main loop. It keeps a live registry of
//! audio nodes (mics, line-ins, and sink monitors for system output) and runs a
//! capture stream whose samples are downmixed to mono and pushed into a
//! lock-free ring buffer that the render thread drains each frame.
//!
//! The render thread can re-target the capture at runtime by sending a
//! [`Command`] over a PipeWire channel (which wakes the loop safely).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pipewire as pw;
use pw::{properties::properties, spa};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;

/// A selectable capture target discovered on the PipeWire graph.
#[derive(Clone, Debug)]
pub struct SourceInfo {
    pub id: u32,
    pub name: String,
    /// A sink: capture its monitor (system output) rather than an input.
    pub is_sink: bool,
}

/// Commands sent from the render thread to the audio loop.
enum Command {
    /// Re-target capture. `None` target + `capture_sink` autoconnects to the
    /// default sink monitor (system output).
    Connect {
        target: Option<u32>,
        capture_sink: bool,
    },
}

/// Per-stream state owned by PipeWire; updated in `param_changed`.
#[derive(Default)]
struct UserData {
    format: AudioInfoRaw,
}

/// Ring buffer capacity in samples (mono). ~340 ms at 48 kHz — plenty of slack
/// between render frames; the consumer always drains fully.
const RING_CAPACITY: usize = 16384;

pub struct AudioEngine {
    sources: Arc<Mutex<Vec<SourceInfo>>>,
    sample_rate: Arc<AtomicU32>,
    cmd_tx: pw::channel::Sender<Command>,
    consumer: HeapCons<f32>,
    _handle: JoinHandle<()>,
}

impl AudioEngine {
    /// Start the audio thread. `capture_system_output` selects the default
    /// behaviour: `true` captures the default sink monitor, `false` the default
    /// audio source (mic).
    pub fn start(capture_system_output: bool) -> Self {
        let rb = HeapRb::<f32>::new(RING_CAPACITY);
        let (producer, consumer) = rb.split();

        let sources = Arc::new(Mutex::new(Vec::new()));
        let sample_rate = Arc::new(AtomicU32::new(48_000));
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Command>();

        let sources_thread = sources.clone();
        let rate_thread = sample_rate.clone();

        let handle = std::thread::Builder::new()
            .name("pw-audio".into())
            .spawn(move || {
                run_audio_loop(
                    producer,
                    sources_thread,
                    rate_thread,
                    cmd_rx,
                    capture_system_output,
                );
            })
            .expect("spawn audio thread");

        Self {
            sources,
            sample_rate,
            cmd_tx,
            consumer,
            _handle: handle,
        }
    }

    /// Drain all currently-available samples into `out` (cleared first).
    pub fn drain_samples(&mut self, out: &mut Vec<f32>) {
        let n = self.consumer.occupied_len();
        out.clear();
        if n == 0 {
            return;
        }
        out.resize(n, 0.0);
        let got = self.consumer.pop_slice(out);
        out.truncate(got);
    }

    /// Snapshot of currently-known capture targets.
    pub fn sources(&self) -> Vec<SourceInfo> {
        self.sources.lock().unwrap().clone()
    }

    /// Negotiated capture sample rate (Hz).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    /// Re-target capture at a specific discovered source.
    pub fn connect_to(&self, src: &SourceInfo) {
        let _ = self.cmd_tx.send(Command::Connect {
            target: Some(src.id),
            capture_sink: src.is_sink,
        });
    }
}

/// Build, register and connect a capture stream for the given target.
/// Returns the stream and its listener, which must be kept alive.
fn connect_stream(
    core: &pw::core::CoreRc,
    producer: Rc<RefCell<HeapProd<f32>>>,
    sample_rate: Arc<AtomicU32>,
    target: Option<u32>,
    capture_sink: bool,
) -> Option<(pw::stream::StreamRc, pw::stream::StreamListener<UserData>)> {
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    if capture_sink {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = pw::stream::StreamRc::new(core.clone(), "visualizer-capture", props).ok()?;

    let rate_cb = sample_rate.clone();
    // Reused across process callbacks to avoid per-buffer heap allocation.
    let mut mono: Vec<f32> = Vec::new();
    let listener = stream
        .add_local_listener_with_user_data(UserData::default())
        .param_changed(move |_, user_data, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            if user_data.format.parse(param).is_ok() {
                let rate = user_data.format.rate();
                if rate > 0 {
                    rate_cb.store(rate, Ordering::Relaxed);
                }
            }
        })
        .process(move |stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let channels = user_data.format.channels().max(1) as usize;
            let n_samples = (data.chunk().size() as usize) / std::mem::size_of::<f32>();
            let Some(bytes) = data.data() else { return };
            if n_samples == 0 || bytes.len() < n_samples * 4 {
                return;
            }

            // Downmix interleaved F32LE channels to mono into a reusable buffer.
            // Decoding from bytes is alignment-safe (no cast_slice panic risk).
            let frames = n_samples / channels;
            mono.clear();
            mono.reserve(frames);
            let inv_channels = 1.0 / channels as f32;
            for f in 0..frames {
                let mut sum = 0.0f32;
                let base = f * channels * 4;
                for c in 0..channels {
                    let i = base + c * 4;
                    sum += f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
                }
                mono.push(sum * inv_channels);
            }
            // Drop on overflow (consumer fell behind) — fine for a visualizer.
            let _ = producer.borrow_mut().push_slice(&mono);
        })
        .register()
        .ok()?;

    // EnumFormat param: F32 audio, native rate/channels (left unspecified).
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .ok()?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values)?];

    stream
        .connect(
            spa::utils::Direction::Input,
            target,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .ok()?;

    Some((stream, listener))
}

/// Classify a registry node from its properties; returns a capture target if it
/// is a usable audio source or sink.
fn classify_node(global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) -> Option<SourceInfo> {
    if global.type_ != pw::types::ObjectType::Node {
        return None;
    }
    let props = global.props?;
    let class = props.get("media.class").unwrap_or("");
    let is_sink = class.contains("Audio/Sink");
    let is_source = class.contains("Audio/Source");
    if !is_sink && !is_source {
        return None;
    }
    let name = props
        .get("node.description")
        .or_else(|| props.get("node.nick"))
        .or_else(|| props.get("node.name"))
        .unwrap_or("(unnamed)")
        .to_string();
    Some(SourceInfo {
        id: global.id,
        name,
        is_sink,
    })
}

fn run_audio_loop(
    producer: HeapProd<f32>,
    sources: Arc<Mutex<Vec<SourceInfo>>>,
    sample_rate: Arc<AtomicU32>,
    cmd_rx: pw::channel::Receiver<Command>,
    capture_system_output: bool,
) {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).expect("create main loop");
    let context = pw::context::ContextRc::new(&mainloop, None).expect("create context");
    let core = context.connect_rc(None).expect("connect core");
    let registry = core.get_registry_rc().expect("get registry");

    let producer = Rc::new(RefCell::new(producer));

    // Live registry of capture targets.
    let sources_add = sources.clone();
    let sources_rm = sources.clone();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(info) = classify_node(global) {
                let mut list = sources_add.lock().unwrap();
                if !list.iter().any(|s| s.id == info.id) {
                    list.push(info);
                }
            }
        })
        .global_remove(move |id| {
            sources_rm.lock().unwrap().retain(|s| s.id != id);
        })
        .register();

    // Holder for the current stream so the channel callback can replace it.
    type Held = Option<(pw::stream::StreamRc, pw::stream::StreamListener<UserData>)>;
    let current: Rc<RefCell<Held>> = Rc::new(RefCell::new(None));

    // Initial connect: autoconnect to default sink monitor or default source.
    let initial = connect_stream(
        &core,
        producer.clone(),
        sample_rate.clone(),
        None,
        capture_system_output,
    );
    if initial.is_none() {
        eprintln!("audio: failed to create the initial capture stream");
    }
    *current.borrow_mut() = initial;

    // Handle runtime re-targeting from the render thread.
    let core_cb = core.clone();
    let producer_cb = producer.clone();
    let rate_cb = sample_rate.clone();
    let current_cb = current.clone();
    let _recv = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        Command::Connect {
            target,
            capture_sink,
        } => {
            if let Some((old, _)) = current_cb.borrow_mut().take() {
                let _ = old.disconnect();
            }
            *current_cb.borrow_mut() = connect_stream(
                &core_cb,
                producer_cb.clone(),
                rate_cb.clone(),
                target,
                capture_sink,
            );
        }
    });

    mainloop.run();
}

/// One-shot enumeration of capture targets (for `--list-sources`).
pub fn enumerate_sources() -> Vec<SourceInfo> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).expect("create main loop");
    let context = pw::context::ContextRc::new(&mainloop, None).expect("create context");
    let core = context.connect_rc(None).expect("connect core");
    let registry = core.get_registry_rc().expect("get registry");

    let found: Rc<RefCell<Vec<SourceInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let found_cb = found.clone();
    let _reg = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(info) = classify_node(global) {
                found_cb.borrow_mut().push(info);
            }
        })
        .register();

    // Roundtrip: quit once the server reports our sync is done.
    let pending = core.sync(0).expect("sync");
    let loop_quit = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                loop_quit.quit();
            }
        })
        .register();

    mainloop.run();

    // Clone out: the registry listener (`_reg`) still holds a reference to
    // `found`, so we can't move it out — copy the gathered list instead.
    found.borrow().clone()
}
