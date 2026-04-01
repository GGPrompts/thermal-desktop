//! Rendering logic for the sessions page.

use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap,
    },
};

use thermal_core::{ClaudeStatus, palette::ThermalPalette};

use super::format::*;
use super::{FocusedPanel, SessionsPage};

impl SessionsPage {
    /// Main render method — delegates from the TuiPage trait impl.
    pub(super) fn render_sessions(&mut self, f: &mut Frame, area: Rect) {
        let chat_msg_height = if self.chat_messages.is_empty() {
            0
        } else {
            (self.chat_messages.len() as u16).min(5)
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),          // table
                Constraint::Length(1),               // timeline bar for selected session
                Constraint::Percentage(35),          // preview pane
                Constraint::Length(chat_msg_height), // recent chat messages
                Constraint::Length(3),               // chat input bar
                Constraint::Length(1),               // footer
            ])
            .split(area);

        // Cache panel rects for mouse hit-testing.
        self.panel_rect_agent = chunks[0];
        self.panel_rect_preview = chunks[2];
        self.panel_rect_chat = chunks[4];

        // Background
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        // -- Session table header info --
        let parent_count = self.display_rows.iter().filter(|r| !r.is_subagent).count();
        let active = self
            .display_rows
            .iter()
            .filter(|r| !r.is_subagent && r.session.status != ClaudeStatus::Idle)
            .count();
        let subagent_count = self.display_rows.iter().filter(|r| r.is_subagent).count();
        let block_title = if subagent_count > 0 {
            format!(
                " Sessions [{} active / {}, {} subagents] ",
                active, parent_count, subagent_count
            )
        } else {
            format!(" Sessions [{} active / {}] ", active, parent_count)
        };

        let header_cells = [
            "\u{2610}", "Name", "Agent", "Status", "Activity", "Ctx%", "Project", "WS", "Age",
            "Cmd",
        ]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            )
        });
        let header_row = Row::new(header_cells).height(1);

        let rows: Vec<Row> = self
            .display_rows
            .iter()
            .enumerate()
            .map(|(row_idx, row)| {
                let s = &row.session;
                let color = status_color(&s.status);
                let label = status_label(&s.status);
                let activity = format_activity(s);
                let (agent_badge, agent_color) = agent_type_badge(s);

                let checkbox = if self.selected_set.contains(&row_idx) {
                    "\u{2611}" // checked box
                } else {
                    "\u{2610}" // unchecked box
                };
                let check_color = if self.selected_set.contains(&row_idx) {
                    pal(ThermalPalette::WARM)
                } else {
                    TEXT_MUTED
                };

                let (ctx_str, ctx_c) = match s.context_percent {
                    Some(pct) => (format!("{:.0}%", pct), ctx_color(pct as f32)),
                    None => ("-".into(), TEXT_MUTED),
                };

                let project = s
                    .working_dir
                    .as_deref()
                    .and_then(|d| std::path::Path::new(d).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("-")
                    .to_string();

                let ws_str = s
                    .workspace
                    .map(|ws| ws.to_string())
                    .or_else(|| {
                        s.working_dir
                            .as_deref()
                            .and_then(|wd| self.workspace_map.get(wd))
                            .map(|ws| ws.to_string())
                    })
                    .unwrap_or_else(|| "-".into());

                let updated = s
                    .last_updated
                    .as_deref()
                    .map(relative_time)
                    .unwrap_or_else(|| "-".into());

                let cmd_str = s
                    .last_command_duration_ms
                    .map(format_duration_ms)
                    .unwrap_or_else(|| "\u{2014}".into());

                if row.is_subagent {
                    let tree = if row.is_last_child {
                        "\u{2514}\u{2500}"
                    } else {
                        "\u{251C}\u{2500}"
                    };
                    let agent_label = s
                        .agent_id
                        .as_deref()
                        .map(|id| if id.len() > 8 { &id[..8] } else { id })
                        .unwrap_or("agent");
                    let id_str = format!("{} {}", tree, agent_label);

                    Row::new(vec![
                        Cell::from(""),
                        Cell::from(id_str).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(agent_badge).style(Style::default().fg(agent_color)),
                        Cell::from(label).style(Style::default().fg(color)),
                        Cell::from(activity).style(Style::default().fg(TEXT)),
                        Cell::from(ctx_str).style(Style::default().fg(ctx_c)),
                        Cell::from(project.clone()).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(ws_str).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(updated).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(cmd_str).style(Style::default().fg(TEXT_MUTED)),
                    ])
                } else {
                    let display_name = s.model_display_name();
                    let name_label = if display_name.len() > 14 {
                        format!("{}..", &display_name[..12])
                    } else {
                        display_name
                    };

                    Row::new(vec![
                        Cell::from(checkbox).style(Style::default().fg(check_color)),
                        Cell::from(name_label).style(Style::default().fg(TEXT)),
                        Cell::from(agent_badge).style(Style::default().fg(agent_color)),
                        Cell::from(label).style(Style::default().fg(color)),
                        Cell::from(activity).style(Style::default().fg(TEXT_BRIGHT)),
                        Cell::from(ctx_str).style(Style::default().fg(ctx_c)),
                        Cell::from(project.clone()).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(ws_str).style(Style::default().fg(ACCENT_COLD)),
                        Cell::from(updated).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(cmd_str).style(Style::default().fg(TEXT_MUTED)),
                    ])
                }
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(3),  // checkbox
                Constraint::Length(14), // session id
                Constraint::Length(5),  // Agent emoji + count
                Constraint::Length(10), // status
                Constraint::Length(24), // activity
                Constraint::Length(6),  // ctx%
                Constraint::Min(14),    // project
                Constraint::Length(4),  // WS
                Constraint::Length(5),  // age
                Constraint::Length(6),  // cmd duration
            ],
        )
        .header(header_row)
        .block(
            Block::default()
                .title(block_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(
                    if self.focused_panel == FocusedPanel::AgentList {
                        pal(ThermalPalette::ACCENT_WARM)
                    } else {
                        TEXT_MUTED
                    },
                ))
                .style(Style::default().bg(BG)),
        )
        .row_highlight_style(Style::default().bg(BG_SURFACE).add_modifier(Modifier::BOLD));

        f.render_stateful_widget(table, chunks[0], &mut self.table_state);

        // -- Timeline bar for selected session --
        {
            let tl_area = chunks[1];
            let bar_width = tl_area.width.saturating_sub(2) as usize;
            let timeline_line = if let Some(idx) = self.table_state.selected() {
                if let Some(row) = self.display_rows.get(idx) {
                    let sid = &row.session.session_id;
                    let label_span =
                        Span::styled(" \u{2502} ", Style::default().fg(TEXT_MUTED));
                    let bar = self.build_timeline_line(sid, bar_width.saturating_sub(3));
                    let mut spans = vec![label_span];
                    spans.extend(bar.spans);
                    Line::from(spans)
                } else {
                    Line::from(Span::styled(
                        " no session selected",
                        Style::default().fg(TEXT_MUTED),
                    ))
                }
            } else {
                Line::from(Span::styled(
                    " no session selected",
                    Style::default().fg(TEXT_MUTED),
                ))
            };
            let tl_widget = Paragraph::new(timeline_line).style(Style::default().bg(BG));
            f.render_widget(tl_widget, tl_area);
        }

        // -- Preview pane --
        {
            let preview_area = chunks[2];
            let inner_height = preview_area.height.saturating_sub(2) as usize;
            let preview_border_color = if self.focused_panel == FocusedPanel::Preview {
                pal(ThermalPalette::ACCENT_WARM)
            } else {
                TEXT_MUTED
            };

            let preview_widget = if self.preview_content.is_empty() {
                Paragraph::new(Line::from(Span::styled(
                    "(no session selected)",
                    Style::default().fg(TEXT_MUTED),
                )))
                .alignment(Alignment::Center)
                .block(
                    Block::default()
                        .title(" Preview ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(preview_border_color))
                        .style(Style::default().bg(BG)),
                )
            } else {
                let total = self.preview_content.len();
                let end = self.preview_scroll.max(1).min(total);
                let start = end.saturating_sub(inner_height);
                let visible_lines: Vec<Line> = self.preview_content[start..end].to_vec();

                let scroll_indicator = if total > inner_height {
                    let pct = if total == 0 { 100 } else { (end * 100) / total };
                    format!(" Preview [{}/{} {}%] ", end, total, pct)
                } else {
                    " Preview ".to_string()
                };

                Paragraph::new(visible_lines).block(
                    Block::default()
                        .title(scroll_indicator)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(preview_border_color))
                        .style(Style::default().bg(BG)),
                )
            };
            f.render_widget(preview_widget, preview_area);
        }

        // -- Recent chat messages --
        if !self.chat_messages.is_empty() {
            let now = Instant::now();
            let msg_lines: Vec<Line> = self
                .chat_messages
                .iter()
                .rev()
                .take(chat_msg_height as usize)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|e| {
                    let ago = now.duration_since(e.timestamp).as_secs();
                    let rel = if ago < 60 {
                        format!("{}s", ago)
                    } else {
                        format!("{}m", ago / 60)
                    };
                    Line::from(vec![
                        Span::styled(format!("[{rel}] "), Style::default().fg(TEXT_MUTED)),
                        Span::styled(
                            format!("{}: ", e.from_label),
                            Style::default()
                                .fg(pal(ThermalPalette::WARM))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(e.content.clone(), Style::default().fg(TEXT_BRIGHT)),
                    ])
                })
                .collect();
            let msgs_widget = Paragraph::new(msg_lines).style(Style::default().bg(BG));
            f.render_widget(msgs_widget, chunks[3]);
        }

        // -- Chat input bar --
        {
            let selected_count = self.selected_set.len();
            let target_hint = if selected_count > 1 {
                format!(" [{} selected] ", selected_count)
            } else if selected_count == 1 {
                let idx = *self.selected_set.iter().next().unwrap();
                let label = self
                    .display_rows
                    .get(idx)
                    .map(|r| r.session.model_display_name())
                    .unwrap_or_else(|| "-".into());
                format!(" [{}] ", label)
            } else if let Some(i) = self.table_state.selected() {
                let label = self
                    .display_rows
                    .get(i)
                    .map(|r| r.session.model_display_name())
                    .unwrap_or_else(|| "-".into());
                format!(" \u{2192} {} ", label)
            } else {
                " \u{2192} bus ".into()
            };

            let chat_focused = self.focused_panel == FocusedPanel::Chat;
            let input_border_color = if chat_focused {
                pal(ThermalPalette::ACCENT_WARM)
            } else {
                TEXT_MUTED
            };
            let input_title = if chat_focused {
                format!("{}Enter=send, Esc=cancel ", target_hint)
            } else {
                format!("{}/ to type ", target_hint)
            };

            let (before_cursor, after_cursor) = self.chat_input.split_at(self.chat_cursor);
            let input_line = if chat_focused {
                Line::from(vec![
                    Span::styled(before_cursor, Style::default().fg(TEXT_BRIGHT)),
                    Span::styled(
                        if after_cursor.is_empty() {
                            " "
                        } else {
                            &after_cursor[..1]
                        },
                        Style::default().fg(BG).bg(TEXT_BRIGHT),
                    ),
                    Span::styled(
                        if after_cursor.len() > 1 {
                            &after_cursor[1..]
                        } else {
                            ""
                        },
                        Style::default().fg(TEXT_BRIGHT),
                    ),
                ])
            } else if self.chat_input.is_empty() {
                Line::from(Span::styled(
                    "Press / to start typing...",
                    Style::default().fg(TEXT_MUTED),
                ))
            } else {
                Line::from(Span::styled(&self.chat_input, Style::default().fg(TEXT)))
            };

            let input_widget = Paragraph::new(input_line).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(input_border_color))
                    .title(input_title)
                    .title_style(Style::default().fg(if chat_focused {
                        pal(ThermalPalette::ACCENT_WARM)
                    } else {
                        TEXT_MUTED
                    }))
                    .style(Style::default().bg(BG)),
            );
            f.render_widget(input_widget, chunks[4]);
        }

        // -- Footer --
        let sel_count = self.selected_set.len();
        let sel_hint = if sel_count > 0 {
            vec![
                Span::styled(
                    format!(" {} selected ", sel_count),
                    Style::default()
                        .fg(pal(ThermalPalette::WARM))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(TEXT_MUTED)),
            ]
        } else {
            vec![]
        };

        let mut footer_spans = sel_hint;
        footer_spans.extend(vec![
            Span::styled(
                "j/k",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": nav  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Space",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": select  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": attach  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "/",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": chat  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "h",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": history", Style::default().fg(TEXT_MUTED)),
        ]);

        let footer = if let Some((ref msg, is_error, _)) = self.chat_status {
            let color = if is_error {
                pal(ThermalPalette::SEARING)
            } else {
                pal(ThermalPalette::WARM)
            };
            Paragraph::new(Line::from(Span::styled(
                msg.as_str(),
                Style::default().fg(color),
            )))
            .alignment(Alignment::Center)
        } else {
            Paragraph::new(Line::from(footer_spans))
        }
        .style(Style::default().bg(BG));
        f.render_widget(footer, chunks[5]);

        // -- Autocomplete popup overlay --
        if self.autocomplete_active && !self.autocomplete_items.is_empty() {
            let chat_area = chunks[4];
            let item_count = self.autocomplete_items.len().min(8);
            let popup_h = item_count as u16 + 2;
            let max_item_len = self
                .autocomplete_items
                .iter()
                .map(|s| s.len())
                .max()
                .unwrap_or(0);
            let popup_w = (max_item_len as u16 + 4).min(chat_area.width);

            let before_cursor = &self.chat_input[..self.chat_cursor];
            let at_offset = before_cursor.rfind('@').unwrap_or(0);
            let popup_x = chat_area.x + 1 + at_offset as u16;
            let popup_x = popup_x.min(area.x + area.width.saturating_sub(popup_w));
            let popup_y = chat_area.y.saturating_sub(popup_h);

            let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

            f.render_widget(Clear, popup_rect);

            let items: Vec<ListItem> = self
                .autocomplete_items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let style = if i == self.autocomplete_index {
                        Style::default()
                            .fg(BG)
                            .bg(ACCENT_COLD)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT_BRIGHT)
                    };
                    ListItem::new(format!(" @{} ", item)).style(style)
                })
                .collect();

            let list = List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(TEXT_MUTED))
                    .style(Style::default().bg(BG_SURFACE)),
            );
            f.render_widget(list, popup_rect);
        }

        // -- History popup overlay --
        if let Some(ref sid) = self.history_popup {
            self.render_history_popup(f, sid);
        }
    }

    pub(super) fn render_history_popup(&self, f: &mut Frame, session_id: &str) {
        let area = f.area();
        let popup_w = (area.width as u32 * 60 / 100).min(72) as u16;
        let popup_h = 16u16.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(popup_w)) / 2;
        let y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(x, y, popup_w, popup_h);

        f.render_widget(Clear, popup_area);

        let short_id = if session_id.len() > 16 {
            &session_id[..16]
        } else {
            session_id
        };
        let title = format!(" History: {} ", short_id);

        let now = Instant::now();
        let lines: Vec<Line> = self
            .history
            .get(session_id)
            .map(|entries| {
                entries
                    .iter()
                    .rev()
                    .map(|e| {
                        let ago = now.duration_since(e.timestamp).as_secs();
                        let rel = if ago < 60 {
                            format!("{}s ago", ago)
                        } else if ago < 3600 {
                            format!("{}m ago", ago / 60)
                        } else {
                            format!("{}h ago", ago / 3600)
                        };
                        Line::from(vec![
                            Span::styled(format!("{:>7}  ", rel), Style::default().fg(TEXT_MUTED)),
                            Span::styled(&e.text, Style::default().fg(TEXT_BRIGHT)),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default();

        let content = if lines.is_empty() {
            Paragraph::new("  No history yet.").style(Style::default().fg(TEXT_MUTED).bg(BG))
        } else {
            Paragraph::new(lines)
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: true })
        };

        let popup = content.block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT_COLD))
                .style(Style::default().bg(BG)),
        );

        f.render_widget(popup, popup_area);
    }
}
