//! hypertop: peek at and act on many devices at once, from one terminal.
//!
//! It knows nothing about any kind of device. Every executable in the sources
//! directory describes some devices as JSON lines: their numbers, what to peek at,
//! and which commands are actions. See the README for the format.

mod model;
mod run;
mod ui;

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use model::{Action, GroupBy, Model, Row, State};
use run::{Msg, Running};

/// How often to look for news from the sources. Input doesn't wait for this.
const FRAME: Duration = Duration::from_millis(100);
/// A redraw with no news, for what depends on the clock ("up for 45s").
const CLOCK: Duration = Duration::from_secs(1);
/// A peek starts once the selection has stayed put this long, so that passing over
/// a device doesn't ask it anything.
const SETTLE: Duration = Duration::from_millis(150);
/// Messages that can wait for the main loop, which is busy elsewhere during a shell.
/// When it's full, what produces them waits instead.
const BACKLOG: usize = 4096;
/// How many it takes in one go, so that a flood can't keep input waiting.
const PUMP_LIMIT: usize = 2000;
const NOTICE: Duration = Duration::from_secs(6);
const PAGE: i32 = 10;

const USAGE: &str = "usage: hypertop [--list] [--sources DIR] [--wait SECONDS]

Peek at and act on many devices at once. Devices come from the executables in the
sources directory; each prints JSON lines describing some devices. See the README.

  --list      print what the sources report, after --wait seconds, and exit
  --sources   the directory (default: $HYPERTOP_SOURCES, or ~/.config/hypertop/sources)
  --wait      how long --list collects (default 6)";

struct Opts {
    list: bool,
    dir: PathBuf,
    wait: u64,
}

fn default_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HYPERTOP_SOURCES") {
        return dir.into();
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/hypertop/sources")
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
    let mut opts = Opts { list: false, dir: default_dir(), wait: 6 };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--list" => opts.list = true,
            "--sources" => opts.dir = args.next().ok_or("--sources needs a value")?.into(),
            "--wait" => {
                let value = args.next().ok_or("--wait needs a value")?;
                opts.wait = value.parse().map_err(|_| format!("bad number {value}"))?;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(opts)
}

/// One action on one device.
pub struct Job {
    pub device: String,
    pub label: String,
    pub output: String,
    pub outcome: Option<Result<i32, String>>,
    pub started: Instant,
    pub took: Option<Duration>,
}

/// A choice in the action menu: an action, and on how many of the targets.
pub struct MenuItem {
    pub id: String,
    pub label: String,
    pub tty: bool,
    pub confirm: bool,
    pub count: usize,
}

/// An action, decided: what runs, and where. It is made when you ask, and carried through
/// any question it needs, so what you confirmed is what happens, whatever the sources
/// say and wherever the selection goes in the meantime.
pub struct Plan {
    pub label: String,
    pub prompt: Option<String>,
    pub confirm: bool,
    pub tty: bool,
    /// Each device's name and its own action.
    pub steps: Vec<(String, Action)>,
}

impl Plan {
    /// What it applies to, in a few words.
    pub fn who(&self) -> String {
        match self.steps.as_slice() {
            [(name, _)] => name.clone(),
            steps => format!("{} devices", steps.len()),
        }
    }
}

pub enum Mode {
    Normal,
    Filter,
    Menu { items: Vec<MenuItem>, at: usize, targets: Vec<String> },
    Input { plan: Plan, text: String },
    Confirm { plan: Plan, input: Option<String> },
    Jobs { scroll: u16 },
    Help,
}

pub struct PeekState {
    pub text: String,
    ran: Instant,
    run: Vec<String>,
}

/// The peek being run. There is one at most: the one on screen.
struct ActivePeek {
    generation: u64,
    key: String,
    label: String,
    stop: run::Stop,
}

