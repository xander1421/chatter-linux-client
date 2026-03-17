//! Chatter TUI — terminal client built on the chatter-core SDK.
//!
//! Demonstrates that the SDK is fully self-contained: no Slint, no UI framework dependency.
//! Features: login, channel list, message history, send messages, real-time events.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::sync::mpsc;

use chatter_core::prelude::*;

// =============================================================================
// App state
// =============================================================================

enum Screen {
    Login,
    Register,
    Main,
}

struct App {
    client: Arc<ChatterClient>,
    screen: Screen,
    // Login / Register shared fields
    auth_field: usize,
    server_url: String,
    username: String,
    password: String,
    // Register-only fields
    display_name: String,
    pin: String,
    login_error: String,
    // Main
    channels: Vec<Channel>,
    dms: Vec<Channel>,
    #[allow(dead_code)]
    selected_sidebar: usize,
    sidebar_state: ListState,
    messages: Vec<Message>,
    active_conversation: Option<ActiveConversation>,
    input: String,
    status: String,
    // Events
    event_rx: Option<mpsc::UnboundedReceiver<String>>,
}

impl App {
    fn new(client: Arc<ChatterClient>) -> Self {
        let mut sidebar_state = ListState::default();
        sidebar_state.select(Some(0));
        Self {
            client,
            screen: Screen::Login,
            auth_field: 0,
            server_url: std::env::var("CHATTER_SERVER_URL")
                .unwrap_or_else(|_| "http://localhost:50051".into()),
            username: String::new(),
            password: String::new(),
            display_name: String::new(),
            pin: String::new(),
            login_error: String::new(),
            channels: Vec::new(),
            dms: Vec::new(),
            selected_sidebar: 0,
            sidebar_state,
            messages: Vec::new(),
            active_conversation: None,
            input: String::new(),
            status: "Not connected".into(),
            event_rx: None,
        }
    }

    fn sidebar_items(&self) -> Vec<String> {
        let mut items = Vec::new();
        for ch in &self.channels {
            items.push(format!("# {}", ch.name));
        }
        for dm in &self.dms {
            items.push(format!("@ {}", dm.name));
        }
        if items.is_empty() {
            items.push("(no conversations)".into());
        }
        items
    }

    fn all_conversations(&self) -> Vec<ActiveConversation> {
        let mut convs = Vec::new();
        for ch in &self.channels {
            convs.push(ActiveConversation::Channel(ch.id));
        }
        for dm in &self.dms {
            // For DMs we need the peer_id — get from members
            if let Ok(Some(peer)) = self.client.channels().get_dm_peer_local(&dm.id) {
                convs.push(ActiveConversation::Dm {
                    channel_id: dm.id,
                    peer_id: peer,
                });
            }
        }
        convs
    }
}

// =============================================================================
// Event forwarder — bridges DomainEvent to a simple string channel
// =============================================================================

fn spawn_event_listener(client: &Arc<ChatterClient>, tx: mpsc::UnboundedSender<String>) {
    let mut rx = client.subscribe_events();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let msg = match &event {
                        DomainEvent::MessageReceived { message } => {
                            let sender = message.sender_id
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| "?".into());
                            let text = message.as_text().unwrap_or("[no text]");
                            format!("New message from {}: {}", &sender[..8.min(sender.len())], text)
                        }
                        DomainEvent::TypingStarted { user_id, .. } => {
                            format!("{} is typing...", &user_id.to_string()[..8])
                        }
                        _ => continue,
                    };
                    let _ = tx.send(msg);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

// =============================================================================
// UI rendering
// =============================================================================

