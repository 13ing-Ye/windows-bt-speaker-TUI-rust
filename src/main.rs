//! TUI entry point.
//!
//! Threading model (see AGENT.md 4.1):
//!   UI thread: crossterm polling + ratatui rendering + App state machine
//!   worker thread: every WinRT object (src/bt/worker.rs)
//!
//! Diagnostics that do not need a human at the keyboard:
//!   --dump-ui     render through ratatui's TestBackend and print the snapshot
//!   --smoke       boot the real TUI, quit after a moment, prove the terminal
//!                 was restored
//!   --headless    run the worker with plain-text event output (used to verify
//!                 real Bluetooth connections without an interactive TTY)

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::panic;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    self as term_event, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

use bluetooth_audio_receiver::app::{App, ConnState};
use bluetooth_audio_receiver::bt::{Cmd, DeviceInfo, Evt, ForcedOpenError, Worker, WorkerOptions};
use bluetooth_audio_receiver::console;
use bluetooth_audio_receiver::event::{map_key, Action};
use bluetooth_audio_receiver::ui::{self, HitArea, UiAction};

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Animation step for the connecting spinner (four frames per 400 ms).
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Two clicks on the same cell within this window connect the row, mirroring a
/// desktop double click. Only applies to device rows.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);

/// Current console input mode, used by `--smoke` to prove that mouse capture
/// was enabled and then restored.
///
/// On Windows crossterm enables mouse capture with `SetConsoleMode`, so there
/// is no `?1000h` escape sequence in the output to check for. `None` means the
/// process has no usable console input handle.
///
/// The handle is opened from `CONIN$` exactly like crossterm does: with a
/// redirected stdin, `GetStdHandle(STD_INPUT_HANDLE)` is not a console handle,
/// but the process still owns a console.
fn console_input_mode() -> Option<u32> {
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{GetConsoleMode, CONSOLE_MODE};

    unsafe {
        let path: Vec<u16> = "CONIN$\0".encode_utf16().collect();
        let handle = CreateFileW(
            windows::core::PCWSTR(path.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
        .ok()?;

        let mut mode = CONSOLE_MODE(0);
        let read = GetConsoleMode(handle, &mut mode).is_ok();
        let _ = CloseHandle(handle);
        if !read {
            return None;
        }
        Some(mode.0)
    }
}

/// `ENABLE_MOUSE_INPUT` bit of a console input mode.
fn has_mouse_input(mode: u32) -> bool {
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    mode & ENABLE_MOUSE_INPUT != 0
}

#[derive(Default)]
struct Options {
    help: bool,
    dump_ui: bool,
    demo_connected: bool,
    demo_empty: bool,
    size: Option<(u16, u16)>,
    smoke: bool,
    headless: bool,
    /// Device indices to connect to in order (`0` or `0,1` for a switch test).
    auto_connect: Vec<usize>,
    auto_disconnect: Option<Duration>,
    auto_force_reconnect: Option<Duration>,
    auto_refresh: Option<Duration>,
    exit_after: Option<Duration>,
    /// Debug: fake `Open()` failures with this `ExtendedError` code.
    force_error: Option<u32>,
    /// Debug: inject the failure only on the first attempt (retry recovers).
    force_error_once: bool,
    /// `--auto-reconnect-on-silence`: one automatic `f` when a fresh link
    /// stays silent for 8 seconds.
    auto_reconnect_on_silence: bool,
    /// Demo level (`0.0..=1.0`) used by `--dump-ui` to render the volume bar.
    dump_level: Option<f32>,
    /// `--keep-ime`: leave the Windows IME attached to the console window.
    /// Escape hatch for environments where detaching it misbehaves.
    keep_ime: bool,
}

fn main() -> Result<()> {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("参数错误：{error:#}");
            std::process::exit(2);
        }
    };

    if options.help {
        print_help();
        return Ok(());
    }
    if options.dump_ui {
        dump_ui(&options)?;
        return Ok(());
    }

    // COM/MTA setup and the watcher live on the worker thread; this returns
    // immediately.
    let worker_options = WorkerOptions {
        forced_open_error: options.force_error.map(|code| {
            if options.force_error_once {
                ForcedOpenError::Once(code)
            } else {
                ForcedOpenError::Always(code)
            }
        }),
        auto_reconnect_on_silence: options.auto_reconnect_on_silence,
    };
    let mut worker = Worker::spawn_with(worker_options)?;
    // The worker waits for this before scanning, so the UI controls when the
    // (possibly slow) enumeration starts.
    let _ = worker.cmd().send(Cmd::RefreshDevices);

    if options.headless {
        run_headless(&mut worker, &options)?;
    } else {
        run_tui(&mut worker, &options)?;
    }

    println!("已退出 Bluetooth Audio Receiver。");
    Ok(())
}

