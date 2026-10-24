//! The little 3.2" screen on the Pi (`mptool screen`), in a loop: the Minipass logo
//! opens out from the middle and the tagline types itself (5 s), then the customers
//! go by 5 per page (10 s a page). Broken customers come first, in red.
//! Draws straight into the screen's framebuffer (/dev/fbN, 16-bit RGB565).

use crate::config::Config;
use crate::menu::age_hours;
use crate::monitor::Monitor;
use crate::status::Status;
use anyhow::{bail, Context, Result};
use chrono::Local;
use fontdue::{Font, FontSettings, Metrics};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long the logo stays up, its animation included.
const LOGO_TIME: f32 = 5.0;
/// How long each page of customers stays up.
const PAGE_TIME: Duration = Duration::from_secs(10);
const PER_PAGE: usize = 5;
/// Frames a second while the logo moves: about what the screen can show.
const FPS: f32 = 20.0;
const TAGLINE: &str = "minipass management tool";
/// No finished backup for this long means a night was missed.
const STALE_HOURS: i64 = 26;
/// Site checks run every 15 minutes: older than this, they have stopped.
const CHECKS_STALE_MINUTES: i64 = 35;

/// The white logo, 180 px wide, RGBA. Made from
/// Marketing/brand/logo-spec/minipass-logo-white.svg (see assets/README.md).
const LOGO: &[u8] = include_bytes!("../assets/logo-180.rgba");
const LOGO_W: i32 = 180;
const SANS: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Regular.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Bold.ttf");
const ICONS: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFont.ttf");

// Nerd Font icons (Material Design set).
const I_OK: char = '\u{F05E0}'; // check-circle
const I_BAD: char = '\u{F0159}'; // close-circle
const I_WARN: char = '\u{F0028}'; // alert-circle
const I_FILES: char = '\u{F0222}'; // file-multiple
const I_SIZE: char = '\u{F01BC}'; // database
const I_LOGIN: char = '\u{F059F}'; // web
const I_WIFI: char = '\u{F05A9}'; // wifi
const I_CABLE: char = '\u{F0200}'; // ethernet
const I_DRIVE: char = '\u{F02CA}'; // harddisk

// Same colors as the menu.
type Rgb = (u8, u8, u8);
const BG: Rgb = (26, 27, 38);
const FG: Rgb = (192, 202, 245);
const DIM: Rgb = (86, 95, 137);
const LINE: Rgb = (41, 46, 66);
const TRACK: Rgb = (36, 40, 59);
const BAR: Rgb = (122, 162, 247);
const OK: Rgb = (158, 206, 106);
const BAD: Rgb = (247, 118, 142);
const WARN: Rgb = (224, 175, 104);

