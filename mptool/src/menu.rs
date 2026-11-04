//! The menu (`mptool` with no arguments): one dashboard screen, actions open in pop-ups.
//! Only the two Restore pop-ups change the server, through restore.rs, after a typed confirm.
//! Everything else here only reads.

use crate::compare::{self, Line as CmpLine};
use crate::config::Config;
use crate::notify;
use crate::restore::{self, Job, Prepared, Report, What, MAIL_PARTS};
use crate::run::{self, human, Event};
use crate::status::{nights, Status, Step};
use crate::unpack;
use anyhow::Result;
use chrono::{Duration as Days, Local, NaiveDateTime};
use ratatui::crossterm::event::{self, Event as Term, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

// Tokyo Night
const BG: Color = Color::Rgb(26, 27, 38);
const FG: Color = Color::Rgb(192, 202, 245);
const DIM: Color = Color::Rgb(86, 95, 137);
const LINE: Color = Color::Rgb(59, 66, 97);
const ACC: Color = Color::Rgb(122, 162, 247);
const OK: Color = Color::Rgb(158, 206, 106);
const BAD: Color = Color::Rgb(247, 118, 142);
const WARN: Color = Color::Rgb(224, 175, 104);
const MAG: Color = Color::Rgb(187, 154, 247);
const CYAN: Color = Color::Rgb(125, 207, 255);
const SEL: Color = Color::Rgb(40, 52, 87);
const DARK: Color = Color::Rgb(22, 22, 30);

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const GUIDE: &[&str] = &[
    "1  Take a Hetzner snapshot first, even of a broken server.",
    "2  Press c (Compare) to see what changed since the backup.",
    "3  One customer lost data:   r  Restore a customer.",
    "4  Mail, website or server settings broken:   s  Restore setup.",
    "5  Whole server gone:   new server, then  s  Everything.",
    "   Every restore saves an undo copy first:   s  Undo the last restore.",
    "",
    "Never on the server:  git reset --hard,  docker image prune,",
    "docker-compose pull  (unless you are upgrading on purpose).",
];

struct Night {
    date: String,
    path: PathBuf,
    status: Option<Status>,
}

#[derive(Clone, Copy, PartialEq)]
enum Pop {
    Backup,
    Compare,
    Unpack,
    Guide,
    Restore,
}

enum Msg {
    Backup(Event),
    BackupEnd(Result<Status, String>, Option<String>),
    Compared(Result<Vec<CmpLine>, String>),
    Unpacked(Result<String, String>),
    RestoreReady(Result<Prepared, String>),
    Restore(Event),
    RestoreEnd(Result<Report, String>),
}

#[derive(Clone, Copy, PartialEq, Default)]
enum Stage {
    /// Picking what to restore.
    #[default]
    Pick,
    /// Checking the backup and making the preview.
    Checking,
    /// Showing the preview, waiting for the typed confirm.
    Confirm,
    Running,
    Done,
}

/// What the Restore pop-ups show. `r` lists customers, `s` lists the rest of the setup.
#[derive(Default)]
struct RestoreUi {
    setup: bool,
    choices: Vec<String>,
    cursor: usize,
    /// Which mail settings to put back, in the order of MAIL_PARTS.
    mail: [bool; 3],
    stage: Stage,
    prepared: Option<std::sync::Arc<Prepared>>,
    typed: String,
    snapshot: bool,
    log: Vec<String>,
    steps: Vec<Step>,
    error: Option<String>,
    report: Option<Report>,
}

const SETUP_CHOICES: [&str; 6] =
    ["Mail settings", "Mail messages (adds back what's missing)", "Website", "Server settings", "Everything (blank server)", "Undo the last restore"];

#[derive(Default)]
struct Backup {
    running: bool,
    total: usize,
    done: Vec<Step>,
    current: Option<String>,
    end: Option<Result<Status, String>>,
}

/// A slower job (compare, unpack): which night, and its result once finished.
struct SlowJob<T> {
    night: String,
    result: Option<Result<T, String>>,
}

struct App {
    cfg: Config,
    nights: Vec<Night>, // newest first
    last: Option<Status>,
    next_run: Option<NaiveDateTime>,
    table: TableState,
    pop: Option<Pop>,
    backup: Backup,
    compare: Option<SlowJob<Vec<CmpLine>>>,
    unpack: Option<SlowJob<String>>,
    restore: RestoreUi,
    scroll: u16,
    note: Option<String>,
    tick: usize,
    quit: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

pub fn run(cfg: Config) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = App::new(cfg).run(&mut terminal);
    ratatui::restore();
    res
}

impl App {
    fn new(cfg: Config) -> App {
        let (tx, rx) = channel();
        let mut app = App {
            cfg,
            nights: vec![],
            last: None,
            next_run: None,
            table: TableState::default().with_selected(0),
            pop: None,
            backup: Backup::default(),
            compare: None,
            unpack: None,
            restore: RestoreUi::default(),
            scroll: 0,
            note: None,
            tick: 0,
            quit: false,
            tx,
            rx,
        };
        app.load();
        app
    }

    fn load(&mut self) {
        let root = self.cfg.root();
        self.nights = nights(&root)
            .into_iter()
            .rev()
            .map(|(d, path)| Night {
                date: d.format("%Y-%m-%d").to_string(),
                status: Status::read(&path.join("status.json")),
                path,
            })
            .collect();
        self.last = Status::read(&root.join("last-run.json"));
        self.next_run = next_run();
        let max = self.nights.len().saturating_sub(1);
        self.table.select(Some(self.selected().min(max)));
    }

    fn selected(&self) -> usize {
        self.table.selected().unwrap_or(0)
    }

    fn night(&self) -> Option<&Night> {
        self.nights.get(self.selected())
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(m) = self.rx.try_recv() {
                self.on_msg(m);
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(120))? {
                if let Term::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        self.on_key(k.code, k.modifiers);
                    }
                }
            }
            self.tick += 1;
            if self.tick % 50 == 0 {
                self.load(); // pick up a night the timer made while the menu is open
            }
        }
        Ok(())
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Backup(Event::Total(n)) => self.backup.total = n,
            Msg::Backup(Event::Start(name)) => self.backup.current = Some(name),
            Msg::Backup(Event::Done(step)) => {
                self.backup.current = None;
                self.backup.done.push(step);
            }
            Msg::Backup(Event::Log(_)) => {}
            Msg::BackupEnd(r, discord_error) => {
                self.backup.running = false;
                self.backup.current = None;
                self.backup.end = Some(r);
                self.note = discord_error.map(|e| format!("Discord message not sent: {e}"));
                self.table.select(Some(0));
                self.load();
            }
            Msg::Compared(r) => {
                if let Some(job) = &mut self.compare {
                    job.result = Some(r);
                }
            }
            Msg::Unpacked(r) => {
                if let Some(job) = &mut self.unpack {
                    job.result = Some(r);
                }
            }
            Msg::RestoreReady(Ok(p)) => {
                self.restore.prepared = Some(std::sync::Arc::new(p));
                self.restore.stage = Stage::Confirm;
            }
            Msg::RestoreReady(Err(e)) => {
                self.restore.error = Some(e);
                self.restore.stage = Stage::Done;
            }
            Msg::Restore(Event::Log(line)) => self.restore.log.push(line),
            Msg::Restore(Event::Done(step)) => self.restore.steps.push(step),
            Msg::Restore(_) => {}
            Msg::RestoreEnd(r) => {
                match r {
                    Ok(report) => self.restore.report = Some(report),
                    Err(e) => self.restore.error = Some(e),
                }
                self.restore.stage = Stage::Done;
                self.load();
            }
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.note = None;
        // The confirm box takes every letter, so nothing typed there can open or quit anything.
        if self.pop == Some(Pop::Restore) && self.restore.stage == Stage::Confirm {
            match code {
                KeyCode::Esc => {
                    self.restore.stage = Stage::Pick;
                    self.restore.prepared = None;
                    self.restore.typed.clear();
                    self.restore.snapshot = false;
                }
                KeyCode::Enter => self.confirm_restore(),
                KeyCode::Backspace => {
                    self.restore.typed.pop();
                }
                KeyCode::Char(c) => self.restore.typed.push(c),
                _ => {}
            }
            return;
        }
        if code == KeyCode::Char('q') || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL)) {
            if self.backup.running {
                self.note = Some("A backup is running. Wait for it to finish, then quit.".into());
            } else if self.restore.stage == Stage::Running {
                self.note = Some("A restore is running. Wait for it to finish, then quit.".into());
            } else {
                self.quit = true;
            }
            return;
        }
        let up = matches!(code, KeyCode::Up | KeyCode::Char('k'));
        let down = matches!(code, KeyCode::Down | KeyCode::Char('j'));
        match code {
            KeyCode::Esc => self.pop = None,
            KeyCode::Enter => match self.pop {
                Some(Pop::Backup) => self.start_backup(),
                Some(Pop::Unpack) => self.start_unpack(),
                Some(Pop::Restore) if self.restore.stage == Stage::Pick => self.start_restore(),
                Some(Pop::Restore) if self.restore.stage == Stage::Done => self.restore.stage = Stage::Pick,
                _ => {}
            },
            _ if (up || down) && self.pop == Some(Pop::Restore) => {
                if self.restore.stage == Stage::Pick {
                    let last = self.restore.choices.len().saturating_sub(1);
                    let at = self.restore.cursor;
                    self.restore.cursor = if up { at.saturating_sub(1) } else { (at + 1).min(last) };
                }
            }
            KeyCode::Char(k @ ('a' | 'f' | 'd'))
                if self.pop == Some(Pop::Restore)
                    && self.restore.setup
                    && self.restore.stage == Stage::Pick
                    && self.restore.cursor == 0 =>
            {
                let i = "afd".find(k).unwrap_or(0);
                self.restore.mail[i] = !self.restore.mail[i];
            }
            _ if (up || down) && self.pop == Some(Pop::Compare) => {
                self.scroll = if up { self.scroll.saturating_sub(1) } else { self.scroll + 1 };
            }
            _ if up => self.table.select(Some(self.selected().saturating_sub(1))),
            _ if down => self.table.select(Some((self.selected() + 1).min(self.nights.len().saturating_sub(1)))),
            KeyCode::Char('b') => self.pop = Some(Pop::Backup),
            KeyCode::Char('c') => {
                self.pop = Some(Pop::Compare);
                self.start_compare();
            }
            KeyCode::Char('u') => {
                self.pop = Some(Pop::Unpack);
                if self.unpack.as_ref().is_some_and(|j| j.result.is_some()) {
                    self.unpack = None;
                }
            }
            KeyCode::Char('g') => self.pop = Some(Pop::Guide),
            KeyCode::Char('r') => self.open_restore(false),
            KeyCode::Char('s') => self.open_restore(true),
            _ => {}
        }
    }

    fn start_backup(&mut self) {
        if self.backup.running {
            return;
        }
        let customers = self.nights.first().and_then(|n| n.status.as_ref()).map_or(4, |s| s.customers.len());
        self.backup = Backup { running: true, total: 11 + customers, ..Default::default() };
        let (cfg, tx) = (self.cfg.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let res = run::run(&cfg, &|e| {
                let _ = tx.send(Msg::Backup(e));
            });
            let sent = match &res {
                Ok(st) => notify::report(&cfg, st),
                Err(e) => notify::send(&cfg, &format!("**Minipass backup FAILED**\n- {e:#}")),
            };
            let _ = tx.send(Msg::BackupEnd(res.map_err(|e| format!("{e:#}")), sent.err().map(|e| format!("{e:#}"))));
        });
    }

    fn open_restore(&mut self, setup: bool) {
        self.pop = Some(Pop::Restore);
        if matches!(self.restore.stage, Stage::Running | Stage::Checking) {
            return; // the one that's going on stays on screen
        }
        let choices = if setup {
            SETUP_CHOICES.iter().map(|c| c.to_string()).collect()
        } else {
            self.night().and_then(|n| n.status.as_ref()).map(|s| s.customers.clone()).unwrap_or_default()
        };
        self.restore = RestoreUi { setup, choices, mail: [true; 3], ..Default::default() };
    }

    /// The same job the command line would make from what's picked on screen.
    fn picked_job(&self) -> Option<Job> {
        let r = &self.restore;
        let what = if r.setup {
            match r.cursor {
                0 => What::MailSettings(
                    MAIL_PARTS.iter().zip(r.mail).filter(|(_, on)| *on).map(|(p, _)| p.to_string()).collect(),
                ),
                1 => What::MailMessages,
                2 => What::Website,
                3 => What::Server,
                4 => What::Everything,
                _ => What::Undo,
            }
        } else {
            What::Customer(r.choices.get(r.cursor)?.clone())
        };
        if matches!(&what, What::MailSettings(parts) if parts.is_empty()) {
            return None;
        }
        Some(Job { what, night: self.night().map(|n| n.date.clone()) })
    }

    fn start_restore(&mut self) {
        let Some(job) = self.picked_job() else { return };
        self.restore.stage = Stage::Checking;
        self.restore.error = None;
        self.restore.report = None;
        self.restore.log.clear();
        self.restore.steps.clear();
        let (cfg, tx) = (self.cfg.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::RestoreReady(restore::prepare(&cfg, &job).map_err(|e| format!("{e:#}"))));
        });
    }

    fn confirm_restore(&mut self) {
        let Some(p) = self.restore.prepared.clone() else { return };
        // The real server asks about the snapshot first, in the same box.
        if p.real_vps && !self.restore.snapshot {
            if self.restore.typed.trim() == "yes" {
                self.restore.snapshot = true;
            } else {
                self.restore.error = Some("Take a Hetzner snapshot first. Nothing was changed.".into());
                self.restore.stage = Stage::Done;
            }
            self.restore.typed.clear();
            return;
        }
        let typed = std::mem::take(&mut self.restore.typed);
        let snapshot = self.restore.snapshot;
        self.restore.stage = Stage::Running;
        let (cfg, tx) = (self.cfg.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let on = |e| {
                let _ = tx.send(Msg::Restore(e));
            };
            let res = restore::execute_and_report(&cfg, &p, &typed, snapshot, &on);
            let _ = tx.send(Msg::RestoreEnd(res.map_err(|e| format!("{e:#}"))));
        });
    }

    fn start_compare(&mut self) {
        if self.compare.as_ref().is_some_and(|j| j.result.is_none()) {
            return; // already comparing
        }
        let Some(n) = self.night() else {
            self.compare = None;
            return;
        };
        let (cfg, tx, path) = (self.cfg.clone(), self.tx.clone(), n.path.clone());
        self.compare = Some(SlowJob { night: n.date.clone(), result: None });
        self.scroll = 0;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Compared(compare::compare(&cfg, &path).map_err(|e| format!("{e:#}"))));
        });
    }

    fn start_unpack(&mut self) {
        if self.unpack.is_some() {
            return; // running, or finished: press u again for another night
        }
        let Some(n) = self.night() else { return };
        let (tx, path, dest) = (self.tx.clone(), n.path.clone(), unpack::dest_for(&n.date));
        self.unpack = Some(SlowJob { night: n.date.clone(), result: None });
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Unpacked(unpack::unpack(&path, &dest).map_err(|e| format!("{e:#}"))));
        });
    }

    // ── drawing ──

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        f.render_widget(Block::new().style(Style::new().bg(BG).fg(FG)), area);
        let [cards, mid, actions, keys] =
            Layout::vertical([Constraint::Length(5), Constraint::Fill(1), Constraint::Length(3), Constraint::Length(1)])
                .areas(area);
        self.draw_cards(f, cards);
        let [left, right] = Layout::horizontal([Constraint::Length(40), Constraint::Fill(1)]).areas(mid);
        self.draw_nights(f, left);
        self.draw_checks(f, right);
        draw_actions(f, actions);
        match &self.note {
            Some(n) => f.render_widget(Line::styled(format!(" {n}"), Style::new().fg(WARN).bold()), keys),
            None => f.render_widget(
                key_hints(&[("↑↓", "pick a night"), ("letter", "open action"), ("Esc", "close pop-up"), ("q", "quit")]),
                keys,
            ),
        }
        if let Some(p) = self.pop {
            self.draw_popup(f, p, area);
        }
    }

    fn draw_cards(&self, f: &mut Frame, area: Rect) {
        let newest = self.nights.first().and_then(|n| n.status.as_ref());
        let before = self.nights.get(1).and_then(|n| n.status.as_ref());

        let (big, color, sub, sub_color) = if self.backup.running {
            let step = self.backup.current.clone().unwrap_or_default();
            ("RUNNING".to_string(), WARN, step, DIM)
        } else {
            match &self.last {
                None => ("none yet".into(), DIM, "press b".into(), DIM),
                Some(st) => {
                    let late = age_hours(&st.finished).is_some_and(|h| h > 26);
                    let sub = match age_hours(&st.finished) {
                        Some(h) if late => format!("{h} h ago, should be daily"),
                        _ => when(&st.finished),
                    };
                    let (big, color) = if st.ok { ("OK", OK) } else { ("FAILED", BAD) };
                    (big.into(), color, sub, if late { BAD } else { DIM })
                }
            }
        };
        let size = newest.map(|s| human(s.size_bytes)).unwrap_or_else(|| "-".into());
        let size_sub = match (newest, before) {
            (Some(a), Some(b)) => {
                let d = a.size_bytes as i64 - b.size_bytes as i64;
                format!("{}{} vs night before", if d < 0 { "-" } else { "+" }, human(d.unsigned_abs()))
            }
            (Some(_), None) => "first backup".into(),
            _ => String::new(),
        };
        let customers = newest.map(|s| s.customers.clone()).unwrap_or_default();
        let (next, next_color, next_sub) = match self.next_run {
            Some(t) => {
                let mins = (t - Local::now().naive_local()).num_minutes().max(0);
                let left = if mins >= 60 { format!("in {} h {} min", mins / 60, mins % 60) } else { format!("in {mins} min") };
                (t.format("%H:%M").to_string(), FG, left)
            }
            None => ("off".into(), BAD, "nightly timer is not running".into()),
        };

        let cards = [
            ("Last backup", big, color, sub, sub_color),
            ("Size", size, FG, size_sub, DIM),
            ("Customers", customers.len().to_string(), FG, customers.join(" "), DIM),
            ("Next run", next, next_color, next_sub, DIM),
        ];
        let areas = Layout::horizontal([Constraint::Fill(1); 4]).split(area);
        for ((title, big, color, sub, sub_color), a) in cards.into_iter().zip(areas.iter()) {
            let text = vec![
                Line::styled(big, Style::new().fg(color).bold()),
                Line::raw(""),
                Line::styled(sub, Style::new().fg(sub_color)),
            ];
            f.render_widget(Paragraph::new(text).alignment(Alignment::Center).block(panel(title, LINE)), *a);
        }
    }

    fn draw_nights(&mut self, f: &mut Frame, area: Rect) {
        let block = panel(&format!("Nights kept · {}", self.nights.len()), ACC);
        if self.nights.is_empty() {
            let p = Paragraph::new("\n No backup yet. Press b to back up now.").fg(DIM).block(block);
            f.render_widget(p, area);
            return;
        }
        let rows = self.nights.iter().map(|n| {
            let (res, color) = match &n.status {
                Some(s) if s.ok => ("OK", OK),
                Some(_) => ("FAILED", BAD),
                None => ("?", DIM),
            };
            let size = n.status.as_ref().map(|s| human(s.size_bytes)).unwrap_or_default();
            Row::new([Cell::from(n.date.clone()), Cell::from(res).fg(color).bold(), Cell::from(size)])
        });
        let table = Table::new(rows, [Constraint::Length(12), Constraint::Length(8), Constraint::Fill(1)])
            .header(Row::new(["Night", "Result", "Size"]).fg(DIM).bold())
            .row_highlight_style(Style::new().bg(SEL))
            .highlight_symbol(Span::styled("> ", Style::new().fg(ACC).bold()))
            .column_spacing(2)
            .block(block);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_checks(&self, f: &mut Frame, area: Rect) {
        let Some(n) = self.night() else {
            f.render_widget(panel("Checks", LINE), area);
            return;
        };
        let Some(st) = &n.status else {
            let p = Paragraph::new("\n No record for this night.").fg(DIM).block(panel(&format!("Checks · {}", n.date), LINE));
            f.render_widget(p, area);
            return;
        };
        let title = format!("Checks · {} · finished {}", n.date, when(&st.finished));
        let name_w = st.steps.iter().map(|s| s.name.chars().count()).max().unwrap_or(10) as u16;
        let rows = st.steps.iter().map(|s| {
            let (mark, color) = if s.ok { (" ok ", OK) } else { ("FAIL", BAD) };
            Row::new([
                Cell::from(mark).fg(color).bold(),
                Cell::from(s.name.clone()),
                Cell::from(s.detail.clone()).fg(if s.ok { DIM } else { BAD }),
            ])
        });
        let table = Table::new(
            rows,
            [Constraint::Length(4), Constraint::Length(name_w.min(area.width.saturating_sub(34))), Constraint::Fill(1)],
        )
        .column_spacing(2)
        .block(panel(&title, LINE));
        f.render_widget(table, area);
    }

    fn draw_popup(&self, f: &mut Frame, p: Pop, area: Rect) {
        let w = area.width.saturating_sub(4).min(90);
        let h = area.height.saturating_sub(2).min(match p {
            Pop::Compare => 34,
            Pop::Backup | Pop::Restore => 26,
            _ => 18,
        });
        let rect = area.centered(Constraint::Length(w), Constraint::Length(h));
        f.render_widget(Clear, rect);
        f.render_widget(Block::new().bg(BG), rect);
        let (title, color) = match p {
            Pop::Backup => ("Back up now".to_string(), ACC),
            Pop::Compare => match &self.compare {
                Some(j) => (format!("Compare · backup {} vs server now", j.night), ACC),
                None => ("Compare".into(), ACC),
            },
            Pop::Unpack => ("Unpack a night on this computer".into(), ACC),
            Pop::Guide => ("Emergency guide".into(), ACC),
            Pop::Restore => ((if self.restore.setup { "Restore setup" } else { "Restore a customer" }).to_string(), MAG),
        };
        let block = panel(&title, color);
        let inner = block.inner(rect).inner(ratatui::layout::Margin::new(1, 1));
        f.render_widget(block, rect);
        let [body, foot] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
        let foot_text = match p {
            Pop::Backup if self.backup.running => "Esc to hide (the backup keeps going)",
            Pop::Backup if self.backup.end.is_some() => "Enter to run again     Esc to close",
            Pop::Backup => "Enter to start     Esc to close",
            Pop::Compare => "c to compare again     ↑↓ scroll     Esc to close",
            Pop::Unpack if self.unpack.is_none() => "↑↓ pick a night     Enter to unpack     Esc to close",
            Pop::Restore => match self.restore.stage {
                Stage::Pick if self.restore.setup && self.restore.cursor == 0 => "↑↓ pick     a f d  choose settings     Enter to preview     Esc to close",
                Stage::Pick => "↑↓ pick     Enter to preview (changes nothing)     Esc to close",
                Stage::Checking => "Checking the backup...",
                Stage::Confirm => "Type it, then Enter     Esc to go back (nothing has changed)",
                Stage::Running => "Esc to hide (the restore keeps going)",
                Stage::Done => "Enter to pick again     Esc to close",
            },
            _ => "Esc to close",
        };
        f.render_widget(Line::styled(foot_text, Style::new().fg(DIM)), foot);
        match p {
            Pop::Backup => self.draw_backup(f, body),
            Pop::Compare => self.draw_compare(f, body),
            Pop::Unpack => self.draw_unpack(f, body),
            Pop::Guide => {
                let mut lines = vec![Line::styled("Something broke? Do it in this order:", Style::new().fg(CYAN).bold()), Line::raw("")];
                lines.extend(GUIDE.iter().map(|l| {
                    Line::styled(*l, Style::new().fg(if l.starts_with(char::is_numeric) || l.starts_with("   ") { FG } else { DIM }))
                }));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled("Backups are in  ", Style::new().fg(DIM)),
                    Span::styled(self.cfg.root().display().to_string(), Style::new().fg(CYAN)),
                ]));
                f.render_widget(Paragraph::new(lines), body);
            }
            Pop::Restore => self.draw_restore(f, body),
        }
    }

    fn draw_restore(&self, f: &mut Frame, area: Rect) {
        let dim = Style::new().fg(DIM);
        let r = &self.restore;
        let night = self.night().map(|n| n.date.clone()).unwrap_or_else(|| "none".into());
        let mut lines = vec![];
        match r.stage {
            Stage::Pick => {
                lines.push(Line::from(vec![Span::styled("From the night of  ", dim), Span::styled(night, Style::new().bold())]));
                lines.push(Line::styled("Pick another night with ↑↓ on the main screen, before opening this.", dim));
                lines.push(Line::raw(""));
                if r.choices.is_empty() {
                    lines.push(Line::styled("This night holds no customers.", dim));
                }
                for (i, c) in r.choices.iter().enumerate() {
                    let here = i == r.cursor;
                    let listed = r.setup || self.cfg.restore_allowed.contains(c);
                    let color = if here { MAG } else if listed { FG } else { DIM };
                    let note = if listed { "" } else { "   (not on restore_allowed)" };
                    lines.push(Line::styled(format!("{} {c}{note}", if here { ">" } else { " " }), Style::new().fg(color)));
                    if r.setup && i == 0 {
                        let boxes: Vec<String> = ["a accounts", "f forwards", "d DKIM key"]
                            .iter()
                            .zip(r.mail)
                            .map(|(name, on)| format!("[{}] {name}", if on { "x" } else { " " }))
                            .collect();
                        lines.push(Line::styled(format!("    {}", boxes.join("   ")), dim));
                    }
                }
                lines.push(Line::raw(""));
                if !self.cfg.restore_enabled {
                    lines.push(Line::styled("Restore is off: restore_enabled is false in the config.", Style::new().fg(WARN)));
                }
            }
            Stage::Checking => {
                lines.push(Line::styled(format!("{} Checking the backup and comparing...", SPIN[self.tick % SPIN.len()]), Style::new().fg(WARN)));
            }
            Stage::Confirm => {
                let Some(p) = &r.prepared else { return };
                let from = if p.night.is_empty() { String::new() } else { format!(" from {}", p.night) };
                lines.push(Line::styled(format!("Restore {}{from}", p.job.what.label()), Style::new().fg(CYAN).bold()));
                lines.push(Line::styled("What differs now. Nothing has changed yet.", dim));
                lines.push(Line::raw(""));
                lines.extend(p.preview.iter().map(|l| Line::raw(format!("  {l}"))));
                lines.push(Line::raw(""));
                let question = if p.real_vps && !r.snapshot {
                    "This is the real server. Did you take a Hetzner snapshot? Type yes:".to_string()
                } else {
                    format!("Type {} to go ahead:", p.job.what.confirm_word())
                };
                lines.push(Line::styled(question, Style::new().fg(MAG).bold()));
                lines.push(Line::from(vec![Span::raw("> "), Span::styled(format!("{}█", r.typed), Style::new().bold())]));
            }
            Stage::Running | Stage::Done => return self.draw_restore_progress(f, area),
        }
        // A long preview scrolls, so the question at the end stays on screen.
        let scroll = rows(&lines, area.width).saturating_sub(area.height);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)), area);
    }

    /// The newest log lines on top, and under them the results, which must never scroll away.
    fn draw_restore_progress(&self, f: &mut Frame, area: Rect) {
        let r = &self.restore;
        let mut results = vec![];
        for s in &r.steps {
            let (mark, color) = if s.ok { ("done", OK) } else { ("FAIL", BAD) };
            results.push(Line::from(vec![
                Span::styled(format!("{mark}  "), Style::new().fg(color).bold()),
                Span::raw(format!("{}: ", s.name)),
                Span::styled(s.detail.clone(), Style::new().fg(if s.ok { DIM } else { BAD })),
            ]));
        }
        if r.stage == Stage::Running {
            results.push(Line::styled(format!("{} Working...", SPIN[self.tick % SPIN.len()]), Style::new().fg(WARN)));
        }
        if let Some(report) = &r.report {
            let (text, color) = if report.ok() { ("OK", OK) } else { ("FAILED", BAD) };
            results.push(Line::styled(format!("Restore of {} {text}. Took {}.", report.label, report.took()), Style::new().fg(color).bold()));
        }
        if let Some(e) = &r.error {
            results.push(Line::styled(format!("Refused or stopped: {e}"), Style::new().fg(BAD).bold()));
        }
        let [top, bottom] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(rows(&results, area.width).min(area.height))]).areas(area);
        let skip = r.log.len().saturating_sub(top.height as usize);
        let log: Vec<Line> = r.log[skip..].iter().map(|l| Line::styled(l.clone(), Style::new().fg(DIM))).collect();
        f.render_widget(Paragraph::new(log), top);
        f.render_widget(Paragraph::new(results).wrap(Wrap { trim: false }), bottom);
    }

    fn draw_backup(&self, f: &mut Frame, area: Rect) {
        let b = &self.backup;
        if !b.running && b.end.is_none() {
            let lines = vec![
                Line::raw(""),
                Line::raw("Copies everything from the server that can't be rebuilt:"),
                Line::styled("customers, mail, keys and passwords, server settings.", Style::new().fg(DIM)),
                Line::raw(""),
                Line::raw("Only reads from the server. Takes about a minute."),
                Line::styled("A Discord message is sent at the end, like every night.", Style::new().fg(DIM)),
            ];
            f.render_widget(Paragraph::new(lines), area);
            return;
        }
        let [steps, _, gauge, _, result] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let mut lines: Vec<Line> = b
            .done
            .iter()
            .map(|s| {
                let (mark, color) = if s.ok { ("done", OK) } else { ("FAIL", BAD) };
                Line::from(vec![
                    Span::styled(format!("{mark}  "), Style::new().fg(color).bold()),
                    Span::raw(format!("{:<49}", s.name)),
                    Span::styled(s.detail.clone(), Style::new().fg(if s.ok { DIM } else { BAD })),
                ])
            })
            .collect();
        if let Some(cur) = &b.current {
            lines.push(Line::from(vec![
                Span::styled(format!("{}     ", SPIN[self.tick % SPIN.len()]), Style::new().fg(WARN).bold()),
                Span::raw(cur.clone()),
            ]));
        }
        let skip = lines.len().saturating_sub(steps.height as usize);
        f.render_widget(Paragraph::new(lines.split_off(skip)), steps);

        let ratio = if b.end.is_some() { 1.0 } else { (b.done.len() as f64 / b.total.max(1) as f64).min(0.99) };
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .label(format!("{:.0} %", ratio * 100.0))
                .gauge_style(Style::new().fg(ACC).bg(LINE))
                .use_unicode(true),
            gauge,
        );
        let res = match &b.end {
            None => Line::styled("Working...", Style::new().fg(DIM)),
            Some(Ok(st)) if st.ok => {
                Line::styled(format!("Backup OK, {}. Saved as {}.", human(st.size_bytes), st.night), Style::new().fg(OK).bold())
            }
            Some(Ok(st)) => Line::styled(
                format!("Backup FAILED: {} step(s) in red. Kept what worked.", st.failed_steps().len()),
                Style::new().fg(BAD).bold(),
            ),
            Some(Err(e)) => Line::styled(format!("Backup stopped: {e}"), Style::new().fg(BAD).bold()),
        };
        f.render_widget(res, result);
    }

    fn draw_compare(&self, f: &mut Frame, area: Rect) {
        let dim = Style::new().fg(DIM);
        let mut lines = vec![Line::styled("Nothing is changed. This only looks.", dim), Line::raw("")];
        match self.compare.as_ref().map(|j| &j.result) {
            None => lines.push(Line::styled("No backup to compare yet.", dim)),
            Some(None) => lines.push(Line::styled(
                format!("{} Comparing with the live server...", SPIN[self.tick % SPIN.len()]),
                Style::new().fg(WARN),
            )),
            Some(Some(Err(e))) => lines.push(Line::styled(format!("Could not compare: {e}"), Style::new().fg(BAD))),
            Some(Some(Ok(rows))) => {
                let n = rows.iter().filter(|r| r.differs).count();
                lines.push(if n == 0 {
                    Line::styled("Everything matches the server.", Style::new().fg(OK).bold())
                } else {
                    Line::from(vec![
                        Span::styled(format!("{n} differ (yellow). "), Style::new().fg(WARN).bold()),
                        Span::styled("Normal for an older backup: the server keeps changing.", dim),
                    ])
                });
                let (mut group, mut who) = ("", String::new());
                for r in rows {
                    if r.group != group {
                        lines.push(Line::raw(""));
                        lines.push(Line::styled(r.group, Style::new().fg(CYAN).bold()));
                        group = r.group;
                    }
                    let name = if r.who != who { r.who.clone() } else { String::new() };
                    who = r.who.clone();
                    let result = Span::styled(
                        r.result.clone(),
                        if r.differs { Style::new().fg(WARN).bold() } else { Style::new().fg(OK) },
                    );
                    lines.push(if r.group == "Customers" {
                        Line::from(vec![
                            Span::styled(format!("  {name:<8}"), Style::new().bold()),
                            Span::raw(format!("{:<12}", r.what)),
                            result,
                        ])
                    } else {
                        Line::from(vec![Span::raw(format!("  {:<28}", r.what)), result])
                    });
                }
            }
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), area);
    }

    fn draw_unpack(&self, f: &mut Frame, area: Rect) {
        let dim = Style::new().fg(DIM);
        let mut lines = vec![
            Line::styled("Puts one night in a folder here, to look at or test. The server is not touched.", dim),
            Line::raw(""),
        ];
        let night = match &self.unpack {
            Some(j) => j.night.clone(),
            None => self.night().map(|n| n.date.clone()).unwrap_or_default(),
        };
        if night.is_empty() {
            lines.push(Line::styled("No backup yet.", dim));
            f.render_widget(Paragraph::new(lines), area);
            return;
        }
        let dest = unpack::dest_for(&night);
        lines.push(Line::from(vec![Span::styled("Night     ", dim), Span::styled(night, Style::new().bold())]));
        lines.push(Line::from(vec![
            Span::styled("Goes to   ", dim),
            Span::styled(format!("{}/", dest.display()), Style::new().fg(CYAN)),
        ]));
        lines.push(Line::raw(""));
        lines.push(match &self.unpack {
            None if dest.exists() => Line::styled("Already unpacked there.", Style::new().fg(WARN)),
            None => Line::styled("Mail and certificates are unpacked too, ready to read.", dim),
            Some(SlowJob { result: None, .. }) => {
                Line::styled(format!("{} Unpacking...", SPIN[self.tick % SPIN.len()]), Style::new().fg(WARN))
            }
            Some(SlowJob { result: Some(Ok(msg)), .. }) => Line::styled(msg.clone(), Style::new().fg(OK).bold()),
            Some(SlowJob { result: Some(Err(e)), .. }) => Line::styled(e.clone(), Style::new().fg(BAD)),
        });
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

/// About how many screen rows these lines take once wrapped.
fn rows(lines: &[Line], width: u16) -> u16 {
    lines.iter().map(|l| (l.width() as u16 / width.max(1)) + 1).sum()
}

/// A bordered card with a rounded frame and its title set into the top line: ╭─ Title ───╮
fn panel(title: &str, color: Color) -> Block<'static> {
    let title_color = if color == LINE { FG } else { color };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(vec![
            Span::styled("─", Style::new().fg(color)),
            Span::styled(format!(" {title} "), Style::new().fg(title_color).add_modifier(Modifier::BOLD)),
        ]))
}