fn print_help() {
    println!("bluetooth-audio-receiver — Bluetooth Audio Receiver (TUI)");
    println!();
    println!("用法:");
    println!("  bluetooth-audio-receiver                          启动 TUI");
    println!("  bluetooth-audio-receiver --dump-ui                用 TestBackend 渲染界面快照并打印");
    println!("      --size <宽>x<高>                    配合 --dump-ui，默认 80x24");
    println!("      --connected                         配合 --dump-ui，渲染已连接状态");
    println!("      --empty                             配合 --dump-ui，渲染空列表状态");
    println!("      --level <0.0-1.0>                   配合 --dump-ui，用假电平渲染音量条");
    println!("  bluetooth-audio-receiver --headless               不启动 TUI，逐条打印 worker 事件");
    println!("  bluetooth-audio-receiver --auto-connect <idx[,idx]>");
    println!("                                          枚举完成后依次连接这些设备（0,1 = 切换测试）");
    println!("  bluetooth-audio-receiver --auto-disconnect <秒>   连接建立 N 秒后自动断开（调试用）");
    println!("  bluetooth-audio-receiver --auto-force-reconnect <秒>");
    println!("                                          连接建立 N 秒后发送一次强制重连（等同按 f）");
    println!("  bluetooth-audio-receiver --auto-refresh <秒>      每 N 秒重建一次 DeviceWatcher（泄漏自检）");
    println!("  bluetooth-audio-receiver --exit-after <秒>        运行指定秒数后自动退出（调试用）");
    println!("  bluetooth-audio-receiver --force-error <hex>      调试：每次 Open() 都注入该错误码（例如 8007001f），");
    println!("                                          用于验证重试与最终错误文案");
    println!("  bluetooth-audio-receiver --force-error-once       调试：只注入第一次 Open()，用于验证重试后恢复");
    println!("  bluetooth-audio-receiver --auto-reconnect-on-silence");
    println!("                                          连接满 8 秒仍无音频时自动强制重连一次（默认关闭）");
    println!("  bluetooth-audio-receiver --smoke                  启动 TUI 后自动退出（终端恢复自检）");
    println!("  bluetooth-audio-receiver --keep-ime               启动 TUI 时不禁用输入法（默认会禁用，");
    println!("                                          否则中文输入法会吞掉字母键）");
    println!("  bluetooth-audio-receiver --help                   显示本帮助");
    println!();
    println!("快捷键: ↑↓/jk 选择  Enter 连接  f 强制重连  d 断开  r 刷新  b 蓝牙设置  q/Esc 退出");
    println!("鼠标:   点击底部按钮执行对应操作，点击设备行选中，双击连接");
    println!("        鼠标捕获期间复制文字请按住 Shift 拖选（终端原生选择）");
}

