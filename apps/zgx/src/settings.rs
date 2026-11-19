//! Local settings, notifications, backgrounds, and project icons.

use serde_json::{Value as Json, json};
use zgx_desktop::WindowContext;
use zgx_script::{ScriptHost, Value};

use crate::auth::text;
use crate::engine;
use crate::plugins::{json_value, value_json};


/// The engine's methods Settings may call: this Mac's own settings.
const ENGINE_METHODS: &[&str] = &[
    "GetWorktreeSettings",
    "SetWorktreeSettings",
    "GetTitleSettings",
    "SetTitleSettings",
    "GetNativeSubagents",
    "SetNativeSubagents",
    // "Keep Keiki running after you quit" (src/quit_prompt.rs).
    "GetBackgroundSettings",
    "SetBackgroundSettings",
    // Whether this window has the keys: banners set to "only when in the
    // background" hold while it does.
    "SetViewportFocus",
];

pub fn install(_cx: &mut WindowContext, host: &ScriptHost) {
    host.async_action("projects.picture_size", |args| {
        let path = args.field("path").map(text).unwrap_or_default();
        async move {
            crate::plugins::preview::refuse("projects.picture_size")?;
            let (w, h) = off_thread(move || picture(&path).map(|(w, h, _)| (w, h))).await.ok_or("failed")??;
            Ok(Value::record([("w", Value::from(w as f64)), ("h", Value::from(h as f64))]))
        }
    });
    host.async_action("projects.keep_icon", |args| {
        let path = args.field("path").map(text).unwrap_or_default();
        let num = |key: &str| args.field(key).and_then(Value::as_f64).unwrap_or(0.0);
        let (x, y, side) = (num("x"), num("y"), num("side"));
        async move {
            crate::plugins::preview::refuse("projects.keep_icon")?;
            let dir = engine::data_dir().ok_or("failed")?.join(ICON_DIR);
            let kept = off_thread(move || keep_icon(&path, &dir, x, y, side)).await.ok_or("failed")??;
            Ok(Value::from(kept.to_string_lossy().as_ref()))
        }
    });
    host.async_action("projects.icon_file", |args| {
        let path = args.field("path").map(text).unwrap_or_default();
        async move {
            crate::plugins::preview::refuse("projects.icon_file")?;
            let dir = engine::data_dir().ok_or("failed")?.join(ICON_DIR);
            Ok(kept_icon_path(&dir, &path).map_or(Value::Nil, |path| Value::from(path.to_string_lossy().as_ref())))
        }
    });
    host.action("projects.forget_icon", |args| {
        if crate::plugins::preview::refuse("projects.forget_icon").is_err() { return; }
        let path = args.field("path").map(text).unwrap_or_default();
        if let Some(dir) = engine::data_dir() {
            if let Some(path) = kept_icon_path(&dir.join(ICON_DIR), &path) { let _ = std::fs::remove_file(path); }
        }
    });
    host.async_action("notifications.get", |_| async {
        // None yet: the engine's defaults (all on). There but unreadable:
        // said, not answered with defaults that aren't what it holds.
        let prefs = match ui_settings_path().map(|path| read_settings_at(&path)) {
            Some(Ok(Some(prefs))) => prefs,
            Some(Ok(None)) => json!({}),
            Some(Err(())) | None => return Err("failed".to_string()),
        };
        let flag = |key: &str| Value::Bool(prefs[key].as_bool().unwrap_or(true));
        Ok(Value::record([
            ("desktop", flag(BANNERS)),
            ("background_only", flag(BACKGROUND_ONLY)),
            ("agent_updates", flag(AGENT_UPDATES)),
        ]))
    });
    host.async_action("notifications.set", |args| {
        let mut changes = serde_json::Map::new();
        for (field, key) in [("desktop", BANNERS), ("background_only", BACKGROUND_ONLY), ("agent_updates", AGENT_UPDATES)] {
            if let Some(Value::Bool(on)) = args.field(field) {
                changes.insert(key.into(), Json::Bool(*on));
            }
        }
        async move { write_ui_settings(changes).map(|()| Value::Nil).map_err(|()| "failed".to_string()) }
    });
    // A session's chime, the shipping app's own: "done", "request" (an
    // agent waiting on you) or "attention" (failed, or the connection gone).
    host.action("sound.play", |kind| {
        let data: &'static [u8] = match text(&kind).as_str() {
            "done" => SOUND_DONE,
            "request" => SOUND_REQUEST,
            "attention" => SOUND_ATTENTION,
            _ => return,
        };
        std::thread::spawn(move || play(data));
    });
    // A new chat's background image, as the shipping app keeps it
    // (settings.rs `install_new_thread_composer_background`): a copy of its
    // own, so moving or deleting the original leaves the wallpaper as it is.
    host.async_action("wallpaper.keep", |args| {
        let path = args.field("path").map(text).unwrap_or_default();
        async move {
            if !std::path::Path::new(&path).is_absolute() {
                return Err("invalid".to_string());
            }
            let dir = engine::data_dir().ok_or("failed")?.join(WALLPAPER_DIR);
            let kept = off_thread(move || keep_wallpaper(&path, &dir, screen_pixels())).await.ok_or("failed")??;
            Ok(Value::record([
                ("path", Value::from(kept.path.as_str())),
                ("name", Value::from(kept.name.as_str())),
                ("w", Value::from(kept.w as f64)),
                ("h", Value::from(kept.h as f64)),
            ]))
        }
    });
    host.action("wallpaper.forget", |args| {
        let path = args.field("path").map(text).unwrap_or_default();
        if let Some(dir) = engine::data_dir() {
            forget_wallpaper(&path, &dir.join(WALLPAPER_DIR));
        }
    });
    crate::quit_prompt::install(host);
    host.async_action("settings.engine", |args| {
        let method = args.field("method").map(text).unwrap_or_default();
        let params = args.field("params").map(value_json).unwrap_or(json!({}));
        async move {
            if !ENGINE_METHODS.contains(&method.as_str()) {
                return Err("refused".into());
            }
            let params = if params.is_object() { params } else { json!({}) };
            engine::get()
                .call(&method, params)
                .await
                .map(|answer| json_value(&answer))
                .map_err(|error| {
                    // An engine too old for the method: said as what to do.
                    if error.to_ascii_lowercase().contains("unknown method") {
                        "outdated".into()
                    } else {
                        engine::outcome(&error).into()
                    }
                })
        }
    });
}