pub fn run(cfg: &Config) -> Result<()> {
    let mut screen = Screen::find()?;
    let mut ink = Ink::new()?;
    let mut f = Frame::new(screen.width as i32, screen.height as i32);
    loop {
        let start = Instant::now();
        loop {
            let t = start.elapsed().as_secs_f32();
            if t >= LOGO_TIME {
                break;
            }
            draw_logo(&mut f, &mut ink, t);
            screen.show(&f)?;
            let next = Duration::from_secs_f32(((t * FPS).floor() + 1.0) / FPS);
            std::thread::sleep(next.saturating_sub(start.elapsed()));
        }

        let view = View::read(cfg);
        let pages = view.rows.len().div_ceil(PER_PAGE).max(1);
        for page in 0..pages {
            let start = Instant::now();
            while start.elapsed() < PAGE_TIME {
                // Redrawn every second for the clock; unchanged frames aren't sent.
                draw_page(&mut f, &mut ink, &view, page, pages);
                screen.show(&f)?;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

// ── The logo ─────────────────────────────────────────────────────────────────

/// The logo `t` seconds in: it opens sideways from the centre (0-1.2 s), the
/// tagline types itself (from 1.5 s), the cursor blinks, then a short hold.
fn draw_logo(f: &mut Frame, ink: &mut Ink, t: f32) {
    f.clear();
    let cx = f.w / 2;
    let h = LOGO.len() as i32 / 4 / LOGO_W;
    let (x0, y0) = (cx - LOGO_W / 2, 92 - h / 2);
    let p = (t / 1.2).min(1.0);
    let half = ((LOGO_W / 2 + 6) as f32 * (1.0 - (1.0 - p).powi(3))) as i32;
    if half > 0 {
        for y in 0..h {
            for x in 0..LOGO_W {
                let sx = x0 + x;
                if sx >= cx - half && sx < cx + half {
                    let at = ((y * LOGO_W + x) * 4) as usize;
                    f.blend(sx, y0 + y, (LOGO[at], LOGO[at + 1], LOGO[at + 2]), LOGO[at + 3]);
                }
            }
        }
        if p < 1.0 {
            for y in y0 - 4..y0 + h + 4 {
                f.blend(cx - half - 1, y, FG, 217);
                f.blend(cx + half, y, FG, 217);
            }
        }
    }

    let tx = cx - ink.width(Face::Sans, TAGLINE, 14) / 2;
    if t >= 1.5 {
        let n = (((t - 1.5) / 0.08) as usize).min(TAGLINE.len());
        let shown = &TAGLINE[..n];
        ink.draw(f, Face::Sans, shown, 14, tx, 146, DIM, Align::Left);
        let blink_on = (t / 0.45) as i32 % 2 == 0;
        if t < 4.5 && (n < TAGLINE.len() || blink_on) {
            f.rect(tx + ink.width(Face::Sans, shown, 14) + 2, 135, 7, 13, FG);
        }
    }
}

// ── The customers ────────────────────────────────────────────────────────────

/// Fine, needs a look, broken: the worst one sets a row's icon.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Ok,
    Warn,
    Bad,
}

struct Row {
    state: State,
    name: String,
    files: String,
    size: String,
    backup: State,
    login: String,
    site: State,
}

/// Everything a loop of pages shows, read once before the first page.
struct View {
    /// The Pi's address, and whether it's on Wi-Fi.
    address: Option<(String, bool)>,
    rows: Vec<Row>,
    /// Bytes used and total on the backup drive.
    drive: Option<(u64, u64)>,
}

impl View {
    fn read(cfg: &Config) -> View {
        View { address: address(), rows: rows(cfg), drive: drive_use(&cfg.root()) }
    }
}

fn rows(cfg: &Config) -> Vec<Row> {
    let st = Status::read(&cfg.root().join("last-run.json"));
    let mon = Monitor::read();
    let checks_on = mon.as_ref().and_then(|m| age_minutes(&m.checked)).is_some_and(|m| m < CHECKS_STALE_MINUTES);
    let stale = st.as_ref().is_some_and(|s| age_hours(last_time(s)).is_none_or(|h| h >= STALE_HOURS));
    let latest = cfg.root().join("latest").join("customers");

    let mut names: Vec<String> = st.iter().flat_map(|s| s.customers.clone()).collect();
    for site in mon.iter().flat_map(|m| &m.sites) {
        if !names.contains(&site.customer) {
            names.push(site.customer.clone());
        }
    }

    let mut rows: Vec<Row> = names
        .into_iter()
        .map(|c| {
            let step = st.as_ref().and_then(|s| s.steps.iter().find(|x| x.name == format!("Customer {c}: photos and settings")));
            let (backup, files) = match (&st, step) {
                (None, _) => (State::Warn, "-".to_string()),
                (_, Some(x)) if x.ok => {
                    let n = x.detail.split_whitespace().next().unwrap_or("-").to_string();
                    (if stale { State::Bad } else { State::Ok }, n)
                }
                (_, Some(_)) => (State::Bad, "failed".to_string()),
                (_, None) => (State::Bad, "-".to_string()),
            };
            let size = dir_size(&latest.join(&c)).map(short_size).unwrap_or_else(|| "-".into());

            let site = mon.as_ref().and_then(|m| m.sites.iter().find(|x| x.customer == c));
            let (site, login) = match site {
                _ if !checks_on => (State::Warn, "-".to_string()),
                None => (State::Warn, "-".to_string()),
                Some(x) if x.down() && x.problem.starts_with("certificate") => {
                    (State::Bad, format!("cert {}d", x.cert_days.unwrap_or(0)))
                }
                Some(x) if x.down() => (State::Bad, "down".to_string()),
                Some(x) if x.ms == 0 => (State::Warn, "?".to_string()),
                Some(x) if !x.problem.is_empty() || x.ms > cfg.monitor_slow_ms => (State::Warn, format!("{} ms", x.ms)),
                Some(x) => (State::Ok, format!("{} ms", x.ms)),
            };
            Row { state: backup.max(site), name: c, files, size, backup, login, site }
        })
        .collect();
    rows.sort_by_key(|r| Reverse(r.state));
    rows
}

fn draw_page(f: &mut Frame, ink: &mut Ink, view: &View, page: usize, pages: usize) {
    f.clear();
    let right = f.w - 10;

    // Top: where to SSH, and the clock (it also shows the screen is alive).
    let (icon, addr, color) = match &view.address {
        Some((ip, wifi)) => (if *wifi { I_WIFI } else { I_CABLE }, ip.as_str(), FG),
        None => (I_CABLE, "no network", WARN),
    };
    ink.draw(f, Face::Icons, &icon.to_string(), 15, 10, 21, DIM, Align::Left);
    ink.draw(f, Face::Sans, addr, 14, 31, 20, color, Align::Left);
    ink.draw(f, Face::Sans, &Local::now().format("%a %H:%M").to_string(), 14, right, 20, FG, Align::Right);
    f.rect(10, 31, f.w - 20, 1, LINE);

    // Column labels.
    let title = if pages > 1 { format!("customers {}/{pages}", page + 1) } else { "customers".to_string() };
    ink.draw(f, Face::Sans, &title, 11, 10, 48, DIM, Align::Left);
    for (icon, label, x) in [(I_FILES, "files", 152), (I_SIZE, "size", 226), (I_LOGIN, "login", right)] {
        let lw = ink.width(Face::Sans, label, 11);
        ink.draw(f, Face::Sans, label, 11, x, 48, DIM, Align::Right);
        ink.draw(f, Face::Icons, &icon.to_string(), 11, x - lw - 4, 49, DIM, Align::Right);
    }

    // One row per customer.
    let shown = view.rows.iter().skip(page * PER_PAGE).take(PER_PAGE);
    for (i, r) in shown.enumerate() {
        let y = 74 + i as i32 * 27;
        let (icon, color) = match r.state {
            State::Ok => (I_OK, OK),
            State::Warn => (I_WARN, WARN),
            State::Bad => (I_BAD, BAD),
        };
        ink.draw(f, Face::Icons, &icon.to_string(), 16, 10, y + 1, color, Align::Left);
        let name = ink.clip(Face::Sans, &r.name, 16, 76);
        ink.draw(f, Face::Sans, &name, 16, 34, y, FG, Align::Left);
        let backup = match r.backup {
            State::Ok => FG,
            State::Warn => DIM,
            State::Bad => BAD,
        };
        ink.draw(f, Face::Sans, &r.files, 14, 152, y, backup, Align::Right);
        ink.draw(f, Face::Sans, &r.size, 14, 226, y, backup, Align::Right);
        let (face, login) = match r.site {
            State::Ok => (Face::Sans, FG),
            State::Warn => (Face::Sans, WARN),
            State::Bad => (Face::Bold, BAD),
        };
        ink.draw(f, face, &r.login, 14, right, y, login, Align::Right);
    }
    if view.rows.is_empty() {
        ink.draw(f, Face::Sans, "No customers yet", 14, 34, 74, DIM, Align::Left);
    }

    // Bottom: how much of the backup drive the backups use.
    f.rect(10, 205, f.w - 20, 1, LINE);
    ink.draw(f, Face::Icons, &I_DRIVE.to_string(), 15, 10, 229, DIM, Align::Left);
    let (label, used, total) = match view.drive {
        Some((used, total)) => (format!("{} of {}", short_size(used), short_size(total)), used, total),
        None => ("no drive".to_string(), 0, 1),
    };
    let lw = ink.width(Face::Sans, &label, 12);
    ink.draw(f, Face::Sans, &label, 12, right, 228, DIM, Align::Right);
    let (bx, bw) = (32, right - lw - 10 - 32);
    f.round_rect(bx, 220, bw, 6, TRACK);
    let fill = (bw as f64 * used as f64 / total.max(1) as f64) as i32;
    f.round_rect(bx, 220, fill.clamp(4, bw), 6, BAR);
}

// ── Facts ────────────────────────────────────────────────────────────────────

/// The Pi's address on the way to the internet, and whether that's Wi-Fi.
fn address() -> Option<(String, bool)> {
    let out = Command::new("ip").args(["-4", "-o", "route", "get", "1.1.1.1"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let after = |key: &str| text.split_whitespace().skip_while(|w| *w != key).nth(1).map(str::to_string);
    let (dev, src) = (after("dev")?, after("src")?);
    Some((src, Path::new("/sys/class/net").join(dev).join("wireless").exists()))
}

/// Bytes used and total on the drive holding `root`.
fn drive_use(root: &Path) -> Option<(u64, u64)> {
    let out = Command::new("df").args(["-P", "-B1"]).arg(root).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let cols: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    let (used, free): (u64, u64) = (cols.get(2)?.parse().ok()?, cols.get(3)?.parse().ok()?);
    Some((used, used + free))
}

/// Everything under `dir`, in bytes.
fn dir_size(dir: &Path) -> Option<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        total += if meta.is_dir() { dir_size(&entry.path()).unwrap_or(0) } else { meta.len() };
    }
    Some(total)
}

/// 464 KB, 1.4 MB, 51 MB, 1.8 TB: few digits, to fit a column.
fn short_size(bytes: u64) -> String {
    let b = bytes as f64;
    for (unit, size) in [("TB", 1u64 << 40), ("GB", 1 << 30), ("MB", 1 << 20)] {
        let v = b / size as f64;
        if v >= 1.0 {
            return if v >= 10.0 { format!("{v:.0} {unit}") } else { format!("{v:.1} {unit}") };
        }
    }
    format!("{} KB", (b / 1024.0).round())
}

fn age_minutes(ts: &str) -> Option<i64> {
    let t = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S").ok()?;
    Some((Local::now().naive_local() - t).num_minutes())
}

/// When the last backup ended (or started, if it never finished).
fn last_time(s: &Status) -> &str {
    if s.finished.is_empty() {
        &s.started
    } else {
        &s.finished
    }
}

// ── Letters and icons ────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Face {
    Sans,
    Bold,
    Icons,
}

enum Align {
    Left,
    Right,
}

/// The fonts, and every letter already drawn once (the Pi is slow to redraw them).
struct Ink {
    sans: Font,
    bold: Font,
    icons: Font,
    cache: HashMap<(Face, char, u32), (Metrics, Vec<u8>)>,
}

impl Ink {
    fn new() -> Result<Ink> {
        let load = |bytes| Font::from_bytes(bytes, FontSettings::default()).map_err(anyhow::Error::msg);
        Ok(Ink { sans: load(SANS)?, bold: load(BOLD)?, icons: load(ICONS)?, cache: HashMap::new() })
    }

    fn font(&self, face: Face) -> &Font {
        match face {
            Face::Sans => &self.sans,
            Face::Bold => &self.bold,
            Face::Icons => &self.icons,
        }
    }

    fn width(&self, face: Face, s: &str, px: u32) -> i32 {
        let font = self.font(face);
        let mut w = 0.0;
        let mut prev = None;
        for c in s.chars() {
            if let Some(p) = prev {
                w += font.horizontal_kern(p, c, px as f32).unwrap_or(0.0);
            }
            w += font.metrics(c, px as f32).advance_width;
            prev = Some(c);
        }
        w.round() as i32
    }

    /// `s` cut with "…" to fit `max` pixels.
    fn clip(&self, face: Face, s: &str, px: u32, max: i32) -> String {
        if self.width(face, s, px) <= max {
            return s.to_string();
        }
        let mut cut: String = s.to_string();
        while !cut.is_empty() && self.width(face, &format!("{cut}…"), px) > max {
            cut.pop();
        }
        format!("{cut}…")
    }

    /// Writes `s` with its baseline at `y`, starting (Left) or ending (Right) at `x`.
    #[allow(clippy::too_many_arguments)]
    fn draw(&mut self, f: &mut Frame, face: Face, s: &str, px: u32, x: i32, y: i32, color: Rgb, align: Align) {
        let start = match align {
            Align::Left => x,
            Align::Right => x - self.width(face, s, px),
        };
        let Ink { sans, bold, icons, cache } = self;
        let font = match face {
            Face::Sans => &*sans,
            Face::Bold => &*bold,
            Face::Icons => &*icons,
        };
        let mut pen = start as f32;
        let mut prev = None;
        for c in s.chars() {
            if let Some(p) = prev {
                pen += font.horizontal_kern(p, c, px as f32).unwrap_or(0.0);
            }
            let (m, bitmap) = cache.entry((face, c, px)).or_insert_with(|| font.rasterize(c, px as f32));
            let gx = pen.round() as i32 + m.xmin;
            let gy = y - m.height as i32 - m.ymin;
            for (i, &a) in bitmap.iter().enumerate() {
                if a > 0 {
                    f.blend(gx + (i % m.width) as i32, gy + (i / m.width) as i32, color, a);
                }
            }
            pen += m.advance_width;
            prev = Some(c);
        }
    }
}

// ── Pixels ───────────────────────────────────────────────────────────────────

/// One full picture for the screen, drawn in memory then sent in one go.
struct Frame {
    w: i32,
    h: i32,
    px: Vec<Rgb>,
}

impl Frame {
    fn new(w: i32, h: i32) -> Frame {
        Frame { w, h, px: vec![BG; (w * h) as usize] }
    }

    fn clear(&mut self) {
        self.px.fill(BG);
    }

    /// Mixes `color` into the pixel at x,y; `alpha` 255 covers it fully.
    fn blend(&mut self, x: i32, y: i32, color: Rgb, alpha: u8) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let p = &mut self.px[(y * self.w + x) as usize];
        let mix = |under: u8, over: u8| ((under as u32 * (255 - alpha as u32) + over as u32 * alpha as u32) / 255) as u8;
        *p = (mix(p.0, color.0), mix(p.1, color.1), mix(p.2, color.2));
    }

    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, color, 255);
            }
        }
    }

    /// A bar with fully round ends, edges smoothed.
    fn round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Rgb) {
        let r = h as f32 / 2.0;
        let (left, right) = (x as f32 + r, (x + w) as f32 - r);
        let mid = y as f32 + r;
        for yy in y..y + h {
            for xx in x..x + w {
                let (px, py) = (xx as f32 + 0.5, yy as f32 + 0.5);
                let dx = (left - px).max(px - right).max(0.0);
                let d = (dx * dx + (py - mid).powi(2)).sqrt();
                let a = (r + 0.5 - d).clamp(0.0, 1.0);
                self.blend(xx, yy, color, (a * 255.0) as u8);
            }
        }
    }
}

