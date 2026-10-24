//! ratatui rendering for the TUI.
//!
//! All width math goes through `unicode-width`: device names and log lines are
//! full of CJK characters, and `.len()` would misalign them.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, ConnState, UNKNOWN_ADDRESS};

/// Column width of the ` Label : ` prefix in the detail pane.
const DETAIL_PREFIX_WIDTH: usize = 8;

/// What a clickable region does. Mouse clicks are translated into the same
/// semantic actions as key presses, so the app keeps a single behaviour path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    Connect,
    Reconnect,
    Disconnect,
    Refresh,
    BluetoothSettings,
    Quit,
    /// Clicking a device row selects that index.
    SelectDevice(usize),
}

/// One mouse-clickable region in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitArea {
    pub rect: Rect,
    pub action: UiAction,
}

impl HitArea {
    /// True when `(column, row)` falls inside the half-open rect.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        column >= self.rect.x
            && column < self.rect.x.saturating_add(self.rect.width)
            && row >= self.rect.y
            && row < self.rect.y.saturating_add(self.rect.height)
    }
}

/// Finds the action under a mouse position.
///
/// Later areas win, so widgets rendered on top of earlier ones stay reachable.
pub fn hit_test(areas: &[HitArea], column: u16, row: u16) -> Option<UiAction> {
    areas
        .iter()
        .rev()
        .find(|area| area.contains(column, row))
        .map(|area| area.action)
}