fn parse_args() -> Result<Options> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut options = Options::default();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => options.help = true,
            "--dump-ui" => options.dump_ui = true,
            "--connected" => options.demo_connected = true,
            "--empty" => options.demo_empty = true,
            "--smoke" => options.smoke = true,
            "--headless" => options.headless = true,
            "--size" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--size 缺少参数（例如 100x30）"))?;
                let (width, height) = raw
                    .split_once(['x', 'X'])
                    .ok_or_else(|| anyhow::anyhow!("--size 需要 <宽>x<高> 格式"))?;
                options.size = Some((
                    width.parse().map_err(|_| anyhow::anyhow!("--size 宽度无效"))?,
                    height.parse().map_err(|_| anyhow::anyhow!("--size 高度无效"))?,
                ));
                index += 1;
            }
            "--auto-disconnect" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--auto-disconnect 缺少秒数"))?;
                let seconds: u64 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--auto-disconnect 需要整数秒数"))?;
                options.auto_disconnect = Some(Duration::from_secs(seconds));
                index += 1;
            }
            "--auto-force-reconnect" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--auto-force-reconnect 缺少秒数"))?;
                let seconds: u64 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--auto-force-reconnect 需要整数秒数"))?;
                options.auto_force_reconnect = Some(Duration::from_secs(seconds));
                index += 1;
            }
            "--auto-refresh" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--auto-refresh 缺少秒数"))?;
                let seconds: u64 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--auto-refresh 需要整数秒数"))?;
                options.auto_refresh = Some(Duration::from_secs(seconds));
                index += 1;
            }
            "--auto-connect" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--auto-connect 缺少设备序号"))?;
                let mut indices = Vec::new();
                for part in raw.split(',') {
                    indices.push(
                        part.trim()
                            .parse()
                            .map_err(|_| anyhow::anyhow!("--auto-connect 需要非负整数"))?,
                    );
                }
                options.auto_connect = indices;
                index += 1;
            }
            "--exit-after" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--exit-after 缺少秒数"))?;
                let seconds: u64 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--exit-after 需要整数秒数"))?;
                options.exit_after = Some(Duration::from_secs(seconds));
                index += 1;
            }
            "--force-error" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--force-error 缺少错误码（例如 8007001f）"))?;
                options.force_error = Some(parse_hex_code(raw)?);
                index += 1;
            }
            "--force-error-once" => options.force_error_once = true,
            "--auto-reconnect-on-silence" => options.auto_reconnect_on_silence = true,
            "--keep-ime" => options.keep_ime = true,
            "--level" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--level 缺少参数（例如 0.6）"))?;
                let level: f32 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--level 需要 0.0 到 1.0 之间的小数"))?;
                if !(0.0..=1.0).contains(&level) {
                    bail!("--level 需要 0.0 到 1.0 之间的小数");
                }
                options.dump_level = Some(level);
                index += 1;
            }
            other => bail!("无法识别的参数: {other}（用 --help 查看用法）"),
        }
        index += 1;
    }

    if options.force_error_once && options.force_error.is_none() {
        bail!("--force-error-once 需要配合 --force-error <十六进制错误码> 使用");
    }
    if options.dump_level.is_some() && !options.dump_ui {
        bail!("--level 需要配合 --dump-ui 使用");
    }

    Ok(options)
}

/// Parses `8007001f` or `0x8007001f` into a 32-bit error code.
fn parse_hex_code(raw: &str) -> Result<u32> {
    let digits = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw);
    u32::from_str_radix(digits, 16)
        .map_err(|_| anyhow::anyhow!("--force-error 需要十六进制错误码（例如 8007001f）"))
}

// ---------------------------------------------------------------------- TUI