/// The largest background image taken.
const WALLPAPER_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Where the kept copies are, in the engine's data folder.
const WALLPAPER_DIR: &str = "new-chat-backgrounds";

/// A background image kept: its copy, the original's name, its size in pixels.
#[derive(Debug)]
struct KeptWallpaper {
    path: String,
    name: String,
    w: u32,
    h: u32,
}

/// The largest screen here, in pixels (its current mode's, not its
/// points): what a background needs to cover at most. None where it can't
/// be asked.
fn screen_pixels() -> Option<(u32, u32)> {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        let ids = CGDisplay::active_displays().ok()?;
        ids.into_iter()
            .filter_map(|id| CGDisplay::new(id).display_mode())
            .map(|mode| (mode.pixel_width() as u32, mode.pixel_height() as u32))
            .max_by_key(|(w, h)| u64::from(*w) * u64::from(*h))
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// The size a `w` by `h` picture is kept at to cover a `screen` (both
/// ways, any shape of window inside it), or none when it's no larger than
/// that already.
fn kept_size(w: u32, h: u32, screen: Option<(u32, u32)>) -> Option<(u32, u32)> {
    let (sw, sh) = screen?;
    let scale = (sw as f64 / w.max(1) as f64).max(sh as f64 / h.max(1) as f64);
    (scale < 1.).then(|| (((w as f64 * scale).ceil() as u32).max(1), ((h as f64 * scale).ceil() as u32).max(1)))
}

/// `path` copied into `dir` under a name of its own (a new name each time,
/// so nothing cached under the old one shows), once it decodes as a
/// picture: written beside it, then moved into place. A picture larger than
/// it takes to cover `screen` is kept at that size (a PNG), so the window
/// never holds more of it than it can show.
fn keep_wallpaper(path: &str, dir: &std::path::Path, screen: Option<(u32, u32)>) -> Result<KeptWallpaper, String> {
    let source = std::path::Path::new(path);
    let meta = std::fs::metadata(source).map_err(|_| "not_found".to_string())?;
    if meta.len() > WALLPAPER_MAX_BYTES {
        return Err("invalid".into());
    }
    let bytes = std::fs::read(source).map_err(|_| "failed".to_string())?;
    let image = zgx_gpu::assets::decode_image(&bytes).map_err(|_| "invalid".to_string())?;
    let (mut w, mut h) = (image.width(), image.height());
    std::fs::create_dir_all(dir).map_err(|_| "failed".to_string())?;
    let mut ext = source.extension().and_then(|e| e.to_str()).filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or("png").to_ascii_lowercase();
    let mut bytes = bytes;
    if let Some((nw, nh)) = kept_size(w, h, screen) {
        let full = image::RgbaImage::from_raw(w, h, image.pixels().to_vec()).ok_or("invalid")?;
        let small = image::imageops::resize(&full, nw, nh, image::imageops::FilterType::CatmullRom);
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, nw, nh);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().and_then(|mut w| w.write_image_data(small.as_raw())).map_err(|_| "failed".to_string())?;
        }
        (bytes, w, h, ext) = (png, nw, nh, "png".to_string());
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let target = dir.join(format!("new-chat-{stamp:x}-{}.{ext}", std::process::id()));
    let temp = target.with_extension(format!("{ext}.tmp"));
    if std::fs::write(&temp, &bytes).and_then(|()| std::fs::rename(&temp, &target)).is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err("failed".into());
    }
    let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(KeptWallpaper { path: target.to_string_lossy().into_owned(), name, w, h })
}