/// Draws the whole UI into `frame` and returns the clickable regions of this
/// frame. `hover` carries the action resolved from the previous frame's hit
/// areas, which is what makes the pointer highlight possible without a second
/// layout pass.
pub fn draw(frame: &mut Frame, app: &App, hover: Option<UiAction>) -> Vec<HitArea> {
    let area = frame.area();

    // Two footer rows: the clickable button bar and a small hint line.
    let [body_area, status_area, footer_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(area);

    let body_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Bluetooth Audio Receiver ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = body_block.inner(body_area);
    frame.render_widget(body_block, body_area);

    if inner.height == 0 || inner.width == 0 {
        draw_status(frame, status_area, app);
        return draw_footer(frame, footer_area, hover);
    }

    // Header row plus one row per device, but never starve the detail/log panes.
    // The empty state needs a couple of rows for its hint.
    let max_device_height = inner.height.saturating_sub(5).max(1);
    let desired_device_height = if app.devices.is_empty() {
        3
    } else {
        app.devices.len() as u16 + 1
    };
    let device_height = desired_device_height.min(max_device_height);

    let [devices_area, sep_one, detail_area, sep_two, log_area] = Layout::vertical([
        Constraint::Length(device_height),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    let mut hits = draw_devices(frame, devices_area, app, hover);
    draw_separator(frame, sep_one, inner, "Detail");
    draw_detail(frame, detail_area, app);
    draw_separator(frame, sep_two, inner, "Log");
    draw_logs(frame, log_area, app);
    draw_status(frame, status_area, app);
    hits.extend(draw_footer(frame, footer_area, hover));
    hits
}

/// Second footer row: live volume bar (only when connected), the silence hint
/// and the auto-reconnect switch state.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let mut spans: Vec<Span<'static>> = Vec::new();

    if let Some(bar) = app.volume_bar() {
        // Level meter colour: quiet is safe (green), loud is hot (red). The
        // bands match the usual "green/yellow/red" meter reading.
        let color = if app.audio_level >= 0.75 {
            Color::Red
        } else if app.audio_level >= 0.4 {
            Color::Yellow
        } else {
            Color::Green
        };
        spans.push(Span::styled(" 音量 ", Style::default().fg(Color::White)));
        spans.push(Span::styled(
            bar,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }

    if app.silence_hint() {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            "未检测到音频 · 若设备正在播放，按 f 重连",
            Style::default().fg(Color::Yellow),
        ));
    }

    // Right-aligned switch indicator: its presence is what answers "is the
    // automatic reconnect on?" at a glance.
    let indicator = if app.auto_reconnect_on_silence {
        "自动重连 开"
    } else {
        "自动重连 关"
    };
    let indicator_style = if app.auto_reconnect_on_silence {
        Style::default().fg(Color::Green)
    } else {
        // "Off" is secondary information: dim it instead of competing with
        // the connection state. DarkGray is safe as a foreground colour.
        Style::default().fg(Color::DarkGray)
    };
    let used = spans_width(&spans);
    let padding = width.saturating_sub(used + UnicodeWidthStr::width(indicator));
    spans.push(Span::raw(" ".repeat(padding.max(1))));
    spans.push(Span::styled(indicator, indicator_style));

    let spans = truncate_spans(spans, width);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_devices(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    hover: Option<UiAction>,
) -> Vec<HitArea> {
    if area.height == 0 || area.width == 0 {
        return Vec::new();
    }
    let width = area.width as usize;

    let left = format!(" Devices ({})", app.devices.len());
    let right = "Status ";
    let gap = width.saturating_sub(UnicodeWidthStr::width(left.as_str()) + right.len());
    let header = Line::from(vec![
        // Section titles are yellow: they mark where a pane starts and never
        // collide with the cyan brand frame or the white/green content.
        Span::styled(
            left,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, Style::default().fg(Color::Gray)),
    ]);
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let rows_height = area.height.saturating_sub(1) as usize;
    if rows_height == 0 {
        return Vec::new();
    }

    if app.devices.is_empty() {
        let message = if app.scanning {
            " 正在扫描蓝牙设备…"
        } else {
            " 未发现可用设备"
        };
        // A scan in progress is "pending", so it shares the yellow used by the
        // status column and the log; the empty state is neutral grey.
        let message_style = if app.scanning {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Gray)
        };
        frame.render_widget(
            Paragraph::new(truncate_to_width(message, width)).style(message_style),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        if rows_height >= 2 && !app.scanning {
            frame.render_widget(
                Paragraph::new(truncate_to_width(
                    " 请确认蓝牙已开启且设备已配对，按 r 刷新",
                    width,
                ))
                .style(Style::default().fg(Color::Gray)),
                Rect::new(area.x, area.y + 2, area.width, 1),
            );
        }
        return Vec::new();
    }

    // Keep the selection visible without a full scroll state machine.
    let offset = if app.selected >= rows_height {
        app.selected + 1 - rows_height
    } else {
        0
    };

    // The selection highlight is painted per span rather than as a paragraph
    // style: that keeps the status column on the default background, where the
    // green/yellow markers stay readable (they would vanish on cyan).
    let mut hits = Vec::new();
    for (row, index) in (offset..app.devices.len()).take(rows_height).enumerate() {
        let rect = Rect::new(area.x, area.y + 1 + row as u16, area.width, 1);
        let hovered = hover == Some(UiAction::SelectDevice(index));
        frame.render_widget(
            Paragraph::new(device_line(app, index, width, hovered)),
            rect,
        );
        hits.push(HitArea {
            rect,
            action: UiAction::SelectDevice(index),
        });
    }
    hits
}

fn device_line(app: &App, index: usize, width: usize, hovered: bool) -> Line<'static> {
    let device = &app.devices[index];
    let selected = index == app.selected;
    // One column of right padding keeps the text off the border.
    let content_width = width.saturating_sub(1);
    let marker = if selected { " ▸ " } else { "   " };
    let status = status_text(app, &device.name);

    let status_width = UnicodeWidthStr::width(status.as_str());
    let name_room = content_width.saturating_sub(3 + status_width);
    let name = truncate_to_width(&device.name, name_room);
    let name_width = UnicodeWidthStr::width(name.as_str());
    let padding = content_width.saturating_sub(3 + name_width + status_width);

    // Highlight only the marker/name/padding block; the status span keeps its
    // own foreground colour on the default background.
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    // Hover is a hint that the row is clickable; it never hides the status
    // colour, which carries the connection state. It uses underline instead of
    // a grey background: DarkGray backgrounds are nearly invisible on the
    // user's terminal palette.
    let hover_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    // The connected device is green even before the eye reaches the status
    // column, so the active row can be found by name alone.
    let connected = matches!(
        &app.connection,
        ConnState::Connected(current) if current == &device.name
    );
    let (marker_style, name_style, padding_style) = if selected {
        (highlight, highlight, highlight)
    } else if hovered {
        (hover_style, hover_style, hover_style)
    } else if connected {
        (
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Green),
            Style::default(),
        )
    } else {
        (
            Style::default().fg(Color::Cyan),
            Style::default().fg(Color::White),
            Style::default(),
        )
    };

    Line::from(vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(name, name_style),
        Span::styled(" ".repeat(padding), padding_style),
        Span::styled(status.to_string(), status_style(app, &device.name)),
        Span::raw(" "),
    ])
}

/// Per-row status marker. Only the active device leaves `Idle`.
///
/// Every variant is exactly 8 columns wide, so the right-aligned column stays
/// put while the spinner animates.
fn status_text(app: &App, name: &str) -> String {
    match &app.connection {
        ConnState::Connected(current) if current == name => "已连接 ●".to_string(),
        ConnState::Connecting(current) if current == name => {
            format!("连接中 {}", app.spinner())
        }
        _ => "Idle".to_string(),
    }
}

