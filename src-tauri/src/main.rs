#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use std::{sync::Mutex, thread::sleep, time::Duration};
use tauri::{Emitter, Manager, PhysicalPosition, State, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use serde_json::json;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use winreg::{enums::*, RegKey};
use windows::Foundation::Collections::IVectorView;
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager as MediaMgr,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlayStatus,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, GetKeyboardLayout, VK_CAPITAL};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetShellWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, GWL_STYLE,
};

static PICK: AtomicBool = AtomicBool::new(false);   // region picking is on: never hide the window
static FS_HIDE: AtomicBool = AtomicBool::new(true); // hide the island while a fullscreen app is in front
struct Sz(Mutex<f64>);                               // UI scale (0.8 .. 2.0)

// size of the clickable pill in CSS px, reported by the UI
struct Hit(Mutex<(f64, f64)>);

// where the island stands: (anchor 0 = left / 1 = centre / 2 = right, x offset, y offset) in CSS px, primary monitor
struct Place(Mutex<(i32, f64, f64)>);

#[tauri::command]
fn set_hit(w: f64, h: f64, s: State<Hit>) { *s.0.lock().unwrap() = (w, h); }

// the window never takes focus, except while you type into one of its fields
#[tauri::command]
fn focus_input(win: WebviewWindow) { let _ = win.set_focusable(true); let _ = win.set_focus(); }
#[tauri::command]
fn release_focus(win: WebviewWindow) { let _ = win.set_focusable(false); }

#[tauri::command]
fn set_clip(text: String) {
    if let Ok(mut c) = arboard::Clipboard::new() { let _ = c.set_text(text); }
}

// put text on the clipboard and press Ctrl+V in the window that still has focus
#[tauri::command]
fn paste_text(text: String) {
    set_clip(text);
    sleep(Duration::from_millis(80));
    if let Ok(mut e) = Enigo::new(&Settings::default()) {
        let _ = e.key(Key::Control, Direction::Press);
        let _ = e.key(Key::Unicode('v'), Direction::Click);
        let _ = e.key(Key::Control, Direction::Release);
    }
}

#[tauri::command]
fn open_url(url: String) {
    if !(url.starts_with("http://") || url.starts_with("https://")) { return; }
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", &url]).spawn();
}

// paste the clipboard as plain text
#[tauri::command]
fn paste_plain() {
    let t = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok());
    if let Some(t) = t { paste_text(t); }
}

// ---- tools for the AI chat ----
const BAD_EXT: [&str; 17] = ["exe","dll","bat","cmd","ps1","vbs","vbe","msi","scr","lnk","reg","js","jse","wsf","com","cpl","jar"];
fn bad_ext(p: &std::path::Path) -> bool {
    p.extension().map(|e| BAD_EXT.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

// CPU / RAM / disks / busiest processes as short text for the model
#[tauri::command(async)]
fn system_stats() -> Result<String, String> {
    use sysinfo::{Disks, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_cpu_usage();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    std::thread::sleep(std::time::Duration::from_millis(450));
    sys.refresh_cpu_usage();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.refresh_memory();
    let gb = |b: u64| format!("{:.1}", b as f64 / 1_073_741_824.0);
    let (used, total) = (sys.used_memory(), sys.total_memory());
    let mut o = format!(
        "CPU: {:.0}% ({} потоков)\nRAM: {} из {} ГБ ({:.0}%)\nВремя работы ПК: {} ч {} мин\n",
        sys.global_cpu_usage(), sys.cpus().len(), gb(used), gb(total), used as f64 * 100.0 / total.max(1) as f64,
        System::uptime() / 3600, System::uptime() % 3600 / 60);
    let n = sys.cpus().len().max(1) as f32;
    let mut m: std::collections::HashMap<String, (f32, u64)> = std::collections::HashMap::new();
    for p in sys.processes().values() {
        let e = m.entry(p.name().to_string_lossy().to_string()).or_default();
        e.0 += p.cpu_usage() / n; e.1 += p.memory();
    }
    let mut v: Vec<(String, (f32, u64))> = m.into_iter().collect();
    v.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap_or(std::cmp::Ordering::Equal));
    o += &format!("Больше всего CPU: {}\n", v.iter().take(5).map(|(n, s)| format!("{} {:.1}%", n, s.0)).collect::<Vec<_>>().join(", "));
    v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    o += &format!("Больше всего RAM: {}\n", v.iter().take(5).map(|(n, s)| format!("{} {} ГБ", n, gb(s.1))).collect::<Vec<_>>().join(", "));
    let disks = Disks::new_with_refreshed_list();
    for d in disks.list() {
        if d.total_space() == 0 { continue; }
        o += &format!("Диск {}: свободно {} из {} ГБ\n", d.mount_point().display(), gb(d.available_space()), gb(d.total_space()));
    }
    Ok(o.trim_end().to_string())
}

// search files and folders by part of the name
#[tauri::command(async)]
fn find_files(dir: String, query: String) -> Result<String, String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() { return Err("пустой запрос".into()); }
    if !std::path::Path::new(&dir).is_dir() { return Err("папка не найдена".into()); }
    let skip = ["node_modules", ".git", "target", "$recycle.bin", "windows", "system volume information"];
    let t0 = std::time::Instant::now();
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![(std::path::PathBuf::from(&dir), 0u8)];
    let mut cut = false;
    while let Some((d, depth)) = stack.pop() {
        if out.len() >= 100 || t0.elapsed().as_secs() > 8 { cut = true; break; }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            let Ok(ft) = e.file_type() else { continue };
            if name.contains(&q) { out.push(format!("{}{}", e.path().display(), if ft.is_dir() { "\\" } else { "" })); }
            if ft.is_dir() && !ft.is_symlink() && depth < 6 && !skip.contains(&name.as_str()) { stack.push((e.path(), depth + 1)); }
        }
    }
    if out.is_empty() { return Ok("Ничего не найдено".into()); }
    Ok(format!("{}{}", out.join("\n"), if cut { "\n(показаны первые результаты, поиск остановлен по лимиту)" } else { "" }))
}

