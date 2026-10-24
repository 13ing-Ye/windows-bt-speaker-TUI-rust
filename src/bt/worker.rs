//! Bluetooth worker thread.
//!
//! Everything that touches WinRT lives here, on one thread with an MTA
//! apartment (constraints C1/C2). The UI only ever sees `Evt` values.
//!
//! Layout:
//!   UI thread ──cmd_tx──> worker loop ──WinRT──> DeviceWatcher / AudioPlaybackConnection
//!   UI thread <──evt_rx── worker loop <──watcher callbacks / StateChanged (value-only)
//!
//! The loop is a 50 ms poll: commands arrive on `cmd_rx`, watcher callbacks are
//! forwarded through an internal channel, and two time-based jobs (enumeration
//! timeout and the "did it really open?" watchdog) are serviced every tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::{IInspectable, Ref, HSTRING, HRESULT};
use windows::Devices::Enumeration::{
    DeviceInformation, DeviceInformationKind, DeviceInformationUpdate, DeviceWatcher,
};
use windows::Foundation::TypedEventHandler;
use windows::Media::Audio::{
    AudioPlaybackConnection, AudioPlaybackConnectionOpenResultStatus, AudioPlaybackConnectionState,
};

use crate::com;

use super::meter::AudioMeter;
use super::types::{silence_due, Cmd, DeviceInfo, Evt, AUDIO_ACTIVE_THRESHOLD};

/// Idle tick of the worker loop.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Cap on how long enumeration may take before we use what we have.
const ENUMERATION_TIMEOUT: Duration = Duration::from_secs(8);
/// Backoff for `Open()` retries (3 s / 6 s / 12 s, three retries).
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(6),
    Duration::from_secs(12),
];
/// Cooldown after closing a connection before opening the next one.
const SWITCH_COOLDOWN: Duration = Duration::from_millis(1000);
/// If `StateChanged` has not reported `Opened` by then, start polling `State()`.
const OPEN_STATE_FALLBACK: Duration = Duration::from_secs(2);
/// Give up (and report failure) if `Opened` still has not arrived.
const OPEN_STATE_DEADLINE: Duration = Duration::from_secs(15);
/// Cadence of the fallback `State()` polling.
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// WASAPI peak sampling cadence (only while the link is `Opened`).
const AUDIO_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);
/// Cool-down between `Close()` and the forced re-`Open()` (at least 1 s).
const FORCE_RECONNECT_COOLDOWN: Duration = Duration::from_secs(1);

/// The Windows error behind "UnknownFailure" when the previous link has not
/// been released yet: `ERROR_GEN_FAILURE`.
const HRESULT_GEN_FAILURE: u32 = 0x8007_001F;

/// Debug-only fault injection for the `Open()` retry path.
///
/// The real `0x8007001F` only shows up after an unlucky quick reconnect, so
/// reproducing it on demand is the only practical way to exercise the retry
/// code against the real worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedOpenError {
    /// Fail the first attempt only; the retry then reaches the real API.
    Once(u32),
    /// Fail every attempt, exhausting the retry budget so the final,
    /// user-visible error message can be inspected.
    Always(u32),
}

/// Knobs for [`Worker::spawn_with`]. Defaults are the production behaviour.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerOptions {
    pub forced_open_error: Option<ForcedOpenError>,
    /// `--auto-reconnect-on-silence`: retry once when a fresh link stays silent.
    pub auto_reconnect_on_silence: bool,
}

/// Handle owned by the UI thread.
pub struct Worker {
    cmd_tx: Sender<Cmd>,
    evt_rx: Receiver<Evt>,
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawns the worker with production behaviour.
    pub fn spawn() -> Result<Self> {
        Self::spawn_with(WorkerOptions::default())
    }

    /// Spawns the worker. COM/MTA setup happens on the worker thread, so this
    /// call never blocks on WinRT.
    pub fn spawn_with(options: WorkerOptions) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (evt_tx, evt_rx) = mpsc::channel::<Evt>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();

        let join = thread::Builder::new()
            .name("bt-worker".to_string())
            .spawn(move || {
                let tx = evt_tx.clone();
                if let Err(error) = run(cmd_rx, evt_tx, flag, options) {
                    let _ = tx.send(Evt::Fatal(format!("{error:#}")));
                }
            })
            .context("创建 worker 线程失败")?;

