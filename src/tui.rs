use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table, Wrap};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui_textarea::{TextArea, WrapMode};

use crate::control::{Board, Control, OutboxFile, Refused, SendForm, Sending, edit};
use crate::frame::{Mode, Precedence};
use crate::history::Entry;
use crate::message;
use crate::station::Inbox;

// below this many columns the panes don't fit side by side
const WIDE: u16 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Send,
    Air,
    Incoming,
    Log,
}

const PANES: [(Pane, &str); 4] = [
    (Pane::Send, "F1 Send"),
    (Pane::Air, "F2 On the air"),
    (Pane::Incoming, "F3 Incoming"),
    (Pane::Log, "F4 Log"),
];

const MODES: [Mode; 3] = [Mode::Addressed, Mode::Bulletin, Mode::Emergency];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    To,
    Mode,
    Precedence,
    Ack,
    Repeat,
    Interval,
    Source,
    Subject,
    Body,
    File,
    Sign,
}

impl Field {
    fn is_text(self) -> bool {
        matches!(self, Field::To | Field::Subject | Field::Body)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Compose,
    Outbox,
}

// more plain keys than this arriving at once is a paste, not typing
const PASTE_BURST: usize = 4;

fn text_box() -> TextArea<'static> {
    let mut t = TextArea::default();
    t.set_cursor_line_style(Style::new());
    t.set_selection_style(Style::new().add_modifier(Modifier::REVERSED));
    t
}

fn body_box() -> TextArea<'static> {
    let mut t = text_box();
    t.set_wrap_mode(WrapMode::Word);
    t
}

/// The send form, as the web page has it.
pub struct Form {
    to: TextArea<'static>,
    pub mode: Mode,
    pub precedence: Precedence,
    ack: bool,
    repeat: u32,
    interval_min: u64,
    source: Source,
    subject: TextArea<'static>,
    body: TextArea<'static>,
    /// The outbox file picked, by name, so files coming and going don't change it.
    file: Option<String>,
    sign: bool,
    pub field: Field,
    has_outbox: bool,
}

/// One step through a list, wrapping at either end.
fn step<T: Copy + PartialEq>(all: &[T], now: T, forward: bool) -> T {
    let i = all.iter().position(|x| *x == now).unwrap_or(0);
    let n = all.len();
    all[if forward {
        (i + 1) % n
    } else {
        (i + n - 1) % n
    }]
}

impl Form {
    fn new(has_outbox: bool) -> Form {
        Form {
            to: text_box(),
            mode: Mode::Addressed,
            precedence: Precedence::Routine,
            ack: false,
            repeat: 0,
            interval_min: 10,
            source: Source::Compose,
            subject: text_box(),
            body: body_box(),
            file: None,
            sign: false,
            field: Field::To,
            has_outbox,
        }
    }

    fn acks(&self) -> bool {
        self.mode == Mode::Addressed || (self.mode == Mode::Emergency && self.ack)
    }

    /// The fields that apply to what's picked so far, in order.
    pub fn fields(&self) -> Vec<Field> {
        let mut f = vec![Field::To, Field::Mode, Field::Precedence];
        if self.mode == Mode::Emergency {
            f.push(Field::Ack);
        }
        if !self.acks() {
            f.push(Field::Repeat);
            if self.repeat > 0 {
                f.push(Field::Interval);
            }
        }
        if self.has_outbox {
            f.push(Field::Source);
        }
        match self.source {
            Source::Compose => f.extend([Field::Subject, Field::Body]),
            Source::Outbox => f.push(Field::File),
        }
        f.push(Field::Sign);
        f
    }

    /// Rows the form needs to show every field, the message at three lines.
    fn height(&self) -> u16 {
        let fields = self.fields();
        let body = if fields.contains(&Field::Body) { 2 } else { 0 };
        fields.len() as u16 + 1 + 2 + body
    }

    /// Whether there's a message written that hasn't gone yet.
    fn has_text(&self) -> bool {
        [&self.subject, &self.body]
            .iter()
            .any(|t| t.lines().iter().any(|l| !l.trim().is_empty()))
    }

    fn move_focus(&mut self, forward: bool) {
        self.field = step(&self.fields(), self.field, forward);
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.precedence = Precedence::default_for(mode);
        if mode != Mode::Emergency {
            self.ack = false;
        }
    }

    /// The outbox file that would go: the one picked, or the first.
    fn picked<'a>(&self, usable: &'a [OutboxFile]) -> Result<&'a OutboxFile, String> {
        match &self.file {
            Some(name) => usable
                .iter()
                .find(|f| &f.file == name)
                .ok_or_else(|| format!("{name} is no longer in the outbox.")),
            None => usable
                .first()
                .ok_or_else(|| "No messages in the outbox yet.".into()),
        }
    }

    fn pick_next(&mut self, usable: &[OutboxFile], forward: bool) {
        let n = usable.len();
        if n == 0 {
            return;
        }
        let now = self
            .picked(usable)
            .ok()
            .and_then(|p| usable.iter().position(|f| f.file == p.file));
        let i = match now {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        };
        self.file = Some(usable[i].file.clone());
    }

    pub fn to_send_form(&self, usable: &[OutboxFile]) -> SendForm {
        let to = self.to.lines().join(" ");
        let (subject, body, file) = match self.source {
            Source::Compose => (
                self.subject.lines().join(" "),
                self.body.lines().join("\n"),
                None,
            ),
            Source::Outbox => (
                String::new(),
                String::new(),
                self.picked(usable).ok().map(|f| f.file.clone()),
            ),
        };
        SendForm {
            to: to
                .split([',', ' '])
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            mode: self.mode.name().into(),
            precedence: Some(self.precedence.name().into()),
            subject,
            body,
            file,
            sign: self.sign,
            ack: self.mode == Mode::Emergency && self.ack,
            repeat: self.repeat,
            interval_min: self.interval_min,
        }
    }
}