// write a text file; mode: create (default, fails if it exists) | overwrite | append
#[tauri::command]
fn write_file(path: String, content: String, mode: Option<String>) -> Result<String, String> {
    use std::io::Write;
    let p = std::path::Path::new(&path);
    if !p.is_absolute() { return Err("нужен полный путь".into()); }
    if bad_dir(&path) { return Err("в эту папку писать нельзя".into()); }
    if bad_ext(p) { return Err("ИИ не может создавать исполняемые файлы и скрипты".into()); }
    if content.len() > 1_000_000 { return Err("слишком большой файл (максимум 1 МБ)".into()); }
    if let Some(par) = p.parent() { std::fs::create_dir_all(par).map_err(|e| e.to_string())?; }
    let mut oo = std::fs::OpenOptions::new();
    match mode.as_deref().unwrap_or("create") {
        "append" => { oo.create(true).append(true); }
        "overwrite" => { oo.create(true).write(true).truncate(true); }
        _ => { oo.write(true).create_new(true); }
    }
    let mut f = oo.open(p).map_err(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { "файл уже существует: нужен режим overwrite или append".to_string() } else { e.to_string() })?;
    f.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("Записано {} байт: {}", content.len(), path))
}

// open a link or a file with the default app (never programs or scripts)
#[tauri::command]
fn open_item(target: String) -> Result<(), String> {
    let t = target.trim();
    if t.starts_with("http://") || t.starts_with("https://") { open_url(t.to_string()); return Ok(()); }
    let p = std::path::Path::new(t);
    if !p.exists() { return Err("не найдено".into()); }
    if bad_ext(p) { return Err("программы и скрипты ИИ не открывает".into()); }
    let _ = std::process::Command::new("explorer").arg(t).spawn();
    Ok(())
}

// ---- archives: zip, tar, tar.gz ----
const ARCH_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const ARCH_MAX_FILES: u64 = 20_000;
fn arch_kind(p: &std::path::Path) -> Option<&'static str> {
    let n = p.file_name()?.to_string_lossy().to_lowercase();
    if n.ends_with(".zip") { Some("zip") } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") { Some("tgz") } else if n.ends_with(".tar") { Some("tar") } else { None }
}
fn bad_dir(path: &str) -> bool {
    let low = path.to_lowercase().replace('/', "\\");
    low.contains("\\windows\\") || low.ends_with("\\windows") || low.contains("\\startup\\") || low.ends_with("\\startup") || low.contains("\\program files")
}
fn zip_err(e: zip::result::ZipError) -> String {
    let t = e.to_string(); let l = t.to_lowercase();
    if l.contains("password") { "архив защищён паролем".into() }
    else if l.contains("eocd") || l.contains("invalid zip") { "это не zip-архив или он повреждён".into() }
    else if l.contains("unsupported") { "неподдерживаемый способ сжатия или шифрования архива".into() }
    else { t }
}
fn mb(b: u64) -> String { format!("{:.1} МБ", b as f64 / 1_048_576.0) }

// what is inside an archive, without unpacking it
#[tauri::command(async)]
fn list_archive(path: String) -> Result<String, String> {
    let p = std::path::Path::new(&path);
    if !p.is_file() { return Err("файл не найден".into()); }
    let kind = arch_kind(p).ok_or("поддерживаются только .zip, .tar, .tar.gz и .tgz")?;
    let mut rows: Vec<(String, u64)> = Vec::new();
    let f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    match kind {
        "zip" => {
            let mut z = zip::ZipArchive::new(f).map_err(zip_err)?;
            for i in 0..z.len() {
                let e = z.by_index_raw(i).map_err(zip_err)?;
                rows.push((e.name().to_string(), if e.is_dir() { 0 } else { e.size() }));
            }
        }
        _ => {
            let r: Box<dyn std::io::Read> = if kind == "tgz" { Box::new(flate2::read::GzDecoder::new(f)) } else { Box::new(f) };
            let mut a = tar::Archive::new(r);
            for e in a.entries().map_err(|e| e.to_string())? {
                let e = e.map_err(|e| e.to_string())?;
                let t = e.header().entry_type();
                rows.push((e.path().map_err(|e| e.to_string())?.to_string_lossy().to_string(), if t.is_file() { e.size() } else { 0 }));
            }
        }
    }
    let files = rows.iter().filter(|r| !r.0.ends_with('/')).count();
    let total: u64 = rows.iter().map(|r| r.1).sum();
    let mut o = format!("Файлов: {}, после распаковки: {}\n", files, mb(total));
    for (n, s) in rows.iter().take(150) { o += &if *s > 0 { format!("{} ({})\n", n, mb(*s)) } else { format!("{}\n", n) }; }
    if rows.len() > 150 { o += &format!("…и ещё {}\n", rows.len() - 150); }
    Ok(o.trim_end().to_string())
}

struct Unpack { dest: std::path::PathBuf, files: u64, bytes: u64, bad: u64, exist: u64 }
impl Unpack {
    // writes one entry; unsafe paths are skipped, existing files are never replaced
    fn put(&mut self, rel: &std::path::Path, r: &mut dyn std::io::Read) -> Result<(), String> {
        if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) || rel.to_string_lossy().contains(':') { return Ok(()); }
        if bad_ext(rel) { self.bad += 1; return Ok(()); }
        if self.files >= ARCH_MAX_FILES { return Err("в архиве слишком много файлов".into()); }
        let out = self.dest.join(rel);
        if let Some(par) = out.parent() { std::fs::create_dir_all(par).map_err(|e| e.to_string())?; }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&out) {
            Ok(mut f) => {
                let left = ARCH_MAX_BYTES - self.bytes.min(ARCH_MAX_BYTES);
                let n = std::io::copy(&mut std::io::Read::take(&mut *r, left + 1), &mut f).map_err(|e| e.to_string())?;
                self.bytes += n;
                if self.bytes > ARCH_MAX_BYTES { return Err("архив слишком большой (лимит 4 ГБ)".into()); }
                self.files += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => self.exist += 1,
            Err(e) => return Err(e.to_string()),
        }
        Ok(())
    }
}