        Ok(Self {
            cmd_tx,
            evt_rx,
            shutdown,
            join: Some(join),
        })
    }

    /// Cloneable sender for the UI -> worker direction.
    pub fn cmd(&self) -> Sender<Cmd> {
        self.cmd_tx.clone()
    }

    /// Non-blocking event poll used by the UI loop.
    pub fn try_recv(&self) -> Option<Evt> {
        self.evt_rx.try_recv().ok()
    }

    pub fn is_finished(&self) -> bool {
        self.join.as_ref().map(|j| j.is_finished()).unwrap_or(true)
    }

    /// Signals shutdown, waits for the worker to close the connection and stop
    /// the watcher, then joins the thread.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.shutdown();
        }
    }
}

fn run(
    cmd_rx: Receiver<Cmd>,
    evt_tx: Sender<Evt>,
    shutdown: Arc<AtomicBool>,
    options: WorkerOptions,
) -> Result<()> {
    com::initialize_mta().context("初始化 MTA 失败")?;

    let selector = AudioPlaybackConnection::GetDeviceSelector()
        .context("AudioPlaybackConnection::GetDeviceSelector() 失败（API 不可用？）")?;
    if selector.to_string_lossy().is_empty() {
        bail!("GetDeviceSelector() 返回空字符串");
    }

    let mut state = WorkerState::new(selector, evt_tx, shutdown);
    state.forced_open_error = options.forced_open_error;
    state.auto_reconnect_on_silence = options.auto_reconnect_on_silence;
    state.evt(Evt::WorkerReady);
    state.log("蓝牙服务已就绪，等待设备扫描指令");

    while !state.shutdown.load(Ordering::SeqCst) {
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Cmd::Shutdown) => break,
            Ok(command) => state.handle_command(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        state.drain_watcher();
        state.service_enumeration_timeout();
        state.service_open_watchdog();
        state.service_audio_watch();
    }

    state.cleanup();
    Ok(())
}

/// Messages forwarded from the watcher callbacks (system threads) to the
/// worker thread. Value types only.
enum WatcherMsg {
    Added(DeviceInfo),
    Removed(String),
    Completed,
    Stopped,
}

enum OpenOutcome {
    /// `Open()` returned Success; `Opened` still has to be confirmed.
    Opened,
    Failed,
    /// Shutdown was requested while waiting.
    Interrupted,
}

struct Connection {
    handle: AudioPlaybackConnection,
    /// Kept so a forced reconnect can recreate the connection object.
    device_id: String,
    name: String,
    state_token: Option<i64>,
    /// Set by the `StateChanged` callback, read by the watchdog.
    opened: Arc<AtomicBool>,
    open_watch: Option<OpenWatch>,
}

struct OpenWatch {
    warn_at: Instant,
    next_poll: Instant,
    hard_deadline: Instant,
    warned: bool,
}

/// Tracks whether a freshly opened link produced any audio at all.
struct AudioWatch {
    started: Instant,
    /// At least one successful meter sample was above the threshold.
    saw_audio: bool,
    /// The silence hint (and possibly the automatic reconnect) already fired.
    warned: bool,
    /// Successful meter reads. Zero means the meter itself is unavailable, in
    /// which case we must not claim the device is silent.
    samples: u32,
    /// Next sampling instant (`AUDIO_SAMPLE_INTERVAL` cadence).
    next_sample: Instant,
}

impl AudioWatch {
    fn start(now: Instant) -> Self {
        Self {
            started: now,
            saw_audio: false,
            warned: false,
            samples: 0,
            next_sample: now,
        }
    }
}

struct WorkerState {
    selector: HSTRING,
    evt_tx: Sender<Evt>,
    shutdown: Arc<AtomicBool>,

    watcher: Option<DeviceWatcher>,
    watch_rx: Option<Receiver<WatcherMsg>>,
    devices: Vec<DeviceInfo>,
    enumeration_done: bool,
    enumeration_deadline: Instant,

    conn: Option<Connection>,
    last_close: Option<Instant>,

    /// WASAPI peak meter for the default render endpoint.
    meter: AudioMeter,
    /// Present while an opened link is being watched for audio.
    audio_watch: Option<AudioWatch>,
    /// `--auto-reconnect-on-silence`.
    auto_reconnect_on_silence: bool,
    /// The automatic reconnect is deliberately one-shot per process.
    auto_reconnect_used: bool,

    /// Debug-only; see [`ForcedOpenError`].
    forced_open_error: Option<ForcedOpenError>,
}