/// What a key press means here. Windows reports AltGr as Ctrl+Alt, and
/// Ctrl+letter in upper case while Caps Lock is on.
fn normalise(mut k: KeyEvent) -> KeyEvent {
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    if let KeyCode::Char(c) = k.code {
        if k.modifiers.contains(altgr) {
            k.modifiers.remove(altgr);
        } else if k.modifiers.contains(KeyModifiers::CONTROL) {
            k.code = KeyCode::Char(c.to_ascii_lowercase());
        }
    }
    k
}

/// A burst of plain keys that arrived together is a paste, the way Windows
/// consoles deliver one: text, tabs and line breaks, more at once than anyone types.
fn as_paste(keys: &[KeyEvent]) -> Option<String> {
    if keys.len() < PASTE_BURST {
        return None;
    }
    keys.iter()
        .map(|k| match k.code {
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Some(c)
            }
            KeyCode::Tab => Some('\t'),
            KeyCode::Enter => Some('\n'),
            _ => None,
        })
        .collect()
}

/// The terminal screen's own state; everything the station reports comes from
/// the board on each draw.
pub struct App {
    control: Arc<Control>,
    pub form: Form,
    pub pane: Pane,
    status: String,
    confirm_quit: Option<&'static str>,
    colour: bool,
    wide: bool,
    form_fits: bool,
    /// The outbox as of the last draw, listed before the board is locked.
    files: Vec<OutboxFile>,
    last_press: Option<char>,
}

impl App {
    pub fn new(control: Arc<Control>, colour: bool) -> App {
        let has_outbox = control.outbox.is_some();
        App {
            control,
            form: Form::new(has_outbox),
            pane: Pane::Send,
            status: String::new(),
            confirm_quit: None,
            colour,
            wide: true,
            form_fits: true,
            files: vec![],
            last_press: None,
        }
    }

    fn on_the_air(&self) -> bool {
        edit(&self.control.board, |b| {
            b.sending.as_ref().is_some_and(|s| s.phase == "sending")
        })
    }

    fn usable_files(&self) -> Vec<OutboxFile> {
        self.control.outbox.as_ref().map_or(vec![], |o| {
            o.list()
                .into_iter()
                .filter(|f| f.problem.is_none())
                .collect()
        })
    }

    fn editing(&self) -> bool {
        self.form_fits && (self.wide || self.pane == Pane::Send)
    }

    /// The key to act on for one key event, if any. Only presses count, except
    /// that Windows delivers a character typed as an Alt code (how a pseudo
    /// console sends symbols that aren't on the keyboard) as a release alone.
    fn effective(&mut self, k: KeyEvent) -> Option<KeyEvent> {
        match (k.kind, k.code) {
            (KeyEventKind::Press | KeyEventKind::Repeat, code) => {
                self.last_press = match code {
                    KeyCode::Char(c) => Some(c),
                    _ => None,
                };
                Some(normalise(k))
            }
            (KeyEventKind::Release, KeyCode::Char(c)) if !c.is_ascii() => {
                (self.last_press.take() != Some(c)).then(|| KeyEvent::new(k.code, k.modifiers))
            }
            _ => None,
        }
    }

    /// Handles one terminal event. False means quit.
    pub fn event(&mut self, ev: Event) -> bool {
        match ev {
            Event::Key(k) => self.effective(k).is_none_or(|k| self.key(k)),
            Event::Paste(s) => {
                self.paste(&s);
                true
            }
            _ => true,
        }
    }

    /// Handles everything that was waiting at once, so a paste that arrives as
    /// keys goes in as text. False means quit.
    fn events(&mut self, events: Vec<Event>) -> bool {
        let mut keys = vec![];
        let mut other = vec![];
        for ev in events {
            match ev {
                Event::Key(k) => keys.extend(self.effective(k)),
                ev => other.push(ev),
            }
        }
        if self.editing()
            && self.form.field.is_text()
            && let Some(text) = as_paste(&keys)
        {
            self.paste(&text);
            keys.clear();
        }
        keys.into_iter().all(|k| self.key(k)) && other.into_iter().all(|ev| self.event(ev))
    }

    fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\t', " ");
        let one_line = text.replace('\n', " ");
        let editing = self.editing();
        let form = &mut self.form;
        match form.field {
            Field::To if editing => {
                form.to.insert_str(one_line);
            }
            Field::Subject if editing => {
                form.subject.insert_str(one_line);
            }
            Field::Body if editing => {
                form.body.insert_str(text);
            }
            _ => self.status = "To paste, move to To, Subject or Message first.".into(),
        }
    }

    /// Handles one key press. False means quit.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        let k = normalise(k);
        if self.confirm_quit.take().is_some() {
            return !matches!(k.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'y'));
        }
        self.status.clear();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let editing = self.editing();
        let quit = (ctrl && k.code == KeyCode::Char('c'))
            || (k.code == KeyCode::Char('q') && !(editing && self.form.field.is_text()));
        if quit {
            self.confirm_quit = if self.on_the_air() {
                Some("A delivery is on the air. Quit anyway? y/n")
            } else if self.form.has_text() {
                Some("Quit and lose the message you haven't sent? y/n")
            } else {
                None
            };
            return self.confirm_quit.is_some();
        }
        match k.code {
            KeyCode::F(n @ 1..=4) => self.pane = PANES[n as usize - 1].0,
            KeyCode::Char('s') if ctrl => self.send(),
            _ if editing => self.edit_form(k),
            _ => {}
        }
        true
    }

    fn send(&mut self) {
        let usable = self.usable_files();
        if self.form.source == Source::Outbox
            && let Err(why) = self.form.picked(&usable)
        {
            self.status = format!("{why} Pick a message from the outbox.");
            return;
        }
        let form = self.form.to_send_form(&usable);
        let to = if form.to.is_empty() {
            "every member of the roster".to_string()
        } else {
            form.to.join(", ")
        };
        self.status = match self.control.send(form) {
            Ok(()) => {
                if self.form.source == Source::Compose {
                    self.form.subject = text_box();
                    self.form.body = body_box();
                }
                if self.wide {
                    format!("Sending to {to}.")
                } else {
                    format!("Sending to {to}. F2 shows each station.")
                }
            }
            Err(Refused::Busy) => "A delivery is already on the air.".into(),
            Err(Refused::Invalid(why)) => format!("Not sent: {why}."),
        };
    }

    fn edit_form(&mut self, k: KeyEvent) {
        let form = &mut self.form;
        let (left, right) = (k.code == KeyCode::Left, k.code == KeyCode::Right);
        let toggle = left || right || k.code == KeyCode::Char(' ');
        match (form.field, k.code) {
            (_, KeyCode::Tab) => form.move_focus(true),
            (_, KeyCode::BackTab) => form.move_focus(false),
            (Field::To | Field::Subject, KeyCode::Enter) => form.move_focus(true),
            (Field::To, _) => {
                form.to.input(k);
            }
            (Field::Subject, _) => {
                form.subject.input(k);
            }
            (Field::Body, _) => {
                form.body.input(k);
            }
            (Field::Mode, _) if left || right => {
                let m = step(&MODES, form.mode, right);
                form.set_mode(m);
            }
            (Field::Precedence, _) if left || right => {
                form.precedence = step(&Precedence::ALL, form.precedence, right);
            }
            (Field::Ack, _) if toggle => form.ack = !form.ack,
            (Field::Repeat, KeyCode::Up) => form.repeat = (form.repeat + 1).min(10),
            (Field::Repeat, KeyCode::Down) => form.repeat = form.repeat.saturating_sub(1),
            (Field::Interval, KeyCode::Up) => form.interval_min = (form.interval_min + 1).min(1440),
            (Field::Interval, KeyCode::Down) => {
                form.interval_min = form.interval_min.saturating_sub(1).max(1)
            }
            (Field::Source, _) if left || right => {
                form.source = match form.source {
                    Source::Compose => Source::Outbox,
                    Source::Outbox => Source::Compose,
                };
            }
            (Field::File, _) if left || right => {
                let usable = self.usable_files();
                self.form.pick_next(&usable, right);
            }
            (Field::Sign, _) if toggle => {
                if self.control.station.key.is_some() {
                    self.form.sign = !self.form.sign;
                } else {
                    self.status = "No station key yet. Run `netcast key` to make one.".into();
                }
            }
            _ => {}
        }
    }

    fn fg(&self, c: Color) -> Style {
        if self.colour {
            Style::new().fg(c)
        } else {
            Style::new()
        }
    }

    /// The colour a station's state is shown in, by its first word, as the web page does.
    fn state_style(&self, state: &str) -> Style {
        match state.split(' ').next().unwrap_or("") {
            "decoded" | "sent" => self.fg(Color::Green),
            "receiving" | "pending" => self.fg(Color::Yellow),
            "failed" => self.fg(Color::Red).add_modifier(Modifier::BOLD),
            "silent" => self.fg(Color::DarkGray),
            _ => Style::new(),
        }
    }
}

/// Puts the terminal back however `run` ends.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}

/// `netcast tui`: the listener plus a terminal screen, until the user quits.
pub fn run(control: Control, inbox: Inbox) -> Result<()> {
    let control = Arc::new(control);
    edit(&control.board, |b| b.quiet = true);
    control.listen(inbox);
    let colour = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut app = App::new(control, colour);
    // before init: it can switch raw mode on and then fail
    let _restore = Restore;
    let mut terminal = ratatui::try_init()?;
    execute!(io::stdout(), EnableBracketedPaste)?;
    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        // the board changes on its own, so redraw a few times a second anyway
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let mut events = vec![event::read()?];
        while event::poll(Duration::ZERO)? {
            events.push(event::read()?);
        }
        if !app.events(events) {
            return Ok(());
        }
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let control = app.control.clone();
    // read before taking the board, so the listener never waits on the disk
    let history = control.history();
    app.files = app.usable_files();
    let form = app.form.height();
    app.wide = area.width >= WIDE && area.height >= form + 11;
    app.form_fits = app.wide || area.height >= form + 3;
    edit(&control.board, |board| {
        if app.wide {
            let [head, main, bottom, foot] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(9),
                Constraint::Length(1),
            ])
            .areas(area);
            let [send, right] =
                Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]).areas(main);
            let [air, incoming] =
                Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]).areas(right);
            let [log, hist] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Percentage(40)]).areas(bottom);
            header(f, app, board, head);
            send_pane(f, app, send);
            air_pane(f, app, board.sending.as_ref(), air);
            incoming_pane(f, app, board, incoming);
            log_pane(f, board, log);
            history_pane(f, app, &history, hist);
            footer(f, app, foot);
        } else {
            let [head, tabs, body, foot] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(1),
            ])
            .areas(area);
            header(f, app, board, head);
            tab_line(f, app, tabs);
            match app.pane {
                Pane::Send => send_pane(f, app, body),
                Pane::Air => air_pane(f, app, board.sending.as_ref(), body),
                Pane::Incoming => incoming_pane(f, app, board, body),
                Pane::Log => {
                    let [log, hist] =
                        Layout::vertical([Constraint::Fill(1), Constraint::Length(9)]).areas(body);
                    log_pane(f, board, log);
                    history_pane(f, app, &history, hist);
                }
            }
            footer(f, app, foot);
        }
    });
}