fn run_tui(worker: &mut Worker, options: &Options) -> Result<()> {
    // Baseline probe: `--smoke` compares it with the mode after the guard is
    // dropped, which makes the enable/disable pair observable.
    let mouse_mode_before = if options.smoke {
        console_input_mode()
    } else {
        None
    };
    install_panic_hook();
    let mut guard = TerminalGuard::init()?;
    let mouse_mode_during = if options.smoke {
        console_input_mode()
    } else {
        None
    };
    let mut app = App::new();
    app.auto_reconnect_on_silence = options.auto_reconnect_on_silence;
    if !guard.mouse_captured() {
        app.log("警告：当前终端无法启用鼠标捕获，按钮不可点击（键盘仍可用）");
    }

    // A Chinese IME in a conhost window treats every letter as pinyin: the
    // candidate window opens and crossterm never sees the key. Detach the IME
    // from this console window for the lifetime of the TUI; `ImeGuard::drop`
    // restores the previous input context. `--keep-ime` is the escape hatch.
    //
    // This only reaches IMM32: Windows 11 IMEs are TSF3 based and were measured
    // to keep swallowing keys. Mouse capture is the workaround that does not
    // depend on the IME architecture, hence the clickable button bar.
    let ime_guard = if options.keep_ime {
        app.log("已按 --keep-ime 保留输入法（字母键可能被输入法接管）".to_string());
        None
    } else {
        match console::ImeGuard::install() {
            Some(guard) => Some(guard),
            None => {
                app.log(
                    "警告：无法获取控制台窗口，未能禁用输入法（字母键可能被输入法接管）"
                        .to_string(),
                );
                None
            }
        }
    };

    // Fallback for Ctrl+C arriving as CTRL_C_EVENT instead of a key event.
    // Defense in depth only: the handler just sets an atomic flag.
    console::reset_quit_flag();
    let ctrl_handler = match console::CtrlHandlerGuard::install() {
        Ok(handler) => Some(handler),
        Err(error) => {
            app.log(format!("警告：无法注册 Ctrl+C 处理程序（{error}）"));
            None
        }
    };

    let cmd_tx = worker.cmd();
    let started = Instant::now();
    let mut last_tick = started;
    let mut pilot = AutoPilot::new(options, started);

    // Clickable regions of the frame that is currently on screen. Mouse events
    // are resolved against exactly what the user sees.
    let mut hits: Vec<HitArea> = Vec::new();
    // Last known pointer cell; resolved against `hits` on the next frame.
    let mut hover: Option<(u16, u16)> = None;
    let mut last_click: Option<(Instant, u16, u16)> = None;

    loop {
        let hovered = hover.and_then(|(column, row)| ui::hit_test(&hits, column, row));
        guard.terminal_mut().draw(|frame| {
            hits = ui::draw(frame, &app, hovered);
        })?;

        if term_event::poll(Duration::from_millis(50))? {
            match term_event::read()? {
                // Windows reports Press + Repeat; ignore Release to avoid
                // double-triggering on terminals that send it.
                Event::Key(key) => {
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        handle_action(&mut app, map_key(key), &cmd_tx);
                    }
                }
                // Mouse input bypasses the IME entirely, which is why the
                // buttons exist: a Chinese IME can swallow every letter key.
                Event::Mouse(mouse) => handle_mouse(
                    &mut app,
                    mouse,
                    &hits,
                    &cmd_tx,
                    &mut hover,
                    &mut last_click,
                ),
                _ => {}
            }
        }

        drain_events(worker, &mut app);
        pilot.poll(&mut app, &cmd_tx, Instant::now());

        // Drives the spinner independently of worker events.
        if last_tick.elapsed() >= TICK_INTERVAL {
            app.tick();
            last_tick = Instant::now();
        }

        // Fallback path: if Ctrl+C arrived as CTRL_C_EVENT instead of a key,
        // fold it into the normal quit flow so the drops still restore the
        // console and the IME.
        if console::quit_requested() && !app.should_quit {
            app.request_quit();
        }

        if options.smoke && started.elapsed() >= Duration::from_millis(600) {
            app.request_quit();
        }
        if let Some(limit) = options.exit_after {
            if started.elapsed() >= limit && !app.should_quit {
                app.request_quit();
            }
        }

        if console::should_exit(console::quit_requested(), app.should_quit) {
            break;
        }
    }

    // Restore the IME and the terminal before waiting for the worker to close
    // Bluetooth. The IME goes first so the console window is never left
    // without an input context while the alternate screen is torn down.
    let mouse_captured = guard.mouse_captured();
    drop(ime_guard);
    drop(ctrl_handler);
    drop(guard);
    let mouse_mode_after = if options.smoke {
        console_input_mode()
    } else {
        None
    };
    worker.shutdown();

    if options.smoke {
        // Printed after the alternate screen is gone, so it stays visible.
        // crossterm implements mouse capture with SetConsoleMode on Windows
        // (no `?1000h` sequence), hence the explicit mode report.
        println!(
            "鼠标捕获: {}",
            if mouse_captured {
                "已开启，退出时已关闭"
            } else {
                "当前环境不可用，已跳过（TUI 仍可用键盘）"
            }
        );
        match (mouse_mode_before, mouse_mode_during, mouse_mode_after) {
            (Some(before), Some(during), Some(after)) => println!(
                "smoke: 控制台输入模式 之前={before:#06x} 运行中={during:#06x} 退出后={after:#06x} \
                 ENABLE_MOUSE_INPUT 运行中={} 恢复一致={}",
                has_mouse_input(during),
                before == after
            ),
            _ => println!("smoke: 无法读取控制台输入模式，跳过位标志校验"),
        }
    }
    Ok(())
}