impl WorkerState {
    fn new(selector: HSTRING, evt_tx: Sender<Evt>, shutdown: Arc<AtomicBool>) -> Self {
        Self {
            selector,
            evt_tx,
            shutdown,
            watcher: None,
            watch_rx: None,
            devices: Vec::new(),
            enumeration_done: true,
            enumeration_deadline: Instant::now(),
            conn: None,
            last_close: None,
            meter: AudioMeter::new(),
            audio_watch: None,
            auto_reconnect_on_silence: false,
            auto_reconnect_used: false,
            forced_open_error: None,
        }
    }

    fn evt(&self, event: Evt) {
        let _ = self.evt_tx.send(event);
    }

    fn log(&self, message: impl AsRef<str>) {
        let _ = self.evt_tx.send(Evt::Log(message.as_ref().to_string()));
    }

    // ---------------------------------------------------------------- commands

    fn handle_command(&mut self, command: Cmd) {
        match command {
            Cmd::RefreshDevices => self.refresh_devices(),
            Cmd::Connect { device_id, name } => self.connect(device_id, name),
            Cmd::ForceReconnect { device_id, name } => self.force_reconnect(device_id, name),
            Cmd::Disconnect => self.disconnect_by_user(),
            Cmd::OpenBluetoothSettings => self.open_bluetooth_settings(),
            Cmd::Shutdown => {}
        }
    }

    fn refresh_devices(&mut self) {
        self.log("正在扫描蓝牙设备…");
        self.stop_watcher();

        self.devices.clear();
        self.enumeration_done = false;
        self.enumeration_deadline = Instant::now() + ENUMERATION_TIMEOUT;
        // The UI clears its own list when it issues the refresh; sending an
        // empty snapshot here would make it drop out of the "scanning" state.

        match self.start_watcher() {
            Ok((watcher, rx)) => {
                self.watcher = Some(watcher);
                self.watch_rx = Some(rx);
            }
            Err(error) => {
                self.enumeration_done = true;
                self.log(format!("启动设备扫描失败：{error:#}"));
                self.evt(Evt::DeviceList(Vec::new()));
            }
        }
    }

    fn disconnect_by_user(&mut self) {
        if self.conn.is_none() {
            self.log("当前没有连接");
            return;
        }
        self.close_current(true);
    }

    fn open_bluetooth_settings(&mut self) {
        // `start` is a cmd builtin; the empty argument is the window title so
        // the URI itself is not mistaken for one.
        match std::process::Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:bluetooth"])
            .spawn()
        {
            Ok(_) => self.log("已尝试打开系统蓝牙设置"),
            Err(error) => self.log(format!("打开系统蓝牙设置失败：{error}")),
        }
    }

    // ----------------------------------------------------------------- watcher

    fn start_watcher(&self) -> Result<(DeviceWatcher, Receiver<WatcherMsg>)> {
        let watcher = DeviceInformation::CreateWatcherAqsFilter(&self.selector)
            .context("DeviceInformation::CreateWatcher(selector) 失败")?;
        let (tx, rx) = mpsc::channel::<WatcherMsg>();

        let added_tx = tx.clone();
        let added = TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(
            move |_sender: Ref<DeviceWatcher>, info: Ref<DeviceInformation>| {
                if let Some(info) = info.as_ref() {
                    let _ = added_tx.send(WatcherMsg::Added(device_from_info(info)));
                }
                Ok(())
            },
        );
        watcher.Added(&added).context("订阅 DeviceWatcher.Added 失败")?;

        let removed_tx = tx.clone();
        let removed = TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(
            move |_sender: Ref<DeviceWatcher>, update: Ref<DeviceInformationUpdate>| {
                if let Some(update) = update.as_ref() {
                    if let Ok(id) = update.Id() {
                        let _ = removed_tx.send(WatcherMsg::Removed(id.to_string_lossy()));
                    }
                }
                Ok(())
            },
        );
        watcher
            .Removed(&removed)
            .context("订阅 DeviceWatcher.Removed 失败")?;

        let completed_tx = tx.clone();
        let completed = TypedEventHandler::<DeviceWatcher, IInspectable>::new(
            move |_sender: Ref<DeviceWatcher>, _args: Ref<IInspectable>| {
                let _ = completed_tx.send(WatcherMsg::Completed);
                Ok(())
            },
        );
        watcher
            .EnumerationCompleted(&completed)
            .context("订阅 DeviceWatcher.EnumerationCompleted 失败")?;

        let stopped_tx = tx.clone();
        let stopped = TypedEventHandler::<DeviceWatcher, IInspectable>::new(
            move |_sender: Ref<DeviceWatcher>, _args: Ref<IInspectable>| {
                let _ = stopped_tx.send(WatcherMsg::Stopped);
                Ok(())
            },
        );
        watcher
            .Stopped(&stopped)
            .context("订阅 DeviceWatcher.Stopped 失败")?;

        watcher.Start().context("DeviceWatcher.Start() 失败")?;
        Ok((watcher, rx))
    }