// unpack into dest (default: a new folder next to the archive); never overwrites, skips programs and scripts
#[tauri::command(async)]
fn extract_archive(path: String, dest: Option<String>) -> Result<String, String> {
    let p = std::path::Path::new(&path);
    if !p.is_file() { return Err("файл не найден".into()); }
    let kind = arch_kind(p).ok_or("поддерживаются только .zip, .tar, .tar.gz и .tgz")?;
    let dir = match dest.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => {
            if !std::path::Path::new(d).is_absolute() { return Err("нужен полный путь к папке".into()); }
            std::path::PathBuf::from(d)
        }
        None => {
            let fname = p.file_name().unwrap().to_string_lossy().to_string();
            let low = fname.to_lowercase();
            let stem = [".tar.gz", ".tgz", ".tar", ".zip"].iter().find(|e| low.ends_with(*e)).map(|e| fname[..fname.len() - e.len()].to_string()).unwrap_or(fname.clone());
            let base = p.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
            let mut d = base.join(&stem);
            let mut k = 2;
            while d.exists() { d = base.join(format!("{} ({})", stem, k)); k += 1; }
            d
        }
    };
    if bad_dir(&dir.to_string_lossy()) { return Err("в эту папку распаковывать нельзя".into()); }
    let created = !dir.exists();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut u = Unpack { dest: dir.clone(), files: 0, bytes: 0, bad: 0, exist: 0 };
    let f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let res: Result<(), String> = (|| {
        if kind == "zip" {
            let mut z = zip::ZipArchive::new(f).map_err(zip_err)?;
            for i in 0..z.len() {
                let mut e = z.by_index(i).map_err(zip_err)?;
                if e.is_dir() { continue; }
                let Some(rel) = e.enclosed_name() else { continue };
                u.put(&rel, &mut e)?;
            }
        } else {
            let r: Box<dyn std::io::Read> = if kind == "tgz" { Box::new(flate2::read::GzDecoder::new(f)) } else { Box::new(f) };
            let mut a = tar::Archive::new(r);
            for e in a.entries().map_err(|e| e.to_string())? {
                let mut e = e.map_err(|e| e.to_string())?;
                if !e.header().entry_type().is_file() { continue; }
                let rel = e.path().map_err(|e| e.to_string())?.to_path_buf();
                u.put(&rel, &mut e)?;
            }
        }
        Ok(())
    })();
    let mut o = format!("Распаковано файлов: {} ({}) в {}", u.files, mb(u.bytes), dir.display());
    if u.bad > 0 { o += &format!("\nПропущено программ и скриптов: {}", u.bad); }
    if u.exist > 0 { o += &format!("\nУже существовали и не заменены: {}", u.exist); }
    match res {
        Ok(()) => Ok(o),
        Err(e) if created && u.files == 0 && u.exist == 0 => { let _ = std::fs::remove_dir_all(&dir); Err(e) }
        Err(e) => Err(format!("{} (успело распаковаться: {})", e, o)),
    }
}

fn ts() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }
fn out_dir(video: bool) -> std::path::PathBuf {
    let base = (if video { dirs::video_dir() } else { dirs::picture_dir() }).unwrap_or_else(|| std::path::PathBuf::from("."));
    let p = base.join("ChaldIsland");
    let _ = std::fs::create_dir_all(&p);
    p
}
fn grab_screen() -> Result<image::RgbaImage, String> {
    let ms = xcap::Monitor::all().map_err(|e| e.to_string())?;
    let m = ms.iter().find(|m| m.is_primary().unwrap_or(false)).or(ms.first()).ok_or("монитор не найден")?;
    m.capture_image().map_err(|e| e.to_string())
}

// temporarily turn the tiny island window into a fullscreen overlay for region picking
#[tauri::command]
fn pick_mode(win: WebviewWindow, on: bool, s: State<Hit>, z: State<Sz>) {
    PICK.store(on, Ordering::Relaxed);
    apply_protect(&win, on || !CAPTURE.load(Ordering::Relaxed));
    if let Ok(Some(m)) = win.primary_monitor() {
        let sf = win.scale_factor().unwrap_or(1.0);
        if on {
            let _ = win.set_position(PhysicalPosition::new(m.position().x, m.position().y));
            let _ = win.set_size(tauri::PhysicalSize::new(m.size().width, m.size().height));
            *s.0.lock().unwrap() = (m.size().width as f64 / sf, m.size().height as f64 / sf);
        } else {
            let k = *z.0.lock().unwrap();
            apply_size(&win, k);
            *s.0.lock().unwrap() = (190.0 * k, 34.0 * k);
        }
        let _ = win.set_ignore_cursor_events(false);
    }
}

// window = 540x350 CSS px times the UI scale, placed on the primary monitor by the saved Place.
// Inside the window the island is pinned to the same side (10 px * scale from the edge), so it grows away from the screen edge.
fn apply_size(win: &WebviewWindow, k: f64) {
    let _ = win.set_size(tauri::LogicalSize::new(540.0 * k, 350.0 * k));
    let (a, ox, oy) = *win.state::<Place>().0.lock().unwrap();
    if let Ok(Some(m)) = win.primary_monitor() {
        let sf = win.scale_factor().unwrap_or(1.0);
        let w = (540.0 * k * sf).round() as i32;
        let wh = (350.0 * k * sf).round() as i32;
        let pad = (10.0 * k * sf).round() as i32;
        let (mw, mh) = (m.size().width as i32, m.size().height as i32);
        let (ox, oy) = ((ox * sf).round() as i32, (oy * sf).round() as i32);
        let x = match a { 0 => ox - pad, 2 => mw - w + pad - ox, _ => (mw - w) / 2 + ox };
        let x = x.clamp(-pad, (mw - w + pad).max(-pad));   // the whole open island stays on the screen
        let y = oy.clamp(0, (mh - wh).max(0));
        let _ = win.set_position(PhysicalPosition::new(m.position().x + x, m.position().y + y));
    }
}
#[tauri::command]
fn set_place(win: WebviewWindow, a: i32, x: f64, y: f64, p: State<Place>, z: State<Sz>) {
    *p.0.lock().unwrap() = (a.clamp(0, 2), x.clamp(-5000.0, 5000.0), y.clamp(0.0, 5000.0));
    if !PICK.load(Ordering::Relaxed) { let k = *z.0.lock().unwrap(); apply_size(&win, k); }
}
#[tauri::command]
fn set_scale(win: WebviewWindow, s: f64, z: State<Sz>) {
    let k = s.clamp(0.8, 2.0);
    *z.0.lock().unwrap() = k;
    if !PICK.load(Ordering::Relaxed) { apply_size(&win, k); }
}
#[tauri::command]
fn set_fs_hide(on: bool) { FS_HIDE.store(on, Ordering::Relaxed); }

// show the island in OBS / screenshots / recordings (on) or hide it from them (off, default).
// The capture flag is touched only when its value really changes: re-applying it makes the transparent
// WebView2 window stop repainting until it is resized.
static CAPTURE: AtomicBool = AtomicBool::new(false);   // user setting: island visible in captures
static PROTECTED: AtomicBool = AtomicBool::new(true);  // what is currently applied to the window
fn apply_protect(win: &WebviewWindow, protect: bool) {
    if PROTECTED.swap(protect, Ordering::Relaxed) != protect { let _ = win.set_content_protected(protect); }
}
#[tauri::command]
fn set_capture_visible(win: WebviewWindow, on: bool) {
    CAPTURE.store(on, Ordering::Relaxed);
    if !PICK.load(Ordering::Relaxed) { apply_protect(&win, !on); }
}

