//! Tiny persistent settings: a flat `key=value` file under the user's config
//! dir. Best-effort — any read/parse/write failure falls back to defaults and
//! never interrupts the visualizer. No external dependencies.

use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub accent: usize,
    pub globe: bool,
    pub overlay_mode: u8,
    pub globe_yaw: f32,
    pub globe_pitch: f32,
    pub globe_zoom: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            accent: 0,
            globe: false,
            overlay_mode: 0,
            globe_yaw: 0.0,
            globe_pitch: 0.0,
            globe_zoom: 1.0,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("visualizer").join("settings.conf"))
}

impl Settings {
    /// Load settings, falling back to defaults for anything missing or invalid.
    pub fn load() -> Self {
        match config_path() {
            Some(path) => Self::load_from(&path),
            None => Settings::default(),
        }
    }

    fn load_from(path: &std::path::Path) -> Self {
        let mut s = Settings::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return s;
        };
        for line in text.lines() {
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let (key, val) = (key.trim(), val.trim());
            match key {
                "accent" => {
                    if let Ok(v) = val.parse() {
                        s.accent = v;
                    }
                }
                "globe" => s.globe = val == "true",
                "overlay_mode" => {
                    if let Ok(v) = val.parse() {
                        s.overlay_mode = v;
                    }
                }
                "globe_yaw" => {
                    if let Ok(v) = val.parse() {
                        s.globe_yaw = v;
                    }
                }
                "globe_pitch" => {
                    if let Ok(v) = val.parse() {
                        s.globe_pitch = v;
                    }
                }
                "globe_zoom" => {
                    if let Ok(v) = val.parse() {
                        s.globe_zoom = v;
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// Persist settings. Best-effort: errors are reported but not fatal.
    pub fn save(&self) {
        if let Some(path) = config_path() {
            self.save_to(&path);
        }
    }

    fn save_to(&self, path: &std::path::Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = format!(
            "accent={}\nglobe={}\noverlay_mode={}\nglobe_yaw={}\nglobe_pitch={}\nglobe_zoom={}\n",
            self.accent,
            self.globe,
            self.overlay_mode,
            self.globe_yaw,
            self.globe_pitch,
            self.globe_zoom,
        );
        match std::fs::File::create(path).and_then(|mut f| f.write_all(body.as_bytes())) {
            Ok(()) => {}
            Err(e) => eprintln!("could not save settings to {}: {e}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let path = std::env::temp_dir().join("visualizer_settings_test.conf");
        let s = Settings {
            accent: 3,
            globe: true,
            overlay_mode: 2,
            globe_yaw: 1.25,
            globe_pitch: -0.5,
            globe_zoom: 1.75,
        };
        s.save_to(&path);
        let loaded = Settings::load_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.accent, 3);
        assert!(loaded.globe);
        assert_eq!(loaded.overlay_mode, 2);
        assert!((loaded.globe_yaw - 1.25).abs() < 1e-6);
        assert!((loaded.globe_pitch + 0.5).abs() < 1e-6);
        assert!((loaded.globe_zoom - 1.75).abs() < 1e-6);
    }

    #[test]
    fn missing_file_is_default() {
        let path = std::env::temp_dir().join("visualizer_no_such_file.conf");
        let _ = std::fs::remove_file(&path);
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.accent, 0);
        assert!(!loaded.globe);
    }
}