/// `keep_wallpaper` for another module's test: its path and size.
#[cfg(test)]
pub(crate) fn keep_wallpaper_for_tests(path: &str, dir: &std::path::Path, screen: Option<(u32, u32)>) -> (String, u32, u32) {
    let kept = keep_wallpaper(path, dir, screen).unwrap();
    (kept.path, kept.w, kept.h)
}

/// A copy `keep_wallpaper` made, removed: only a file directly in `dir`.
fn forget_wallpaper(path: &str, dir: &std::path::Path) {
    let path = std::path::Path::new(path);
    if path.parent() == Some(dir) && path.is_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// `work` on a thread of its own, its answer awaited here (None only if it
/// panicked), as images.rs's.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, mut rx) = zgx::task::latest();
    std::thread::spawn(move || {
        tx.send(work());
    });
    rx.recv().await
}

const BANNERS: &str = "notificationsEnabled";
const BACKGROUND_ONLY: &str = "notificationsBackgroundOnly";
const AGENT_UPDATES: &str = "agentUpdateNotifications";

static SOUND_DONE: &[u8] = include_bytes!("../sounds/done.wav");
static SOUND_REQUEST: &[u8] = include_bytes!("../sounds/request.wav");
static SOUND_ATTENTION: &[u8] = include_bytes!("../sounds/attention.wav");

/// The engine's `ui-settings.json` (the shipping app's file, in the engine's
/// data folder), or none.
fn ui_settings_path() -> Option<std::path::PathBuf> {
    engine::get().ipc_endpoint().map(|(_, dir)| dir.join("ui-settings.json"))
}

/// The file as it stands: `None` when there is none yet; an error when it
/// is there but isn't a JSON object (or can't be read), so a write never
/// replaces the shipping app's settings with ours alone.
fn read_settings_at(path: &std::path::Path) -> Result<Option<Json>, ()> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    match serde_json::from_slice::<Json>(&bytes) {
        Ok(value) if value.is_object() => Ok(Some(value)),
        _ => Err(()),
    }
}

/// `changes` merged into the file, every other key kept as it was; written
/// whole to a file of its own beside it (unique to this write), then moved
/// over it. A file that can't be read as settings is left alone.
fn write_ui_settings(changes: serde_json::Map<String, Json>) -> Result<(), ()> {
    write_settings_at(&ui_settings_path().ok_or(())?, changes)
}

