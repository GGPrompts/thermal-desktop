//! TUI dashboard for thermal-conductor (`thc tui`).
//!
//! A tabbed ratatui interface with pluggable pages. Ships with:
//! - **Sessions** — live Claude session monitoring (absorbed from thermal-monitor)
//! - **Spawn** — interactive form to spawn new therminal sessions

pub mod chat;
pub mod profiles;
pub mod services;
pub mod sessions;
pub mod settings;
pub mod settings_page;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Tabs},
};

use thermal_core::ClaudeStatePoller;

use self::chat::ChatPage;
use self::profiles::ProfilesPage;
use self::services::ServicesPage;
use self::sessions::SessionsPage;
use self::settings_page::SettingsPage;
use crate::backend::BackendPreference;

// ---------------------------------------------------------------------------
// Palette helpers
// ---------------------------------------------------------------------------

pub mod palette {
    use ratatui::style::Color;
    use thermal_core::palette::ThermalPalette;

    pub const fn pal(c: [f32; 4]) -> Color {
        Color::Rgb(
            (c[0] * 255.0) as u8,
            (c[1] * 255.0) as u8,
            (c[2] * 255.0) as u8,
        )
    }

    pub const BG: Color = pal(ThermalPalette::BG);
    pub const BG_SURFACE: Color = pal(ThermalPalette::BG_SURFACE);
    pub const TEXT_BRIGHT: Color = pal(ThermalPalette::TEXT_BRIGHT);
    pub const TEXT_MUTED: Color = pal(ThermalPalette::TEXT_MUTED);
    pub const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);
}

const BG: Color = palette::BG;
const BG_SURFACE: Color = palette::BG_SURFACE;
const TEXT_BRIGHT: Color = palette::TEXT_BRIGHT;
const TEXT_MUTED: Color = palette::TEXT_MUTED;
const ACCENT_COLD: Color = palette::ACCENT_COLD;
const TAB_DIVIDER: &str = " | ";
const TAB_PADDING_LEFT: &str = " ";
const TAB_PADDING_RIGHT: &str = " ";

struct TuiScreenGuard {
    active: bool,
}

impl TuiScreenGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(Self { active: true })
    }
}

impl Drop for TuiScreenGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }

        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture, Show);
    }
}

fn tab_title_line(index: usize, title: &str) -> Line<'static> {
    let num = format!("{}", index + 1);
    Line::from(vec![
        Span::styled(
            num,
            Style::default()
                .fg(ACCENT_COLD)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(":", Style::default().fg(TEXT_MUTED)),
        Span::styled(title.to_owned(), Style::default().fg(TEXT_BRIGHT)),
    ])
}

fn tab_hit_index_for_titles(titles: &[&str], column: u16) -> Option<usize> {
    let left_padding = Line::from(TAB_PADDING_LEFT).width() as u16;
    let right_padding = Line::from(TAB_PADDING_RIGHT).width() as u16;
    let divider_width = Span::raw(TAB_DIVIDER).width() as u16;
    let mut x = 0u16;

    for (i, title) in titles.iter().enumerate() {
        let title_width = tab_title_line(i, title).width() as u16;
        let tab_width = left_padding + title_width + right_padding;
        if column >= x && column < x + tab_width {
            return Some(i);
        }
        x += tab_width;
        if i + 1 < titles.len() {
            x += divider_width;
        }
    }

    None
}

fn tab_hit_index(app: &App, column: u16) -> Option<usize> {
    let titles: Vec<&str> = app.pages.iter().map(|page| page.title()).collect();
    tab_hit_index_for_titles(&titles, column)
}

// ---------------------------------------------------------------------------
// Page trait
// ---------------------------------------------------------------------------

/// Result from a key event handler.
#[derive(Default)]
pub struct KeyResult {
    /// The app should quit.
    pub quit: bool,
    /// The terminal needs a full clear + redraw (e.g. after spawning an editor).
    pub needs_clear: bool,
}

impl KeyResult {
    pub const NONE: Self = Self {
        quit: false,
        needs_clear: false,
    };
    pub const QUIT: Self = Self {
        quit: true,
        needs_clear: false,
    };
    pub const CLEAR: Self = Self {
        quit: false,
        needs_clear: true,
    };
}

/// Trait for a TUI page/tab. Each page manages its own state and rendering.
pub trait TuiPage {
    /// Tab title shown in the tab bar.
    fn title(&self) -> &str;

    /// Called every tick (~250ms) to update state from the poller.
    fn tick(&mut self, poller: &mut ClaudeStatePoller);

    /// Render the page into the given area.
    fn render(&mut self, f: &mut Frame, area: Rect);