fn draw_actions(f: &mut Frame, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    let items = [
        ("b", "Back up now", ACC),
        ("c", "Compare", ACC),
        ("u", "Unpack", ACC),
        ("g", "Guide", ACC),
        ("r", "Restore customer", MAG),
        ("s", "Restore setup", MAG),
    ];
    for (k, label, color) in items {
        if k == "r" {
            spans.push(Span::styled("│   ", Style::new().fg(LINE)));
        }
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(DARK).bg(color).bold()));
        spans.push(Span::styled(format!(" {label}    "), Style::new().fg(if color == MAG { MAG } else { FG })));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).block(panel("Actions", LINE)), area);
}

fn key_hints(list: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, label) in list {
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(DARK).bg(ACC).bold()));
        spans.push(Span::styled(format!(" {label}   "), Style::new().fg(DIM)));
    }
    Line::from(spans)
}

/// When the nightly timer fires next, as systemd reports it ("Mon 2026-10-19 03:04:37 EDT").
pub(crate) fn next_run() -> Option<NaiveDateTime> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "mptool-backup.timer", "--property=NextElapseUSecRealtime", "--value"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace().skip(1);
    let (date, time) = (parts.next()?, parts.next()?);
    NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S").ok()
}

fn parse(ts: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S").ok()
}

/// "today 03:04", "yesterday 03:04", "Oct 12 03:04".
pub(crate) fn when(ts: &str) -> String {
    let Some(t) = parse(ts) else { return ts.to_string() };
    let today = Local::now().date_naive();
    let day = if t.date() == today {
        "today".to_string()
    } else if t.date() == today - Days::days(1) {
        "yesterday".to_string()
    } else {
        t.format("%b %d").to_string()
    };
    format!("{day} {}", t.format("%H:%M"))
}

pub(crate) fn age_hours(ts: &str) -> Option<i64> {
    parse(ts).map(|t| (Local::now().naive_local() - t).num_hours())
}