    fn stop_watcher(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.Stop();
        }
        // Dropping the receiver makes any in-flight callback a no-op.
        self.watch_rx = None;
    }

    fn drain_watcher(&mut self) {
        let mut messages = Vec::new();
        if let Some(rx) = &self.watch_rx {
            while let Ok(message) = rx.try_recv() {
                messages.push(message);
            }
        }

        for message in messages {
            match message {
                WatcherMsg::Added(info) => {
                    if self.devices.iter().any(|known| known.id == info.id) {
                        continue;
                    }
                    self.devices.push(info.clone());
                    if self.enumeration_done {
                        self.evt(Evt::DeviceAdded(info));
                    }
                }
                WatcherMsg::Removed(id) => {
                    if let Some(position) = self.devices.iter().position(|known| known.id == id) {
                        self.devices.remove(position);
                        if self.enumeration_done {
                            self.evt(Evt::DeviceRemoved(id));
                        }
                    }
                }
                WatcherMsg::Completed => self.finish_enumeration(),
                WatcherMsg::Stopped => {
                    if !self.enumeration_done {
                        self.finish_enumeration();
                    }
                }
            }
        }
    }

    fn service_enumeration_timeout(&mut self) {
        if self.enumeration_done || Instant::now() < self.enumeration_deadline {
            return;
        }
        self.log("等待设备枚举完成超时（8 秒），先使用已发现的设备");
        self.finish_enumeration();
    }

    fn finish_enumeration(&mut self) {
        if self.enumeration_done {
            return;
        }
        self.enumeration_done = true;
        let count = self.devices.len();
        self.evt(Evt::DeviceList(self.devices.clone()));
        self.log(format!("扫描完成，共 {count} 个设备"));
        if count == 0 {
            self.log("未发现可用设备，请确认蓝牙已开启且设备已配对");
        }
    }

    // -------------------------------------------------------------- connection

    fn connect(&mut self, device_id: String, name: String) {
        if device_id.is_empty() {
            self.log("设备 Id 为空，无法连接");
            return;
        }
        // A new link always gets a fresh audio watch once it reaches Opened.
        self.audio_watch = None;
        if self.conn.as_ref().is_some_and(|conn| conn.name == name) {
            self.log(format!("{name} 已经连接，无需重复连接"));
            return;
        }

        if self.conn.is_some() {
            self.log("切换设备：先断开当前连接");
            self.close_current(true);
        }
        if !self.wait_cooldown() {
            return;
        }

        self.evt(Evt::Connecting(name.clone()));

        let handle = match AudioPlaybackConnection::TryCreateFromId(&HSTRING::from(device_id.as_str()))
        {
            Ok(handle) => handle,
            // windows-rs maps a null WinRT object to an error carrying S_OK.
            Err(error) if error.code() == HRESULT(0) => {
                let reason = "该设备不支持音频流（TryCreateFromId 返回 null）";
                self.log(format!("{name}：{reason}"));
                self.evt(Evt::OpenFailed {
                    name,
                    reason: reason.to_string(),
                });
                return;
            }
            Err(error) => {
                let reason = format!(
                    "创建设备连接失败：0x{:08X} {}",
                    error.code().0 as u32,
                    error
                );
                self.log(&reason);
                self.evt(Evt::OpenFailed { name, reason });
                return;
            }
        };

        if let Err(error) = handle.Start() {
            let reason = format!("Start() 失败：0x{:08X} {}", error.code().0 as u32, error);
            self.log(&reason);
            self.evt(Evt::OpenFailed { name, reason });
            return;
        }
        self.log("Start() 完成，正在建立音频链路");

        let opened = Arc::new(AtomicBool::new(false));
        let state_token = self.subscribe_state_changed(&handle, &name, &opened);

        self.conn = Some(Connection {
            handle,
            device_id,
            name: name.clone(),
            state_token,
            opened,
            open_watch: None,
        });

        match self.open_with_retry(&name) {
            OpenOutcome::Opened => self.arm_open_watch(),
            OpenOutcome::Failed | OpenOutcome::Interrupted => {
                self.close_current(false);
            }
        }
    }

    /// `f`: close the active link, wait out the A2DP cooldown, then open again.
    ///
    /// With no active link it connects the device the UI selected instead, so
    /// the key never becomes a silent no-op.
    fn force_reconnect(&mut self, device_id: Option<String>, name: Option<String>) {
        if self.conn.is_some() {
            let target_id = self
                .conn
                .as_ref()
                .map(|conn| conn.device_id.clone())
                .or(device_id);
            let target_name = self
                .conn
                .as_ref()
                .map(|conn| conn.name.clone())
                .or(name);

            self.log("强制重连：断开中…");
            self.close_current(true);

            self.log(format!(
                "冷却 {} 秒…",
                FORCE_RECONNECT_COOLDOWN.as_secs()
            ));
            if !sleep_interruptible(FORCE_RECONNECT_COOLDOWN, &self.shutdown) {
                return;
            }

            let (Some(device_id), Some(name)) = (target_id, target_name) else {
                self.log("强制重连失败：无法确定要连接哪台设备");
                return;
            };
            self.log("重新连接中…");
            self.connect(device_id, name);
            return;
        }

        match (device_id, name) {
            (Some(device_id), Some(name)) => {
                self.log(format!("当前没有连接，正在连接 {name}…"));
                self.connect(device_id, name);
            }
            _ => self.log("当前没有连接，无法强制重连（先按 Enter 连接设备）"),
        }
    }

    fn subscribe_state_changed(
        &self,
        handle: &AudioPlaybackConnection,
        name: &str,
        opened: &Arc<AtomicBool>,
    ) -> Option<i64> {
        let evt_tx = self.evt_tx.clone();
        let event_name = name.to_string();
        let flag = opened.clone();

        // Callback runs on a WinRT thread: it only reads `State()` and sends
        // plain values back (constraint C2).
        let handler = TypedEventHandler::<AudioPlaybackConnection, IInspectable>::new(
            move |sender: Ref<AudioPlaybackConnection>, _args: Ref<IInspectable>| {
                let Some(sender) = sender.as_ref() else {
                    return Ok(());
                };
                match sender.State() {
                    Ok(state) if state == AudioPlaybackConnectionState::Opened => {
                        flag.store(true, Ordering::SeqCst);
                        let _ = evt_tx.send(Evt::Opened {
                            name: event_name.clone(),
                        });
                    }
                    Ok(state) if state == AudioPlaybackConnectionState::Closed => {
                        flag.store(false, Ordering::SeqCst);
                        let _ = evt_tx.send(Evt::Closed {
                            name: event_name.clone(),
                        });
                    }
                    _ => {}
                }
                Ok(())
            },
        );

        match handle.StateChanged(&handler) {
            Ok(token) => Some(token),
            Err(error) => {
                self.log(format!(
                    "订阅 StateChanged 失败（0x{:08X}），改用轮询 State()",
                    error.code().0 as u32
                ));
                None
            }
        }
    }

    /// `Open()` retries. Returns once the OS reports Success or the attempts
    /// are exhausted.
    fn open_with_retry(&mut self, name: &str) -> OpenOutcome {
        let Some(handle) = self.conn.as_ref().map(|conn| conn.handle.clone()) else {
            return OpenOutcome::Interrupted;
        };
        let mut retries = 0usize;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return OpenOutcome::Interrupted;
            }

            let (status, extended) = match consume_forced_error(&mut self.forced_open_error) {
                Some(code) => {
                    self.log(format!(
                        "调试：强制注入 Open() 失败，ExtendedError 0x{code:08X}"
                    ));
                    (
                        AudioPlaybackConnectionOpenResultStatus::UnknownFailure,
                        Some(code),
                    )
                }
                None => {
                    let result = match handle.Open() {
                        Ok(result) => result,
                        Err(error) => {
                            let reason = format!(
                                "Open() 调用失败：0x{:08X} {}",
                                error.code().0 as u32,
                                error
                            );
                            self.log(&reason);
                            self.evt(Evt::OpenFailed {
                                name: name.to_string(),
                                reason,
                            });
                            return OpenOutcome::Failed;
                        }
                    };

                    let status = result
                        .Status()
                        .unwrap_or(AudioPlaybackConnectionOpenResultStatus::UnknownFailure);
                    if status == AudioPlaybackConnectionOpenResultStatus::Success {
                        self.log("Open() 返回 Success，等待系统确认 Opened…");
                        return OpenOutcome::Opened;
                    }
                    let extended = result.ExtendedError().ok().map(|hr| hr.0 as u32);
                    (status, extended)
                }
            };

            if should_retry(status) && retries < RETRY_DELAYS.len() {
                let delay = RETRY_DELAYS[retries];
                retries += 1;
                let detail = describe_failure(status, extended);
                self.log(format!(
                    "{detail}；{} 秒后重试（第 {retries} 次，最多 {} 次）",
                    delay.as_secs(),
                    RETRY_DELAYS.len()
                ));
                if !sleep_interruptible(delay, &self.shutdown) {
                    return OpenOutcome::Interrupted;
                }
                continue;
            }

            let mut reason = describe_failure(status, extended);
            if retries > 0 {
                reason.push_str(&format!("（已重试 {retries} 次）"));
            }
            self.log(format!("{name} 连接失败：{reason}"));
            self.evt(Evt::OpenFailed {
                name: name.to_string(),
                reason,
            });
            return OpenOutcome::Failed;
        }
    }

    fn arm_open_watch(&mut self) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let now = Instant::now();
        conn.open_watch = Some(OpenWatch {
            warn_at: now + OPEN_STATE_FALLBACK,
            next_poll: now + OPEN_STATE_FALLBACK,
            hard_deadline: now + OPEN_STATE_DEADLINE,
            warned: false,
        });
    }

    /// Confirms that the connection really reached `Opened`.
    ///
    /// `Open()` returning Success does not mean audio is streaming: `State` can
    /// still be `Closed` at that moment (observed in P1). The event subscription
    /// is primary; if nothing arrives within 2 s we poll `State()` ourselves.
    fn service_open_watchdog(&mut self) {
        let Some(conn) = self.conn.as_ref() else {
            return;
        };
        let Some(watch) = conn.open_watch.as_ref() else {
            return;
        };

        let opened = conn.opened.load(Ordering::SeqCst);
        let name = conn.name.clone();
        let handle = conn.handle.clone();
        let now = Instant::now();
        let timed_out = now >= watch.hard_deadline;
        let should_warn = !watch.warned && now >= watch.warn_at && !timed_out;
        let should_poll = !timed_out && now >= watch.next_poll;

        if opened {
            // The StateChanged callback got there first.
            if let Some(conn) = self.conn.as_mut() {
                conn.open_watch = None;
            }
            self.start_audio_watch();
            return;
        }

        if timed_out {
            self.log(format!("{name} 在 {:?} 内仍未进入 Opened", OPEN_STATE_DEADLINE));
            self.evt(Evt::OpenFailed {
                name,
                reason: format!(
                    "已向系统发起连接，但 {} 秒内未进入 Opened 状态（设备可能未响应）",
                    OPEN_STATE_DEADLINE.as_secs()
                ),
            });
            self.close_current(false);
            return;
        }

        if should_warn {
            if let Some(conn) = self.conn.as_mut() {
                if let Some(watch) = conn.open_watch.as_mut() {
                    watch.warned = true;
                }
            }
            self.log("尚未收到 Opened 事件，改为每 0.5 秒主动查询 State()");
        }

        if should_poll {
            if let Some(conn) = self.conn.as_mut() {
                if let Some(watch) = conn.open_watch.as_mut() {
                    watch.next_poll = now + STATE_POLL_INTERVAL;
                }
            }
            match handle.State() {
                Ok(state) if state == AudioPlaybackConnectionState::Opened => {
                    if let Some(conn) = self.conn.as_mut() {
                        conn.opened.store(true, Ordering::SeqCst);
                        conn.open_watch = None;
                    }
                    self.log("主动查询确认 State = Opened");
                    self.evt(Evt::Opened { name });
                    self.start_audio_watch();
                }
                Ok(_) => {}
                Err(error) => self.log(format!(
                    "查询 State() 失败：0x{:08X}",
                    error.code().0 as u32
                )),
            }
        }
    }

    /// Begins watching a confirmed `Opened` link for actual audio.
    fn start_audio_watch(&mut self) {
        if self.conn.is_some() && self.audio_watch.is_none() {
            self.audio_watch = Some(AudioWatch::start(Instant::now()));
        }
    }

    /// Samples the default render endpoint while a link is `Opened` and raises
    /// the silence hint once, if the link never produced audible audio.
    fn service_audio_watch(&mut self) {
        if self.audio_watch.is_none() {
            return;
        }

        // Stop watching as soon as the connection is gone or no longer opened
        // (the OS may close it on its own).
        let still_opened = self
            .conn
            .as_ref()
            .map(|conn| conn.opened.load(Ordering::SeqCst))
            .unwrap_or(false);
        if !still_opened {
            self.audio_watch = None;
            return;
        }

        let now = Instant::now();
        let sample_due = self
            .audio_watch
            .as_ref()
            .map(|watch| now >= watch.next_sample)
            .unwrap_or(false);

        if sample_due {
            if let Some(watch) = self.audio_watch.as_mut() {
                watch.next_sample = now + AUDIO_SAMPLE_INTERVAL;
            }
            // `None` (no default endpoint, endpoint switching, WASAPI failure)
            // is swallowed: the meter retries later and the UI simply keeps the
            // last level until then.
            if let Some(level) = self.meter.sample() {
                if let Some(watch) = self.audio_watch.as_mut() {
                    watch.samples = watch.samples.saturating_add(1);
                    if level >= AUDIO_ACTIVE_THRESHOLD {
                        watch.saw_audio = true;
                    }
                }
                self.evt(Evt::AudioLevel(level));
            }
        }

        let due = self
            .audio_watch
            .as_ref()
            .map(|watch| {
                !watch.warned
                    && silence_due(
                        now.duration_since(watch.started),
                        watch.saw_audio,
                        watch.samples,
                    )
            })
            .unwrap_or(false);
        if !due {
            return;
        }

        if let Some(watch) = self.audio_watch.as_mut() {
            watch.warned = true;
        }
        self.evt(Evt::SilenceDetected);

        if self.auto_reconnect_on_silence && !self.auto_reconnect_used {
            self.auto_reconnect_used = true;
            self.log("自动重连：已启用 --auto-reconnect-on-silence，执行一次强制重连");
            self.force_reconnect(None, None);
        }
    }

    /// Closes the active connection and removes its event handler.
    fn close_current(&mut self, emit_closed: bool) -> Option<String> {
        self.audio_watch = None;
        let conn = self.conn.take()?;

        if let Some(token) = conn.state_token {
            let _ = conn.handle.RemoveStateChanged(token);
        }
        if let Err(error) = conn.handle.Close() {
            self.log(format!(
                "Close() 失败：0x{:08X} {}",
                error.code().0 as u32,
                error
            ));
        }

        self.last_close = Some(Instant::now());
        if emit_closed {
            self.evt(Evt::Closed {
                name: conn.name.clone(),
            });
        }
        Some(conn.name)
    }

    /// Sleeps out the post-close cooldown (at least 1 s) before the next Open.
    fn wait_cooldown(&mut self) -> bool {
        let Some(closed_at) = self.last_close else {
            return true;
        };
        let elapsed = closed_at.elapsed();
        if elapsed >= SWITCH_COOLDOWN {
            return true;
        }
        let remaining = SWITCH_COOLDOWN - elapsed;
        self.log(format!(
            "等待蓝牙链路释放（{} 毫秒）…",
            remaining.as_millis()
        ));
        sleep_interruptible(remaining, &self.shutdown)
    }

    // ---------------------------------------------------------------- shutdown

    fn cleanup(&mut self) {
        if let Some(conn) = self.conn.take() {
            if let Some(token) = conn.state_token {
                let _ = conn.handle.RemoveStateChanged(token);
            }
            let _ = conn.handle.Close();
            self.log(format!("已关闭连接 {}", conn.name));
        }
        self.stop_watcher();
        self.log("worker 线程已退出");
    }
}

