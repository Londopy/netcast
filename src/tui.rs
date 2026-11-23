use std::io;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table};

use crate::control::{Board, Control, Sending, edit};
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

/// The terminal screen's own state; everything the station reports comes from
/// the board on each draw.
pub struct App {
    control: Arc<Control>,
    pub pane: Pane,
    status: String,
    confirm_quit: bool,
    colour: bool,
}

impl App {
    pub fn new(control: Arc<Control>, colour: bool) -> App {
        App {
            control,
            pane: Pane::Send,
            status: String::new(),
            confirm_quit: false,
            colour,
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

fn send_pane(f: &mut Frame, _app: &mut App, area: Rect) {
    f.render_widget(Block::bordered().title(" Send "), area);
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
    use crate::control::{Incoming, Sending, edit};
    use crate::station::{Radio, Station, Transport};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
}
