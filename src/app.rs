//! Application state machine for the TUI.
//!
//! P3: the state is driven entirely by [`Evt`] values from the worker thread.
//! This module never touches WinRT and never blocks; it is pure enough to test
//! without a Bluetooth adapter.

use std::collections::VecDeque;
use std::time::Instant;

use crate::bt::{
    mac_address, silence_due, Cmd, DeviceInfo, Evt, AUDIO_ACTIVE_THRESHOLD,
};

/// Upper bound for the log ring buffer.
pub const MAX_LOGS: usize = 200;

/// Exponential smoothing factor applied to every peak sample.
///
/// Raw WASAPI peaks jump by tens of percent between 200 ms samples; smoothing
/// makes the volume bar readable without hiding a real silence.
pub const AUDIO_SMOOTHING: f32 = 0.35;

/// Eight-step volume bar glyphs, lowest to highest.
pub const VOLUME_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Number of cells in the rendered volume bar.
const VOLUME_CELLS: usize = 8;

/// Shown in the detail pane when the device id carries no readable address.
/// The full id is logged instead, so the path is never silently lost.
pub const UNKNOWN_ADDRESS: &str = "未知（见日志）";

/// Spinner glyphs for the "connecting" state, advanced by [`App::tick`].
const SPINNER_FRAMES: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

/// One row of the device list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub name: String,
    /// Raw PnP id. Kept because `TryCreateFromId` needs it verbatim.
    pub id: String,
    /// Peer address for display only; `None` when the id could not be parsed.
    pub addr: Option<String>,
}

impl From<DeviceInfo> for DeviceRow {
    fn from(info: DeviceInfo) -> Self {
        let addr = mac_address(&info.id);
        Self {
            name: display_name(&info.name),
            id: info.id,
            addr,
        }
    }
}

/// Connection state. The payload is the device name the state refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Idle,
    Connecting(String),
    Connected(String),
}

impl ConnState {
    /// Short label used in the status column and the detail pane.
    pub fn label(&self) -> &'static str {
        match self {
            ConnState::Idle => "Idle",
            ConnState::Connecting(_) => "Connecting",
            ConnState::Connected(_) => "Connected",
        }
    }

    /// Device name this state is attached to, if any.
    pub fn device_name(&self) -> Option<&str> {
        match self {
            ConnState::Idle => None,
            ConnState::Connecting(name) | ConnState::Connected(name) => Some(name),
        }
    }
}

/// Whole-application state owned by the UI thread.
pub struct App {
    pub devices: Vec<DeviceRow>,
    pub selected: usize,
    pub connection: ConnState,
    /// Ring buffer, oldest first. Capped at [`MAX_LOGS`].
    pub logs: VecDeque<String>,
    pub should_quit: bool,
    /// True between a refresh request and the next `DeviceList`.
    pub scanning: bool,
    /// 100 ms animation clock driving the connecting spinner.
    pub ticks: u64,