fn header(f: &mut Frame, app: &App, board: &Board, area: Rect) {
    let st = &app.control.station;
    let who = format!(" netcast  {} on {}", st.call, st.net);
    let tnc = format!("{} ", board.tnc);
    let tnc_style = if board.tnc.starts_with("connected") {
        Style::new()
    } else {
        app.fg(Color::Red)
    };
    // the station's name always shows; a long TNC error is cut short instead
    let [left, right] = Layout::horizontal([
        Constraint::Length(who.chars().count() as u16 + 2),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(
        Line::styled(who, Style::new().add_modifier(Modifier::BOLD)),
        left,
    );
    let room = right.width as usize;
    let tnc = if tnc.chars().count() > room {
        let cut: String = tnc.chars().take(room.saturating_sub(4)).collect();
        format!("{cut}... ")
    } else {
        tnc
    };
    f.render_widget(Line::styled(tnc, tnc_style).right_aligned(), right);
}

fn tab_line(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    for (pane, label) in PANES {
        let style = if pane == app.pane {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Line::from(spans), area);
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Some(ask) = app.confirm_quit {
        Line::styled(
            format!(" {ask}"),
            app.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else if !app.status.is_empty() {
        Line::raw(format!(" {}", app.status))
    } else {
        Line::styled(
            " Tab next field  Ctrl+S send  F1-F4 panes  q quit",
            Style::new().add_modifier(Modifier::DIM),
        )
    };
    f.render_widget(line, area);
}

fn field_label(app: &App, field: Field, label: &str) -> Span<'static> {
    let style = if app.form.field == field {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    };
    Span::styled(format!("{label:<10}"), style)
}

fn choice(app: &App, field: Field, label: &str, value: String) -> Line<'static> {
    let value = if app.form.field == field {
        format!(" < {value} >")
    } else {
        format!("   {value}")
    };
    Line::from(vec![field_label(app, field, label), Span::raw(value)])
}

fn check(app: &App, field: Field, label: &str, on: bool, what: &str) -> Line<'static> {
    let mark = if on { "[x]" } else { "[ ]" };
    Line::from(vec![
        field_label(app, field, label),
        Span::raw(format!("   {mark} {what}")),
    ])
}

/// One row of the send form: a text box, or a line of text.
enum FormRow {
    Text(Field, &'static str),
    Shown(Line<'static>),
}

fn send_pane(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(" Send ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if !app.form_fits {
        let need = app.form.height() + 3;
        f.render_widget(
            Paragraph::new(format!(
                "The terminal is too short for the send form: make it at least {need} rows."
            ))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    let own = app.control.station.call.to_string();
    let roster_hint = match &app.control.roster {
        Some(r) => {
            let names: Vec<String> = r
                .calls()
                .into_iter()
                .filter(|c| *c != own)
                .chain(std::iter::once(r.net.to_string()))
                .chain(r.groups.iter().map(|g| g.to_string()))
                .collect();
            format!("blank for all: {}", names.join(" "))
        }
        None => "no roster; type callsigns".into(),
    };
    let mut rows = vec![];
    for field in app.form.fields() {
        let line = match field {
            Field::To => {
                rows.push(FormRow::Text(field, "To"));
                Line::styled(
                    format!("{:13}{roster_hint}", ""),
                    Style::new().add_modifier(Modifier::DIM),
                )
            }
            Field::Subject => {
                rows.push(FormRow::Text(field, "Subject"));
                continue;
            }
            Field::Body => {
                rows.push(FormRow::Text(field, "Message"));
                continue;
            }
            Field::Mode => choice(app, field, "Mode", app.form.mode.name().into()),
            Field::Precedence => {
                choice(app, field, "Precedence", app.form.precedence.name().into())
            }
            Field::Ack => check(
                app,
                field,
                "Confirm",
                app.form.ack,
                "ask stations to confirm",
            ),
            Field::Repeat => choice(
                app,
                field,
                "Repeats",
                format!("{} extra passes", app.form.repeat),
            ),
            Field::Interval => choice(
                app,
                field,
                "Interval",
                format!("{} minutes apart", app.form.interval_min),
            ),
            Field::Source => {
                let source = match app.form.source {
                    Source::Compose => "write one",
                    Source::Outbox => "from the outbox",
                };
                choice(app, field, "Source", source.into())
            }
            Field::File => {
                let shown = match app.form.picked(&app.files) {
                    Ok(f) => format!(
                        "{}: {}",
                        f.file,
                        f.subject.as_deref().or(f.mid.as_deref()).unwrap_or("")
                    ),
                    Err(why) => why,
                };
                choice(app, field, "File", shown)
            }
            Field::Sign => {
                let what = if app.control.station.key.is_some() {
                    "sign with this station's key"
                } else {
                    "sign (no station key yet)"
                };
                check(app, field, "Sign", app.form.sign, what)
            }
        };
        rows.push(FormRow::Shown(line));
    }
    let heights = rows.iter().map(|r| match r {
        FormRow::Text(Field::Body, _) => Constraint::Fill(1),
        _ => Constraint::Length(1),
    });
    let areas = Layout::vertical(heights).split(inner);
    for (row, &area) in rows.into_iter().zip(areas.iter()) {
        match row {
            FormRow::Shown(line) => f.render_widget(line, area),
            FormRow::Text(field, name) => {
                let [label, input] =
                    Layout::horizontal([Constraint::Length(13), Constraint::Fill(1)]).areas(area);
                f.render_widget(Line::from(field_label(app, field, name)), label);
                let focused = app.form.field == field;
                let ta = match field {
                    Field::To => &mut app.form.to,
                    Field::Subject => &mut app.form.subject,
                    _ => &mut app.form.body,
                };
                ta.set_cursor_style(if focused {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                });
                f.render_widget(&*ta, input);
            }
        }
    }
}
fn air_pane(f: &mut Frame, app: &App, sending: Option<&Sending>, area: Rect) {
    let block = Block::bordered().title(" On the air ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(s) = sending else {
        f.render_widget(Paragraph::new("Nothing sent yet."), inner);
        return;
    };
    let mut meta = vec![s.mode.to_string(), s.precedence.to_string()];
    if s.signed {
        meta.push("signed".into());
    }
    if s.k > 0 {
        meta.push(format!("{} symbols", s.k));
    }
    if s.round > 0 {
        meta.push(format!("round {}", s.round));
    }
    let phase = match s.phase {
        "sending" => "on the air",
        "done" => "finished",
        _ => "failed",
    };
    meta.push(phase.into());
    let mid = if s.mid.is_empty() {
        "preparing"
    } else {
        &s.mid
    };
    let [head, table, note] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(if s.error.is_some() { 2 } else { 0 }),
    ])
    .areas(inner);
    f.render_widget(
        Line::from(vec![
            Span::styled(mid, Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {}", meta.join(", "))),
        ]),
        head,
    );
    let rows = s.recipients.iter().map(|(call, state)| {
        Row::new(vec![
            Span::raw(call.as_str()),
            Span::styled(state.as_str(), app.state_style(state)),
        ])
    });
    let header = Row::new(vec!["Station", "State"]).style(Style::new().add_modifier(Modifier::DIM));
    f.render_widget(
        Table::new(rows, [Constraint::Length(10), Constraint::Fill(1)]).header(header),
        table,
    );
    if let Some(e) = &s.error {
        f.render_widget(Paragraph::new(e.as_str()).style(app.fg(Color::Red)), note);
    }
}

fn bar(have: u32, k: u32, width: usize) -> String {
    let done = (have.min(k) as usize * width)
        .checked_div(k as usize)
        .unwrap_or(0);
    format!(
        "{}{}",
        "\u{2501}".repeat(done),
        "\u{2500}".repeat(width - done)
    )
}

fn incoming_pane(f: &mut Frame, app: &App, board: &Board, area: Rect) {
    let block = Block::bordered().title(" Incoming ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if board.incoming.is_empty() {
        let net = &app.control.station.net;
        f.render_widget(
            Paragraph::new(format!("Nothing received yet. Listening on {net}.")),
            inner,
        );
        return;
    }
    let mut lines = vec![];
    for m in &board.incoming {
        lines.push(Line::from(vec![
            Span::styled(m.from.as_str(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {}  {}, {}, ", m.mid, m.mode, m.precedence)),
            Span::raw(m.trust),
        ]));
        if let Some(subject) = &m.subject {
            lines.push(Line::raw(format!("  {subject}")));
        }
        match m.status {
            "receiving" => lines.push(Line::styled(
                format!("  {} {} of {}", bar(m.have, m.k, 20), m.have, m.k),
                app.state_style("receiving"),
            )),
            "decoded" => lines.push(Line::styled(
                "  in the Pat inbox",
                app.state_style("decoded"),
            )),
            _ => {}
        }
        if let Some(note) = &m.note {
            lines.push(Line::styled(format!("  {note}"), app.state_style(m.status)));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn log_pane(f: &mut Frame, board: &Board, area: Rect) {
    let block = Block::bordered().title(" Log ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let skip = board.log.len().saturating_sub(inner.height as usize);
    let lines: Vec<Line> = board
        .log
        .iter()
        .skip(skip)
        .map(|l| Line::raw(l.as_str()))
        .collect();
    if lines.is_empty() {
        f.render_widget(Paragraph::new("Nothing yet."), inner);
    } else {
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn history_pane(
    f: &mut Frame,
    app: &App,
    history: &io::Result<(Vec<Entry>, Vec<Entry>)>,
    area: Rect,
) {
    let block = Block::bordered().title(" History ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (sent, received) = match history {
        Ok(h) => h,
        Err(e) => {
            let why = format!("Can't read the history: {e}");
            f.render_widget(Paragraph::new(why).style(app.fg(Color::Red)), inner);
            return;
        }
    };
    let mut lines = vec![];
    for (title, rows) in [("Sent", sent), ("Received", received)] {
        lines.push(Line::styled(
            title,
            Style::new().add_modifier(Modifier::DIM),
        ));
        if rows.is_empty() {
            lines.push(Line::raw("  nothing yet"));
        }
        for e in rows {
            lines.push(Line::raw(format!(
                "  {} UTC  {}  {}  {}",
                message::date(e.unix),
                e.mid,
                e.mode,
                e.who
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Outbox;
    use crate::control::{self, Sending, edit};
    use crate::frame::{Mode, Precedence};
    use crate::message;
    use crate::receiver::{Announced, Trust};
    use crate::roster::Roster;
    use crate::station::{Radio, Station, Transport};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn control() -> Control {
        let radio = Radio {
            transport: Transport::Kiss {
                addr: "127.0.0.1:1".into(),
                baud: 1200,
                txdelay_ms: 300,
            },
            guard_ms: 500,
        };
        let station = Station::new(
            "N0NCS".parse().unwrap(),
            "NET1".parse().unwrap(),
            radio,
            tempfile::tempdir().unwrap().keep(),
            None,
        );
        Control::new(station, None, None)
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn on_the_air(c: &Control) {
        edit(&c.board, |b| {
            b.sending = Some(Sending {
                id: "459d1845".into(),
                mid: "J7CNWF489H0L".into(),
                mode: "addressed",
                precedence: "priority",
                k: 53,
                signed: false,
                recipients: vec![
                    ("K1ABC".into(), "decoded".into()),
                    ("W2XYZ".into(), "receiving (needs 15)".into()),
                    ("N3OOO".into(), "silent".into()),
                ],
                round: 2,
                phase: "sending",
                error: None,
            })
        });
    }

    #[test]
    fn the_air_table_shows_each_station() {
        let c = control();
        on_the_air(&c);
        let s = screen(&mut App::new(Arc::new(c), true), 120, 30);
        for want in [
            "K1ABC",
            "decoded",
            "W2XYZ",
            "receiving (needs 15)",
            "N3OOO",
            "silent",
            "round 2",
            "priority",
        ] {
            assert!(s.contains(want), "{want:?} missing from\n{s}");
        }
    }

    #[test]
    fn empty_board_says_so() {
        let c = control();
        edit(&c.board, |b| b.tnc = "connecting to the TNC".into());
        let s = screen(&mut App::new(Arc::new(c), true), 120, 30);
        for want in [
            "Nothing sent yet.",
            "Nothing received yet.",
            "connecting to the TNC",
        ] {
            assert!(s.contains(want), "{want:?} missing from\n{s}");
        }
    }

    #[test]
    fn narrow_terminal_shows_one_pane_with_tabs() {
        let c = control();
        on_the_air(&c);
        let mut app = App::new(Arc::new(c), true);
        app.pane = Pane::Air;
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("F1 Send") && s.contains("F2 On the air"), "{s}");
        assert!(s.contains("receiving (needs 15)"), "{s}");
        assert!(!s.contains("Nothing received yet."), "{s}");
    }

    #[test]
    fn a_long_tnc_error_leaves_the_station_name() {
        let c = control();
        edit(&c.board, |b| {
            b.tnc = format!(
                "can't reach the KISS port at 127.0.0.1:1: {}",
                "refused ".repeat(20)
            )
        });
        let s = screen(&mut App::new(Arc::new(c), true), 100, 30);
        let top = s.lines().next().unwrap();
        assert!(top.contains("N0NCS on NET1"), "{top}");
        assert!(top.contains("can't reach the KISS port"), "{top}");
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let c = control();
        on_the_air(&c);
        let mut app = App::new(Arc::new(c), true);
        screen(&mut app, 20, 5);
        screen(&mut app, 1, 1);
    }

    #[test]
    fn off_air_text_is_cleaned() {
        let c = control();
        let long = "x".repeat(500);
        edit(&c.board, |b| {
            for (id, subject) in [(1, "Net\u{1b}[2J news"), (2, long.as_str())] {
                let a = Announced {
                    id,
                    mode: Mode::Bulletin,
                    precedence: Precedence::Routine,
                    from: "K1ABC".into(),
                    mid: format!("MID{id}"),
                    k: 3,
                    trust: Trust::Unsigned,
                };
                control::announced(&mut b.incoming, &a);
                control::decoded(&mut b.incoming, id, Some(subject.into()), None);
            }
        });
        let s = screen(&mut App::new(Arc::new(c), true), 120, 30);
        assert!(!s.contains('\u{1b}'), "{s}");
        assert!(s.contains("Net [2J news"), "{s}");
        assert!(s.lines().all(|l| l.chars().count() == 120));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_in(app: &mut App, s: &str) {
        for c in s.chars() {
            let k = if c == '\n' {
                key(KeyCode::Enter)
            } else {
                key(KeyCode::Char(c))
            };
            app.key(k);
        }
    }

    fn focus(app: &mut App, field: Field) {
        while app.form.field != field {
            app.key(key(KeyCode::Tab));
        }
    }

    #[test]
    fn the_form_becomes_the_send_form() {
        let mut app = App::new(Arc::new(control()), true);
        type_in(&mut app, "k1abc, w2xyz");
        focus(&mut app, Field::Mode);
        app.key(key(KeyCode::Right));
        assert_eq!(app.form.mode, Mode::Bulletin);
        focus(&mut app, Field::Precedence);
        app.key(key(KeyCode::Right));
        app.key(key(KeyCode::Right));
        focus(&mut app, Field::Repeat);
        app.key(key(KeyCode::Up));
        app.key(key(KeyCode::Up));
        focus(&mut app, Field::Interval);
        for _ in 0..5 {
            app.key(key(KeyCode::Up));
        }
        focus(&mut app, Field::Subject);
        type_in(&mut app, "Sitrep");
        focus(&mut app, Field::Body);
        type_in(&mut app, "line one\nline two");
        let f = app.form.to_send_form(&[]);
        assert_eq!(f.to, vec!["k1abc", "w2xyz"]);
        assert_eq!(f.mode, "bulletin");
        assert_eq!(f.precedence.as_deref(), Some("priority"));
        assert_eq!((f.repeat, f.interval_min), (2, 15));
        assert_eq!(f.subject, "Sitrep");
        assert_eq!(f.body, "line one\nline two");
        assert_eq!(f.file, None);
    }

    #[test]
    fn emergency_mode_brings_emergency_precedence_and_the_ack_field() {
        let mut app = App::new(Arc::new(control()), true);
        assert!(!app.form.fields().contains(&Field::Ack));
        focus(&mut app, Field::Mode);
        app.key(key(KeyCode::Left));
        assert_eq!(app.form.mode, Mode::Emergency);
        assert_eq!(app.form.precedence, Precedence::Emergency);
        assert!(app.form.fields().contains(&Field::Ack));
    }

    #[test]
    fn ctrl_s_while_on_the_air_says_busy() {
        let c = control();
        on_the_air(&c);
        let mut app = App::new(Arc::new(c), true);
        type_in(&mut app, "K1ABC");
        focus(&mut app, Field::Subject);
        type_in(&mut app, "Second");
        app.key(ctrl('s'));
        assert!(app.status.contains("already on the air"), "{}", app.status);
        let mid = edit(&app.control.board, |b| {
            b.sending.as_ref().map(|s| s.mid.clone())
        });
        assert_eq!(mid.as_deref(), Some("J7CNWF489H0L"));
    }

    #[test]
    fn an_invalid_form_shows_why() {
        let mut app = App::new(Arc::new(control()), true);
        type_in(&mut app, "K1ABC");
        app.key(ctrl('s'));
        assert!(app.status.contains("needs a subject"), "{}", app.status);
        assert!(edit(&app.control.board, |b| b.sending.is_none()));
    }

    #[test]
    fn outbox_source_without_files_says_so() {
        let app = App::new(Arc::new(control()), true);
        assert!(!app.form.fields().contains(&Field::Source));

        let dir = tempfile::tempdir().unwrap();
        let mut c = control();
        c.outbox = Some(Outbox::new(dir.path().to_path_buf()));
        let mut app = App::new(Arc::new(c), true);
        focus(&mut app, Field::Source);
        app.key(key(KeyCode::Right));
        assert!(app.form.fields().contains(&Field::File));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("No messages in the outbox"), "{s}");
    }

    #[test]
    fn quitting_mid_delivery_asks_first() {
        let c = control();
        on_the_air(&c);
        let mut app = App::new(Arc::new(c), true);
        assert!(app.key(ctrl('c')));
        assert!(app.confirm_quit.is_some());
        assert!(app.key(key(KeyCode::Char('n'))));
        assert!(app.confirm_quit.is_none());
        assert!(app.key(ctrl('c')));
        assert!(!app.key(key(KeyCode::Char('y'))));
    }

    #[test]
    fn q_types_in_text_fields_and_quits_elsewhere() {
        let mut app = App::new(Arc::new(control()), true);
        assert!(app.key(key(KeyCode::Char('q'))));
        assert_eq!(app.form.to_send_form(&[]).to, vec!["q"]);
        app.key(key(KeyCode::Backspace));
        focus(&mut app, Field::Mode);
        assert!(!app.key(key(KeyCode::Char('q'))));
    }

    fn with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn release(c: char) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(c), KeyModifiers::NONE, KeyEventKind::Release)
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(c), KeyModifiers::NONE, KeyEventKind::Press)
    }

    fn outbox_with(files: &[(&str, &str)]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        for (name, subject) in files {
            let (_, b2f) = message::compose("N0NCS", &["K1ABC".into()], subject, "body", 0);
            std::fs::write(dir.path().join(name), b2f).unwrap();
        }
        let mut c = control();
        c.outbox = Some(Outbox::new(dir.path().to_path_buf()));
        let mut app = App::new(Arc::new(c), true);
        type_in(&mut app, "K1ABC");
        focus(&mut app, Field::Source);
        app.key(key(KeyCode::Right));
        (dir, app)
    }

    #[test]
    fn ctrl_letters_work_with_caps_lock() {
        let mut app = App::new(Arc::new(control()), true);
        assert!(!app.key(with(KeyCode::Char('C'), KeyModifiers::CONTROL)));
        let mut app = App::new(Arc::new(control()), true);
        type_in(&mut app, "K1ABC");
        app.key(with(KeyCode::Char('S'), KeyModifiers::CONTROL));
        assert!(app.status.contains("needs a subject"), "{}", app.status);
    }

    #[test]
    fn altgr_characters_are_typed() {
        let mut app = App::new(Arc::new(control()), true);
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        app.key(with(KeyCode::Char('@'), altgr));
        focus(&mut app, Field::Subject);
        type_in(&mut app, "20");
        app.key(with(KeyCode::Char('\u{20ac}'), altgr));
        let f = app.form.to_send_form(&[]);
        assert_eq!(f.to, vec!["@"]);
        assert_eq!(f.subject, "20\u{20ac}");
    }

    #[test]
    fn alt_code_characters_arrive_on_release() {
        let mut app = App::new(Arc::new(control()), true);
        assert!(app.event(Event::Key(press('5'))));
        app.event(Event::Key(release('5')));
        app.event(Event::Key(release('\u{b0}')));
        app.event(Event::Key(press('\u{e9}')));
        app.event(Event::Key(release('\u{e9}')));
        assert_eq!(app.form.to_send_form(&[]).to, vec!["5\u{b0}\u{e9}"]);
    }

    #[test]
    fn a_paste_goes_into_the_field_as_text() {
        let mut app = App::new(Arc::new(control()), true);
        app.event(Event::Paste("K1ABC\nW2XYZ".into()));
        focus(&mut app, Field::Body);
        assert!(app.event(Event::Paste("Need:\nwater\t20 cases\nquilts\t15".into())));
        assert_eq!(app.form.field, Field::Body);
        let f = app.form.to_send_form(&[]);
        assert_eq!(f.to, vec!["K1ABC", "W2XYZ"]);
        assert_eq!(f.body, "Need:\nwater 20 cases\nquilts 15");
    }

    #[test]
    fn a_burst_of_keys_is_taken_as_a_paste() {
        let burst: Vec<KeyEvent> = "water\t20\nq"
            .chars()
            .map(|c| match c {
                '\t' => key(KeyCode::Tab),
                '\n' => key(KeyCode::Enter),
                c => key(KeyCode::Char(c)),
            })
            .collect();
        assert_eq!(as_paste(&burst).as_deref(), Some("water\t20\nq"));
        assert_eq!(as_paste(&burst[..2]), None);
        let mut with_send = burst.clone();
        with_send.push(ctrl('s'));
        assert_eq!(as_paste(&with_send), None);
    }

    #[test]
    fn quitting_with_an_unsent_message_asks_first() {
        let mut app = App::new(Arc::new(control()), true);
        type_in(&mut app, "K1ABC");
        focus(&mut app, Field::Mode);
        // recipients alone aren't a message
        assert!(!app.key(key(KeyCode::Char('q'))));
        let mut app = App::new(Arc::new(control()), true);
        focus(&mut app, Field::Subject);
        type_in(&mut app, "Sitrep");
        focus(&mut app, Field::Mode);
        assert!(app.key(key(KeyCode::Char('q'))));
        assert!(app.confirm_quit.is_some());
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("message you haven't sent"), "{s}");
        assert!(!app.key(key(KeyCode::Char('Y'))));
    }

    #[test]
    fn the_outbox_pick_follows_the_file_not_its_place() {
        let (dir, mut app) = outbox_with(&[("b.b2f", "Bravo"), ("c.b2f", "Charlie")]);
        focus(&mut app, Field::File);
        app.key(key(KeyCode::Right));
        let (_, b2f) = message::compose("N0NCS", &["K1ABC".into()], "Alpha", "body", 0);
        std::fs::write(dir.path().join("a.b2f"), b2f).unwrap();
        assert_eq!(
            app.form.to_send_form(&app.usable_files()).file.as_deref(),
            Some("c.b2f")
        );
        std::fs::remove_file(dir.path().join("c.b2f")).unwrap();
        app.key(ctrl('s'));
        assert!(app.status.contains("c.b2f"), "{}", app.status);
        assert!(edit(&app.control.board, |b| b.sending.is_none()));
    }

    #[test]
    fn sending_from_an_empty_outbox_says_pick_one() {
        let (_dir, mut app) = outbox_with(&[]);
        app.key(ctrl('s'));
        assert!(app.status.contains("Pick a message"), "{}", app.status);
    }

    #[test]
    fn a_short_wide_terminal_still_shows_every_field() {
        let (_dir, mut app) = outbox_with(&[("b.b2f", "Bravo")]);
        focus(&mut app, Field::Mode);
        app.key(key(KeyCode::Right));
        let s = screen(&mut app, 120, 20);
        for want in ["Repeats", "Source", "File", "Sign"] {
            assert!(s.contains(want), "{want:?} missing from\n{s}");
        }
        focus(&mut app, Field::Sign);
        let s = screen(&mut app, 120, 9);
        assert!(s.contains("too short"), "{s}");
        app.key(key(KeyCode::Char(' ')));
        assert!(!app.form.sign);
    }

    #[test]
    fn a_send_says_where_it_went_and_clears_the_text() {
        let mut app = App::new(Arc::new(control()), true);
        type_in(&mut app, "K1ABC");
        focus(&mut app, Field::Subject);
        type_in(&mut app, "Sitrep");
        focus(&mut app, Field::Body);
        type_in(&mut app, "all ok");
        app.key(ctrl('s'));
        assert!(app.status.contains("Sending to K1ABC"), "{}", app.status);
        let f = app.form.to_send_form(&[]);
        assert_eq!((f.subject.as_str(), f.body.as_str()), ("", ""));
        assert_eq!(f.to, vec!["K1ABC"]);
    }

    #[test]
    fn an_outbox_send_keeps_what_was_written() {
        let (_dir, mut app) = outbox_with(&[("b.b2f", "Bravo")]);
        focus(&mut app, Field::Source);
        app.key(key(KeyCode::Right));
        focus(&mut app, Field::Subject);
        type_in(&mut app, "Draft");
        focus(&mut app, Field::Source);
        app.key(key(KeyCode::Right));
        app.key(ctrl('s'));
        assert!(app.status.contains("Sending"), "{}", app.status);
        focus(&mut app, Field::Source);
        app.key(key(KeyCode::Right));
        assert_eq!(app.form.to_send_form(&[]).subject, "Draft");
    }

    #[test]
    fn ack_takes_away_repeats_and_leaving_emergency_clears_it() {
        let mut app = App::new(Arc::new(control()), true);
        focus(&mut app, Field::Mode);
        app.key(key(KeyCode::Left));
        assert!(app.form.fields().contains(&Field::Repeat));
        focus(&mut app, Field::Ack);
        app.key(key(KeyCode::Char(' ')));
        assert!(!app.form.fields().contains(&Field::Repeat));
        focus(&mut app, Field::Mode);
        app.key(key(KeyCode::Left));
        app.key(key(KeyCode::Right));
        assert!(!app.form.to_send_form(&[]).ack);
    }

    #[test]
    fn interval_shows_only_with_repeats() {
        let mut app = App::new(Arc::new(control()), true);
        focus(&mut app, Field::Mode);
        app.key(key(KeyCode::Right));
        assert!(!app.form.fields().contains(&Field::Interval));
        focus(&mut app, Field::Repeat);
        app.key(key(KeyCode::Up));
        assert!(app.form.fields().contains(&Field::Interval));
    }

    #[test]
    fn the_to_hint_lists_the_others_and_the_groups() {
        let mut roster = Roster::new(&"NET1".parse().unwrap());
        for c in ["N0NCS", "K1ABC"] {
            roster.add_member(&c.parse().unwrap(), None).unwrap();
        }
        let mut c = control();
        c.roster = Some(roster);
        let s = screen(&mut App::new(Arc::new(c), true), 120, 30);
        let hint = s.lines().find(|l| l.contains("blank for all")).unwrap();
        assert!(hint.contains("K1ABC") && hint.contains("NET1"), "{hint}");
        assert!(!hint.contains("N0NCS"), "{hint}");
    }

    #[test]
    fn no_colour_means_no_colour_at_all() {
        let mut app = App::new(Arc::new(control()), false);
        type_in(&mut app, "K1ABC");
        for _ in 0..3 {
            app.key(with(KeyCode::Left, KeyModifiers::SHIFT));
        }
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        t.draw(|f| draw(f, &mut app)).unwrap();
        let buf = t.backend().buffer();
        assert!(
            buf.content()
                .iter()
                .all(|c| c.fg == ratatui::style::Color::Reset
                    && c.bg == ratatui::style::Color::Reset)
        );
    }
}