// cut a user-chosen region out of the screen (window CSS px -> screen physical px)
fn region_img(win: &WebviewWindow, x: f64, y: f64, w: f64, h: f64) -> Result<image::RgbaImage, String> {
    let img = grab_screen()?;
    let sf = win.scale_factor().unwrap_or(1.0);
    let m = win.primary_monitor().map_err(|e| e.to_string())?.ok_or("монитор не найден")?;
    let iw = img.width() as i32;
    let ih = img.height() as i32;
    let x0 = ((m.position().x as f64 + x * sf).round() as i32).clamp(0, iw);
    let y0 = ((m.position().y as f64 + y * sf).round() as i32).clamp(0, ih);
    let w0 = ((w * sf).round() as i32).clamp(1, iw - x0);
    let h0 = ((h * sf).round() as i32).clamp(1, ih - y0);
    Ok(image::imageops::crop_imm(&img, x0 as u32, y0 as u32, w0 as u32, h0 as u32).to_image())
}

// find QR codes inside a user-chosen screen region
#[tauri::command]
async fn scan_qr_region(win: WebviewWindow, x: f64, y: f64, w: f64, h: f64) -> Result<Vec<String>, String> {
    sleep(Duration::from_millis(150));
    let gray = image::DynamicImage::ImageRgba8(region_img(&win, x, y, w, h)?).to_luma8();
    let mut prep = rqrr::PreparedImage::prepare(gray);
    Ok(prep.detect_grids().iter().filter_map(|g| g.decode().ok().map(|(_, s)| s)).collect())
}

// screenshot of a chosen screen region -> clipboard + Pictures\ChaldIsland
#[tauri::command]
async fn screenshot_region(win: WebviewWindow, x: f64, y: f64, w: f64, h: f64) -> Result<String, String> {
    sleep(Duration::from_millis(150));
    let img = region_img(&win, x, y, w, h)?;
    let path = out_dir(false).join(format!("shot-{}-area.png", ts()));
    img.save(&path).map_err(|e| e.to_string())?;
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_image(arboard::ImageData { width: img.width() as usize, height: img.height() as usize, bytes: std::borrow::Cow::Borrowed(img.as_raw()) });
    }
    Ok(path.to_string_lossy().into_owned())
}

// screenshot of the primary monitor -> clipboard + Pictures\ChaldIsland
#[tauri::command]
async fn screenshot() -> Result<String, String> {
    sleep(Duration::from_millis(200));
    let img = grab_screen()?;
    let path = out_dir(false).join(format!("shot-{}.png", ts()));
    img.save(&path).map_err(|e| e.to_string())?;
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_image(arboard::ImageData { width: img.width() as usize, height: img.height() as usize, bytes: std::borrow::Cow::Borrowed(img.as_raw()) });
    }
    Ok(path.to_string_lossy().into_owned())
}

// find QR codes on the primary monitor
#[tauri::command]
async fn scan_qr() -> Result<Vec<String>, String> {
    sleep(Duration::from_millis(200));
    let gray = image::DynamicImage::ImageRgba8(grab_screen()?).to_luma8();
    let mut prep = rqrr::PreparedImage::prepare(gray);
    Ok(prep.detect_grids().iter().filter_map(|g| g.decode().ok().map(|(_, s)| s)).collect())
}

// save a recording sent from the UI as base64
#[tauri::command]
fn save_bytes(name: String, data: String) -> Result<String, String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let bytes = STANDARD.decode(data).map_err(|e| e.to_string())?;
    let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect();
    let path = out_dir(true).join(safe);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

// tools for the AI: read a text file / list a folder
#[tauri::command]
fn read_file(path: String) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.by_ref().take(200_000).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.contains(&0) { return Err("бинарный файл, прочитать как текст нельзя".into()); }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
#[tauri::command]
fn list_dir(path: String) -> Result<String, String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(&path).map_err(|e| e.to_string())?.flatten().take(300) {
        let m = e.metadata().ok();
        let kind = if m.as_ref().map(|m| m.is_dir()).unwrap_or(false) { "dir " } else { "file" };
        out.push(format!("{} {} {}", kind, e.file_name().to_string_lossy(), m.map(|m| m.len()).unwrap_or(0)));
    }
    Ok(out.join("\n"))
}


// ================= v3: system integration =================
const NOWIN: u32 = 0x0800_0000; // CREATE_NO_WINDOW
fn run(p: &str, a: &[&str]) { let _ = std::process::Command::new(p).args(a).creation_flags(NOWIN).spawn(); }
fn com() { unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); } }

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const PERS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
#[tauri::command]
fn get_autostart() -> bool {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN_KEY).and_then(|k| k.get_value::<String, _>("ChaldIsland")).is_ok()
}
#[tauri::command]
fn set_autostart(on: bool) -> Result<(), String> {
    let (k, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY).map_err(|e| e.to_string())?;
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        k.set_value("ChaldIsland", &format!("\"{}\"", exe.display())).map_err(|e| e.to_string())
    } else { let _ = k.delete_value("ChaldIsland"); Ok(()) }
}
#[tauri::command]
fn get_win_dark() -> bool {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey(PERS_KEY).and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme")).map(|v| v == 0).unwrap_or(false)
}
#[tauri::command]
fn set_win_dark(on: bool) -> Result<(), String> {
    let k = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(PERS_KEY, KEY_SET_VALUE).map_err(|e| e.to_string())?;
    let v: u32 = if on { 0 } else { 1 };
    k.set_value("AppsUseLightTheme", &v).map_err(|e| e.to_string())?;
    k.set_value("SystemUsesLightTheme", &v).map_err(|e| e.to_string())
}
#[tauri::command]
fn power(action: String) {
    match action.as_str() {
        "lock" => run("rundll32.exe", &["user32.dll,LockWorkStation"]),
        "sleep" => run("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"]),
        "restart" => run("shutdown", &["/r", "/t", "0"]),
        "shutdown" => run("shutdown", &["/s", "/t", "0"]),
        _ => {}
    }
}