pub struct App {
    /// The machine this is running on, as shown in the title. Empty if it won't say.
    pub host: String,
    pub model: Model,
    pub selected: Option<String>,
    /// You have moved. Until then the first row stays selected, in whatever order
    /// the sources happen to answer at startup.
    chosen: bool,
    pub top: usize,
    pub filter: String,
    pub hide_down: bool,
    pub group_by: GroupBy,
    pub detail_open: bool,
    /// 0 is the overview; after that, the selected device's peeks.
    pub tab: usize,
    pub detail_scroll: u16,
    pub mode: Mode,
    pub jobs: Vec<Job>,
    pub notice: Option<(String, Instant)>,
    pub peeks: HashMap<(String, String), PeekState>,
    active_peek: Option<ActivePeek>,
    generation: u64,
    moved: Instant,
    /// The last pump left messages waiting.
    behind: bool,
    /// What's listed, worked out again only when the model or the view changes.
    pub rows: Vec<Row>,
    /// Where the rows landed on screen, for the mouse.
    pub list_area: ratatui::layout::Rect,
    pub list_lines: Vec<ui::ListLine>,
    pub sources: usize,
    pub dir: PathBuf,
    tty_request: Option<Vec<String>>,
    wakes: Vec<Sender<()>>,
    running: Running,
    tx: SyncSender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    fn new(opts: &Opts) -> App {
        let (tx, rx) = mpsc::sync_channel(BACKLOG);
        let running = Running::default();
        let found = run::find_sources(&opts.dir);
        let sources = found.len();
        let wakes = found.into_iter().map(|path| run::spawn_source(path, tx.clone(), running.clone())).collect();
        App {
            host: host_name(),
            model: Model::default(),
            selected: None,
            chosen: false,
            top: 0,
            filter: String::new(),
            hide_down: false,
            group_by: GroupBy::Group,
            detail_open: true,
            tab: 0,
            detail_scroll: 0,
            mode: Mode::Normal,
            jobs: Vec::new(),
            notice: None,
            peeks: HashMap::new(),
            active_peek: None,
            generation: 0,
            moved: Instant::now(),
            behind: false,
            rows: Vec::new(),
            list_area: Default::default(),
            list_lines: Vec::new(),
            sources,
            dir: opts.dir.clone(),
            tty_request: None,
            wakes,
            running,
            tx,
            rx,
        }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    fn device_keys(&self) -> impl Iterator<Item = &String> {
        self.rows.iter().filter_map(|row| match row {
            Row::Device(key) => Some(key),
            Row::Header { .. } => None,
        })
    }

    /// The model or the view changed. The selection follows its device; if that went
    /// away, take the first.
    fn refresh_rows(&mut self) {
        self.rows = self.model.rows(&self.filter, self.hide_down, self.group_by);
        let present = self.selected.as_ref().is_some_and(|s| self.device_keys().any(|k| k == s));
        if !self.chosen || !present {
            let first = self.device_keys().next().cloned();
            if self.selected != first {
                self.selected = first;
                self.tab = 0;
                self.detail_scroll = 0;
                self.moved = Instant::now();
            }
        }
        let model = &self.model;
        self.peeks.retain(|(key, _), _| model.get(key).is_some());
    }

    /// Take in what happened in the background. True if the screen may have to change.
    fn pump(&mut self) -> bool {
        let mut changed = false;
        let mut devices_changed = false;
        let mut taken = 0;
        while taken < PUMP_LIMIT {
            let Ok(message) = self.rx.try_recv() else { break };
            taken += 1;
            changed = true;
            match message {
                Msg::Line { source, line } => {
                    self.model.line(&source, &line);
                    devices_changed = true;
                }
                Msg::Trouble { source, why } => {
                    self.model.source_trouble(&source, &why);
                    devices_changed = true;
                }
                Msg::Exited { source, success, why } => {
                    self.model.source_exited(&source, success, &why);
                    devices_changed = true;
                }
                Msg::Peek { generation, key, label, run, text } => {
                    // The answer to a peek you have looked away from isn't wanted.
                    if self.active_peek.as_ref().is_some_and(|a| a.generation == generation) {
                        self.active_peek = None;
                        self.peeks.insert((key, label), PeekState { text, ran: Instant::now(), run });
                    }
                }
                Msg::JobOutput { job, text } => {
                    if let Some(job) = self.jobs.get_mut(job) {
                        job.output.push_str(&text);
                        run::keep_tail(&mut job.output);
                    }
                }
                Msg::JobDone { job, outcome } => self.job_done(job, outcome),
            }
        }
        self.behind = taken == PUMP_LIMIT;
        if devices_changed {
            self.refresh_rows();
        }
        if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() >= NOTICE) {
            self.notice = None;
            changed = true;
        }
        self.run_visible_peek();
        changed
    }