fn device_from_info(info: &DeviceInformation) -> DeviceInfo {
    let name = info
        .Name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let id = info
        .Id()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let is_paired = info
        .Pairing()
        .ok()
        .and_then(|pairing| pairing.IsPaired().ok())
        .unwrap_or(false);
    let kind = info
        .Kind()
        .map(|kind| kind_label(kind).to_string())
        .unwrap_or_else(|_| "Unknown".to_string());

    DeviceInfo {
        id,
        name,
        is_paired,
        kind,
    }
}

fn kind_label(kind: DeviceInformationKind) -> &'static str {
    match kind.0 {
        0 => "Unknown",
        1 => "DeviceInterface",
        2 => "DeviceContainer",
        3 => "Device",
        4 => "DeviceInterfaceClass",
        5 => "AssociationEndpoint",
        6 => "AssociationEndpointContainer",
        7 => "AssociationEndpointService",
        8 => "DevicePanel",
        9 => "AssociationEndpointProtocol",
        _ => "Unrecognized",
    }
}

/// `UnknownFailure` is the only status worth retrying: it is what the stack
/// reports when the previous link has not been released yet.
fn should_retry(status: AudioPlaybackConnectionOpenResultStatus) -> bool {
    status == AudioPlaybackConnectionOpenResultStatus::UnknownFailure
}

