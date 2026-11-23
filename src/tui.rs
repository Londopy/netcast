use std::io;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::TextArea;

use crate::control::{Board, Control, OutboxFile, Refused, SendForm, Sending, edit};
use crate::frame::{Mode, Precedence};
use crate::history::Entry;
use crate::message;

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
    file: usize,
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
            to: TextArea::default(),
            mode: Mode::Addressed,
            precedence: Precedence::Routine,
            ack: false,
            repeat: 0,
            interval_min: 10,
            source: Source::Compose,
            subject: TextArea::default(),
            body: TextArea::default(),
            file: 0,
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
            f.extend([Field::Repeat, Field::Interval]);
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
                usable.get(self.file).map(|f| f.file.clone()),
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

/// The terminal screen's own state; everything the station reports comes from
/// the board on each draw.
pub struct App {
    control: Arc<Control>,
    pub form: Form,
    pub pane: Pane,
    status: String,
    confirm_quit: bool,
    colour: bool,
    wide: bool,
}

impl App {
    pub fn new(control: Arc<Control>, colour: bool) -> App {
        let has_outbox = control.outbox.is_some();
        App {
            control,
            form: Form::new(has_outbox),
            pane: Pane::Send,
            status: String::new(),
            confirm_quit: false,
            colour,
            wide: true,
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

    /// Handles one key press. False means quit.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        if self.confirm_quit {
            self.confirm_quit = false;
            return k.code != KeyCode::Char('y');
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let editing = self.wide || self.pane == Pane::Send;
        let quit = (ctrl && k.code == KeyCode::Char('c'))
            || (k.code == KeyCode::Char('q') && !(editing && self.form.field.is_text()));
        if quit {
            if self.on_the_air() {
                self.confirm_quit = true;
                return true;
            }
            return false;
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
        let form = self.form.to_send_form(&usable);
        self.status = match self.control.send(form) {
            Ok(()) => {
                if self.form.source == Source::Compose {
                    self.form.subject = TextArea::default();
                    self.form.body = TextArea::default();
                }
                String::new()
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
                let n = self.usable_files().len().max(1);
                let form = &mut self.form;
                form.file = if right {
                    (form.file + 1) % n
                } else {
                    (form.file + n - 1) % n
                };
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
        // a change of mode can take away the field that had the focus
        let fields = self.form.fields();
        if !fields.contains(&self.form.field) {
            self.form.field = Field::Mode;
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

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let control = app.control.clone();
    // read before taking the board, so the listener never waits on the disk
    let history = control.history();
    edit(&control.board, |board| {
        if area.width >= WIDE {
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
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(tnc.len() as u16)]).areas(area);
    f.render_widget(
        Line::styled(who, Style::new().add_modifier(Modifier::BOLD)),
        left,
    );
    f.render_widget(Line::styled(tnc, tnc_style), right);
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
    let line = if app.confirm_quit {
        Line::styled(
            " A delivery is on the air. Quit anyway? y/n",
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
    let roster_hint = match &app.control.roster {
        Some(r) => format!("Empty sends to every member: {}", r.calls().join(" ")),
        None => "No roster loaded, so type the callsigns.".into(),
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
                choice(app, field, "Message", source.into())
            }
            Field::File => {
                let shown = match app.usable_files().get(app.form.file) {
                    Some(f) => format!(
                        "{}: {}",
                        f.file,
                        f.subject.as_deref().or(f.mid.as_deref()).unwrap_or("")
                    ),
                    None => "No messages in the outbox yet.".into(),
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
                ta.set_cursor_line_style(Style::new());
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
                "  {}  {}  {}  {}",
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
    use crate::control::{Incoming, Sending, edit};
    use crate::frame::{Mode, Precedence};
    use crate::station::{Radio, Station, Transport};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
            for (id, subject) in [("00000001", "Net\u{1b}[2J news"), ("00000002", &long)] {
                b.incoming.push(Incoming {
                    id: id.into(),
                    from: "K1ABC".into(),
                    mid: format!("MID{id}"),
                    mode: "bulletin",
                    precedence: "routine",
                    trust: "unsigned",
                    have: 3,
                    k: 3,
                    status: "decoded",
                    subject: Some(crate::control::clean(subject, 120)),
                    note: None,
                })
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
        let mut app = App::new(Arc::new(control()), true);
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
        assert!(app.confirm_quit);
        assert!(app.key(key(KeyCode::Char('n'))));
        assert!(!app.confirm_quit);
        assert!(app.key(ctrl('c')));
        assert!(!app.key(key(KeyCode::Char('y'))));
    }

    #[test]
    fn q_types_in_text_fields_and_quits_elsewhere() {
        let mut app = App::new(Arc::new(control()), true);
        assert!(app.key(key(KeyCode::Char('q'))));
        assert_eq!(app.form.to_send_form(&[]).to, vec!["q"]);
        focus(&mut app, Field::Mode);
        assert!(!app.key(key(KeyCode::Char('q'))));
    }
}
