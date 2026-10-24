//! The menu (`mpbackup` with no arguments): one dashboard screen, actions open in pop-ups.
//! Everything here only reads from the server. Restores come later and will be the only writers.

use crate::compare::{self, Line as CmpLine};
use crate::config::Config;
use crate::notify;
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
    "3  One customer lost data:   Restore a customer.",
    "4  Mail, keys or forwards broken:   Restore setup.",
    "5  Whole server gone:   new server from the Hetzner snapshot,",
    "   then Restore setup and each customer from last night.",
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
}

#[derive(Default)]
struct Backup {
    running: bool,
    total: usize,
    done: Vec<Step>,
    current: Option<String>,
    end: Option<Result<Status, String>>,
}

/// A slower job (compare, unpack): which night, and its result once finished.
struct Job<T> {
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
    compare: Option<Job<Vec<CmpLine>>>,
    unpack: Option<Job<String>>,
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
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.note = None;
        if code == KeyCode::Char('q') || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL)) {
            if self.backup.running {
                self.note = Some("A backup is running. Wait for it to finish, then quit.".into());
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
                _ => {}
            },
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
            KeyCode::Char('r') | KeyCode::Char('s') => self.pop = Some(Pop::Restore),
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

    fn start_compare(&mut self) {
        if self.compare.as_ref().is_some_and(|j| j.result.is_none()) {
            return; // already comparing
        }
        let Some(n) = self.night() else {
            self.compare = None;
            return;
        };
        let (cfg, tx, path) = (self.cfg.clone(), self.tx.clone(), n.path.clone());
        self.compare = Some(Job { night: n.date.clone(), result: None });
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
        self.unpack = Some(Job { night: n.date.clone(), result: None });
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
            Pop::Backup => 26,
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
            Pop::Restore => ("Restore".into(), MAG),
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
            Pop::Restore => {
                let lines = vec![
                    Line::raw(""),
                    Line::styled("Restores are the next step.", Style::new().fg(MAG).bold()),
                    Line::raw(""),
                    Line::raw("They get built after you approve this menu, and are tested on demo first."),
                    Line::raw("Until then, this tool never changes anything on the server."),
                ];
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
            }
        }
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
            Some(Job { result: None, .. }) => {
                Line::styled(format!("{} Unpacking...", SPIN[self.tick % SPIN.len()]), Style::new().fg(WARN))
            }
            Some(Job { result: Some(Ok(msg)), .. }) => Line::styled(msg.clone(), Style::new().fg(OK).bold()),
            Some(Job { result: Some(Err(e)), .. }) => Line::styled(e.clone(), Style::new().fg(BAD)),
        });
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
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
fn next_run() -> Option<NaiveDateTime> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "mpbackup.timer", "--property=NextElapseUSecRealtime", "--value"])
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
fn when(ts: &str) -> String {
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

fn age_hours(ts: &str) -> Option<i64> {
    parse(ts).map(|t| (Local::now().naive_local() - t).num_hours())
}