fn write_settings_at(path: &std::path::Path, changes: serde_json::Map<String, Json>) -> Result<(), ()> {
    let mut prefs = read_settings_at(path)?.unwrap_or_else(|| json!({}));
    let object = prefs.as_object_mut().ok_or(())?;
    for (key, value) in changes {
        object.insert(key, value);
    }
    let bytes = serde_json::to_vec_pretty(&prefs).map_err(|_| ())?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let next = path.with_extension(format!("json.zgx-{}-{id}", std::process::id()));
    std::fs::write(&next, bytes).map_err(|_| ())?;
    std::fs::rename(&next, path).map_err(|_| {
        let _ = std::fs::remove_file(&next);
    })
}

/// A chime, from a file of its own that goes once it has played: on macOS
/// through the System Sound server (src/system_sound.rs says why not
/// `afplay`), on Linux through `paplay`; on Windows, `PlaySound` straight
/// from memory. It returns once the sound has played.
pub fn play(data: &[u8]) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};
        // SAFETY: with SND_MEMORY the first argument is the WAV's bytes,
        // which `data` keeps alive for the whole (synchronous) play.
        unsafe { PlaySoundW(data.as_ptr().cast(), std::ptr::null_mut(), SND_MEMORY | SND_SYNC | SND_NODEFAULT) };
    }
    #[cfg(not(windows))]
    play_file(data);
}

#[cfg(not(windows))]
fn play_file(data: &[u8]) {
    let Some(path) = sound_file(data) else {
        return;
    };
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::system_sound::play_file(&path) {
        eprintln!("zeron-zgx: sound: {error}");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = std::process::Command::new("paplay")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_file(&path);
}

/// `data` in a temp file of its own, named for this process.
#[cfg(not(windows))]
fn sound_file(data: &[u8]) -> Option<std::path::PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("zeron-zgx-sound-{}-{id}.wav", std::process::id()));
    std::fs::write(&path, data).ok()?;
    Some(path)
}

const ICON_DIR: &str = "profiles/local/project-icons";

fn kept_icon_path(dir: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    let root = dir.canonicalize().ok()?;
    let path = std::path::Path::new(path).canonicalize().ok()?;
    (path.parent() == Some(root.as_path()) && path.is_file()
        && path.extension().is_some_and(|ext| ext == "png")
        && path.file_name()?.to_str()?.starts_with("project-icon-"))
        .then_some(path)
}

fn keep_icon(path: &str, dir: &std::path::Path, x: f64, y: f64, side: f64) -> Result<std::path::PathBuf, String> {
    let png = square_png(path, x, y, side)?;
    std::fs::create_dir_all(dir).map_err(|_| "failed")?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let target = dir.join(format!("project-icon-{stamp:x}-{}.png", std::process::id()));
    let temp = target.with_extension("part");
    if std::fs::write(&temp, png).and_then(|()| std::fs::rename(&temp, &target)).is_err() {
        let _ = std::fs::remove_file(temp);
        return Err("failed".into());
    }
    Ok(target)
}

/// The largest source picture read for a project icon.
const ICON_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// A kept project icon's side and encoded-byte limit.
const ICON_SIDE: u32 = 256;
const ICON_MAX_UPLOAD: usize = 1_000_000;

/// A picture's size and straight RGBA pixels.
fn picture(path: &str) -> Result<(u32, u32, Vec<u8>), String> {
    if !std::path::Path::new(path).is_absolute() {
        return Err("invalid".into());
    }
    let meta = std::fs::metadata(path).map_err(|_| "not_found".to_string())?;
    if meta.len() > ICON_MAX_BYTES {
        return Err("too_large".into());
    }
    let bytes = std::fs::read(path).map_err(|_| "failed".to_string())?;
    let image = zgx_gpu::assets::decode_image(&bytes).map_err(|_| "invalid".to_string())?;
    Ok((image.width(), image.height(), image.pixels().to_vec()))
}

/// The square at `x`, `y` (`side` pixels, kept inside the picture) cut from
/// the picture, made `ICON_SIDE` at most, as a PNG; smaller again if that's
/// past the encoded-byte limit.
fn square_png(path: &str, x: f64, y: f64, side: f64) -> Result<Vec<u8>, String> {
    square_png_at(path, x, y, side, &[ICON_SIDE, 192, 128])
}