    fn job_done(&mut self, job: usize, outcome: Result<i32, String>) {
        let Some(entry) = self.jobs.get_mut(job) else { return };
        entry.took = Some(entry.started.elapsed());
        let line = match &outcome {
            Ok(0) => format!("{} on {}: done", entry.label, entry.device),
            Ok(code) => format!("{} on {}: failed (exit {code}). o shows it", entry.label, entry.device),
            Err(why) => format!("{} on {}: {why}. o shows it", entry.label, entry.device),
        };
        entry.outcome = Some(outcome);
        self.say(line);
    }

    /// The peek you can see right now, if any.
    fn peek_on_screen(&self) -> Option<(String, model::Peek)> {
        if !self.detail_open || self.tab == 0 || matches!(self.mode, Mode::Jobs { .. }) {
            return None;
        }
        let key = self.selected.clone()?;
        let peek = self.model.get(&key)?.peeks.get(self.tab - 1)?.clone();
        Some((key, peek))
    }

    /// Peeks run only for the tab on screen, and stop when you look away: nothing is
    /// asked of a device unless someone is reading the answer.
    fn run_visible_peek(&mut self) {
        let wanted = self.peek_on_screen();
        if let Some(active) = &self.active_peek {
            if wanted.as_ref().is_some_and(|(key, peek)| *key == active.key && peek.label == active.label) {
                return;
            }
            active.stop.now();
            self.active_peek = None;
        }
        let Some((key, peek)) = wanted else { return };
        if self.moved.elapsed() < SETTLE {
            return;
        }
        let known = self.peeks.get(&(key.clone(), peek.label.clone()));
        if known.is_some_and(|state| state.run == peek.run && state.ran.elapsed() < Duration::from_secs(peek.every)) {
            return;
        }
        self.generation += 1;
        let stop = run::spawn_peek(self.generation, key.clone(), peek.label.clone(), peek.run, self.tx.clone());
        self.active_peek = Some(ActivePeek { generation: self.generation, key, label: peek.label, stop });
    }

    /// Everything that would otherwise outlive this program.
    fn stop_all(&self) {
        self.running.stop_all();
        if let Some(active) = &self.active_peek {
            active.stop.now();
        }
    }

    fn move_selection(&mut self, by: i32) {
        let keys: Vec<&String> = self.device_keys().collect();
        if keys.is_empty() {
            return;
        }
        let at = self.selected.as_ref().and_then(|s| keys.iter().position(|k| *k == s)).unwrap_or(0) as i32;
        let next = keys[(at + by).clamp(0, keys.len() as i32 - 1) as usize].clone();
        self.select(next);
    }

    fn select(&mut self, key: String) {
        self.chosen = true;
        if self.selected.as_ref() != Some(&key) {
            self.selected = Some(key);
            self.detail_scroll = 0;
            self.moved = Instant::now();
            // Tabs differ between devices; stay on the same one if it exists here.
            let peeks = self.selected.as_ref().and_then(|k| self.model.get(k)).map_or(0, |d| d.peeks.len());
            if self.tab > peeks {
                self.tab = 0;
            }
        }
    }

