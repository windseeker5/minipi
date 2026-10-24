//! The little 3.2" screen on the Pi (`mpbackup screen`): the Minipass logo and the
//! backup status take turns. When something is wrong the status stays up, in red.
//! Draws straight into the screen's framebuffer (/dev/fbN, 16-bit RGB565).

use crate::config::Config;
use crate::menu::{age_hours, next_run, when};
use crate::run::human;
use crate::status::Status;
use anyhow::{bail, Context, Result};
use chrono::Local;
use embedded_graphics::pixelcolor::{IntoStorage, Rgb565};
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, PrimitiveStyle, Rectangle};
use std::convert::Infallible;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

/// How long the logo and the status each stay up.
const TURN: Duration = Duration::from_secs(10);
/// No finished backup for this long means a night was missed.
const STALE_HOURS: i64 = 26;
/// Customer rows that fit between the banner and the footer.
const MAX_ROWS: usize = 5;

/// The white logo centered on BG, 320x240, RGB565 little-endian. Made from
/// Marketing/brand/logo-spec/minipass-logo-white.svg rendered 260 px wide.
const LOGO: &[u8] = include_bytes!("../assets/logo-320x240.rgb565");

// Same colors as the menu.
const BG: Rgb565 = rgb(26, 27, 38);
const BAD_BG: Rgb565 = rgb(70, 18, 30);
const FG: Rgb565 = rgb(192, 202, 245);
const DIM: Rgb565 = rgb(86, 95, 137);
const LINE: Rgb565 = rgb(59, 66, 97);
const ACC: Rgb565 = rgb(122, 162, 247);
const OK: Rgb565 = rgb(158, 206, 106);
const BAD: Rgb565 = rgb(247, 118, 142);
const WARN: Rgb565 = rgb(224, 175, 104);
const DARK: Rgb565 = rgb(22, 22, 30);

const fn rgb(r: u8, g: u8, b: u8) -> Rgb565 {
    Rgb565::new(r >> 3, g >> 2, b >> 3)
}

pub fn run(cfg: &Config) -> Result<()> {
    let screen = Screen::find()?;
    let logo = Frame::logo(screen.width, screen.height);
    loop {
        let mut status = Frame::new(screen.width, screen.height, BG);
        let healthy = draw_status(cfg, &mut status);
        if healthy {
            screen.show(&logo)?;
            std::thread::sleep(TURN);
        }
        screen.show(&status)?;
        std::thread::sleep(TURN);
    }
}

// ── What the screen says ─────────────────────────────────────────────────────

enum Health {
    Ok,
    Warn(String),
    Bad(String),
}

/// Draws the status card. Returns false when something needs attention.
fn draw_status(cfg: &Config, f: &mut Frame) -> bool {
    let st = Status::read(&cfg.root().join("last-run.json"));
    let health = match &st {
        None => Health::Warn("No backup has run yet".into()),
        Some(s) if !s.ok => {
            let failed = s.failed_steps().first().map(|x| x.name.clone()).unwrap_or_default();
            Health::Bad(format!("Failed: {failed}"))
        }
        Some(s) if age_hours(last_time(s)).is_none_or(|h| h >= STALE_HOURS) => {
            Health::Bad(format!("No backup since {}", when(last_time(s))))
        }
        Some(_) => Health::Ok,
    };
    if matches!(health, Health::Bad(_)) {
        f.fill(BAD_BG);
    }
    let w = f.w as i32;

    // Header: title and clock (the clock shows the screen is alive).
    let small = FontRenderer::new::<fonts::u8g2_font_helvR10_tf>().with_ignore_unknown_chars(true);
    let body = FontRenderer::new::<fonts::u8g2_font_helvR12_tf>().with_ignore_unknown_chars(true);
    let bold = FontRenderer::new::<fonts::u8g2_font_helvB12_tf>().with_ignore_unknown_chars(true);
    let big = FontRenderer::new::<fonts::u8g2_font_helvB18_tf>().with_ignore_unknown_chars(true);
    text(f, &bold, "MINIPASS BACKUP", 10, 8, ACC, HorizontalAlignment::Left);
    text(f, &body, &Local::now().format("%a %H:%M").to_string(), w - 10, 8, DIM, HorizontalAlignment::Right);
    hline(f, 30, LINE);

    // Banner: the one thing to know.
    let (color, title, line) = match &health {
        Health::Ok => {
            let s = st.as_ref().unwrap();
            (OK, "Backup OK".to_string(), format!("{}  ·  {}", when(last_time(s)), human(s.size_bytes)))
        }
        Health::Warn(msg) => (WARN, "Waiting".to_string(), msg.clone()),
        Health::Bad(msg) => (BAD, "BACKUP PROBLEM".to_string(), msg.clone()),
    };
    Rectangle::new(Point::new(0, 36), Size::new(f.w, 36))
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(f)
        .ok();
    text(f, &big, &title, w / 2, 44, DARK, HorizontalAlignment::Center);
    text(f, &body, &clip(&line, 40), w / 2, 80, FG, HorizontalAlignment::Center);

    // One row per customer.
    if let Some(s) = &st {
        let mut y = 104;
        let shown = if s.customers.len() > MAX_ROWS { MAX_ROWS - 1 } else { s.customers.len() };
        for c in &s.customers[..shown] {
            let step = s.steps.iter().find(|x| x.name == format!("Customer {c}: photos and settings"));
            let (dot, detail) = match step {
                Some(x) if x.ok => (OK, x.detail.clone()),
                Some(_) => (BAD, "failed".to_string()),
                None => (DIM, "not backed up".to_string()),
            };
            Circle::new(Point::new(12, y + 2), 10)
                .into_styled(PrimitiveStyle::with_fill(dot))
                .draw(f)
                .ok();
            text(f, &bold, c, 30, y, FG, HorizontalAlignment::Left);
            text(f, &body, &detail, w - 10, y, if dot == BAD { BAD } else { DIM }, HorizontalAlignment::Right);
            y += 20;
        }
        if shown < s.customers.len() {
            let more = format!("+ {} more", s.customers.len() - shown);
            text(f, &body, &more, 30, y, DIM, HorizontalAlignment::Left);
        }
    }

    // Footer: next run and room left on the drive.
    hline(f, 208, LINE);
    let next = next_run()
        .map(|n| format!("Next {}", when(&n.format("%Y-%m-%d %H:%M:%S").to_string())))
        .unwrap_or_else(|| "Timer off".into());
    text(f, &small, &next, 10, 218, DIM, HorizontalAlignment::Left);
    if let Some((pct, free)) = drive_use(&cfg.root()) {
        let drive = format!("Drive {pct}%  ·  {} free", human(free));
        text(f, &small, &drive, w - 10, 218, if pct >= 90 { WARN } else { DIM }, HorizontalAlignment::Right);
    }

    matches!(health, Health::Ok)
}