// Wi-Fi / Bluetooth through the Windows radio API (no admin rights needed)
fn radios_state() -> windows::core::Result<(Option<bool>, Option<bool>)> {
    let _ = Radio::RequestAccessAsync()?.get()?;
    let list: IVectorView<Radio> = Radio::GetRadiosAsync()?.get()?;
    let (mut w, mut b) = (None, None);
    for r in list {
        let on = r.State()? == RadioState::On;
        match r.Kind()? { RadioKind::WiFi => w = Some(on), RadioKind::Bluetooth => b = Some(on), _ => {} }
    }
    Ok((w, b))
}
fn radio_set_inner(kind: &str, on: bool) -> windows::core::Result<()> {
    let _ = Radio::RequestAccessAsync()?.get()?;
    let list: IVectorView<Radio> = Radio::GetRadiosAsync()?.get()?;
    let want = if kind == "wifi" { RadioKind::WiFi } else { RadioKind::Bluetooth };
    for r in list {
        if r.Kind()? == want { r.SetStateAsync(if on { RadioState::On } else { RadioState::Off })?.get()?; }
    }
    Ok(())
}
#[tauri::command]
async fn radio_get() -> Result<(Option<bool>, Option<bool>), String> {
    tauri::async_runtime::spawn_blocking(|| { com(); radios_state().map_err(|e| e.to_string()) }).await.map_err(|e| e.to_string())?
}
#[tauri::command]
async fn radio_set(kind: String, on: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || { com(); radio_set_inner(&kind, on).map_err(|e| e.to_string()) }).await.map_err(|e| e.to_string())?
}

// open a file / show it in Explorer
#[tauri::command]
fn reveal(path: String) { let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path)).spawn(); }
#[tauri::command]
fn open_path(path: String) { if std::path::Path::new(&path).exists() { let _ = std::process::Command::new("explorer").arg(&path).spawn(); } }

// icon of a file as a data: URI (thumbnail for pictures, the Explorer icon for everything else)
#[tauri::command]
async fn file_icon(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let low = path.to_lowercase();
        if ["png", "jpg", "jpeg", "webp", "gif", "bmp"].iter().any(|e| low.ends_with(&format!(".{}", e))) {
            if let Ok(img) = image::open(&path) {
                let mut buf = Vec::new();
                if img.thumbnail(64, 64).write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).is_ok() {
                    return Ok(format!("data:image/png;base64,{}", STANDARD.encode(buf)));
                }
            }
        }
        const PS: &str = "Add-Type -AssemblyName System.Drawing; $i=[System.Drawing.Icon]::ExtractAssociatedIcon($env:ISL_P); $ms=New-Object IO.MemoryStream; $i.ToBitmap().Save($ms,[System.Drawing.Imaging.ImageFormat]::Png); [Convert]::ToBase64String($ms.ToArray())";
        let out = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", PS])
            .env("ISL_P", &path).creation_flags(NOWIN).output().map_err(|e| e.to_string())?;
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if t.is_empty() { Err("нет иконки".into()) } else { Ok(format!("data:image/png;base64,{}", t)) }
    }).await.map_err(|e| e.to_string())?
}

// small round icon that follows the cursor while a file is dragged out of the shelf
#[tauri::command]
fn drag_icon() -> Result<String, String> {
    let p = std::env::temp_dir().join("chaldisland-drag.png");
    if !p.exists() {
        let mut img = image::RgbaImage::new(40, 40);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let d = ((x as f32 - 19.5).powi(2) + (y as f32 - 19.5).powi(2)).sqrt();
            *px = if d < 18.0 { image::Rgba([127, 209, 185, 200]) } else { image::Rgba([0, 0, 0, 0]) };
        }
        img.save(&p).map_err(|e| e.to_string())?;
    }
    Ok(p.to_string_lossy().into_owned())
}

// ---- media (SMTC) ----
fn media_snapshot() -> Option<(String, String, bool)> {
    let mgr = MediaMgr::RequestAsync().ok()?.get().ok()?;
    let s = mgr.GetCurrentSession().ok()?;
    let p = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let playing = s.GetPlaybackInfo().ok()?.PlaybackStatus().ok()? == PlayStatus::Playing;
    Some((p.Title().ok()?.to_string(), p.Artist().ok()?.to_string(), playing))
}
#[tauri::command]
fn media_ctl(cmd: String) {
    std::thread::spawn(move || {
        com();
        let _ = (|| -> windows::core::Result<()> {
            let s = MediaMgr::RequestAsync()?.get()?.GetCurrentSession()?;
            match cmd.as_str() {
                "next" => { s.TrySkipNextAsync()?.get()?; }
                "prev" => { s.TrySkipPreviousAsync()?.get()?; }
                _ => { s.TryTogglePlayPauseAsync()?.get()?; }
            }
            Ok(())
        })();
    });
}
fn media_thread(h: tauri::AppHandle) {
    com();
    let mut last = String::new();
    loop {
        sleep(Duration::from_millis(900));
        let (has, t, a, p) = match media_snapshot() { Some((t, a, p)) => (true, t, a, p), None => (false, String::new(), String::new(), false) };
        let j = json!({ "has": has, "title": t, "artist": a, "playing": p });
        let k = j.to_string();
        if k != last { last = k; let _ = h.emit("media", j); }
    }
}

// ---- brightness: one hidden PowerShell loop (laptop screens only; ends by itself on desktops) ----
fn brightness_thread(h: tauri::AppHandle) {
    let script = format!(
        "$pp={}; while($true){{ if(-not (Get-Process -Id $pp -ErrorAction SilentlyContinue)){{exit}}; try{{ $b=(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness -ErrorAction Stop | Select-Object -First 1).CurrentBrightness; [Console]::Out.WriteLine($b); [Console]::Out.Flush() }}catch{{exit}}; Start-Sleep -Milliseconds 1000 }}",
        std::process::id());
    let Ok(mut ch) = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).creation_flags(NOWIN).spawn() else { return };
    let Some(out) = ch.stdout.take() else { return };
    let mut last: Option<i32> = None;
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        if let Ok(v) = line.trim().parse::<i32>() {
            if let Some(l) = last { if l != v { let _ = h.emit("osd", json!({ "k": "bri", "v": v })); } }
            last = Some(v);
        }
    }
}