/// Resolves a mouse event against the last frame's hit areas.
///
/// Buttons take the exact same path as their keyboard shortcuts; a device row
/// click only moves the selection, and a double click also connects.
fn handle_mouse(
    app: &mut App,
    mouse: MouseEvent,
    hits: &[HitArea],
    cmd_tx: &Sender<Cmd>,
    hover: &mut Option<(u16, u16)>,
    last_click: &mut Option<(Instant, u16, u16)>,
) {
    match mouse.kind {
        MouseEventKind::Moved => *hover = Some((mouse.column, mouse.row)),

        MouseEventKind::Down(MouseButton::Left) => {
            let Some(action) = ui::hit_test(hits, mouse.column, mouse.row) else {
                *last_click = None;
                return;
            };
            match action {
                UiAction::SelectDevice(index) => {
                    app.select_index(index);
                    let now = Instant::now();
                    let double_click = last_click.is_some_and(|(at, column, row)| {
                        now.duration_since(at) <= DOUBLE_CLICK_INTERVAL
                            && (column, row) == (mouse.column, mouse.row)
                    });
                    if double_click {
                        *last_click = None;
                        // Same as pressing Enter on the row.
                        handle_action(app, Action::Connect, cmd_tx);
                    } else {
                        *last_click = Some((now, mouse.column, mouse.row));
                    }
                }
                other => {
                    *last_click = None;
                    handle_action(app, other.to_action(), cmd_tx);
                }
            }
        }

        // Wheel scrolling is intentionally not handled: the device list is
        // short and the draw loop keeps the selection visible.
        _ => {}
    }
}

fn handle_action(app: &mut App, action: Action, cmd_tx: &Sender<Cmd>) {
    match action {
        Action::Up => app.select_previous(),
        Action::Down => app.select_next(),
        Action::Connect => {
            if let Some(command) = app.connect_selected() {
                let _ = cmd_tx.send(command);
            }
        }
        Action::ForceReconnect => {
            let _ = cmd_tx.send(app.request_force_reconnect());
        }
        Action::Disconnect => {
            let _ = cmd_tx.send(app.request_disconnect());
        }
        Action::Refresh => {
            let _ = cmd_tx.send(app.request_refresh());
        }
        Action::BluetoothSettings => {
            let _ = cmd_tx.send(app.request_bluetooth_settings());
        }
        Action::Quit => app.request_quit(),
        Action::None => {}
    }
}

fn drain_events(worker: &Worker, app: &mut App) {
    while let Some(event) = worker.try_recv() {
        app.apply(event);
    }
}

/// Drives the debug flags that replace human key presses.
struct AutoPilot {
    /// Devices to connect to, in order. More than one exercises switching.
    queue: VecDeque<usize>,
    /// Dwell time on a connection before switching to the next device.
    switch_after: Duration,
    disconnect_after: Option<Duration>,
    force_reconnect_after: Option<Duration>,
    refresh_every: Option<Duration>,
    next_refresh: Option<Instant>,
    connected_at: Option<Instant>,
    disconnect_sent: bool,
    force_reconnect_sent: bool,
}

impl AutoPilot {
    fn new(options: &Options, now: Instant) -> Self {
        Self {
            queue: options.auto_connect.iter().copied().collect(),
            switch_after: Duration::from_secs(2),
            disconnect_after: options.auto_disconnect,
            force_reconnect_after: options.auto_force_reconnect,
            refresh_every: options.auto_refresh,
            next_refresh: options.auto_refresh.map(|interval| now + interval),
            connected_at: None,
            disconnect_sent: false,
            force_reconnect_sent: false,
        }
    }