/// When the last backup ended (or started, if it never finished).
fn last_time(s: &Status) -> &str {
    if s.finished.is_empty() {
        &s.started
    } else {
        &s.finished
    }
}

/// Percent used and bytes free on the drive holding `root`.
fn drive_use(root: &Path) -> Option<(u64, u64)> {
    let out = Command::new("df").args(["-P", "-B1"]).arg(root).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let cols: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    let (used, free): (u64, u64) = (cols.get(2)?.parse().ok()?, cols.get(3)?.parse().ok()?);
    Some((used * 100 / (used + free).max(1), free))
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max - 3).collect::<String>())
    }
}

fn text(f: &mut Frame, font: &FontRenderer, s: &str, x: i32, y: i32, color: Rgb565, align: HorizontalAlignment) {
    font.render_aligned(s, Point::new(x, y), VerticalPosition::Top, align, FontColor::Transparent(color), f)
        .ok();
}

fn hline(f: &mut Frame, y: i32, color: Rgb565) {
    Line::new(Point::new(0, y), Point::new(f.w as i32 - 1, y))
        .into_styled(PrimitiveStyle::with_stroke(color, 1))
        .draw(f)
        .ok();
}

// ── Pixels ───────────────────────────────────────────────────────────────────

/// One full picture for the screen, drawn in memory then sent in one go.
struct Frame {
    w: u32,
    h: u32,
    px: Vec<u16>,
}

impl Frame {
    fn new(w: u32, h: u32, color: Rgb565) -> Frame {
        Frame { w, h, px: vec![color.into_storage(); (w * h) as usize] }
    }

    fn logo(w: u32, h: u32) -> Frame {
        let mut f = Frame::new(w, h, BG);
        if LOGO.len() == (w * h * 2) as usize {
            for (p, b) in f.px.iter_mut().zip(LOGO.chunks_exact(2)) {
                *p = u16::from_le_bytes([b[0], b[1]]);
            }
        } else {
            // A screen of another size: just the name.
            let font = FontRenderer::new::<fonts::u8g2_font_helvB24_tf>().with_ignore_unknown_chars(true);
            text(&mut f, &font, "minipass", w as i32 / 2, h as i32 / 2 - 12, FG, HorizontalAlignment::Center);
        }
        f
    }

    fn fill(&mut self, color: Rgb565) {
        self.px.fill(color.into_storage());
    }
}

impl OriginDimensions for Frame {
    fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }
}

impl DrawTarget for Frame {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && (p.x as u32) < self.w && (p.y as u32) < self.h {
                self.px[(p.y as u32 * self.w + p.x as u32) as usize] = c.into_storage();
            }
        }
        Ok(())
    }
}

/// The small screen's framebuffer. Small SPI screens use the fbtft drivers,
/// which name their framebuffer "fb_<chip>" (ours: fb_ili9340).
struct Screen {
    dev: PathBuf,
    width: u32,
    height: u32,
    stride: usize,
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
            return Ok(Screen {
                dev: PathBuf::from("/dev").join(entry.file_name()),
                width: w.parse()?,
                height: h.parse()?,
                stride: read_sys(&sys, "stride").parse()?,
            });
        }
        bail!("no small screen found (run pi/screen.sh on the Pi, then restart)")
    }

    fn show(&self, f: &Frame) -> Result<()> {
        let mut bytes = vec![0u8; self.stride * f.h as usize];
        for (y, row) in f.px.chunks_exact(f.w as usize).enumerate() {
            for (x, p) in row.iter().enumerate() {
                let at = y * self.stride + x * 2;
                bytes[at..at + 2].copy_from_slice(&p.to_le_bytes());
            }
        }
        let mut dev = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.dev)
            .with_context(|| format!("cannot open {}", self.dev.display()))?;
        dev.write_all(&bytes)?;
        Ok(())
    }
}

fn read_sys(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).unwrap_or_default().trim().to_string()
}