/// Maps an `AudioPlaybackConnectionOpenResultStatus` (plus `ExtendedError`) to
/// a user-readable Chinese message.
fn describe_failure(
    status: AudioPlaybackConnectionOpenResultStatus,
    extended: Option<u32>,
) -> String {
    if status == AudioPlaybackConnectionOpenResultStatus::DeniedBySystem {
        return "系统拒绝连接，请检查蓝牙已开启、设备已配对".to_string();
    }
    if status == AudioPlaybackConnectionOpenResultStatus::RequestTimedOut {
        return "连接超时，请靠近设备后重试".to_string();
    }
    if status == AudioPlaybackConnectionOpenResultStatus::UnknownFailure {
        return match extended {
            Some(HRESULT_GEN_FAILURE) => {
                "设备未就绪（0x8007001F），蓝牙链路可能仍被占用".to_string()
            }
            Some(code) => format!("未知失败（ExtendedError 0x{code:08X}）"),
            None => "未知失败（无法读取 ExtendedError）".to_string(),
        };
    }
    if status == AudioPlaybackConnectionOpenResultStatus::Success {
        return "成功".to_string();
    }
    format!("无法识别的状态码 {}", status.0)
}

/// Returns the error code for the next `Open()` attempt, consuming a `Once`
/// injection so the following attempt hits the real API.
fn consume_forced_error(forced: &mut Option<ForcedOpenError>) -> Option<u32> {
    match *forced {
        Some(ForcedOpenError::Once(code)) => {
            *forced = None;
            Some(code)
        }
        Some(ForcedOpenError::Always(code)) => Some(code),
        None => None,
    }
}