    fn next_attention(&mut self) {
        let keys: Vec<&String> = self.device_keys().collect();
        let at = self.selected.as_ref().and_then(|s| keys.iter().position(|k| *k == s)).unwrap_or(0);
        let wants = |key: &String| self.model.get(key).is_some_and(|d| d.attention().is_some());
        let next = keys.iter().skip(at + 1).chain(keys.iter().take(at + 1)).find(|k| wants(k)).map(|k| (*k).clone());
        match next {
            Some(key) => self.select(key),
            None => self.say("nothing needs attention"),
        }
    }

    /// What an action applies to: everything tagged, shown or not, as the title counts
    /// them, or else the selected device.
    fn targets(&self) -> Vec<String> {
        let tagged: Vec<String> = self.model.devices.iter().filter(|d| d.tagged).map(|d| d.key.clone()).collect();
        if !tagged.is_empty() {
            return tagged;
        }
        self.selected.iter().cloned().collect()
    }

    fn open_menu(&mut self) {
        let targets = self.targets();
        let mut items: Vec<MenuItem> = Vec::new();
        for key in &targets {
            let Some(device) = self.model.get(key) else { continue };
            for action in &device.actions {
                match items.iter_mut().find(|i| i.id == action.id) {
                    Some(item) => item.count += 1,
                    None => items.push(MenuItem {
                        id: action.id.clone(),
                        label: action.label.clone(),
                        tty: action.tty,
                        confirm: action.confirm,
                        count: 1,
                    }),
                }
            }
        }
        if items.is_empty() {
            self.say("no actions here");
        } else {
            self.mode = Mode::Menu { items, at: 0, targets };
        }
    }

    /// One action on these devices, with whatever any of them needs: one that asks first
    /// or takes a line of text decides for the batch, whichever order they come in.
    fn plan(&self, id: &str, keys: &[String]) -> Option<Plan> {
        let steps: Vec<(String, Action)> = keys
            .iter()
            .filter_map(|key| {
                let device = self.model.get(key)?;
                Some((device.label(), device.actions.iter().find(|a| a.id == id)?.clone()))
            })
            .collect();
        Some(Plan {
            label: steps.first()?.1.label.clone(),
            prompt: steps.iter().find_map(|(_, a)| a.input.clone()),
            confirm: steps.iter().any(|(_, a)| a.confirm),
            tty: steps.iter().any(|(_, a)| a.tty),
            steps,
        })
    }

    /// Start an action, stopping first for whatever it needs from you.
    fn begin(&mut self, id: &str, keys: &[String]) {
        self.mode = Mode::Normal;
        let Some(plan) = self.plan(id, keys) else {
            self.say(format!("nothing selected can do {id}"));
            return;
        };
        if plan.tty && plan.steps.len() > 1 {
            self.say(format!("{} takes over the terminal, so it works on one device at a time", plan.label));
            return;
        }
        if plan.prompt.is_some() {
            self.mode = Mode::Input { plan, text: String::new() };
            return;
        }
        self.confirm_or_launch(plan, None);
    }

    fn confirm_or_launch(&mut self, plan: Plan, input: Option<String>) {
        if plan.confirm {
            self.mode = Mode::Confirm { plan, input };
        } else {
            self.launch(plan, input);
        }
    }

    fn launch(&mut self, plan: Plan, input: Option<String>) {
        self.mode = Mode::Normal;
        let fill = |argv: &[String]| -> Vec<String> {
            argv.iter()
                .map(|arg| match &input {
                    Some(text) => arg.replace("{input}", text),
                    None => arg.clone(),
                })
                .collect()
        };
        if plan.tty {
            self.tty_request = plan.steps.first().map(|(_, action)| fill(&action.run));
            return;
        }
        let show = plan.steps.len() > 1 || input.is_some();
        for (device, action) in &plan.steps {
            let job = self.jobs.len();
            let label = match &input {
                Some(text) => format!("{}: {text}", action.label),
                None => action.label.clone(),
            };
            self.jobs.push(Job {
                device: device.clone(),
                label,
                output: String::new(),
                outcome: None,
                started: Instant::now(),
                took: None,
            });
            run::spawn_job(job, fill(&action.run), self.tx.clone());
        }
        // Scroll to where this batch starts.
        if show {
            self.mode = Mode::Jobs { scroll: ui::jobs_line_of(&self.jobs, self.jobs.len() - plan.steps.len()) };
        } else {
            self.say(format!("{} on {}…", plan.label, plan.who()));
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return false;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => return self.key_normal(key),
            Mode::Filter => self.key_filter(key),
            Mode::Help => {}
            Mode::Menu { items, at, targets } => self.key_menu(key, items, at, targets),
            Mode::Input { plan, text } => self.key_input(key, plan, text),
            Mode::Confirm { plan, input } => self.key_confirm(key, plan, input),
            Mode::Jobs { scroll } => self.key_jobs(key, scroll),
        }
        true
    }