// ---- foreground window helpers ----
fn fullscreen_active() -> bool {
    unsafe {
        let hw = GetForegroundWindow();
        if hw.0.is_null() || hw == GetShellWindow() { return false; }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hw, Some(&mut pid as *mut u32));
        if pid == std::process::id() { return false; }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(hw, &mut cls).max(0) as usize;
        let c = String::from_utf16_lossy(&cls[..n]);
        if c == "Progman" || c == "WorkerW" || c == "Shell_TrayWnd" { return false; }
        let mut r = RECT::default();
        if GetWindowRect(hw, &mut r).is_err() { return false; }
        let mon = MonitorFromWindow(hw, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() { return false; }
        if mi.dwFlags & 1 == 0 { return false; } // only the primary monitor, where the island lives
        let m = mi.rcMonitor;
        let style = GetWindowLongW(hw, GWL_STYLE) as u32;
        // covers the whole monitor and has no title bar (a maximised normal window keeps its caption)
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom && (style & 0x00C0_0000) != 0x00C0_0000
    }
}
fn layout_name() -> String {
    unsafe {
        let hw = GetForegroundWindow();
        let tid = if hw.0.is_null() { 0 } else { GetWindowThreadProcessId(hw, None) };
        let lang = (GetKeyboardLayout(tid).0 as usize as u32) & 0xFFFF;
        match lang & 0x3FF {
            0x09 => "EN", 0x19 => "RU", 0x22 => "UK", 0x23 => "BE", 0x07 => "DE", 0x0C => "FR", 0x0A => "ES",
            0x10 => "IT", 0x15 => "PL", 0x16 => "PT", 0x04 => "ZH", 0x11 => "JA", 0x12 => "KO", _ => "??",
        }.to_string()
    }
}
fn battery() -> (bool, i32, bool) {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { if GetSystemPowerStatus(&mut s).is_err() { return (false, 0, false); } }
    let has = s.BatteryFlag != 128 && s.BatteryFlag != 255 && s.BatteryLifePercent <= 100;
    (has, s.BatteryLifePercent as i32, s.ACLineStatus == 1)
}
// microphone / camera in use: Windows writes LastUsedTimeStart/Stop per app (Stop == 0 means "in use now")
fn cap_in_use(cap: &str) -> bool {
    let base = format!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{}", cap);
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(&base) else { return false };
    let busy = |k: &RegKey| -> bool {
        let a: u64 = k.get_value("LastUsedTimeStart").unwrap_or(0);
        let b: u64 = k.get_value("LastUsedTimeStop").unwrap_or(0);
        a > 0 && b == 0
    };
    for name in root.enum_keys().flatten() {
        let Ok(k) = root.open_subkey(&name) else { continue };
        if name == "NonPackaged" {
            for n2 in k.enum_keys().flatten() { if let Ok(k2) = k.open_subkey(&n2) { if busy(&k2) { return true; } } }
        } else if busy(&k) { return true; }
    }
    false
}
fn get_volume_ep() -> Option<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

// one polling thread: fullscreen guard, Caps Lock, layout, mic/camera, battery, volume
fn sys_loop(h: tauri::AppHandle, win: WebviewWindow) {
    com();
    let mut ep: Option<IAudioEndpointVolume> = None;
    let mut last_vol: Option<(i32, bool)> = None;
    let mut last_ind = String::new();
    let (mut fs, mut tick) = (false, 0u64);
    let (mut mic, mut cam) = (false, false);
    let (mut has_bat, mut bat, mut chg) = (false, 0i32, false);
    loop {
        sleep(Duration::from_millis(120));
        tick += 1;

        let f = FS_HIDE.load(Ordering::Relaxed) && !PICK.load(Ordering::Relaxed) && fullscreen_active();
        if f != fs {
            fs = f;
            if f { let _ = win.hide(); } else { let _ = win.show(); let _ = win.set_always_on_top(true); }
        }

        if tick % 6 == 1 { mic = cap_in_use("microphone"); cam = cap_in_use("webcam"); }
        if tick % 50 == 1 { (has_bat, bat, chg) = battery(); }
        let caps = unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 != 0;
        let j = json!({ "mic": mic, "cam": cam, "caps": caps, "lay": layout_name(), "hasbat": has_bat, "bat": bat, "chg": chg });
        let k = j.to_string();
        if k != last_ind { last_ind = k; let _ = h.emit("ind", j); }

        if ep.is_none() && tick % 25 == 1 { ep = get_volume_ep(); }
        if let Some(e) = &ep {
            match unsafe { (e.GetMasterVolumeLevelScalar(), e.GetMute()) } {
                (Ok(v), Ok(m)) => {
                    let cur = ((v * 100.0).round() as i32, m.as_bool());
                    if let Some(l) = last_vol { if l != cur { let _ = h.emit("osd", json!({ "k": "vol", "v": cur.0, "m": cur.1 })); } }
                    last_vol = Some(cur);
                }
                _ => { ep = None; last_vol = None; }
            }
        }
    }
}

// writes the bundled Chrome extension next to the app data, opens that folder and the browser's extensions page
#[tauri::command]
fn install_ext() -> Result<String, String> {
    let dir = dirs::data_local_dir().ok_or("нет папки данных")?.join("ChaldIsland").join("chrome-extension");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for (n, c) in [
        ("manifest.json", include_str!("../../chrome-extension/manifest.json")),
        ("background.js", include_str!("../../chrome-extension/background.js")),
    ] { std::fs::write(dir.join(n), c).map_err(|e| e.to_string())?; }
    let pf = std::env::var("ProgramFiles").unwrap_or_default();
    let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_default();
    let la = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let browsers = [
        (format!(r"{pf}\Google\Chrome\Application\chrome.exe"), "chrome://extensions"),
        (format!(r"{pf86}\Google\Chrome\Application\chrome.exe"), "chrome://extensions"),
        (format!(r"{la}\Google\Chrome\Application\chrome.exe"), "chrome://extensions"),
        (format!(r"{pf86}\Microsoft\Edge\Application\msedge.exe"), "edge://extensions"),
        (format!(r"{pf}\Microsoft\Edge\Application\msedge.exe"), "edge://extensions"),
        (format!(r"{la}\BraveSoftware\Brave-Browser\Application\brave.exe"), "brave://extensions"),
        (format!(r"{pf}\BraveSoftware\Brave-Browser\Application\brave.exe"), "brave://extensions"),
    ];
    // spawn the exe directly (a missing browser must not pop up a Windows error dialog)
    for (exe, url) in browsers {
        if std::path::Path::new(&exe).exists() { let _ = std::process::Command::new(&exe).arg(url).spawn(); break; }
    }
    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    Ok(dir.to_string_lossy().into_owned())
}