    /// Smoothed peak (`0.0..=1.0`) of the default render endpoint.
    pub audio_level: f32,
    /// Most recent raw sample, kept for diagnostics and tests.
    pub audio_peak: f32,
    /// Mirrors `--auto-reconnect-on-silence` so the status bar can show it.
    pub auto_reconnect_on_silence: bool,
    /// At least one sample since `Opened` was above the audibility threshold.
    audio_seen: bool,
    /// Successful meter reads since `Opened`; zero means "meter unavailable".
    audio_samples: u32,
    /// When the current link reached `Opened`.
    pub(crate) connected_at: Option<Instant>,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            devices: Vec::new(),
            selected: 0,
            connection: ConnState::Idle,
            logs: VecDeque::new(),
            should_quit: false,
            scanning: true,
            ticks: 0,
            audio_level: 0.0,
            audio_peak: 0.0,
            auto_reconnect_on_silence: false,
            audio_seen: false,
            audio_samples: 0,
            connected_at: None,
        };
        app.log("启动中，等待蓝牙服务…");
        app
    }

    /// Follows a worker peak sample with exponential smoothing.
    ///
    /// Only samples for the active `Connected` link are accepted: a late event
    /// from a previous link must not light up the bar.
    fn apply_audio_level(&mut self, level: f32) {
        let level = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.audio_peak = level;

        if !matches!(self.connection, ConnState::Connected(_)) {
            return;
        }

        self.audio_level += (level - self.audio_level) * AUDIO_SMOOTHING;
        if self.audio_level < 0.0001 {
            self.audio_level = 0.0;
        }
        self.audio_samples = self.audio_samples.saturating_add(1);
        if level >= AUDIO_ACTIVE_THRESHOLD {
            self.audio_seen = true;
        }
    }

    /// Eight-cell volume bar, or `None` while not connected.
    pub fn volume_bar(&self) -> Option<String> {
        if !matches!(self.connection, ConnState::Connected(_)) {
            return None;
        }
        Some(render_volume_bar(self.audio_level))
    }

    /// True when the link has been open for [`SILENCE_HINT_DELAY`] with only
    /// silent samples. Detecting audio once since connecting suppresses it:
    /// a later pause is just a pause.
    pub fn silence_hint(&self) -> bool {
        let Some(connected_at) = self.connected_at else {
            return false;
        };
        silence_due(
            connected_at.elapsed(),
            self.audio_seen,
            self.audio_samples,
        )
    }

    fn reset_audio(&mut self) {
        self.audio_level = 0.0;
        self.audio_peak = 0.0;
        self.audio_seen = false;
        self.audio_samples = 0;
        self.connected_at = None;
    }

    /// Advances the animation clock. Called once per 100 ms by the UI loop.
    pub fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
    }

    /// Current spinner glyph, rotating through four frames.
    pub fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[(self.ticks % SPINNER_FRAMES.len() as u64) as usize]
    }

    /// Currently selected row, if the list is non-empty.
    pub fn selected_device(&self) -> Option<&DeviceRow> {
        self.devices.get(self.selected)
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.devices.len() {
            self.selected += 1;
        }
    }

    /// Selects a row by index, ignoring indices that are no longer valid (a
    /// device can disappear between the frame that was drawn and the click).
    pub fn select_index(&mut self, index: usize) {
        if index < self.devices.len() {
            self.selected = index;
        }
    }

    /// Asks the worker to connect to the selected device.
    ///
    /// Returns the command for the caller to send; `None` when there is
    /// nothing sensible to do (empty list, already connected/connecting).
    pub fn connect_selected(&mut self) -> Option<Cmd> {
        let Some(device) = self.devices.get(self.selected) else {
            self.log("没有可连接的设备");
            return None;
        };
        let name = device.name.clone();
        let device_id = device.id.clone();

        match &self.connection {
            ConnState::Connected(current) if current == &name => {
                self.log(format!("{name} 已经连接"));
                return None;
            }
            ConnState::Connecting(current) if current == &name => {
                self.log(format!("{name} 正在连接中，请稍候"));
                return None;
            }
            _ => {}
        }

        // The detail pane hides the raw PnP path, so keep it in the log: it is
        // the only thing that makes a failed `TryCreateFromId` diagnosable.
        self.log(format!("设备 ID（原始）：{device_id}"));
        self.log(format!("请求连接 {name}…"));
        Some(Cmd::Connect { device_id, name })
    }

    pub fn request_disconnect(&mut self) -> Cmd {
        Cmd::Disconnect
    }

    /// `f`: ask the worker to close and reopen the link.
    ///
    /// While connected the worker already knows the device; with no link it
    /// takes the selected row instead. With neither, the worker just logs a
    /// hint (it never panics).
    pub fn request_force_reconnect(&mut self) -> Cmd {
        if let Some(name) = self.connection.device_name().map(str::to_string) {
            self.log(format!("强制重连 {name}：断开并重新连接…"));
            return Cmd::ForceReconnect {
                device_id: None,
                name: Some(name),
            };
        }

        match self
            .selected_device()
            .map(|device| (device.id.clone(), device.name.clone()))
        {
            Some((device_id, name)) => {
                self.log(format!("当前没有连接，将改为连接 {name}…"));
                Cmd::ForceReconnect {
                    device_id: Some(device_id),
                    name: Some(name),
                }
            }
            None => {
                self.log("当前没有连接，也没有可连接的设备");
                Cmd::ForceReconnect {
                    device_id: None,
                    name: None,
                }
            }
        }
    }

    pub fn request_refresh(&mut self) -> Cmd {
        // Clear locally so the pane shows the "scanning" placeholder instead of
        // a stale list until the worker sends the new snapshot.
        self.scanning = true;
        self.devices.clear();
        self.selected = 0;
        Cmd::RefreshDevices
    }

    pub fn request_bluetooth_settings(&self) -> Cmd {
        Cmd::OpenBluetoothSettings
    }

    pub fn request_quit(&mut self) {
        self.should_quit = true;
        self.log("正在退出…");
    }

    /// Applies one worker event. This is the only way the connection state and
    /// the device list change.
    pub fn apply(&mut self, event: Evt) {
        match event {
            Evt::WorkerReady => self.log("蓝牙 worker 已就绪"),

            Evt::DeviceList(list) => {
                self.scanning = false;
                let previous_id = self.selected_device().map(|device| device.id.clone());
                self.devices = list.into_iter().map(DeviceRow::from).collect();
                self.selected = previous_id
                    .and_then(|id| self.devices.iter().position(|device| device.id == id))
                    .unwrap_or(0);
                self.clamp_selection();
                self.log_unresolved_addresses();
            }

            Evt::DeviceAdded(info) => {
                if !self.devices.iter().any(|device| device.id == info.id) {
                    let row = DeviceRow::from(info);
                    self.log(format!("发现设备 {}", row.name));
                    if row.addr.is_none() {
                        self.log(format!("无法解析蓝牙地址，原始设备 ID：{}", row.id));
                    }
                    self.devices.push(row);
                }
            }

            Evt::DeviceRemoved(id) => {
                if let Some(position) = self.devices.iter().position(|device| device.id == id) {
                    let removed = self.devices.remove(position);
                    self.log(format!("设备已移除：{}", removed.name));
                    self.clamp_selection();
                }
            }

            Evt::Connecting(name) => {
                self.reset_audio();
                self.connection = ConnState::Connecting(name.clone());
                self.log(format!("正在连接 {name}…"));
            }

            Evt::Opened { name } => {
                let connecting =
                    matches!(&self.connection, ConnState::Connecting(current) if current == &name);
                let connected =
                    matches!(&self.connection, ConnState::Connected(current) if current == &name);
                if !connecting && !connected {
                    self.log(format!("忽略 {name} 的过期 Opened 事件"));
                    return;
                }
                if !connected {
                    // A fresh link: audio metrics start from zero.
                    self.reset_audio();
                    self.connected_at = Some(Instant::now());
                }
                self.connection = ConnState::Connected(name.clone());
                if connected {
                    self.log(format!("{name} 已确认处于 Opened 状态"));
                } else {
                    self.log(format!("已连接 {name}（音频已开始传输）"));
                }
            }

            Evt::OpenFailed { name, reason } => {
                if matches!(&self.connection,
                    ConnState::Connecting(current) | ConnState::Connected(current) if current == &name)
                {
                    self.reset_audio();
                    self.connection = ConnState::Idle;
                }
                self.log(format!("连接 {name} 失败：{reason}"));
            }

            Evt::Closed { name } => {
                let ours = matches!(
                    &self.connection,
                    ConnState::Connecting(current) | ConnState::Connected(current)
                        if current == &name
                );
                if ours {
                    let was_connected =
                        matches!(&self.connection, ConnState::Connected(current) if current == &name);
                    self.reset_audio();
                    self.connection = ConnState::Idle;
                    if was_connected {
                        self.log(format!("已断开 {name}"));
                    } else {
                        self.log(format!("已取消连接 {name}"));
                    }
                } else {
                    self.log(format!("连接已关闭：{name}"));
                }
            }

            Evt::AudioLevel(level) => self.apply_audio_level(level),

            Evt::SilenceDetected => {
                self.log("未检测到音频：设备可能没有播放，或音频未路由到本机");
            }

            Evt::Log(message) => self.log(message),

            Evt::Fatal(message) => {
                self.reset_audio();
                self.connection = ConnState::Idle;
                self.log(format!("错误：{message}"));
                self.should_quit = true;
            }
        }
    }

    /// Appends a timestamped log entry, evicting the oldest once full.
    pub fn log(&mut self, message: impl AsRef<str>) {
        let entry = format!("[{}] {}", timestamp(), message.as_ref());
        self.logs.push_back(entry);
        while self.logs.len() > MAX_LOGS {
            self.logs.pop_front();
        }
    }

    /// Keeps the full PnP path discoverable when no readable address could be
    /// extracted: the detail pane shows a placeholder instead of the path.
    fn log_unresolved_addresses(&mut self) {
        let unresolved: Vec<String> = self
            .devices
            .iter()
            .filter(|device| device.addr.is_none())
            .map(|device| format!("无法解析蓝牙地址，原始设备 ID：{}", device.id))
            .collect();
        for message in unresolved {
            self.log(message);
        }
    }

    fn clamp_selection(&mut self) {
        if self.devices.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.devices.len() {
            self.selected = self.devices.len() - 1;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn display_name(name: &str) -> String {
    if name.trim().is_empty() {
        "(无名称)".to_string()
    } else {
        name.to_string()
    }
}

/// Renders an eight-cell level bar. Filled cells use the glyph matching the
/// level, so the bar visibly grows with the peak; the rest fall back to `▁`.
pub fn render_volume_bar(level: f32) -> String {
    let level = if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (level * VOLUME_CELLS as f32).round().clamp(0.0, VOLUME_CELLS as f32) as usize;
    let glyph = if filled == 0 {
        VOLUME_GLYPHS[0]
    } else {
        VOLUME_GLYPHS[(filled - 1).min(VOLUME_CELLS - 1)]
    };

    let mut bar = String::with_capacity(VOLUME_CELLS);
    for index in 0..VOLUME_CELLS {
        bar.push(if index < filled { glyph } else { VOLUME_GLYPHS[0] });
    }
    bar
}

/// Local wall-clock time as `HH:MM:SS`.
///
/// A Win32 call is used instead of a date crate: it needs no extra dependency
/// and returns local time directly (no timezone math).
fn timestamp() -> String {
    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}:{:02}", now.wHour, now.wMinute, now.wSecond)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::SILENCE_HINT_DELAY;
    use std::time::Duration;

    fn device(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            is_paired: true,
            kind: "DeviceInterface".to_string(),
        }
    }

    fn app_with_devices() -> App {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![
            device("id-ipad", "张三的iPad"),
            device("id-civi", "李四的手机"),
            device("id-q35", "Soundcore Life Q35"),
        ]));
        app
    }

    #[test]
    fn device_list_replaces_rows_and_resets_scanning() {
        let app = app_with_devices();
        assert_eq!(app.devices.len(), 3);
        assert_eq!(app.devices[0].name, "张三的iPad");
        assert!(!app.scanning);
    }

    #[test]
    fn empty_names_get_a_placeholder() {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![device("id", "  ")]));
        assert_eq!(app.devices[0].name, "(无名称)");
    }

    #[test]
    fn selection_stays_in_bounds() {
        let mut app = app_with_devices();
        assert_eq!(app.selected, 0);
        app.select_previous();
        assert_eq!(app.selected, 0);
        for _ in 0..10 {
            app.select_next();
        }
        assert_eq!(app.selected, 2);

        app.apply(Evt::DeviceList(vec![device("only", "only")]));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn select_index_ignores_stale_rows() {
        let mut app = app_with_devices();
        app.select_index(2);
        assert_eq!(app.selected, 2);

        // A click can arrive for a row that disappeared with the last snapshot.
        app.select_index(99);
        assert_eq!(app.selected, 2);

        app.apply(Evt::DeviceList(Vec::new()));
        app.select_index(0);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn selection_follows_the_same_device_across_refresh() {
        let mut app = app_with_devices();
        app.select_next();
        let selected_id = app.devices[app.selected].id.clone();

        app.apply(Evt::DeviceList(vec![
            device("id-new", "新设备"),
            device(&selected_id, "李四的手机"),
        ]));
        assert_eq!(app.devices[app.selected].id, selected_id);
    }

    #[test]
    fn connect_selected_returns_a_command() {
        let mut app = app_with_devices();
        match app.connect_selected() {
            Some(Cmd::Connect { device_id, name }) => {
                assert_eq!(device_id, "id-ipad");
                assert_eq!(name, "张三的iPad");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn connect_without_devices_is_a_no_op() {
        let mut app = App::new();
        assert!(app.connect_selected().is_none());
        assert!(app.logs.back().unwrap().contains("没有可连接的设备"));
    }

    #[test]
    fn connecting_then_opened_updates_state() {
        let mut app = app_with_devices();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        assert_eq!(
            app.connection,
            ConnState::Connecting("张三的iPad".to_string())
        );

        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        assert_eq!(
            app.connection,
            ConnState::Connected("张三的iPad".to_string())
        );
        assert!(app.logs.back().unwrap().contains("音频已开始传输"));
    }

    #[test]
    fn stale_opened_event_is_ignored() {
        let mut app = app_with_devices();
        app.apply(Evt::Connecting("李四的手机".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        assert_eq!(
            app.connection,
            ConnState::Connecting("李四的手机".to_string())
        );
        assert!(app.logs.back().unwrap().contains("过期"));
    }

    #[test]
    fn open_failure_returns_to_idle() {
        let mut app = app_with_devices();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::OpenFailed {
            name: "张三的iPad".to_string(),
            reason: "设备未就绪".to_string(),
        });
        assert_eq!(app.connection, ConnState::Idle);
        assert!(app.logs.back().unwrap().contains("连接 张三的iPad 失败"));
    }

    #[test]
    fn closed_event_distinguishes_cancel_and_disconnect() {
        let mut app = app_with_devices();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Closed {
            name: "张三的iPad".to_string(),
        });
        assert_eq!(app.connection, ConnState::Idle);
        assert!(app.logs.back().unwrap().contains("已取消连接"));

        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        app.apply(Evt::Closed {
            name: "张三的iPad".to_string(),
        });
        assert_eq!(app.connection, ConnState::Idle);
        assert!(app.logs.back().unwrap().contains("已断开"));
    }

    #[test]
    fn device_added_and_removed() {
        let mut app = app_with_devices();
        app.apply(Evt::DeviceAdded(device("id-new", "新设备")));
        assert_eq!(app.devices.len(), 4);

        app.apply(Evt::DeviceRemoved("id-new".to_string()));
        assert_eq!(app.devices.len(), 3);
    }

    #[test]
    fn refresh_clears_the_list_and_marks_scanning() {
        let mut app = app_with_devices();
        let command = app.request_refresh();
        assert_eq!(command, Cmd::RefreshDevices);
        assert!(app.devices.is_empty());
        assert!(app.scanning);

        app.apply(Evt::DeviceList(vec![device("id-only", "only")]));
        assert!(!app.scanning);
        assert_eq!(app.devices.len(), 1);
    }

    #[test]
    fn fatal_event_quits() {
        let mut app = App::new();
        app.apply(Evt::Fatal("boom".to_string()));
        assert!(app.should_quit);
    }

    #[test]
    fn real_device_id_becomes_a_readable_address() {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![DeviceInfo {
            id: concat!(
                r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}",
                r"_VID&0001004c_PID&7012#7&d8f392&0&AABBCCDDEE01_C00000000",
                r"#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK"
            )
            .to_string(),
            name: "张三的iPad".to_string(),
            is_paired: false,
            kind: "DeviceInterface".to_string(),
        }]));

        assert_eq!(
            app.devices[0].addr.as_deref(),
            Some("AA:BB:CC:DD:EE:01")
        );
        // The raw id must survive: it is the argument for TryCreateFromId.
        assert!(app.devices[0].id.contains("BTHENUM"));
        assert!(!app.logs.iter().any(|line| line.contains("无法解析蓝牙地址")));
    }

    #[test]
    fn unparsable_id_is_logged_and_not_exposed_as_an_address() {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![device("opaque-id-without-mac", "神秘设备")]));

        assert_eq!(app.devices[0].addr, None);
        assert!(app
            .logs
            .iter()
            .any(|line| line.contains("opaque-id-without-mac")));
    }

    #[test]
    fn connect_request_logs_the_raw_device_id() {
        let mut app = app_with_devices();
        let _ = app.connect_selected();
        assert!(app
            .logs
            .iter()
            .any(|line| line.contains("设备 ID（原始）：id-ipad")));
    }

    #[test]
    fn spinner_rotates_through_four_frames() {
        let mut app = App::new();
        let mut frames = Vec::new();
        for _ in 0..4 {
            frames.push(app.spinner());
            app.tick();
        }
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|frame| frame.chars().count() == 1));
        for (index, frame) in frames.iter().enumerate() {
            assert!(
                !frames[..index].contains(frame),
                "spinner repeated {frame:?} too early"
            );
        }
        // The cycle wraps back to the first frame.
        assert_eq!(app.spinner(), frames[0]);
    }

    #[test]
    fn log_buffer_is_capped() {
        let mut app = App::new();
        for i in 0..(MAX_LOGS + 50) {
            app.log(format!("entry {i}"));
        }
        assert_eq!(app.logs.len(), MAX_LOGS);
        assert!(app
            .logs
            .back()
            .unwrap()
            .contains(&format!("entry {}", MAX_LOGS + 49)));
    }

    fn connected_app() -> App {
        let mut app = app_with_devices();
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        app
    }

    #[test]
    fn audio_level_is_smoothed_towards_each_sample() {
        let mut app = connected_app();
        assert_eq!(app.audio_level, 0.0);

        app.apply(Evt::AudioLevel(1.0));
        assert!((app.audio_level - AUDIO_SMOOTHING).abs() < 1e-6);
        assert_eq!(app.audio_peak, 1.0);

        app.apply(Evt::AudioLevel(0.0));
        assert!(app.audio_level > 0.0);
        assert!(app.audio_level < AUDIO_SMOOTHING);

        // Non-finite samples must never poison the bar.
        app.apply(Evt::AudioLevel(f32::NAN));
        assert!(app.audio_level.is_finite());
    }

    #[test]
    fn audio_level_is_ignored_while_idle() {
        let mut app = app_with_devices();
        app.apply(Evt::AudioLevel(0.8));
        assert_eq!(app.audio_level, 0.0);
        assert!(app.volume_bar().is_none());
    }

    #[test]
    fn volume_bar_is_only_available_when_connected() {
        let mut app = app_with_devices();
        assert!(app.volume_bar().is_none());

        app.apply(Evt::Connecting("张三的iPad".to_string()));
        assert!(app.volume_bar().is_none());

        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        let bar = app.volume_bar().expect("connected links show a bar");
        assert_eq!(bar.chars().count(), 8);
    }

    #[test]
    fn silence_hint_needs_samples_and_eight_seconds() {
        let mut app = connected_app();
        // The meter has not produced a reading yet: do not claim silence.
        app.connected_at = Some(Instant::now() - SILENCE_HINT_DELAY - Duration::from_millis(10));
        assert!(!app.silence_hint());

        // A silent reading starts the clock, but not enough time has passed.
        app.connected_at = Some(Instant::now());
        app.apply(Evt::AudioLevel(0.0));
        assert!(!app.silence_hint());

        // Eight seconds of silence fires the hint.
        app.connected_at = Some(Instant::now() - SILENCE_HINT_DELAY - Duration::from_millis(10));
        assert!(app.silence_hint());

        // Audio seen once since connecting suppresses it: a pause is not a fault.
        app.apply(Evt::AudioLevel(0.8));
        assert!(!app.silence_hint());
    }

    #[test]
    fn closing_the_link_resets_the_audio_metrics() {
        let mut app = connected_app();
        app.apply(Evt::AudioLevel(0.7));
        assert!(app.audio_level > 0.0);

        app.apply(Evt::Closed {
            name: "张三的iPad".to_string(),
        });
        assert_eq!(app.audio_level, 0.0);
        assert_eq!(app.audio_samples, 0);
        assert!(app.connected_at.is_none());
        assert!(app.volume_bar().is_none());
    }

    #[test]
    fn force_reconnect_uses_the_active_or_selected_device() {
        let mut app = app_with_devices();
        // Idle: the selected row is offered to the worker.
        match app.request_force_reconnect() {
            Cmd::ForceReconnect { device_id, name } => {
                assert_eq!(device_id.as_deref(), Some("id-ipad"));
                assert_eq!(name.as_deref(), Some("张三的iPad"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Connected: the worker already knows the device.
        app.apply(Evt::Connecting("李四的手机".to_string()));
        app.apply(Evt::Opened {
            name: "李四的手机".to_string(),
        });
        match app.request_force_reconnect() {
            Cmd::ForceReconnect { device_id, name } => {
                assert_eq!(device_id, None);
                assert_eq!(name.as_deref(), Some("李四的手机"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn force_reconnect_with_nothing_selected_does_not_panic() {
        let mut app = App::new();
        match app.request_force_reconnect() {
            Cmd::ForceReconnect { device_id, name } => {
                assert!(device_id.is_none());
                assert!(name.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(app.logs.back().unwrap().contains("当前没有连接"));
    }

    #[test]
    fn volume_bar_has_eight_levels() {
        assert_eq!(render_volume_bar(0.0), "▁▁▁▁▁▁▁▁");
        assert_eq!(render_volume_bar(1.0), "████████");
        assert_eq!(render_volume_bar(f32::NAN), "▁▁▁▁▁▁▁▁");
        assert!(render_volume_bar(0.5).starts_with('▄'));
        for level in [0.0, 0.1, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(render_volume_bar(level).chars().count(), 8);
        }
    }
}