fn status_style(app: &App, name: &str) -> Style {
    match &app.connection {
        ConnState::Connected(current) if current == name => {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        }
        ConnState::Connecting(current) if current == name => {
            Style::default().fg(Color::Yellow)
        }
        // Idle rows are secondary content. DarkGray is used as a foreground
        // only; as a background it would vanish on this terminal palette.
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn draw_separator(frame: &mut Frame, area: Rect, inner: Rect, title: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let label = truncate_to_width(&format!(" {title} "), width);
    let label_width = UnicodeWidthStr::width(label.as_str());
    let rule = "─".repeat(width.saturating_sub(label_width));

    // Yellow title over a dim rule: the section name stands out while the rule
    // itself stays quiet between panes.
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(rule, Style::default().fg(Color::DarkGray)),
        ])),
        area,
    );

    // Turn the surrounding border cells into proper T-junctions, keeping the
    // cyan of the frame.
    let junction_style = Style::default().fg(Color::Cyan);
    let buffer = frame.buffer_mut();
    if inner.x > 0 {
        buffer[(inner.x - 1, area.y)]
            .set_symbol("├")
            .set_style(junction_style);
    }
    if inner.right() < buffer.area().width {
        buffer[(inner.right(), area.y)]
            .set_symbol("┤")
            .set_style(junction_style);
    }
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let device = app.selected_device();
    // An empty list has no device at all, which is different from a device
    // whose id happens to be unparsable.
    let address = match device {
        Some(device) => device.addr.as_deref().unwrap_or(UNKNOWN_ADDRESS),
        None => "-",
    };

    // The state value shares the colour of the status column, so both places
    // read the same way. The device name is the primary subject and gets
    // magenta; the MAC address gets bright cyan (same family as the frame).
    let state_color = match &app.connection {
        ConnState::Connected(_) => Color::Green,
        ConnState::Connecting(_) => Color::Yellow,
        ConnState::Idle => Color::DarkGray,
    };
    let lines = vec![
        detail_line(
            "Name",
            device.map(|d| d.name.as_str()).unwrap_or("-"),
            Color::Magenta,
            width,
        ),
        detail_line("Addr", address, Color::LightCyan, width),
        detail_line("State", app.connection.label(), state_color, width),
    ];

    let lines: Vec<Line> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// One detail row: a blue `Label :` prefix and a value that carries its own
/// semantic colour.
fn detail_line(label: &str, value: &str, value_color: Color, width: usize) -> Line<'static> {
    let prefix = format!(" {label:<5}: ");
    let room = width.saturating_sub(DETAIL_PREFIX_WIDTH);
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(Color::Blue)),
        Span::styled(
            truncate_to_width(value, room),
            Style::default().fg(value_color),
        ),
    ])
}