    fn poll(&mut self, app: &mut App, cmd_tx: &Sender<Cmd>, now: Instant) {
        // Periodic refresh: rebuilding the watcher repeatedly doubles as a
        // leak check for the diagnostic runs.
        if let (Some(interval), Some(next)) = (self.refresh_every, self.next_refresh) {
            if now >= next {
                self.next_refresh = Some(now + interval);
                let _ = cmd_tx.send(app.request_refresh());
            }
        }

        // Connect the next queued device once the current link is settled.
        // Waiting for the enumeration is implicit: the list is empty until then.
        if !self.queue.is_empty() && !app.devices.is_empty() {
            let ready = match &app.connection {
                ConnState::Idle => true,
                ConnState::Connected(_) => self
                    .connected_at
                    .map(|at| now.duration_since(at) >= self.switch_after)
                    .unwrap_or(false),
                ConnState::Connecting(_) => false,
            };

            if ready {
                let index = self.queue.pop_front().unwrap_or(0).min(app.devices.len() - 1);
                app.selected = index;
                app.log(format!(
                    "--auto-connect：选中 #{index} {}",
                    app.devices[index].name
                ));
                if let Some(command) = app.connect_selected() {
                    let _ = cmd_tx.send(command);
                    self.connected_at = None;
                }
            }
        }

        if matches!(app.connection, ConnState::Connected(_)) {
            let connected_at = *self.connected_at.get_or_insert(now);
            if !self.force_reconnect_sent {
                if let Some(delay) = self.force_reconnect_after {
                    if now.duration_since(connected_at) >= delay {
                        self.force_reconnect_sent = true;
                        app.log("--auto-force-reconnect：发送强制重连指令");
                        let _ = cmd_tx.send(app.request_force_reconnect());
                    }
                }
            }
            if !self.disconnect_sent {
                if let Some(delay) = self.disconnect_after {
                    if now.duration_since(connected_at) >= delay {
                        self.disconnect_sent = true;
                        app.log("--auto-disconnect：发送断开指令");
                        let _ = cmd_tx.send(app.request_disconnect());
                    }
                }
            }
        } else {
            self.connected_at = None;
        }
    }
}

// ----------------------------------------------------------------- headless

fn run_headless(worker: &mut Worker, options: &Options) -> Result<()> {
    let mut app = App::new();
    app.auto_reconnect_on_silence = options.auto_reconnect_on_silence;
    let cmd_tx = worker.cmd();
    let started = Instant::now();
    let mut pilot = AutoPilot::new(options, started);

    println!("headless：不启动 TUI，仅输出 worker 事件");

    loop {
        while let Some(event) = worker.try_recv() {
            println!("[evt] {event:?}");
            app.apply(event);
        }

        pilot.poll(&mut app, &cmd_tx, Instant::now());

        if app.should_quit || worker.is_finished() {
            break;
        }
        if let Some(limit) = options.exit_after {
            if started.elapsed() >= limit {
                println!("[..] 到达 --exit-after 时间，退出");
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    worker.shutdown();

    println!("--- 最终连接状态: {:?} ---", app.connection);
    for line in &app.logs {
        println!("{line}");
    }
    Ok(())
}

// ---------------------------------------------------------------- dump / demo

fn demo_app(
    connected: bool,
    empty: bool,
    level: Option<f32>,
    auto_reconnect: bool,
) -> App {
    let mut app = App::new();
    app.auto_reconnect_on_silence = auto_reconnect;
    if empty {
        app.apply(Evt::DeviceList(Vec::new()));
        return app;
    }

    app.apply(Evt::DeviceList(vec![
        DeviceInfo {
            id: concat!(
                r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}",
                r"_VID&0001004c_PID&7012#7&d8f392&0&AABBCCDDEE01_C00000000",
                r"#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK"
            )
            .to_string(),
            name: "张三的iPad".to_string(),
            is_paired: false,
            kind: "DeviceInterface".to_string(),
        },
        DeviceInfo {
            id: concat!(
                r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}",
                r"_VID&0001038f_PID&1200#7&d8f392&0&AABBCCDDEE02_C00000000",
                r"#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK"
            )
            .to_string(),
            name: "李四的手机".to_string(),
            is_paired: false,
            kind: "DeviceInterface".to_string(),
        },
        DeviceInfo {
            id: concat!(
                r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}",
                r"_VID&0001004c_PID&7012#7&d8f392&0&001A7DDA7113_C00000000",
                r"#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK"
            )
            .to_string(),
            name: "Soundcore Life Q35".to_string(),
            is_paired: true,
            kind: "DeviceInterface".to_string(),
        },
    ]));
    app.apply(Evt::Log("扫描完成，共 3 个设备".to_string()));

    if connected || level.is_some() {
        app.apply(Evt::Connecting("张三的iPad".to_string()));
        app.apply(Evt::Opened {
            name: "张三的iPad".to_string(),
        });
        if let Some(level) = level {
            // Repeated samples let the exponential smoothing converge, so the
            // dump shows the steady-state bar rather than a damped first read.
            for _ in 0..20 {
                app.apply(Evt::AudioLevel(level));
            }
        }
    }
    app
}