    fn key_normal(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-PAGE),
            KeyCode::PageDown => self.move_selection(PAGE),
            KeyCode::Home => self.move_selection(i32::MIN / 2),
            KeyCode::End => self.move_selection(i32::MAX / 2),
            KeyCode::Char(' ') => {
                if let Some(key) = self.selected.clone() {
                    if let Some(device) = self.model.get_mut(&key) {
                        device.tagged = !device.tagged;
                    }
                }
                self.move_selection(1);
            }
            KeyCode::Char('t') => {
                let keys: Vec<String> = self.device_keys().cloned().collect();
                let all = keys.iter().all(|k| self.model.get(k).is_some_and(|d| d.tagged));
                for key in keys {
                    if let Some(device) = self.model.get_mut(&key) {
                        device.tagged = !all;
                    }
                }
            }
            KeyCode::Char('T') => self.model.devices.iter_mut().for_each(|d| d.tagged = false),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refresh_rows();
                } else {
                    self.model.devices.iter_mut().for_each(|d| d.tagged = false);
                }
            }
            KeyCode::Tab => self.detail_open = !self.detail_open,
            KeyCode::Right | KeyCode::Char('l') => self.change_tab(1),
            KeyCode::Left | KeyCode::Char('h') => self.change_tab(-1),
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::Enter | KeyCode::Char('x') => self.open_menu(),
            KeyCode::Char('s') => {
                // A shell is for the device under the cursor, tagged or not.
                let device = self.selected.as_ref().and_then(|k| self.model.get(k));
                match device.and_then(|d| d.actions.iter().find(|a| a.tty)).map(|a| a.id.clone()) {
                    Some(id) => {
                        let here: Vec<String> = self.selected.iter().cloned().collect();
                        self.begin(&id, &here);
                    }
                    None => self.say("this device has no shell"),
                }
            }
            KeyCode::Char('!') => {
                let targets = self.targets();
                self.begin("run", &targets);
            }
            KeyCode::Char('o') => self.mode = Mode::Jobs { scroll: ui::jobs_line_of(&self.jobs, self.jobs.len().saturating_sub(8)) },
            KeyCode::Char('b') => {
                self.group_by = match self.group_by {
                    GroupBy::Group => GroupBy::Kind,
                    GroupBy::Kind => GroupBy::Nothing,
                    GroupBy::Nothing => GroupBy::Group,
                };
                self.refresh_rows();
            }
            KeyCode::Char('d') => {
                self.hide_down = !self.hide_down;
                self.refresh_rows();
            }
            KeyCode::Char('n') => self.next_attention(),
            KeyCode::Char('r') => {
                self.wakes.iter().for_each(|wake| {
                    let _ = wake.send(());
                });
                self.say("asked every source again");
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        true
    }

    fn change_tab(&mut self, by: i32) {
        self.chosen = true;
        let peeks = self.selected.as_ref().and_then(|k| self.model.get(k)).map_or(0, |d| d.peeks.len()) as i32;
        self.detail_open = true;
        self.tab = (self.tab as i32 + by).clamp(0, peeks) as usize;
        self.detail_scroll = 0;
    }

    fn key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {}
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Backspace => {
                self.filter.pop();
                self.mode = Mode::Filter;
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.mode = Mode::Filter;
            }
            _ => self.mode = Mode::Filter,
        }
        self.refresh_rows();
    }

    fn key_menu(&mut self, key: KeyEvent, items: Vec<MenuItem>, at: usize, targets: Vec<String>) {
        let mut at = at;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Up | KeyCode::Char('k') => at = at.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => at = (at + 1).min(items.len() - 1),
            KeyCode::Enter => {
                let id = items[at].id.clone();
                self.begin(&id, &targets);
                return;
            }
            KeyCode::Char(c) => {
                if let Some(n) = c.to_digit(10) {
                    if n >= 1 && (n as usize) <= items.len() {
                        let id = items[n as usize - 1].id.clone();
                        self.begin(&id, &targets);
                        return;
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::Menu { items, at, targets };
    }

    fn key_input(&mut self, key: KeyEvent, plan: Plan, mut text: String) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                if !text.trim().is_empty() {
                    self.confirm_or_launch(plan, Some(text));
                }
                return;
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => {}
        }
        self.mode = Mode::Input { plan, text };
    }

    /// One device: y. Several: capital Y, because a stray y must not switch off a
    /// whole room.
    fn key_confirm(&mut self, key: KeyEvent, plan: Plan, input: Option<String>) {
        let wanted = if plan.steps.len() > 1 { 'Y' } else { 'y' };
        if key.code == KeyCode::Char(wanted) {
            self.launch(plan, input);
        } else {
            self.say(format!("{}: cancelled", plan.label));
        }
    }

    fn key_jobs(&mut self, key: KeyEvent, scroll: u16) {
        let mut scroll = scroll;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('o') => return,
            KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
            KeyCode::PageUp => scroll = scroll.saturating_sub(PAGE as u16),
            KeyCode::PageDown => scroll = scroll.saturating_add(PAGE as u16),
            KeyCode::Char('c') if self.jobs.iter().all(|j| j.outcome.is_some()) => {
                self.jobs.clear();
                scroll = 0;
            }
            _ => {}
        }
        self.mode = Mode::Jobs { scroll };
    }

    fn mouse(&mut self, event: MouseEvent) {
        let area = self.list_area;
        let inside = event.column >= area.x && event.column < area.right() && event.row >= area.y && event.row < area.bottom();
        match event.kind {
            MouseEventKind::ScrollUp if matches!(self.mode, Mode::Normal) => self.move_selection(-1),
            MouseEventKind::ScrollDown if matches!(self.mode, Mode::Normal) => self.move_selection(1),
            MouseEventKind::Down(MouseButton::Left) if inside && matches!(self.mode, Mode::Normal) => {
                let line = self.list_lines.get(self.top + (event.row - area.y) as usize);
                let Some((row, into)) = line.and_then(|l| l.at((event.column - area.x) as usize)) else { return };
                if let Some(Row::Device(key)) = self.rows.get(row).cloned() {
                    // The first columns are the tag; elsewhere a click selects.
                    if into <= 2 {
                        if let Some(device) = self.model.get_mut(&key) {
                            device.tagged = !device.tagged;
                        }
                    }
                    self.select(key);
                }
            }
            _ => {}
        }
    }
}