/// Sleeps in 100 ms slices so shutdown is not delayed by long backoffs.
/// Returns `false` when shutdown was requested.
fn sleep_interruptible(duration: Duration, shutdown: &AtomicBool) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unknown_failure_is_retried() {
        assert!(should_retry(
            AudioPlaybackConnectionOpenResultStatus::UnknownFailure
        ));
        assert!(!should_retry(
            AudioPlaybackConnectionOpenResultStatus::DeniedBySystem
        ));
        assert!(!should_retry(
            AudioPlaybackConnectionOpenResultStatus::RequestTimedOut
        ));
        assert!(!should_retry(
            AudioPlaybackConnectionOpenResultStatus::Success
        ));
    }

    #[test]
    fn failure_messages_are_actionable() {
        let denied = describe_failure(
            AudioPlaybackConnectionOpenResultStatus::DeniedBySystem,
            None,
        );
        assert!(denied.contains("系统拒绝连接"));

        let timeout = describe_failure(
            AudioPlaybackConnectionOpenResultStatus::RequestTimedOut,
            None,
        );
        assert!(timeout.contains("连接超时"));

        let busy = describe_failure(
            AudioPlaybackConnectionOpenResultStatus::UnknownFailure,
            Some(HRESULT_GEN_FAILURE),
        );
        assert!(busy.contains("0x8007001F"));

        let other = describe_failure(
            AudioPlaybackConnectionOpenResultStatus::UnknownFailure,
            Some(0x8000_4005),
        );
        assert!(other.contains("0x80004005"));

        let bare = describe_failure(
            AudioPlaybackConnectionOpenResultStatus::UnknownFailure,
            None,
        );
        assert!(bare.contains("ExtendedError"));
    }

    #[test]
    fn retry_schedule_is_3_6_12_seconds() {
        let seconds: Vec<u64> = RETRY_DELAYS.iter().map(|d| d.as_secs()).collect();
        assert_eq!(seconds, vec![3, 6, 12]);
    }

    #[test]
    fn forced_error_once_is_consumed_and_always_persists() {
        let code = HRESULT_GEN_FAILURE;

        let mut once = Some(ForcedOpenError::Once(code));
        assert_eq!(consume_forced_error(&mut once), Some(code));
        assert_eq!(consume_forced_error(&mut once), None);

        let mut always = Some(ForcedOpenError::Always(code));
        assert_eq!(consume_forced_error(&mut always), Some(code));
        assert_eq!(consume_forced_error(&mut always), Some(code));

        let mut none = None;
        assert_eq!(consume_forced_error(&mut none), None);
    }

    #[test]
    fn sleep_interruptible_stops_early() {
        let flag = AtomicBool::new(false);
        assert!(sleep_interruptible(Duration::from_millis(50), &flag));

        let flag = AtomicBool::new(true);
        let started = Instant::now();
        assert!(!sleep_interruptible(Duration::from_secs(5), &flag));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