    /// Handle a key event. Returns flags indicating what the main loop should do.
    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        poller: &mut ClaudeStatePoller,
    ) -> KeyResult;

    /// Handle a mouse event.
    fn handle_mouse(&mut self, event: crossterm::event::MouseEvent, poller: &mut ClaudeStatePoller);

    /// Whether focus is currently on a text input field (suppresses global hotkeys).
    fn has_text_focus(&self) -> bool {
        false
    }

    /// Hint the page with a working directory from the currently selected
    /// session. Pages that spawn sessions can use this as the default cwd.
    fn set_context_cwd(&mut self, _cwd: &str) {}

    /// Return the cwd of the currently selected session, if any.
    fn selected_session_cwd(&self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    poller: ClaudeStatePoller,
    pages: Vec<Box<dyn TuiPage>>,
    active_tab: usize,
    should_quit: bool,
    needs_clear: bool,
}

impl App {
    fn new(
        backend_pref: BackendPreference,
        message_bus: &std::sync::Arc<crate::messages::MessageBus>,
        bus_send_tx: std::sync::mpsc::SyncSender<thermal_core::message::Message>,
    ) -> Result<Self> {
        let poller = ClaudeStatePoller::new()?;

        let mut sessions_page = SessionsPage::new(backend_pref);
        sessions_page.set_bus_handles(message_bus.subscribe(), bus_send_tx);

        let mut chat_page = ChatPage::new();
        chat_page.set_bus_receiver(message_bus.subscribe());

        let pages: Vec<Box<dyn TuiPage>> = vec![
            Box::new(sessions_page),
            Box::new(ProfilesPage::new(backend_pref)),
            Box::new(ServicesPage::new()),
            Box::new(SettingsPage::new()),
            Box::new(chat_page),
        ];

        Ok(Self {
            poller,
            pages,
            active_tab: 0,
            should_quit: false,
            needs_clear: false,
        })
    }

    fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.pages.len();
        self.propagate_session_cwd();
    }

    fn prev_tab(&mut self) {
        if self.active_tab == 0 {
            self.active_tab = self.pages.len() - 1;
        } else {
            self.active_tab -= 1;
        }
        self.propagate_session_cwd();
    }

    fn set_tab(&mut self, idx: usize) {
        if idx < self.pages.len() {
            self.active_tab = idx;
            self.propagate_session_cwd();
        }
    }

    /// Pass the Sessions tab's selected cwd to the newly active page.
    fn propagate_session_cwd(&mut self) {
        // Grab cwd from the Sessions page (index 0).
        let cwd = self.pages[0].selected_session_cwd();
        if let Some(cwd) = cwd {
            self.pages[self.active_tab].set_context_cwd(&cwd);
        }
    }

    fn tick(&mut self) {
        for page in &mut self.pages {
            page.tick(&mut self.poller);
        }
    }
}