/// The same, made the first of `sides` fitting the encoded-byte limit.
fn square_png_at(path: &str, x: f64, y: f64, side: f64, sides: &[u32]) -> Result<Vec<u8>, String> {
    let (w, h, pixels) = picture(path)?;
    let rgba = image::RgbaImage::from_raw(w, h, pixels).ok_or("invalid")?;
    let short = w.min(h).max(1);
    let side = (side.round() as u32).clamp(1, short);
    let x = (x.round().max(0.0) as u32).min(w - side);
    let y = (y.round().max(0.0) as u32).min(h - side);
    let square = image::imageops::crop_imm(&rgba, x, y, side, side).to_image();
    for &target in sides {
        let out = if side > target {
            image::imageops::resize(&square, target, target, image::imageops::FilterType::Lanczos3)
        } else {
            square.clone()
        };
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, out.width(), out.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::High);
            encoder
                .write_header()
                .and_then(|mut w| w.write_image_data(out.as_raw()))
                .map_err(|_| "failed".to_string())?;
        }
        if png.len() <= ICON_MAX_UPLOAD {
            return Ok(png);
        }
    }
    Err("too_large".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Registering is everything short of sounding: the System Sound server
    // opens and accepts every sound zgx plays, from the file it's played
    // from. (A test can't listen for it.)
    #[cfg(target_os = "macos")]
    #[test]
    fn every_sound_registers_with_the_system_sound_server() {
        for data in [SOUND_DONE, SOUND_REQUEST, SOUND_ATTENTION, crate::appshots::SOUND] {
            let path = sound_file(data).expect("write the sound");
            let registered = crate::system_sound::register(&path);
            let _ = std::fs::remove_file(&path);
            registered.expect("the sound server should take the sound");
        }
    }

    // Sounds a chime, the app's own way, for checking by hand who holds the
    // audio assertion: `log stream --process runningboardd` should name
    // systemsoundserverd, not this process. Returns once it has played and
    // its file is gone.
    #[test]
    #[ignore = "plays a sound"]
    fn a_chime_plays_and_its_file_goes() {
        let before = std::time::Instant::now();
        play(SOUND_DONE);
        eprintln!("played in {} ms", before.elapsed().as_millis());
        #[cfg(not(windows))]
        {
            let prefix = format!("zeron-zgx-sound-{}-", std::process::id());
            let left = std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix));
            assert!(!left, "the chime's file should be gone");
        }
    }

    #[test]
    fn a_settings_file_keeps_its_other_keys_and_a_broken_one_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("zgx-ui-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-settings.json");
        let mut changes = serde_json::Map::new();
        changes.insert("notificationsEnabled".into(), Json::Bool(false));
        // None yet: made, with ours.
        write_settings_at(&path, changes.clone()).unwrap();
        assert_eq!(read_settings_at(&path).unwrap(), Some(json!({ "notificationsEnabled": false })));
        // The shipping app's keys stay.
        std::fs::write(&path, r#"{"theme":"dark","notificationsEnabled":true}"#).unwrap();
        write_settings_at(&path, changes.clone()).unwrap();
        assert_eq!(read_settings_at(&path).unwrap(), Some(json!({ "theme": "dark", "notificationsEnabled": false })));
        // Not settings (half written, or another shape): refused, untouched.
        std::fs::write(&path, "{\"theme\": ").unwrap();
        assert!(write_settings_at(&path, changes.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"theme\": ");
        std::fs::write(&path, "[1, 2]").unwrap();
        assert!(write_settings_at(&path, changes).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1, 2]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_background_image_is_kept_as_a_copy_and_forgotten_only_there() {
        let dir = std::env::temp_dir().join(format!("zgx-wallpaper-test-{}", std::process::id()));
        let kept_dir = dir.join("kept");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Orange.PNG");
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 200, 90);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&[230, 100, 25, 255].repeat(200 * 90)).unwrap();
        }
        std::fs::write(&path, &png).unwrap();
        let kept = keep_wallpaper(path.to_str().unwrap(), &kept_dir, None).unwrap();
        assert_eq!((kept.name.as_str(), kept.w, kept.h), ("Orange.PNG", 200, 90));
        assert!(kept.path.ends_with(".png") && kept.path.starts_with(kept_dir.to_str().unwrap()));
        assert_eq!(std::fs::read(&kept.path).unwrap(), png);
        // Not a picture, or not there: refused, nothing kept.
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        assert_eq!(keep_wallpaper(dir.join("notes.txt").to_str().unwrap(), &kept_dir, None).unwrap_err(), "invalid");
        assert_eq!(keep_wallpaper(dir.join("gone.png").to_str().unwrap(), &kept_dir, None).unwrap_err(), "not_found");
        assert_eq!(std::fs::read_dir(&kept_dir).unwrap().count(), 1);
        // Larger than the screen needs: kept just covering it, as a PNG.
        let small = keep_wallpaper(path.to_str().unwrap(), &kept_dir, Some((100, 30))).unwrap();
        assert_eq!((small.w, small.h), (100, 45), "200 × 90 covering 100 × 30");
        assert!(small.path.ends_with(".png"));
        let shrunk = zgx_gpu::assets::decode_image(&std::fs::read(&small.path).unwrap()).unwrap();
        assert_eq!((shrunk.width(), shrunk.height()), (100, 45));
        assert_eq!(&shrunk.pixels()[..4], &[230, 100, 25, 255], "its colours kept");
        forget_wallpaper(&small.path, &kept_dir);
        // No larger than it: kept as it is.
        assert_eq!(kept_size(200, 90, Some((400, 300))), None);
        assert_eq!(kept_size(4000, 3000, Some((3456, 2234))), Some((3456, 2592)));
        // Forgetting touches only a kept copy: the original stays.
        forget_wallpaper(path.to_str().unwrap(), &kept_dir);
        assert!(path.is_file());
        forget_wallpaper(&kept.path, &kept_dir);
        assert!(!std::path::Path::new(&kept.path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_project_icon_is_cropped_kept_and_jailed() {
        let dir = std::env::temp_dir().join(format!("zgx-icon-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.png");
        // 1200 × 700: left half red, right half blue.
        let (w, h) = (1200u32, 700u32);
        let mut raw = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..h {
            for x in 0..w {
                raw.extend_from_slice(if x < w / 2 { &[220, 30, 30, 255] } else { &[30, 30, 220, 255] });
            }
        }
        let mut file = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut file, w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&raw).unwrap();
        }
        std::fs::write(&path, file).unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(picture(path).map(|(w, h, _)| (w, h)), Ok((1200, 700)));
        // Asked past the right and top edges: moved back inside (x 500,
        // y 0), so it starts in the red and ends in the blue; made 512.
        let png = square_png(path, 900.0, -40.0, 700.0).unwrap();
        let image = zgx_gpu::assets::decode_image(&png).unwrap();
        assert_eq!((image.width(), image.height()), (256, 256));
        assert_eq!(&image.pixels()[..4], &[220, 30, 30, 255]);
        let last = image.pixels().len() - 4;
        assert_eq!(&image.pixels()[last..], &[30, 30, 220, 255]);
        // A small square stays its size.
        let small = zgx_gpu::assets::decode_image(&square_png(path, 0.0, 0.0, 100.0).unwrap()).unwrap();
        assert_eq!((small.width(), small.height()), (100, 100));
        assert_eq!(&small.pixels()[..4], &[220, 30, 30, 255]);
        assert_eq!(square_png("relative.png", 0.0, 0.0, 10.0), Err("invalid".into()));
        let icons = dir.join("icons");
        let kept = keep_icon(path, &icons, 900.0, -40.0, 700.0).unwrap();
        assert_eq!(kept_icon_path(&icons, kept.to_str().unwrap()), Some(kept.canonicalize().unwrap()));
        assert_eq!(kept_icon_path(&icons, path), None);
        assert_eq!(kept_icon_path(&icons, ""), None);
        assert_eq!(kept_icon_path(&icons, "https://example.test/icon.png"), None);
        assert_eq!(keep_icon("", &icons, 0.0, 0.0, 1.0), Err("invalid".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

}