fn draw_auth_fields(f: &mut Frame, app: &App, title_text: &str, fields: &[(&str, &str, bool)], help_text: &str) {
    let mut constraints = vec![Constraint::Length(3)]; // title
    for _ in fields {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(2)); // error
    constraints.push(Constraint::Min(0));    // help

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints(constraints)
        .split(f.area());

    let title = Paragraph::new(title_text)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default());
    f.render_widget(title, chunks[0]);

    for (i, (label, value, is_secret)) in fields.iter().enumerate() {
        let style = if app.auth_field == i {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let display = if *is_secret { "*".repeat(value.len()) } else { value.to_string() };
        let marker = if app.auth_field == i { "> " } else { "  " };
        let input = Paragraph::new(format!("{}: {}", label, display))
            .style(style)
            .block(Block::default().borders(Borders::ALL).title(marker));
        f.render_widget(input, chunks[i + 1]);
    }

    let err_idx = fields.len() + 1;
    if !app.login_error.is_empty() {
        let err = Paragraph::new(app.login_error.as_str())
            .style(Style::default().fg(Color::Red));
        f.render_widget(err, chunks[err_idx]);
    }

    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(help, chunks[err_idx + 1]);
}

fn draw_login(f: &mut Frame, app: &App) {
    let fields: Vec<(&str, &str, bool)> = vec![
        ("Server", &app.server_url, false),
        ("Username", &app.username, false),
        ("Password", &app.password, true),
    ];
    draw_auth_fields(f, app, "Chatter TUI — Login", &fields,
        "Tab: next field | Enter: login | F2: register | Ctrl-C: quit");
}

fn draw_register(f: &mut Frame, app: &App) {
    let fields: Vec<(&str, &str, bool)> = vec![
        ("Server", &app.server_url, false),
        ("Username", &app.username, false),
        ("Display Name", &app.display_name, false),
        ("Password", &app.password, true),
        ("PIN (6 digits)", &app.pin, true),
    ];
    draw_auth_fields(f, app, "Chatter TUI — Register", &fields,
        "Tab: next field | Enter: register | F1: back to login | Ctrl-C: quit");
}

fn draw_main(f: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3), Constraint::Length(1)])
        .split(f.area());

    let main_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(outer[0]);

    // Sidebar
    let sidebar_items = app.sidebar_items();
    let items: Vec<ListItem> = sidebar_items.iter().enumerate().map(|(i, name)| {
        let style = if Some(i) == app.sidebar_state.selected() {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        ListItem::new(name.as_str()).style(style)
    }).collect();

    let sidebar = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Channels "));
    f.render_stateful_widget(sidebar, main_area[0], &mut app.sidebar_state);

    // Messages
    let msg_lines: Vec<Line> = app.messages.iter().rev().map(|msg| {
        let sender = msg.sender_id
            .map(|id| app.client.users().get_display_name(&id))
            .unwrap_or_else(|| "System".into());
        let text = msg.as_text().unwrap_or("[attachment]");
        let time = chrono::DateTime::from_timestamp_nanos(msg.sent_at as i64)
            .with_timezone(&chrono::Local)
            .format("%H:%M");
        Line::from(vec![
            Span::styled(format!("[{}] ", time), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}: ", sender), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(text),
            if msg.is_edited { Span::styled(" (edited)", Style::default().fg(Color::DarkGray)) } else { Span::raw("") },
        ])
    }).collect();

    let messages_widget = Paragraph::new(msg_lines)
        .block(Block::default().borders(Borders::ALL).title(" Messages "))
        .wrap(Wrap { trim: false });
    f.render_widget(messages_widget, main_area[1]);

    // Input
    let input_widget = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(" Type a message (Enter to send) "))
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(input_widget, outer[1]);

    // Status bar
    let status = Paragraph::new(app.status.as_str())
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, outer[2]);
}