fn dump_ui(options: &Options) -> Result<()> {
    let (width, height) = options.size.unwrap_or((80, 24));
    let app = demo_app(
        options.demo_connected,
        options.demo_empty,
        options.dump_level,
        options.auto_reconnect_on_silence,
    );

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    let mut hits: Vec<HitArea> = Vec::new();
    terminal.draw(|frame| {
        hits = ui::draw(frame, &app, None);
    })?;

    let buffer = terminal.backend().buffer();
    println!(
        "UI dump: {}x{}  state={}",
        width,
        height,
        if options.demo_empty {
            "empty"
        } else if options.demo_connected || options.dump_level.is_some() {
            "connected"
        } else {
            "idle"
        }
    );

    println!("可点击区域: {} 个（按钮/设备行）", hits.len());

    let mut mismatched = 0usize;
    for (y, line) in ui::buffer_lines(buffer, width, height)
        .into_iter()
        .enumerate()
    {
        let measured = UnicodeWidthStr::width(line.as_str());
        if measured != width as usize {
            mismatched += 1;
        }
        // Delimiters make trailing spaces visible, which is what alignment
        // checks depend on.
        println!("{y:02}|{line}|");
    }

    if mismatched == 0 {
        println!("宽度校验: 所有行 = {width} 列（unicode-width），通过");
    } else {
        println!("宽度校验: {mismatched} 行宽度异常");
    }
    Ok(())
}

// ------------------------------------------------------------ terminal guard

/// Restores the terminal. Created only after raw mode is enabled, so `Drop`
/// always pairs with `init`.
struct TerminalGuard {
    terminal: Tui,
    /// False when the environment has no console input handle; the TUI then
    /// stays usable with the keyboard.
    mouse_captured: bool,
}

impl TerminalGuard {
    fn init() -> Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }

        // Best effort: mouse capture is what lets the user bypass a keyboard
        // IME, but it must never prevent the TUI from starting. On Windows
        // crossterm implements this with SetConsoleMode (ENABLE_MOUSE_INPUT).
        let mouse_captured = execute!(stdout, EnableMouseCapture).is_ok();

        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(mut terminal) => {
                let _ = terminal.clear();
                Ok(Self {
                    terminal,
                    mouse_captured,
                })
            }
            Err(error) => {
                // Alt screen is already active; undo it before bailing out.
                if mouse_captured {
                    let _ = execute!(io::stdout(), DisableMouseCapture);
                }
                let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }

    fn terminal_mut(&mut self) -> &mut Tui {
        &mut self.terminal
    }

    fn mouse_captured(&self) -> bool {
        self.mouse_captured
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Mouse capture must be undone while raw mode is still active:
        // crossterm's disable restores the console mode it saved when the
        // capture started, and that snapshot is the raw-mode one. The reverse
        // order would leave the console in raw mode after exit.
        if self.mouse_captured {
            let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        }
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        let _ = self.terminal.show_cursor();
    }
}

