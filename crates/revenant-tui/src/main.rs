//! revenant-tui: terminal dashboard over the control-plane API.
//!
//! Screens: chat (default), approval modal takeover, status bar. The TUI
//! holds zero business logic — it is proof the API is complete.

use anyhow::{Context, Result};
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use revenant_core::home::Home;
use revenant_core::Event;
use tokio::sync::mpsc;

#[derive(Clone)]
struct PendingApproval {
    id: String,
    summary: String,
}

struct App {
    client: revenant_client::Client,
    session_id: i64,
    messages: Vec<(String, String)>, // (role, text)
    input: String,
    streaming: bool,
    approval: Option<PendingApproval>,
    gateway_healthy: bool,
    spend_line: String,
    status_note: String,
    scroll_from_bottom: u16,
}

impl App {
    fn push_delta(&mut self, text: &str) {
        match self.messages.last_mut() {
            Some((role, buf)) if role == "rev" && self.streaming => buf.push_str(text),
            _ => {
                self.messages.push(("rev".into(), text.to_string()));
                self.streaming = true;
            }
        }
    }

    async fn refresh_status(&mut self) {
        if let Ok(health) = self.client.health().await {
            self.gateway_healthy = health["gateway_healthy"].as_bool().unwrap_or(false);
        }
        if let Ok(rows) = self.client.spend("today").await {
            let (i, o): (i64, i64) =
                rows.iter().fold((0, 0), |(i, o), r| (i + r.tokens_in, o + r.tokens_out));
            self.spend_line = format!("today {i}in/{o}out tok");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let home = Home::resolve();
    let client = revenant_client::Client::from_env(&home)?;
    client
        .health()
        .await
        .context("daemon not reachable — start it with `revenant up`")?;
    let session_id = client.create_session("tui").await?;

    // SSE events → channel
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(256);
    {
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(mut stream) = client.events().await {
                    while let Some(Ok(event)) = stream.next().await {
                        if event_tx.send(event).await.is_err() {
                            return;
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }

    let mut app = App {
        client,
        session_id,
        messages: vec![],
        input: String::new(),
        streaming: false,
        approval: None,
        gateway_healthy: false,
        spend_line: String::new(),
        status_note: "Enter=send · Ctrl-C=quit".into(),
        scroll_from_bottom: 0,
    };
    app.refresh_status().await;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &mut event_rx).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    event_rx: &mut mpsc::Receiver<Event>,
) -> Result<()> {
    let mut term_events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));

    loop {
        terminal.draw(|frame| draw(frame, app))?;

        tokio::select! {
            term_event = term_events.next() => {
                if let Some(Ok(TermEvent::Key(key))) = term_event {
                    if handle_key(app, key).await? {
                        return Ok(());
                    }
                }
            }
            bus_event = event_rx.recv() => {
                let Some(event) = bus_event else { return Ok(()) };
                handle_bus(app, event);
            }
            _ = ticker.tick() => {
                app.refresh_status().await;
            }
        }
    }
}

async fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    // Approval modal captures keys first.
    if let Some(pending) = app.approval.clone() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('a') => {
                let _ = app.client.decide(&pending.id, true, "tui").await;
                app.approval = None;
            }
            KeyCode::Char('n') | KeyCode::Char('d') | KeyCode::Esc => {
                let _ = app.client.decide(&pending.id, false, "tui").await;
                app.approval = None;
            }
            _ => {}
        }
        return Ok(false);
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(true),
        (KeyCode::Enter, _) => {
            let text = app.input.trim().to_string();
            if !text.is_empty() {
                app.messages.push(("you".into(), text.clone()));
                app.streaming = false;
                app.scroll_from_bottom = 0;
                app.input.clear();
                if let Err(err) = app.client.send_message(app.session_id, &text, None).await {
                    app.messages.push(("sys".into(), format!("send failed: {err:#}")));
                }
            }
        }
        (KeyCode::Backspace, _) => {
            app.input.pop();
        }
        (KeyCode::Up, _) => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(1),
        (KeyCode::Down, _) => app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(1),
        (KeyCode::Char(c), _) => app.input.push(c),
        _ => {}
    }
    Ok(false)
}

fn handle_bus(app: &mut App, event: Event) {
    let mine = event.session_id().is_none_or(|id| id == app.session_id);
    match event {
        Event::TurnDelta { text, .. } if mine => app.push_delta(&text),
        Event::ToolStarted { summary, .. } if mine => {
            app.messages.push(("sys".into(), format!("[tool] {summary}")));
            app.streaming = false;
        }
        Event::TurnCompleted { routed_model, input_tokens, output_tokens, .. } if mine => {
            app.streaming = false;
            app.status_note = format!(
                "last turn: {} · {}in/{}out",
                routed_model.unwrap_or_default(),
                input_tokens,
                output_tokens
            );
        }
        Event::TurnFailed { error, .. } if mine => {
            app.messages.push(("sys".into(), format!("error: {error}")));
            app.streaming = false;
        }
        // Approvals from ANY session take the screen — that's the point.
        Event::ApprovalCreated { id, summary, .. } => {
            app.approval = Some(PendingApproval { id, summary });
        }
        Event::ApprovalResolved { id, .. } => {
            if app.approval.as_ref().is_some_and(|p| p.id == id) {
                app.approval = None;
            }
        }
        Event::GatewayStatus { healthy, .. } => app.gateway_healthy = healthy,
        _ => {}
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(3)])
        .split(frame.area());

    // Status bar
    let gw = if app.gateway_healthy {
        Span::styled(" gateway ✓ ", Style::default().fg(Color::Green))
    } else {
        Span::styled(" gateway ✗ ", Style::default().fg(Color::Red))
    };
    let status = Line::from(vec![
        Span::styled(" revenant ", Style::default().add_modifier(Modifier::BOLD)),
        gw,
        Span::raw(format!("· {} · {} ", app.spend_line, app.status_note)),
    ]);
    frame.render_widget(Paragraph::new(status).style(Style::default().bg(Color::Black)), chunks[0]);

    // Messages
    let mut lines: Vec<Line> = Vec::new();
    for (role, text) in &app.messages {
        let (label, style) = match role.as_str() {
            "you" => ("you> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            "rev" => ("rev> ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            _ => ("     ", Style::default().fg(Color::DarkGray)),
        };
        for (i, part) in text.lines().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(label, style),
                    Span::raw(part.to_string()),
                ]));
            } else {
                lines.push(Line::from(format!("     {part}")));
            }
        }
    }
    let inner_height = chunks[1].height.saturating_sub(2);
    let total = lines.len() as u16;
    let scroll = total
        .saturating_sub(inner_height)
        .saturating_sub(app.scroll_from_bottom);
    let messages = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(format!(" session {} ", app.session_id)))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(messages, chunks[1]);

    // Input
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(" message "));
    frame.render_widget(input, chunks[2]);

    // Approval modal takeover
    if let Some(pending) = &app.approval {
        let area = centered_rect(70, 30, frame.area());
        frame.render_widget(Clear, area);
        let modal = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                pending.summary.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "[y] approve    [n] deny",
                Style::default().fg(Color::Yellow),
            )),
        ])
        .alignment(ratatui::layout::Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" ⚠ approval required ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(modal, area);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
