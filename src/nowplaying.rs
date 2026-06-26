//! Now-playing metadata via MPRIS (D-Bus). A background thread polls the active
//! media player for artist/title and publishes a formatted string the render
//! thread can read cheaply. Entirely optional: if D-Bus/MPRIS is unavailable
//! the thread just exits and the overlay stays hidden.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use mpris::{PlaybackStatus, PlayerFinder};

pub struct NowPlaying {
    current: Arc<Mutex<Option<String>>>,
    _handle: JoinHandle<()>,
}

impl NowPlaying {
    pub fn start() -> Self {
        let current = Arc::new(Mutex::new(None));
        let shared = current.clone();
        let handle = std::thread::Builder::new()
            .name("mpris".into())
            .spawn(move || run(shared))
            .expect("spawn mpris thread");
        Self {
            current,
            _handle: handle,
        }
    }

    /// Current "Artist — Title", or `None` if nothing is playing / unavailable.
    pub fn current(&self) -> Option<String> {
        self.current.lock().unwrap().clone()
    }
}

fn run(out: Arc<Mutex<Option<String>>>) {
    let finder = match PlayerFinder::new() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("now-playing: D-Bus unavailable, overlay disabled ({e})");
            return;
        }
    };

    loop {
        let text = finder.find_active().ok().and_then(|player| {
            // Skip stopped players; keep showing paused ones (still "current").
            if matches!(player.get_playback_status(), Ok(PlaybackStatus::Stopped)) {
                return None;
            }
            let md = player.get_metadata().ok()?;
            let title = md.title()?.trim().to_string();
            if title.is_empty() {
                return None;
            }
            let artist = md
                .artists()
                .and_then(|a| a.into_iter().find(|s| !s.is_empty()).map(str::to_string));
            Some(match artist {
                Some(a) => format!("{a}  —  {title}"),
                None => title,
            })
        });

        *out.lock().unwrap() = text;
        std::thread::sleep(Duration::from_millis(1500));
    }
}