/// Clicks and the wheel. crossterm's EnableMouseCapture also asks for every movement
/// of the mouse, which nothing here reads.
fn mouse_capture(on: bool) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(if on { b"\x1b[?1000h\x1b[?1006h" } else { b"\x1b[?1006l\x1b[?1000l" })?;
    out.flush()
}

/// Wait for input, for at most `wait`, and handle all that has come, so that a held
/// key can't fall behind the screen. Some(true) if there was any, None to quit.
fn take_input(app: &mut App, wait: Duration) -> io::Result<Option<bool>> {
    if !event::poll(wait)? {
        return Ok(Some(false));
    }
    loop {
        match event::read()? {
            Event::Key(key) => {
                if !app.key(key) {
                    return Ok(None);
                }
            }
            Event::Mouse(mouse) => app.mouse(mouse),
            _ => {}
        }
        if !event::poll(Duration::ZERO)? {
            return Ok(Some(true));
        }
    }
}

/// Draws only when something changed: input at once, what the sources say at most
/// every FRAME, and the rest of the time nothing.
fn run_tui(opts: &Opts) -> io::Result<()> {
    let mut app = App::new(opts);
    let mut terminal = ratatui::init();
    mouse_capture(true)?;
    let mut dirty = true;
    let mut drawn = Instant::now();
    let result = loop {
        dirty |= app.pump();
        if dirty || drawn.elapsed() >= CLOCK {
            if let Err(e) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
                break Err(e);
            }
            dirty = false;
            drawn = Instant::now();
        }
        if let Some(argv) = app.tty_request.take() {
            // Hand the terminal over, and take it back when the command ends.
            let _ = mouse_capture(false);
            ratatui::restore();
            println!("hypertop: {}", argv.join(" "));
            let status = Command::new(&argv[0]).args(&argv[1..]).status();
            // A new terminal starts with a blank screen and an empty buffer, so the next
            // draw paints everything. Terminal::clear would add nothing, except a wait of
            // up to 2 s for the terminal to report its cursor, swallowing keys meanwhile.
            terminal = ratatui::init();
            mouse_capture(true)?;
            if let Err(e) = status {
                app.say(format!("{}: {e}", argv[0]));
            }
            dirty = true;
            continue;
        }
        let wait = if app.behind { Duration::ZERO } else { FRAME };
        match take_input(&mut app, wait) {
            Ok(Some(input)) => dirty |= input,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    app.stop_all();
    let _ = mouse_capture(false);
    ratatui::restore();
    result
}

/// This machine's name, without its domain.
fn host_name() -> String {
    let mut name = [0u8; 256];
    // One byte less than there is: a name that gets cut needn't come with its end.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len() - 1) } != 0 {
        return String::new();
    }
    let end = name.iter().position(|byte| *byte == 0).unwrap_or(name.len());
    run::clean(&String::from_utf8_lossy(&name[..end])).split('.').next().unwrap_or_default().to_string()
}

