//! TUI dashboard for thermal-conductor (`thc tui`).
//!
//! A tabbed ratatui interface with pluggable pages. Ships with:
//! - **Sessions** — live Claude session monitoring (absorbed from thermal-monitor)
//! - **Spawn** — interactive form to spawn new therminal sessions

pub mod profiles;
pub mod services;
pub mod sessions;
pub mod spawn;

use std::any::Any;
use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Tabs},
    Frame, Terminal,
};

use thermal_core::ClaudeStatePoller;

use crate::backend::BackendPreference;
use self::profiles::ProfilesPage;
use self::services::ServicesPage;
use self::sessions::SessionsPage;
use self::spawn::SpawnPage;

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
    pub const COLD: Color = pal(ThermalPalette::COLD);
    pub const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);
}

const BG: Color = palette::BG;
const BG_SURFACE: Color = palette::BG_SURFACE;
const TEXT_BRIGHT: Color = palette::TEXT_BRIGHT;
const TEXT_MUTED: Color = palette::TEXT_MUTED;
const COLD: Color = palette::COLD;
const ACCENT_COLD: Color = palette::ACCENT_COLD;

// ---------------------------------------------------------------------------
// Page trait
// ---------------------------------------------------------------------------

/// Trait for a TUI page/tab. Each page manages its own state and rendering.
pub trait TuiPage: Any {
    /// Tab title shown in the tab bar.
    fn title(&self) -> &str;

    /// Called every tick (~250ms) to update state from the poller.
    fn tick(&mut self, poller: &mut ClaudeStatePoller);

    /// Render the page into the given area.
    fn render(&mut self, f: &mut Frame, area: Rect);

    /// Handle a key event. Return `true` if the app should quit.
    fn handle_key(&mut self, key: crossterm::event::KeyEvent, poller: &mut ClaudeStatePoller)
        -> bool;

    /// Handle a mouse event.
    fn handle_mouse(
        &mut self,
        event: crossterm::event::MouseEvent,
        poller: &mut ClaudeStatePoller,
    );

    /// Whether focus is currently on a text input field (suppresses global hotkeys).
    fn has_text_focus(&self) -> bool {
        false
    }

    /// Downcast helper for cross-page communication.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    poller: ClaudeStatePoller,
    pages: Vec<Box<dyn TuiPage>>,
    active_tab: usize,
    should_quit: bool,
}

impl App {
    fn new(backend_pref: BackendPreference) -> Result<Self> {
        let poller = ClaudeStatePoller::new()?;

        let pages: Vec<Box<dyn TuiPage>> = vec![
            Box::new(SessionsPage::new()),
            Box::new(SpawnPage::new(backend_pref)),
            Box::new(ProfilesPage::new()),
            Box::new(ServicesPage::new()),
        ];

        Ok(Self {
            poller,
            pages,
            active_tab: 0,
            should_quit: false,
        })
    }

    fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.pages.len();
    }

    fn prev_tab(&mut self) {
        if self.active_tab == 0 {
            self.active_tab = self.pages.len() - 1;
        } else {
            self.active_tab -= 1;
        }
    }

    fn set_tab(&mut self, idx: usize) {
        if idx < self.pages.len() {
            self.active_tab = idx;
        }
    }

    fn tick(&mut self) {
        // Check if the profiles page signaled a change.
        // If so, tell the spawn page to reload on its next tick.
        let profiles_changed = self.pages.get_mut(2)
            .and_then(|p| p.as_any_mut().downcast_mut::<ProfilesPage>())
            .map(|pp| {
                let changed = pp.profiles_changed;
                pp.profiles_changed = false;
                changed
            })
            .unwrap_or(false);

        if profiles_changed {
            if let Some(spawn) = self.pages.get_mut(1)
                .and_then(|p| p.as_any_mut().downcast_mut::<SpawnPage>())
            {
                spawn.needs_reload = true;
            }
        }

        // Tick all pages so background state stays current.
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
            Constraint::Min(5),   // page content
        ])
        .split(f.area());

    // Background
    f.render_widget(
        Block::default().style(Style::default().bg(BG)),
        f.area(),
    );

    // -- Tab bar --
    let titles: Vec<Line> = app
        .pages
        .iter()
        .enumerate()
        .map(|(i, page)| {
            let num = format!("{}", i + 1);
            Line::from(vec![
                Span::styled(
                    num,
                    Style::default()
                        .fg(ACCENT_COLD)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(":", Style::default().fg(TEXT_MUTED)),
                Span::styled(
                    page.title(),
                    Style::default().fg(TEXT_BRIGHT),
                ),
            ])
        })
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
        .divider(Span::styled(" | ", Style::default().fg(COLD)))
        .block(
            Block::default()
                .title(" THERMAL CONDUCTOR ")
                .title_alignment(Alignment::Center)
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(COLD))
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
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(backend_pref)?;

    // Initial tick to populate sessions.
    app.tick();

    loop {
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
                                    let quit = page.handle_key(key, &mut app.poller);
                                    if quit {
                                        app.should_quit = true;
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
                                let quit = page.handle_key(key, &mut app.poller);
                                if quit {
                                    app.should_quit = true;
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
                        // Tab titles are: "1:Sessions", "2:Spawn", "3:Profiles", "4:Services"
                        // separated by " | " (3 chars). The Tabs widget has a Block with
                        // Borders::BOTTOM, adding 1 row of border at bottom but the text
                        // is on row 0 (inside the block). The block also has padding from
                        // the title " THERMAL CONDUCTOR " centered, but tab content starts
                        // at column 1 (left border area).
                        //
                        // Calculate tab boundaries from title widths + divider " | " (3 chars).
                        // Each title is "{N}:{name}" so widths are:
                        //   "1:Sessions" = 10, "2:Spawn" = 7, "3:Profiles" = 10, "4:Services" = 10
                        let tab_titles: Vec<&str> = app.pages.iter().map(|p| p.title()).collect();
                        // +2 for the "N:" prefix on each tab
                        let mut x = 1u16; // start after left border
                        for (i, title) in tab_titles.iter().enumerate() {
                            let tab_width = (title.len() + 2) as u16; // "N:" + title
                            if mouse.column >= x && mouse.column < x + tab_width {
                                app.set_tab(i);
                                break;
                            }
                            x += tab_width + 3; // " | " divider
                        }
                    } else if mouse.row >= 3 {
                        // Delegate to the active page for clicks below the tab bar.
                        if let Some(page) = app.pages.get_mut(app.active_tab) {
                            page.handle_mouse(mouse, &mut app.poller);
                        }
                    } else {
                        // Scroll events in tab bar area still go to page
                        if !matches!(mouse.kind, MouseEventKind::Down(_)) {
                            if let Some(page) = app.pages.get_mut(app.active_tab) {
                                page.handle_mouse(mouse, &mut app.poller);
                            }
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

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

/// Check if the active tab is a text input page (like Spawn) where
/// single-character keys should go to the page rather than be global shortcuts.
fn is_text_input_page(app: &App) -> bool {
    app.pages
        .get(app.active_tab)
        .map_or(false, |page| page.has_text_focus())
}