/// The small screen's framebuffer. Small SPI screens use the fbtft drivers,
/// which name their framebuffer "fb_<chip>" (ours: fb_ili9340).
struct Screen {
    dev: File,
    width: u32,
    height: u32,
    stride: usize,
    /// What the screen shows now, so an unchanged frame isn't sent again.
    shown: Vec<u8>,
}

impl Screen {
    fn find() -> Result<Screen> {
        for entry in std::fs::read_dir("/sys/class/graphics").context("no framebuffers")?.flatten() {
            let sys = entry.path();
            let name = read_sys(&sys, "name");
            if !name.starts_with("fb_") {
                continue;
            }
            if read_sys(&sys, "bits_per_pixel") != "16" {
                bail!("{name}: only 16-bit screens are supported");
            }
            let size = read_sys(&sys, "virtual_size");
            let (w, h) = size.split_once(',').context("bad screen size")?;
            let dev = PathBuf::from("/dev").join(entry.file_name());
            return Ok(Screen {
                dev: std::fs::OpenOptions::new()
                    .write(true)
                    .open(&dev)
                    .with_context(|| format!("cannot open {}", dev.display()))?,
                width: w.parse()?,
                height: h.parse()?,
                stride: read_sys(&sys, "stride").parse()?,
                shown: Vec::new(),
            });
        }
        bail!("no small screen found (run pi/screen.sh on the Pi, then restart)")
    }

    fn show(&mut self, f: &Frame) -> Result<()> {
        let mut bytes = vec![0u8; self.stride * f.h as usize];
        for (y, row) in f.px.chunks_exact(f.w as usize).enumerate() {
            for (x, &(r, g, b)) in row.iter().enumerate() {
                let p = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
                let at = y * self.stride + x * 2;
                bytes[at..at + 2].copy_from_slice(&p.to_le_bytes());
            }
        }
        if bytes != self.shown {
            self.dev.seek(SeekFrom::Start(0))?;
            self.dev.write_all(&bytes)?;
            self.shown = bytes;
        }
        Ok(())
    }
}

fn read_sys(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).unwrap_or_default().trim().to_string()
}