// ---------------------------------------------------------------------------
// UI rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(5),    // page content
        ])
        .split(f.area());

    // Background
    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());

    // -- Tab bar --
    let titles: Vec<Line> = app
        .pages
        .iter()
        .enumerate()
        .map(|(i, page)| tab_title_line(i, page.title()))
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.active_tab)
        .style(Style::default().fg(TEXT_MUTED).bg(BG_SURFACE))
        .highlight_style(
            Style::default()
                .fg(TEXT_BRIGHT)
                .bg(BG)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .divider(Span::styled(TAB_DIVIDER, Style::default().fg(TEXT_MUTED)))
        .padding(TAB_PADDING_LEFT, TAB_PADDING_RIGHT)
        .block(
            Block::default()
                .title(" THERMAL CONDUCTOR ")
                .title_alignment(Alignment::Center)
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(TEXT_MUTED))
                .style(Style::default().bg(BG_SURFACE)),
        );
    f.render_widget(tabs, chunks[0]);

    // -- Active page --
    if let Some(page) = app.pages.get_mut(app.active_tab) {
        page.render(f, chunks[1]);
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Launch the TUI dashboard. This blocks until the user quits.
pub fn run(backend_pref: BackendPreference) -> Result<()> {
    let _screen = TuiScreenGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Initialize the internal message bus (replaces thermal-messages daemon).
    let message_bus = std::sync::Arc::new(crate::messages::MessageBus::new(true)?);

    // Create a sync -> async bridge: the TUI sends messages via this channel
    // and a background thread drains them into the async MessageBus.
    let (bus_send_tx, bus_send_rx) = std::sync::mpsc::sync_channel::<thermal_core::message::Message>(64);
    {
        let bus = std::sync::Arc::clone(&message_bus);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for message bus");
            rt.block_on(async move {
                while let Ok(msg) = bus_send_rx.recv() {
                    bus.send(msg).await;
                }
                // Channel closed — flush persistence and exit.
                bus.flush_persist().await;
            });
        });
    }

    let mut app = App::new(backend_pref, &message_bus, bus_send_tx)?;

    // Initial tick to populate sessions.
    app.tick();

    loop {
        if app.needs_clear {
            terminal.clear()?;
            app.needs_clear = false;
        }
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => {
                    // Global key bindings (tab switching, quit) take priority.
                    match key.code {
                        KeyCode::Char('q') if !is_text_input_page(&app) => {
                            app.should_quit = true;
                        }
                        KeyCode::Char('1') if !is_text_input_page(&app) => {
                            app.set_tab(0);
                        }
                        KeyCode::Char('2') if !is_text_input_page(&app) => {
                            app.set_tab(1);
                        }
                        KeyCode::Char('3') if !is_text_input_page(&app) => {
                            app.set_tab(2);
                        }
                        KeyCode::Char('4') if !is_text_input_page(&app) => {
                            app.set_tab(3);
                        }
                        KeyCode::Char('5') if !is_text_input_page(&app) => {
                            app.set_tab(4);
                        }
                        // Ctrl+C always quits
                        KeyCode::Char('c')
                            if key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            app.should_quit = true;
                        }
                        // Global tab switching with Ctrl+Tab / Shift+Tab
                        KeyCode::BackTab => {
                            // Only use BackTab for tab switching when NOT on spawn page
                            if !is_text_input_page(&app) {
                                app.prev_tab();
                            } else {
                                // Let the page handle BackTab for field switching
                                if let Some(page) = app.pages.get_mut(app.active_tab) {
                                    let result = page.handle_key(key, &mut app.poller);
                                    if result.quit {
                                        app.should_quit = true;
                                    }
                                    if result.needs_clear {
                                        app.needs_clear = true;
                                    }
                                }
                            }
                        }
                        // Ctrl+N / Ctrl+P for tab switching (works everywhere)
                        KeyCode::Char('n')
                            if key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            app.next_tab();
                        }
                        KeyCode::Char('p')
                            if key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            app.prev_tab();
                        }
                        _ => {
                            // Delegate to the active page.
                            if let Some(page) = app.pages.get_mut(app.active_tab) {
                                let result = page.handle_key(key, &mut app.poller);
                                if result.quit {
                                    app.should_quit = true;
                                }
                                if result.needs_clear {
                                    app.needs_clear = true;
                                }
                            }
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    // Tab bar occupies rows 0..3 (Constraint::Length(3)).
                    // Intercept left clicks in that region for tab switching.
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                        && mouse.row < 3
                    {
                        if let Some(idx) = tab_hit_index(&app, mouse.column) {
                            app.set_tab(idx);
                        }
                    } else if mouse.row >= 3 {
                        // Delegate to the active page for clicks below the tab bar.
                        if let Some(page) = app.pages.get_mut(app.active_tab) {
                            page.handle_mouse(mouse, &mut app.poller);
                        }
                    } else {
                        // Scroll events in tab bar area still go to page
                        if !matches!(mouse.kind, MouseEventKind::Down(_))
                            && let Some(page) = app.pages.get_mut(app.active_tab)
                        {
                            page.handle_mouse(mouse, &mut app.poller);
                        }
                    }
                }
                _ => {}
            }
        }

        app.tick();

        if app.should_quit {
            break;
        }
    }

    terminal.show_cursor()?;

    Ok(())
}

/// Check if the active tab is a text input page (like Spawn) where
/// single-character keys should go to the page rather than be global shortcuts.
fn is_text_input_page(app: &App) -> bool {
    app.pages
        .get(app.active_tab)
        .is_some_and(|page| page.has_text_focus())
}

#[cfg(test)]
mod tests {
    use super::{
        TAB_DIVIDER, TAB_PADDING_LEFT, TAB_PADDING_RIGHT, tab_hit_index_for_titles, tab_title_line,
    };
    use ratatui::text::{Line, Span};

    #[test]
    fn tab_hit_testing_matches_rendered_widths() {
        let titles = ["Sessions", "Profiles", "Services", "Settings", "Chat"];
        let left_padding = Line::from(TAB_PADDING_LEFT).width() as u16;
        let right_padding = Line::from(TAB_PADDING_RIGHT).width() as u16;
        let divider_width = Span::raw(TAB_DIVIDER).width() as u16;

        let mut x = 0u16;
        for (i, title) in titles.iter().enumerate() {
            let title_width = tab_title_line(i, title).width() as u16;
            let tab_width = left_padding + title_width + right_padding;

            assert_eq!(tab_hit_index_for_titles(&titles, x), Some(i));
            assert_eq!(tab_hit_index_for_titles(&titles, x + tab_width - 1), Some(i));

            x += tab_width;
            if i + 1 < titles.len() {
                for divider_col in x..x + divider_width {
                    assert_eq!(tab_hit_index_for_titles(&titles, divider_col), None);
                }
                x += divider_width;
            }
        }
    }
}