fn print_list(app: &App) {
    if !app.host.is_empty() {
        println!("hypertop @ {}", app.host);
    }
    if app.sources == 0 {
        println!("no sources in {}", app.dir.display());
    }
    // Nothing on the way to a terminal here strips what a source sent, as drawing does.
    let plain = |text: &str| run::clean(text).replace('\n', " ");
    for row in &app.rows {
        match row {
            Row::Header { title, up, total } => println!("{} ({up}/{total} up)", plain(title)),
            Row::Device(key) => {
                let Some(d) = app.model.get(key) else { continue };
                let state = match d.state {
                    State::Up => "up",
                    State::Idle => "idle",
                    State::Down => "down",
                    State::Unknown => "?",
                };
                let numbers: Vec<String> = d.metrics.iter().map(|(k, v)| format!("{}={v:.0}", plain(k))).collect();
                let flag = d.attention().map_or(String::new(), |why| format!("  ! {}", plain(&why)));
                println!("  {:<16} {:<5} {:<9} {:<46} {}{flag}", plain(&d.label()), state, plain(&d.kind), numbers.join(" "), plain(&d.note));
            }
        }
    }
}

fn main() {
    let opts = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        if !e.is_empty() {
            eprintln!("hypertop: {e}");
        }
        eprintln!("{USAGE}");
        std::process::exit(2);
    });
    if opts.list {
        let mut app = App::new(&opts);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(opts.wait) {
            app.pump();
            std::thread::sleep(FRAME);
        }
        app.pump();
        print_list(&app);
        app.stop_all();
        return;
    }
    if let Err(e) = run_tui(&opts) {
        eprintln!("hypertop: {e}");
        std::process::exit(1);
    }
}