// =============================================================================
// Main loop
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use separate data directory to avoid conflicts with the Slint client
    if std::env::var("XDG_DATA_HOME").is_err() {
        let tui_data = format!("{}/.local/share/chatter-tui",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        // SAFETY: called before any threads are spawned
        unsafe { std::env::set_var("XDG_DATA_HOME", &tui_data); }
    }

    // Logging to file (don't pollute terminal)
    let log_file = std::fs::File::create("/tmp/chatter-tui.log")?;
    tracing_subscriber::fmt()
        .with_env_filter("chatter_core=info,chatter_tui=debug")
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    // Build SDK client
    let client = ChatterClient::builder().build()?;

    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(client);

    // Event channel for real-time updates
    let (event_tx, event_rx) = mpsc::unbounded_channel::<String>();
    app.event_rx = Some(event_rx);

    loop {
        // Draw
        terminal.draw(|f| {
            match app.screen {
                Screen::Login => draw_login(f, &app),
                Screen::Register => draw_register(f, &app),
                Screen::Main => draw_main(f, &mut app),
            }
        })?;

        // Poll for real-time events
        if let Some(ref mut rx) = app.event_rx {
            while let Ok(msg) = rx.try_recv() {
                app.status = msg;
                // Refresh messages if we're viewing a conversation
                if let Some(ref conv) = app.active_conversation {
                    app.messages = app.client.get_messages(conv, 50).unwrap_or_default();
                }
            }
        }

        // Handle input (with 50ms timeout for responsiveness)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Global: Ctrl-C to quit
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    break;
                }

                match app.screen {
                    Screen::Login => {
                        let field_count = 3; // server, username, password
                        match key.code {
                            KeyCode::Tab => app.auth_field = (app.auth_field + 1) % field_count,
                            KeyCode::BackTab => app.auth_field = if app.auth_field == 0 { field_count - 1 } else { app.auth_field - 1 },
                            KeyCode::F(2) => {
                                app.screen = Screen::Register;
                                app.auth_field = 0;
                                app.login_error.clear();
                            }
                            KeyCode::Char(c) => {
                                match app.auth_field {
                                    0 => app.server_url.push(c),
                                    1 => app.username.push(c),
                                    2 => app.password.push(c),
                                    _ => {}
                                }
                            }
                            KeyCode::Backspace => {
                                match app.auth_field {
                                    0 => { app.server_url.pop(); }
                                    1 => { app.username.pop(); }
                                    2 => { app.password.pop(); }
                                    _ => {}
                                }
                            }
                            KeyCode::Enter => {
                                app.login_error.clear();
                                if let Err(e) = app.client.auth().set_server_url(&app.server_url) {
                                    app.login_error = format!("Bad URL: {}", e);
                                    continue;
                                }
                                if let Err(e) = app.client.connect().await {
                                    app.login_error = format!("Connection failed: {}", e);
                                    continue;
                                }
                                match app.client.login(&app.username, &app.password, "chatter-tui").await {
                                    Ok(_) => {
                                        spawn_event_listener(&app.client, event_tx.clone());
                                        app.client.sync().await;
                                        app.channels = app.client.list_channels().unwrap_or_default();
                                        app.dms = app.client.list_dms().unwrap_or_default();
                                        app.status = format!("Logged in as {} | {} channels, {} DMs",
                                            app.username, app.channels.len(), app.dms.len());
                                        app.screen = Screen::Main;
                                        let convs = app.all_conversations();
                                        if let Some(first) = convs.first() {
                                            app.active_conversation = Some(first.clone());
                                            app.messages = app.client.get_messages(first, 50).unwrap_or_default();
                                        }
                                    }
                                    Err(e) => app.login_error = format!("Login failed: {}", e),
                                }
                            }
                            _ => {}
                        }
                    }
                    Screen::Register => {
                        let field_count = 5; // server, username, display_name, password, pin
                        match key.code {
                            KeyCode::Tab => app.auth_field = (app.auth_field + 1) % field_count,
                            KeyCode::BackTab => app.auth_field = if app.auth_field == 0 { field_count - 1 } else { app.auth_field - 1 },
                            KeyCode::F(1) => {
                                app.screen = Screen::Login;
                                app.auth_field = 0;
                                app.login_error.clear();
                            }
                            KeyCode::Char(c) => {
                                match app.auth_field {
                                    0 => app.server_url.push(c),
                                    1 => app.username.push(c),
                                    2 => app.display_name.push(c),
                                    3 => app.password.push(c),
                                    4 => if c.is_ascii_digit() && app.pin.len() < 6 { app.pin.push(c); },
                                    _ => {}
                                }
                            }
                            KeyCode::Backspace => {
                                match app.auth_field {
                                    0 => { app.server_url.pop(); }
                                    1 => { app.username.pop(); }
                                    2 => { app.display_name.pop(); }
                                    3 => { app.password.pop(); }
                                    4 => { app.pin.pop(); }
                                    _ => {}
                                }
                            }
                            KeyCode::Enter => {
                                app.login_error.clear();
                                if app.pin.len() != 6 {
                                    app.login_error = "PIN must be exactly 6 digits".into();
                                    continue;
                                }
                                if let Err(e) = app.client.auth().set_server_url(&app.server_url) {
                                    app.login_error = format!("Bad URL: {}", e);
                                    continue;
                                }
                                if let Err(e) = app.client.connect().await {
                                    app.login_error = format!("Connection failed: {}", e);
                                    continue;
                                }
                                match app.client.register(
                                    &app.username, &app.password, &app.display_name,
                                    "chatter-tui", None, Some(&app.pin),
                                ).await {
                                    Ok(_) => {
                                        spawn_event_listener(&app.client, event_tx.clone());
                                        app.client.sync().await;
                                        app.channels = app.client.list_channels().unwrap_or_default();
                                        app.dms = app.client.list_dms().unwrap_or_default();
                                        app.status = format!("Registered as {} | {} channels, {} DMs",
                                            app.display_name, app.channels.len(), app.dms.len());
                                        app.screen = Screen::Main;
                                        let convs = app.all_conversations();
                                        if let Some(first) = convs.first() {
                                            app.active_conversation = Some(first.clone());
                                            app.messages = app.client.get_messages(first, 50).unwrap_or_default();
                                        }
                                    }
                                    Err(e) => app.login_error = format!("Registration failed: {}", e),
                                }
                            }
                            _ => {}
                        }
                    }
                    Screen::Main => {
                        match key.code {
                            KeyCode::Up => {
                                let items = app.sidebar_items();
                                let i = app.sidebar_state.selected().unwrap_or(0);
                                let next = if i == 0 { items.len().saturating_sub(1) } else { i - 1 };
                                app.sidebar_state.select(Some(next));

                                // Load conversation
                                let convs = app.all_conversations();
                                if let Some(conv) = convs.get(next) {
                                    app.active_conversation = Some(conv.clone());
                                    app.messages = app.client.get_messages(conv, 50).unwrap_or_default();
                                }
                            }
                            KeyCode::Down => {
                                let items = app.sidebar_items();
                                let i = app.sidebar_state.selected().unwrap_or(0);
                                let next = if i >= items.len().saturating_sub(1) { 0 } else { i + 1 };
                                app.sidebar_state.select(Some(next));

                                let convs = app.all_conversations();
                                if let Some(conv) = convs.get(next) {
                                    app.active_conversation = Some(conv.clone());
                                    app.messages = app.client.get_messages(conv, 50).unwrap_or_default();
                                }
                            }
                            KeyCode::Char(c) => {
                                app.input.push(c);
                            }
                            KeyCode::Backspace => {
                                app.input.pop();
                            }
                            KeyCode::Enter => {
                                if !app.input.is_empty() {
                                    if let Some(ref conv) = app.active_conversation {
                                        let text = std::mem::take(&mut app.input);
                                        match app.client.send_text(conv.clone(), &text).await {
                                            Ok(_) => {
                                                app.status = "Message sent".into();
                                                // Refresh messages
                                                app.messages = app.client.get_messages(conv, 50).unwrap_or_default();
                                            }
                                            Err(e) => {
                                                app.status = format!("Send failed: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Cleanup
    app.client.shutdown().await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