fn draw_logs(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let height = area.height as usize;
    // Auto-scroll: always show the newest entries that fit.
    let start = app.logs.len().saturating_sub(height);
    let lines: Vec<Line> = app
        .logs
        .iter()
        .skip(start)
        .map(|entry| log_line(entry, width))
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

/// Byte length of the stamp `App::log` prepends: `[HH:MM:SS]`.
const LOG_STAMP_BYTES: usize = 10;

/// Wording that marks a log line as failed, successful or still in progress.
///
/// The log protocol is plain strings (no severity field), so the tone is
/// inferred here. Keywords are lowercase because the body is lowercased first.
const LOG_ERROR_WORDS: [&str; 6] = ["失败", "错误", "拒绝", "超时", "unknownfailure", "denied"];
const LOG_SUCCESS_WORDS: [&str; 4] = ["已连接", "成功", "success", "opened"];
const LOG_PENDING_WORDS: [&str; 5] = ["正在", "等待", "冷却", "重连", "扫描"];

/// Splits the fixed timestamp prefix from the message so the two can be
/// coloured separately. Returns the whole entry as body when the prefix is not
/// the expected `[HH:MM:SS] ` shape.
fn split_log_stamp(entry: &str) -> (&str, &str) {
    if entry.len() > LOG_STAMP_BYTES {
        let bytes = entry.as_bytes();
        if bytes[0] == b'[' && bytes[9] == b']' && bytes[10] == b' ' {
            // Skip the separating space so the gap is added exactly once.
            return (&entry[..LOG_STAMP_BYTES], &entry[LOG_STAMP_BYTES + 1..]);
        }
    }
    ("", entry)
}

/// Message colour, in priority order: error > success > pending > neutral.
///
/// `忽略` wins over the success keywords: a stale `Opened` event that was
/// discarded is not a success even though it mentions the word.
fn log_color(body: &str) -> Color {
    let lower = body.to_lowercase();
    if LOG_ERROR_WORDS.iter().any(|word| lower.contains(word)) {
        Color::Red
    } else if lower.contains("忽略") {
        Color::Gray
    } else if LOG_SUCCESS_WORDS.iter().any(|word| lower.contains(word)) {
        Color::Green
    } else if LOG_PENDING_WORDS.iter().any(|word| lower.contains(word)) {
        Color::Yellow
    } else {
        Color::Gray
    }
}

/// One log row: dim timestamp, then the message coloured by its tone.
fn log_line(entry: &str, width: usize) -> Line<'static> {
    let (stamp, body) = split_log_stamp(entry);
    let gap = if stamp.is_empty() { "" } else { " " };
    let reserved = 1 + UnicodeWidthStr::width(stamp) + UnicodeWidthStr::width(gap);
    let room = width.saturating_sub(reserved);
    let shown = truncate_to_width(body, room);

    Line::from(vec![
        Span::raw(" "),
        Span::styled(stamp.to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled(gap.to_string(), Style::default().fg(Color::DarkGray)),
        // Classify the full body, not the truncated text: an ellipsis could
        // otherwise hide the keyword.
        Span::styled(shown, Style::default().fg(log_color(body))),
    ])
}

/// One footer button: wide label (with the keyboard shortcut), the condensed
/// label used on narrow terminals, and the action a click produces.
const FOOTER_BUTTONS: [(&str, &str, UiAction); 6] = [
    ("连接 Enter", "连接", UiAction::Connect),
    ("重连 f", "重连", UiAction::Reconnect),
    ("断开 d", "断开", UiAction::Disconnect),
    ("刷新 r", "刷新", UiAction::Refresh),
    ("蓝牙设置 b", "设置", UiAction::BluetoothSettings),
    ("退出 q", "退出", UiAction::Quit),
];

/// Columns added around a wide label by `[ ` and ` ]`.
const BUTTON_CHROME: usize = 4;
/// Columns added around a condensed label by `[` and `]`.
const COMPACT_BUTTON_CHROME: usize = 2;

/// Width of a button rendered with the given label.
fn button_width(label: &str, compact: bool) -> usize {
    UnicodeWidthStr::width(label)
        + if compact {
            COMPACT_BUTTON_CHROME
        } else {
            BUTTON_CHROME
        }
}

/// Button text. The condensed form drops the inner padding to save two columns
/// per button, which is what lets all six fit on a 40-column terminal.
fn button_text(label: &str, compact: bool) -> String {
    if compact {
        format!("[{label}]")
    } else {
        format!("[ {label} ]")
    }
}

/// Picks wide or condensed labels, then packs the buttons into `width` columns.
///
/// Degradation order: drop the keyboard shortcuts, drop the gaps between
/// buttons, then clip the trailing button. Returns `(offset, visible width,
/// button index)` for every button that starts inside the area.
fn layout_buttons(width: usize, compact: bool) -> Vec<(usize, usize, usize)> {
    let sizes: Vec<usize> = FOOTER_BUTTONS
        .iter()
        .map(|(wide, narrow, _)| button_width(if compact { narrow } else { wide }, compact))
        .collect();
    let separators = sizes.len().saturating_sub(1);
    let packed: usize = sizes.iter().sum::<usize>() + separators;
    // A single space between buttons makes them read as separate controls; it
    // is the first thing to go when the terminal is too narrow.
    let gap = usize::from(packed + 1 <= width);
    let total: usize = sizes.iter().sum::<usize>() + gap * separators;
    let start = usize::from(total + 1 <= width);

    let mut layout = Vec::with_capacity(FOOTER_BUTTONS.len());
    let mut offset = start;
    for (index, size) in sizes.iter().enumerate() {
        if offset >= width {
            break;
        }
        let visible = (*size).min(width - offset);
        // A one- or two-column sliver would render as "…" but still act as a
        // button, which is more confusing than an unreachable action: the
        // keyboard shortcut remains available.
        if visible < 4 {
            break;
        }
        layout.push((offset, visible, index));
        offset += size + gap;
    }
    layout
}

/// Footer: a row of clickable buttons plus a one-line hint, both padded to the
/// terminal width so the alignment checks stay valid.
fn draw_footer(frame: &mut Frame, area: Rect, hover: Option<UiAction>) -> Vec<HitArea> {
    if area.height == 0 || area.width == 0 {
        return Vec::new();
    }
    let width = area.width as usize;

    let wide_total = FOOTER_BUTTONS
        .iter()
        .map(|(wide, _, _)| button_width(wide, false))
        .sum::<usize>()
        + FOOTER_BUTTONS.len()
        - 1
        + 1;
    let compact = wide_total > width;

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut hits = Vec::new();
    let mut cursor = 0usize;
    for (offset, visible, index) in layout_buttons(width, compact) {
        let (wide, narrow, action) = FOOTER_BUTTONS[index];
        let label = if compact { narrow } else { wide };
        let text = button_text(label, compact);

        if offset > cursor {
            spans.push(Span::raw(" ".repeat(offset - cursor)));
        }
        let shown = if visible >= UnicodeWidthStr::width(text.as_str()) {
            text
        } else {
            truncate_to_width(&text, visible)
        };
        cursor = offset + UnicodeWidthStr::width(shown.as_str());
        spans.push(Span::styled(shown, button_style(hover == Some(action))));
        hits.push(HitArea {
            rect: Rect::new(area.x + offset as u16, area.y, visible as u16, 1),
            action,
        });
    }
    if cursor < width {
        spans.push(Span::raw(" ".repeat(width - cursor)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if area.height >= 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                hint_text(width),
                Style::default().fg(Color::DarkGray),
            )),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }

    hits
}

fn button_style(hovered: bool) -> Style {
    if hovered {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        // Cyan on the default background, matching the frame: a DarkGray
        // background disappears on terminals whose palette maps that slot
        // close to the window colour. The brackets carry the "button" cue.
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

/// Keyboard hints for the row under the buttons. Kept short so the mouse hint
/// survives on narrow terminals.
fn hint_text(width: usize) -> String {
    const FULL: &str = " ↑↓/jk 选择 · Enter 连接 · 鼠标：点击按钮或设备行，双击连接 · Shift+拖选复制";
    // Exactly 40 columns: keeps both the keyboard and the mouse hint usable on
    // the narrow layout.
    const COMPACT: &str = " ↑↓/jk 选择 · 鼠标操作 · Shift+拖选复制";

    let text = if UnicodeWidthStr::width(FULL) <= width {
        FULL
    } else {
        COMPACT
    };
    let mut out = truncate_to_width(text, width);
    let used = UnicodeWidthStr::width(out.as_str());
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Converts a rendered buffer into plain-text rows.
///
/// A double-width symbol owns the cell after it, and ratatui blanks that
/// continuation cell; skipping it makes the dump and the real screen agree.
pub fn buffer_lines(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Vec<String> {
    (0..height)
        .map(|y| {
            let mut line = String::new();
            let mut x = 0u16;
            while x < width {
                let symbol = buffer[(x, y)].symbol();
                line.push_str(symbol);
                x += UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            line
        })
        .collect()
}

/// Truncates `text` to at most `max_width` display cells, appending `…` when
/// something was cut. Never splits a wide character in half.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + width > max_width - 1 {
            break;
        }
        out.push(character);
        used += width;
    }
    out.push('…');
    out
}

fn truncate_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= max_width {
            break;
        }
        let content = span.content.to_string();
        let width = UnicodeWidthStr::width(content.as_str());
        if used + width <= max_width {
            used += width;
            out.push(Span::styled(content, span.style));
        } else {
            let clipped = truncate_to_width(&content, max_width - used);
            out.push(Span::styled(clipped, span.style));
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::{DeviceInfo, Evt, SILENCE_HINT_DELAY};
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::{Duration, Instant};

    fn render_with_hits(
        app: &App,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Vec<HitArea>) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                hits = draw(frame, app, None);
            })
            .unwrap();

        (buffer_lines(terminal.backend().buffer(), width, height), hits)
    }

    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        render_with_hits(app, width, height).0
    }

    fn device(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            is_paired: true,
            kind: "DeviceInterface".to_string(),
        }
    }

    fn sample_app() -> App {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![
            device("id-ipad", "张三的iPad"),
            device("id-civi", "李四的手机"),
            device("id-q35", "Soundcore Life Q35"),
        ]));
        app
    }

    #[test]
    fn renders_all_sections() {
        let app = sample_app();
        let lines = render(&app, 80, 24);
        let text = lines.join("\n");

        assert!(text.contains("Bluetooth Audio Receiver"));
        assert!(text.contains("Devices (3)"));
        assert!(text.contains("Status"));
        assert!(text.contains("张三的iPad"));
        assert!(text.contains("李四的手机"));
        assert!(text.contains("Soundcore Life Q35"));
        assert!(text.contains("Detail"));
        assert!(text.contains("Name"));
        assert!(text.contains("State"));
        assert!(text.contains("Log"));
        assert!(text.contains("启动中"));
    }

    #[test]
    fn every_line_keeps_terminal_width() {
        let app = sample_app();
        for width in [40u16, 60, 80, 120] {
            for line in render(&app, width, 24) {
                assert_eq!(UnicodeWidthStr::width(line.as_str()), width as usize);
            }
        }
    }

    #[test]
    fn connected_marker_is_rendered() {
        let mut app = sample_app();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });

        let text = render(&app, 80, 24).join("\n");
        assert!(text.contains("已连接 ●"));
        assert!(text.contains("Connected"));
    }

    fn real_device_info(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            is_paired: false,
            kind: "DeviceInterface".to_string(),
        }
    }

    const IPAD_ID: &str = concat!(
        r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}",
        r"_VID&0001004c_PID&7012#7&d8f392&0&AABBCCDDEE01_C00000000",
        r"#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK"
    );

    #[test]
    fn detail_pane_shows_a_mac_address_instead_of_the_pnp_path() {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![real_device_info(IPAD_ID, "张三的iPad")]));

        let text = render(&app, 80, 24).join("\n");
        assert!(text.contains("Addr : AA:BB:CC:DD:EE:01"));
        assert!(!text.contains("BTHENUM"));
    }

    #[test]
    fn detail_pane_fits_a_mac_on_a_40_column_terminal() {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![real_device_info(IPAD_ID, "张三的iPad")]));

        let lines = render(&app, 40, 14);
        let text = lines.join("\n");
        assert!(text.contains("Addr : AA:BB:CC:DD:EE:01"));
        for line in lines {
            assert_eq!(UnicodeWidthStr::width(line.as_str()), 40);
        }
    }

    #[test]
    fn unknown_address_uses_a_placeholder() {
        let text = render(&sample_app(), 80, 24).join("\n");
        assert!(text.contains(UNKNOWN_ADDRESS));
    }

    #[test]
    fn connecting_row_animates_the_spinner() {
        let mut app = sample_app();
        app.apply(Evt::Connecting("张三的iPad".to_string()));

        let first_frame = app.spinner();
        let first = render(&app, 80, 24).join("\n");
        assert!(first.contains(&format!("连接中 {first_frame}")));

        app.tick();
        let second_frame = app.spinner();
        assert_ne!(first_frame, second_frame);
        let second = render(&app, 80, 24).join("\n");
        assert!(second.contains(&format!("连接中 {second_frame}")));
    }

    #[test]
    fn status_colors_are_green_when_connected_and_yellow_while_connecting() {
        let mut app = sample_app();
        app.apply(Evt::Connecting("张三的iPad".to_string()));

        // The active device is also the selected row: the marker colour must
        // survive the selection highlight.
        let frame = app.spinner();
        let (_, connecting) = render_colors(&app, frame);
        assert_eq!(connecting, Some(Color::Yellow));

        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        let (connected, connecting) = render_colors(&app, frame);
        assert_eq!(connected, Some(Color::Green));
        assert_eq!(connecting, None);
    }

    /// Returns the foreground colours of the marker glyphs in the device rows.
    fn render_colors(app: &App, spinner: &str) -> (Option<Color>, Option<Color>) {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, app, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let mut marker = None;
        let mut spinner_color = None;
        for y in 0..24 {
            for x in 0..80 {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "●" {
                    marker = Some(cell.fg);
                } else if cell.symbol() == spinner {
                    spinner_color = Some(cell.fg);
                }
            }
        }
        (marker, spinner_color)
    }

    #[test]
    fn long_log_lines_are_truncated_inside_the_pane() {
        let mut app = sample_app();
        app.log("很长的日志".repeat(40));

        let lines = render(&app, 40, 14);
        for line in &lines {
            assert_eq!(UnicodeWidthStr::width(line.as_str()), 40);
        }
        let log_line = lines
            .iter()
            .find(|line| line.contains("很长的日志"))
            .expect("long log entry should be visible");
        assert!(log_line.contains('…'), "expected an ellipsis: {log_line:?}");
    }

    #[test]
    fn log_pane_follows_the_newest_entries() {
        let mut app = sample_app();
        for index in 0..30 {
            app.log(format!("MARK{index:02}"));
        }

        let text = render(&app, 80, 24).join("\n");
        assert!(text.contains("MARK29"));
        assert!(!text.contains("MARK00"));
    }

    #[test]
    fn empty_state_explains_what_to_do() {
        let mut app = App::new();
        let scanning = render(&app, 80, 24).join("\n");
        assert!(scanning.contains("正在扫描蓝牙设备…"));

        app.apply(Evt::DeviceList(Vec::new()));
        let empty = render(&app, 80, 24).join("\n");
        assert!(empty.contains("未发现可用设备"));
        assert!(empty.contains("按 r 刷新"));
        assert!(empty.contains("Devices (0)"));
        assert!(empty.contains("Addr : -"));
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        for app in [App::new(), sample_app()] {
            for (width, height) in [(20u16, 8u16), (30, 10), (80, 12)] {
                let _ = render(&app, width, height);
            }
        }
    }

    #[test]
    fn truncation_respects_wide_chars() {
        assert_eq!(truncate_to_width("张三的iPad", 100), "张三的iPad");
        assert_eq!(
            UnicodeWidthStr::width(truncate_to_width("张三的iPad", 9).as_str()),
            9
        );
        assert_eq!(truncate_to_width("abcdef", 0), "");
    }

    #[test]
    fn volume_bar_is_hidden_until_connected() {
        let mut app = sample_app();
        assert!(!render(&app, 80, 24).join("\n").contains("音量"));

        app.apply(Evt::Connecting("张三的iPad".to_string()));
        assert!(!render(&app, 80, 24).join("\n").contains("音量"));

        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        // Repeated samples converge the exponential smoothing on 0.6.
        for _ in 0..20 {
            app.apply(Evt::AudioLevel(0.6));
        }

        let text = render(&app, 80, 24).join("\n");
        assert!(text.contains("音量"));
        // Level 0.6 fills five cells with the fifth glyph.
        assert!(text.contains("▅▅▅▅▅▁▁▁"), "volume bar missing:\n{text}");
    }

    #[test]
    fn silence_hint_appears_after_eight_silent_seconds() {
        let mut app = sample_app();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        app.apply(Evt::AudioLevel(0.0));
        assert!(!render(&app, 80, 24).join("\n").contains("未检测到音频"));

        app.connected_at =
            Some(Instant::now() - SILENCE_HINT_DELAY - Duration::from_millis(10));

        let text = render(&app, 80, 24).join("\n");
        assert!(text.contains("未检测到音频"), "hint missing:\n{text}");
        assert!(text.contains("按 f 重连"), "hint is not actionable:\n{text}");
    }

    #[test]
    fn detected_audio_suppresses_the_silence_hint() {
        let mut app = sample_app();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        app.apply(Evt::AudioLevel(0.4));
        app.connected_at =
            Some(Instant::now() - SILENCE_HINT_DELAY - Duration::from_millis(10));

        assert!(!render(&app, 80, 24).join("\n").contains("未检测到音频"));
    }

    fn button_actions(hits: &[HitArea]) -> Vec<UiAction> {
        hits.iter()
            .filter(|hit| !matches!(hit.action, UiAction::SelectDevice(_)))
            .map(|hit| hit.action)
            .collect()
    }

    #[test]
    fn footer_buttons_carry_labels_and_shortcuts() {
        let app = sample_app();
        let (lines, hits) = render_with_hits(&app, 80, 24);
        // The footer is the third-to-last row: buttons, then the hint line.
        let footer = &lines[22];
        assert!(footer.contains("[ 连接 Enter ]"), "footer:\n{footer}");
        assert!(footer.contains("[ 重连 f ]"), "footer:\n{footer}");
        assert!(footer.contains("[ 蓝牙设置 b ]"), "footer:\n{footer}");
        assert!(footer.contains("[ 退出 q ]"), "footer:\n{footer}");

        assert_eq!(
            button_actions(&hits),
            vec![
                UiAction::Connect,
                UiAction::Reconnect,
                UiAction::Disconnect,
                UiAction::Refresh,
                UiAction::BluetoothSettings,
                UiAction::Quit,
            ]
        );
        // Keyboard hints survive next to the buttons.
        assert!(lines[23].contains("Shift+拖选复制"), "hints:\n{}", lines[23]);
    }

    #[test]
    fn narrow_button_bar_drops_shortcuts_but_keeps_every_action() {
        let (lines, hits) = render_with_hits(&sample_app(), 40, 14);
        assert_eq!(button_actions(&hits).len(), 6);
        let footer = &lines[12];
        assert!(footer.contains("[连接]"), "footer:\n{footer}");
        assert!(!footer.contains("Enter"), "footer:\n{footer}");
    }

    #[test]
    fn clicking_a_device_row_targets_that_index() {
        let (lines, hits) = render_with_hits(&sample_app(), 80, 24);
        let row = hits
            .iter()
            .find(|hit| hit.action == UiAction::SelectDevice(1))
            .expect("the second device row must be clickable");
        assert!(lines[row.rect.y as usize].contains("李四的手机"));

        // A point inside the row resolves back to the same index.
        assert_eq!(
            hit_test(&hits, row.rect.x + 2, row.rect.y),
            Some(UiAction::SelectDevice(1))
        );
        // One row lower belongs to the next device.
        assert_eq!(
            hit_test(&hits, row.rect.x + 2, row.rect.y + 1),
            Some(UiAction::SelectDevice(2))
        );
    }

    #[test]
    fn partially_visible_buttons_stay_wide_enough_to_read() {
        for width in [24u16, 25, 26, 27, 28, 29, 32, 39] {
            let (_, hits) = render_with_hits(&sample_app(), width, 10);
            for hit in hits
                .iter()
                .filter(|hit| !matches!(hit.action, UiAction::SelectDevice(_)))
            {
                assert!(
                    hit.rect.width >= 4,
                    "{:?} would render as a sliver at width {width}",
                    hit.rect
                );
            }
        }
    }

    #[test]
    fn hit_test_covers_inside_edges_and_rejects_outside() {
        let hits = [
            HitArea {
                rect: Rect::new(10, 5, 6, 1),
                action: UiAction::Connect,
            },
            HitArea {
                rect: Rect::new(16, 5, 4, 1),
                action: UiAction::Quit,
            },
        ];

        assert_eq!(hit_test(&hits, 10, 5), Some(UiAction::Connect));
        // Inclusive left edge, inclusive last column.
        assert_eq!(hit_test(&hits, 15, 5), Some(UiAction::Connect));
        // The adjacent button starts exactly at the exclusive right edge.
        assert_eq!(hit_test(&hits, 16, 5), Some(UiAction::Quit));
        // One past the last column, and one row outside.
        assert_eq!(hit_test(&hits, 20, 5), None);
        assert_eq!(hit_test(&hits, 9, 5), None);
        assert_eq!(hit_test(&hits, 10, 4), None);
        assert_eq!(hit_test(&hits, 10, 6), None);
        assert_eq!(hit_test(&[], 10, 5), None);
    }

    #[test]
    fn later_hit_areas_win() {
        let hits = [
            HitArea {
                rect: Rect::new(0, 0, 10, 2),
                action: UiAction::Refresh,
            },
            HitArea {
                rect: Rect::new(0, 0, 2, 1),
                action: UiAction::Quit,
            },
        ];
        assert_eq!(hit_test(&hits, 1, 0), Some(UiAction::Quit));
        assert_eq!(hit_test(&hits, 1, 1), Some(UiAction::Refresh));
    }

    #[test]
    fn hit_areas_stay_inside_the_terminal() {
        let app = sample_app();
        for (width, height) in [(24u16, 10u16), (40, 14), (60, 20), (80, 24), (120, 30)] {
            let (_, hits) = render_with_hits(&app, width, height);
            assert!(!hits.is_empty(), "no hit areas at {width}x{height}");
            for hit in &hits {
                assert!(
                    hit.rect.right() <= width,
                    "{:?} exceeds width {width}",
                    hit.rect
                );
                assert!(
                    hit.rect.bottom() <= height,
                    "{:?} exceeds height {height}",
                    hit.rect
                );
            }
        }
    }

    #[test]
    fn hovered_button_is_recoloured_without_changing_layout() {
        let app = sample_app();
        let (plain, hits) = render_with_hits(&app, 80, 24);
        let connect = hits
            .iter()
            .find(|hit| hit.action == UiAction::Connect)
            .expect("connect button");

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &app, Some(UiAction::Connect));
            })
            .unwrap();

        let hovered = buffer_lines(terminal.backend().buffer(), 80, 24);
        assert_eq!(plain, hovered, "hover must not reflow the text");

        let cell = &terminal.backend().buffer()[(connect.rect.x, connect.rect.y)];
        assert_eq!(cell.symbol(), "[");
        assert_eq!(cell.bg, Color::Cyan);
    }

    #[test]
    fn auto_reconnect_state_is_visible() {
        let mut app = sample_app();
        assert!(render(&app, 80, 24).join("\n").contains("自动重连 关"));

        app.auto_reconnect_on_silence = true;
        assert!(render(&app, 80, 24).join("\n").contains("自动重连 开"));
    }

    /// Renders into a backend and returns both the text rows and the buffer,
    /// so tests can assert on cell colours.
    fn render_coloured(app: &App, width: u16, height: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, app, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (buffer_lines(&buffer, width, height), buffer)
    }

    #[test]
    fn log_tone_follows_the_keyword_priority() {
        assert_eq!(log_color("连接 手机 失败：超时"), Color::Red);
        // Red beats green when one line reports both.
        assert_eq!(log_color("已连接后失败"), Color::Red);
        assert_eq!(log_color("已连接 手机（音频已开始传输）"), Color::Green);
        assert_eq!(log_color("强制重连 手机：断开并重新连接…"), Color::Yellow);
        assert_eq!(log_color("设备已移除：手机"), Color::Gray);
        // A discarded stale `Opened` is not a success.
        assert_eq!(log_color("忽略 手机 的过期 Opened 事件"), Color::Gray);
    }

    #[test]
    fn log_timestamp_is_dimmed_while_the_body_keeps_its_colour() {
        let mut app = sample_app();
        app.log("连接失败：超时");
        let (lines, buffer) = render_coloured(&app, 80, 24);
        let row = lines
            .iter()
            .position(|line| line.contains("连接失败"))
            .expect("failure log must be visible");
        // Column 0 is the pane border and column 1 the line's leading space,
        // so the stamp starts at column 2; the ASCII prefix keeps byte and
        // display columns in sync here.
        let stamp_column = lines[row].find('[').unwrap() as u16;
        let body_column = lines[row].find("连接失败").unwrap() as u16;
        assert_eq!(buffer[(stamp_column, row as u16)].fg, Color::DarkGray);
        assert_eq!(buffer[(body_column, row as u16)].fg, Color::Red);
    }

    #[test]
    fn section_titles_are_yellow() {
        let (lines, buffer) = render_coloured(&sample_app(), 80, 24);
        for needle in ["Devices (3)", " Detail ", " Log "] {
            let row = lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not rendered"));
            let column = lines[row].find(needle.trim_start()).unwrap() as u16;
            assert_eq!(
                buffer[(column, row as u16)].fg,
                Color::Yellow,
                "{needle:?} should be yellow"
            );
        }
    }

    #[test]
    fn detail_labels_are_blue_and_values_carry_their_own_colour() {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![real_device_info(IPAD_ID, "张三的iPad")]));
        let (lines, buffer) = render_coloured(&app, 80, 24);

        let row_of = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not rendered")) as u16
        };
        let name_row = row_of("Name :");
        let addr_row = row_of("Addr :");
        let state_row = row_of("State:");

        // Column 0 is the pane border, column 1 the line's leading space, so
        // the label starts at 2 and the value at 1 + DETAIL_PREFIX_WIDTH.
        let label_column = 2u16;
        let value_column = 1 + DETAIL_PREFIX_WIDTH as u16;
        assert_eq!(buffer[(label_column, name_row)].fg, Color::Blue);
        assert_eq!(buffer[(label_column, addr_row)].fg, Color::Blue);
        // The device name is the primary subject and gets magenta.
        assert_eq!(buffer[(value_column, name_row)].fg, Color::Magenta);
        // The MAC address gets bright cyan.
        assert_eq!(buffer[(value_column, addr_row)].fg, Color::LightCyan);
        // Idle state is dimmed, matching the status column.
        assert_eq!(buffer[(value_column, state_row)].fg, Color::DarkGray);
    }

    #[test]
    fn volume_bar_colour_reads_like_a_level_meter() {
        let mut app = sample_app();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });

        let mut bar_colour = |level: f32| {
            app.audio_level = level;
            let (lines, buffer) = render_coloured(&app, 80, 24);
            let row = lines.iter().position(|line| line.contains("音量")).unwrap();
            // `find` is a byte offset and the label is CJK, so convert to a
            // display column before indexing the buffer.
            let byte = lines[row].find('▁').expect("bar glyph");
            let column = UnicodeWidthStr::width(&lines[row][..byte]) as u16;
            buffer[(column, row as u16)].fg
        };

        assert_eq!(bar_colour(0.9), Color::Red);
        assert_eq!(bar_colour(0.5), Color::Yellow);
        assert_eq!(bar_colour(0.1), Color::Green);
    }
}