// Chrome extension bridge: the extension POSTs the state of the browser's downloads to 127.0.0.1:47821.
// Only requests that carry the custom X-ChaldIsland header and no foreign Origin are accepted, so a web page cannot feed it.
fn bridge_serve(l: std::net::TcpListener, emit: impl Fn(serde_json::Value) + Send + Sync + Clone + 'static) {
    use std::io::{Read, Write};
    for s in l.incoming() {
        let Ok(mut s) = s else { continue };
        let emit = emit.clone();
        std::thread::spawn(move || {
            let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            let (head_end, clen) = loop {
                match s.read(&mut tmp) { Ok(0) | Err(_) => return, Ok(n) => buf.extend_from_slice(&tmp[..n]) }
                if buf.len() > 262_144 { return; }
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                    let clen = head.lines().find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    break (p + 4, clen);
                }
            };
            while buf.len() < head_end + clen {
                match s.read(&mut tmp) { Ok(0) | Err(_) => return, Ok(n) => buf.extend_from_slice(&tmp[..n]) }
                if buf.len() > 262_144 { return; }
            }
            let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
            let ok = head.starts_with("post /dl ")
                && head.lines().any(|l| l.starts_with("x-chaldisland:"))
                && head.lines().all(|l| !l.starts_with("origin:") || l.contains("chrome-extension://"));
            let body = &buf[head_end..head_end + clen];
            let v = if ok { serde_json::from_slice::<serde_json::Value>(body).ok() } else { None };
            let resp: &[u8] = if v.is_some() { b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n" } else { b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n" };
            let _ = s.write_all(resp);
            if let Some(v) = v { emit(v); }
        });
    }
}
fn chrome_bridge(h: tauri::AppHandle) {
    let Ok(l) = std::net::TcpListener::bind("127.0.0.1:47821") else { return };
    bridge_serve(l, move |v| { let _ = h.emit("chrome_dl", v); });
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_drag::init())
        .manage(Hit(Mutex::new((190.0, 34.0))))
        .manage(Sz(Mutex::new(1.0)))
        .manage(Place(Mutex::new((1, 0.0, 0.0))))
        .invoke_handler(tauri::generate_handler![set_hit, set_place, install_ext, focus_input, release_focus, set_clip, paste_text, open_url, paste_plain, screenshot, scan_qr, save_bytes, read_file, list_dir, system_stats, find_files, write_file, open_item, list_archive, extract_archive, pick_mode, scan_qr_region, screenshot_region,
            set_scale, set_fs_hide, set_capture_visible, get_autostart, set_autostart, get_win_dark, set_win_dark, power, radio_get, radio_set,
            reveal, open_path, file_icon, drag_icon, media_ctl])
        .setup(|app| {
            let h = app.handle().clone();
            let win = app.get_webview_window("main").unwrap();

            // top centre of the primary monitor
            if let (Ok(Some(m)), Ok(sz)) = (win.primary_monitor(), win.outer_size()) {
                let x = m.position().x + (m.size().width as i32 - sz.width as i32) / 2;
                let _ = win.set_position(PhysicalPosition::new(x, m.position().y));
            }
            let _ = win.set_focusable(false);
            let _ = win.set_ignore_cursor_events(true);
            let _ = win.set_content_protected(true); // keep the island out of screenshots and recordings

            // hotkeys -> "hotkey" event with the tab to open
            for (keys, name) in [("ctrl+alt+i", "toggle"), ("alt+space", "cmd"), ("ctrl+alt+v", "clip"), ("ctrl+alt+a", "chat"),
                                 ("ctrl+alt+s", "shot"), ("ctrl+alt+x", "area"), ("ctrl+alt+q", "qr"), ("ctrl+alt+r", "rec"), ("ctrl+alt+d", "voice"),
                                 ("ctrl+alt+c", "color"), ("ctrl+alt+shift+v", "plain")] {
                let _ = app.global_shortcut().on_shortcut(keys, move |app, _, ev| {
                    if ev.state() != ShortcutState::Pressed { return; }
                    if name == "plain" { std::thread::spawn(|| { sleep(Duration::from_millis(250)); paste_plain(); }); }
                    else { let _ = app.emit("hotkey", name); }
                });
            }

            // cursor tracking: the transparent window is click-through except over the pill
            let (hh, w) = (h.clone(), win.clone());
            std::thread::spawn(move || {
                let (mut was, mut ign) = (false, true);
                loop {
                    sleep(Duration::from_millis(40));
                    let (Ok(c), Ok(p), Ok(sz)) = (hh.cursor_position(), w.outer_position(), w.inner_size()) else { continue };
                    let sf = w.scale_factor().unwrap_or(1.0);
                    let (pw, ph) = *hh.state::<Hit>().0.lock().unwrap();
                    // the island is pinned to the left / centre / right of the window (see apply_size); region picking covers the whole screen
                    let (a, _, _) = *hh.state::<Place>().0.lock().unwrap();
                    let a = if PICK.load(Ordering::Relaxed) { 1 } else { a };
                    let k = *hh.state::<Sz>().0.lock().unwrap();
                    let pad = 10.0 * k * sf;
                    let (wx, ww) = (p.x as f64, sz.width as f64);
                    let (x0, x1) = match a {
                        0 => (wx + pad, wx + pad + pw * sf),
                        2 => (wx + ww - pad - pw * sf, wx + ww - pad),
                        _ => (wx + ww / 2.0 - pw * sf / 2.0, wx + ww / 2.0 + pw * sf / 2.0),
                    };
                    let inside = c.x >= x0 && c.x <= x1 && c.y >= p.y as f64 && c.y <= p.y as f64 + (ph + 8.0) * sf;
                    if inside != was { was = inside; let _ = hh.emit("hover", inside); }
                    if inside == ign { ign = !inside; let _ = w.set_ignore_cursor_events(ign); }
                }
            });

            // downloads from any app: watch partial files (.crdownload/.part/...) in ~/Downloads,
            // follow renames, and report finished / cancelled so nothing "downloads" forever
            let hd = h.clone();
            std::thread::spawn(move || {
                use std::collections::{HashMap, HashSet};
                use std::path::PathBuf;
                const EXTS: [&str; 6] = [".crdownload", ".part", ".download", ".opdownload", ".partial", ".!ut"];
                fn is_partial(low: &str) -> bool { EXTS.iter().any(|x| low.ends_with(x)) }
                fn clean(name: &str) -> String {
                    let low = name.to_lowercase();
                    for ext in EXTS { if low.ends_with(ext) { return name[..name.len() - ext.len()].to_string(); } }
                    name.to_string()
                }
                struct Tr { key: String, name: String, size: u64, at: u64, changed: u64 }
                struct Pend { key: String, name: String, bytes: u64, since: u64 }
                // payload: (key, name, bytes, speed, state)  state: 0 = running, 1 = finished, 2 = cancelled
                let mut tracked: HashMap<PathBuf, Tr> = HashMap::new();
                let mut pending: Vec<Pend> = Vec::new();
                let mut known: Option<HashSet<PathBuf>> = None;
                let mut logged_dir = false;
                loop {
                    sleep(Duration::from_millis(600));
                    let Some(dir) = dirs::download_dir() else { continue };
                    if !logged_dir { logged_dir = true; eprintln!("[dl] watching {}", dir.display()); }
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);

                    // scan: partial files and finished files
                    let mut partials: HashMap<PathBuf, (u64, String, u64)> = HashMap::new(); // size, clean name, age (s)
                    let mut finals: HashMap<PathBuf, (u64, String, u64)> = HashMap::new();   // size, name, age (s)
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        for e in rd.flatten() {
                            let Ok(md) = e.metadata() else { continue };
                            if !md.is_file() { continue }
                            let name = e.file_name().to_string_lossy().to_string();
                            let age = md.modified().ok().and_then(|m| std::time::SystemTime::now().duration_since(m).ok()).map(|d| d.as_secs()).unwrap_or(9999);
                            if is_partial(&name.to_lowercase()) { partials.insert(e.path(), (md.len(), clean(&name), age)); }
                            else { finals.insert(e.path(), (md.len(), name, age)); }
                        }
                    }

                    // progress of partials we already follow
                    let mut stalled: Vec<PathBuf> = Vec::new();
                    for (p, (size, nm, _)) in &partials {
                        if let Some(t) = tracked.get_mut(p) {
                            let dt = now.saturating_sub(t.at).max(1);
                            let speed = if *size > t.size { (*size - t.size) as f64 / (dt as f64 / 1000.0) } else { 0.0 };
                            if *size != t.size { t.changed = now; }
                            t.size = *size; t.at = now; t.name = nm.clone();
                            if now - t.changed > 90_000 { stalled.push(p.clone()); continue }   // leftover temp file
                            let _ = hd.emit("dl", (t.key.clone(), nm.clone(), *size, speed.round() as u64, 0u8, String::new()));
                        }
                    }
                    for p in stalled { if let Some(t) = tracked.remove(&p) { let _ = hd.emit("dl", (t.key, t.name, t.size, 0u64, 2u8, String::new())); } }

                    // partial file vanished -> it was renamed (finished or new partial name) or cancelled
                    let gone: Vec<PathBuf> = tracked.keys().filter(|p| !partials.contains_key(*p)).cloned().collect();
                    for p in gone { if let Some(t) = tracked.remove(&p) { pending.push(Pend { key: t.key, name: t.name, bytes: t.size, since: now }); } }

                    let prev: HashSet<PathBuf> = known.take().unwrap_or_else(|| finals.keys().cloned().collect());
                    let mut used_f: HashSet<PathBuf> = HashSet::new();
                    let mut keep: Vec<Pend> = Vec::new();
                    for pd in pending.drain(..) {
                        // 1) a finished file appeared (same name, or any new file at least as big)
                        let mut best: Option<(PathBuf, u64, String, u64)> = None; // path, size, name, score
                        for (p, (size, name, age)) in &finals {
                            if used_f.contains(p) || *size < pd.bytes { continue }
                            let exact = name.eq_ignore_ascii_case(&pd.name) && *age <= 15;
                            if !exact && (prev.contains(p) || *age > 15) { continue }
                            let score = if exact { 0 } else { *size - pd.bytes + 1 };
                            if best.as_ref().map(|b| score < b.3).unwrap_or(true) { best = Some((p.clone(), *size, name.clone(), score)); }
                        }
                        if let Some((p, size, name, _)) = best {
                            let ps = p.to_string_lossy().into_owned();
                            used_f.insert(p);
                            let _ = hd.emit("dl", (pd.key, name, size, 0u64, 1u8, ps));
                            continue;
                        }
                        // 2) renamed to another partial name (Chrome: "Unconfirmed 123.crdownload" -> "file.zip.crdownload")
                        let mut bp: Option<(PathBuf, u64, String, u64)> = None;
                        for (p, (size, nm, _)) in &partials {
                            if tracked.contains_key(p) || *size < pd.bytes { continue }
                            let diff = *size - pd.bytes;
                            if bp.as_ref().map(|b| diff < b.3).unwrap_or(true) { bp = Some((p.clone(), *size, nm.clone(), diff)); }
                        }
                        if let Some((p, size, nm, _)) = bp {
                            let _ = hd.emit("dl", (pd.key.clone(), nm.clone(), size, 0u64, 0u8, String::new()));
                            tracked.insert(p, Tr { key: pd.key, name: nm, size, at: now, changed: now });
                            continue;
                        }
                        // 3) nothing found for a while -> cancelled
                        if now - pd.since > 8_000 { let _ = hd.emit("dl", (pd.key, pd.name, pd.bytes, 0u64, 2u8, String::new())); }
                        else { keep.push(pd); }
                    }
                    pending = keep;

                    // brand new partial files (stale leftovers older than 20 s are ignored)
                    for (p, (size, nm, age)) in &partials {
                        if tracked.contains_key(p) || *age > 20 { continue }
                        eprintln!("[dl] new partial: {}", p.display());
                        let key = format!("{}|{}", now, nm);
                        let _ = hd.emit("dl", (key.clone(), nm.clone(), *size, 0u64, 0u8, String::new()));
                        tracked.insert(p.clone(), Tr { key, name: nm.clone(), size: *size, at: now, changed: now });
                    }
                    known = Some(finals.keys().cloned().collect());
                }
            });

            // downloads reported by the Chrome extension (exact name, size, progress)
            { let hc = h.clone(); std::thread::spawn(move || chrome_bridge(hc)); }

            // system integration threads
            { let (h2, w2) = (h.clone(), win.clone()); std::thread::spawn(move || sys_loop(h2, w2)); }
            { let h3 = h.clone(); std::thread::spawn(move || media_thread(h3)); }
            { let h4 = h.clone(); std::thread::spawn(move || brightness_thread(h4)); }

            // tray icon: left click = show/hide island, menu = show, settings, quit
            let m_show = MenuItem::with_id(app, "show", "Показать остров", true, None::<&str>)?;
            let m_set = MenuItem::with_id(app, "settings", "Настройки", true, None::<&str>)?;
            let m_quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&m_show, &m_set, &m_quit])?;
            let mut tb = TrayIconBuilder::new().tooltip("ChaldIsland").menu(&menu)
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "show" => { let _ = app.emit("hotkey", "peek"); }
                    "settings" => { let _ = app.emit("hotkey", "settings"); }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                        let _ = tray.app_handle().emit("hotkey", "toggle");
                    }
                });
            #[allow(deprecated)]
            { tb = tb.menu_on_left_click(false); }
            if let Some(ic) = app.default_window_icon() { tb = tb.icon(ic.clone()); }
            tb.build(app)?;

            // clipboard watcher (text)
            std::thread::spawn(move || {
                let Ok(mut cb) = arboard::Clipboard::new() else { return };
                let mut last = cb.get_text().unwrap_or_default();
                loop {
                    sleep(Duration::from_millis(500));
                    if let Ok(t) = cb.get_text() {
                        if !t.is_empty() && t != last { last = t.clone(); let _ = h.emit("clip", t); }
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ChaldIsland");
}