/// Restores the terminal before the default hook prints the panic message;
/// otherwise a panic leaves the console in raw mode with garbled output.
fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Same order as TerminalGuard::drop.
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;
    use std::sync::mpsc::channel;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    fn sample_app() -> App {
        let mut app = App::new();
        app.apply(Evt::DeviceList(vec![
            DeviceInfo {
                id: "id-ipad".to_string(),
                name: "张三的iPad".to_string(),
                is_paired: true,
                kind: "DeviceInterface".to_string(),
            },
            DeviceInfo {
                id: "id-civi".to_string(),
                name: "李四的手机".to_string(),
                is_paired: true,
                kind: "DeviceInterface".to_string(),
            },
        ]));
        app
    }

    #[test]
    fn clicking_a_button_sends_the_keyboard_command() {
        let mut app = sample_app();
        let (tx, rx) = channel();
        let hits = vec![HitArea {
            rect: Rect::new(1, 20, 12, 1),
            action: UiAction::Disconnect,
        }];
        let mut hover = None;
        let mut last_click = None;

        handle_mouse(
            &mut app,
            click(3, 20),
            &hits,
            &tx,
            &mut hover,
            &mut last_click,
        );

        assert_eq!(rx.try_recv().unwrap(), Cmd::Disconnect);
    }

    #[test]
    fn clicking_a_row_selects_without_connecting() {
        let mut app = sample_app();
        let (tx, rx) = channel();
        let hits = vec![HitArea {
            rect: Rect::new(1, 5, 40, 1),
            action: UiAction::SelectDevice(1),
        }];
        let mut hover = None;
        let mut last_click = None;

        handle_mouse(
            &mut app,
            click(3, 5),
            &hits,
            &tx,
            &mut hover,
            &mut last_click,
        );

        assert_eq!(app.selected, 1);
        assert!(rx.try_recv().is_err(), "a single click must not connect");
    }

    #[test]
    fn double_clicking_a_row_connects_it() {
        let mut app = sample_app();
        let (tx, rx) = channel();
        let hits = vec![HitArea {
            rect: Rect::new(1, 5, 40, 1),
            action: UiAction::SelectDevice(1),
        }];
        let mut hover = None;
        let mut last_click = None;

        for _ in 0..2 {
            handle_mouse(
                &mut app,
                click(3, 5),
                &hits,
                &tx,
                &mut hover,
                &mut last_click,
            );
        }

        assert_eq!(app.selected, 1);
        match rx.try_recv().unwrap() {
            Cmd::Connect { device_id, name } => {
                assert_eq!(device_id, "id-civi");
                assert_eq!(name, "李四的手机");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clicking_outside_every_hit_area_does_nothing() {
        let mut app = sample_app();
        let (tx, rx) = channel();
        let hits = vec![HitArea {
            rect: Rect::new(1, 20, 12, 1),
            action: UiAction::Quit,
        }];
        let mut hover = None;
        let mut last_click = None;

        handle_mouse(
            &mut app,
            click(1, 19),
            &hits,
            &tx,
            &mut hover,
            &mut last_click,
        );

        assert!(!app.should_quit);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn mouse_move_records_the_hover_cell() {
        let mut app = sample_app();
        let (tx, _rx) = channel();
        let mut hover = None;
        let mut last_click = None;

        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Moved, 7, 9),
            &[],
            &tx,
            &mut hover,
            &mut last_click,
        );

        assert_eq!(hover, Some((7, 9)));
    }

    #[test]
    fn clicking_quit_sets_the_quit_flag() {
        let mut app = sample_app();
        let (tx, _rx) = channel();
        let hits = vec![HitArea {
            rect: Rect::new(1, 20, 12, 1),
            action: UiAction::Quit,
        }];
        let mut hover = None;
        let mut last_click = None;

        handle_mouse(
            &mut app,
            click(1, 20),
            &hits,
            &tx,
            &mut hover,
            &mut last_click,
        );

        assert!(app.should_quit);
    }
}
