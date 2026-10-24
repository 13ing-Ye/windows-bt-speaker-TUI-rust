//! P1 probe: a plain-text, UI-free walk through the whole
//! `AudioPlaybackConnection` call chain.
//!
//! Usage:
//!   probe                 list devices and exit
//!   probe --index 0       open the device at list index 0
//!   probe --id <device>   open a specific device id
//!   probe --index 0 --hold 10
//!                         open, then auto-close after 10 seconds
//!
//! There is deliberately no TUI code in this file (P2 is gated on P1 results).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{Ref, BOOL, HSTRING};
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind, DeviceWatcher};
use windows::Foundation::TypedEventHandler;
use windows::Media::Audio::{AudioPlaybackConnection, AudioPlaybackConnectionOpenResultStatus};
use windows::Win32::System::Console::{SetConsoleCtrlHandler, SetConsoleOutputCP};

use crate::com;

/// Set by the console control handler, polled by the main thread.
static CTRL_C_PRESSED: AtomicBool = AtomicBool::new(false);

/// Console control handler. Returning `TRUE` means "handled", so Windows does
/// not terminate the process and the main thread gets a chance to `Close()`.
unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> BOOL {
    // 0 = CTRL_C_EVENT, 1 = CTRL_BREAK_EVENT
    if ctrl_type == 0 || ctrl_type == 1 {
        CTRL_C_PRESSED.store(true, Ordering::SeqCst);
        return BOOL(1);
    }
    BOOL(0)
}

#[derive(Clone, Debug)]
struct Device {
    name: String,
    id: String,
    is_paired: bool,
    kind_code: i32,
}

enum WatchMsg {
    Added(Device),
    Completed,
}

struct Options {
    index: Option<usize>,
    id: Option<String>,
    hold: Option<u64>,
}

impl Options {
    fn has_target(&self) -> bool {
        self.index.is_some() || self.id.is_some()
    }
}

fn print_help() {
    println!("用法:");
    println!("  probe                          列出设备后退出");
    println!("  probe --index <N>              连接列表中编号为 N 的设备（从 0 开始）");
    println!("  probe --id <device_id>         连接指定 Id 的设备");
    println!("  probe --hold <秒>              连接建立后等待指定秒数自动断开（默认等 Ctrl+C）");
    println!("  probe --help                   显示本帮助");
}

fn parse_args() -> Result<Option<Options>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = Options { index: None, id: None, hold: None };
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--index" => {
                let raw = args.get(i + 1).ok_or_else(|| anyhow!("--index 缺少参数"))?;
                opts.index = Some(raw.parse::<usize>().context("--index 需要非负整数")?);
                i += 2;
            }
            "--id" => {
                opts.id = Some(args.get(i + 1).ok_or_else(|| anyhow!("--id 缺少参数"))?.clone());
                i += 2;
            }
            "--hold" => {
                let raw = args.get(i + 1).ok_or_else(|| anyhow!("--hold 缺少参数"))?;
                opts.hold = Some(raw.parse::<u64>().context("--hold 需要秒数（整数）")?);
                i += 2;
            }
            other => bail!("无法识别的参数: {other}（用 --help 查看用法）"),
        }
    }

    if opts.index.is_some() && opts.id.is_some() {
        bail!("--index 和 --id 只能选一个");
    }
    Ok(Some(opts))
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

fn device_from_info(info: &DeviceInformation) -> Result<Device> {
    let name = info.Name().map(|s| s.to_string_lossy()).unwrap_or_default();
    let id = info.Id().context("读取 DeviceInformation.Id() 失败")?.to_string_lossy();
    // DeviceInformation has no IsPaired() itself; it lives on DeviceInformation.Pairing().
    let is_paired = info.Pairing().ok().and_then(|p| p.IsPaired().ok()).unwrap_or(false);
    let kind = info.Kind().unwrap_or_default();

    Ok(Device { name, id, is_paired, kind_code: kind.0 })
}

fn enumerate_devices(selector: &HSTRING) -> Result<Vec<Device>> {
    let watcher = DeviceInformation::CreateWatcherAqsFilter(selector)
        .context("DeviceInformation::CreateWatcher(selector) 失败")?;

    let (tx, rx) = mpsc::channel::<WatchMsg>();

    let added_tx = tx.clone();
    let added_handler = TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(
        move |_sender: Ref<DeviceWatcher>, info: Ref<DeviceInformation>| {
            if let Some(info) = info.as_ref() {
                if let Ok(device) = device_from_info(info) {
                    let _ = added_tx.send(WatchMsg::Added(device));
                }
            }
            Ok(())
        },
    );
    watcher.Added(&added_handler).context("订阅 DeviceWatcher.Added 失败")?;

    let done_tx = tx.clone();
    let completed_handler = TypedEventHandler::<DeviceWatcher, windows::core::IInspectable>::new(
        move |_sender: Ref<DeviceWatcher>, _args: Ref<windows::core::IInspectable>| {
            let _ = done_tx.send(WatchMsg::Completed);
            Ok(())
        },
    );
    watcher
        .EnumerationCompleted(&completed_handler)
        .context("订阅 DeviceWatcher.EnumerationCompleted 失败")?;

    watcher.Start().context("DeviceWatcher.Start() 失败")?;

    let mut devices: Vec<Device> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(8);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            println!("[!] 等待枚举完成超时（8 秒），先使用已收集到的设备");
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(WatchMsg::Added(device)) => {
                if !devices.iter().any(|d| d.id == device.id) {
                    devices.push(device);
                }
            }
            Ok(WatchMsg::Completed) => break,
            Err(RecvTimeoutError::Timeout) => {
                println!("[!] 等待枚举完成超时（8 秒），先使用已收集到的设备");
                break;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = watcher.Stop();
    Ok(devices)
}

fn print_devices(devices: &[Device]) {
    for (i, d) in devices.iter().enumerate() {
        let name = if d.name.is_empty() { "(无名称)" } else { d.name.as_str() };
        println!(
            "  #{i}  {name}  [已配对={}]  [Kind={}({})]",
            if d.is_paired { "是" } else { "否" },
            kind_label(DeviceInformationKind(d.kind_code)),
            d.kind_code
        );
        println!("       Id: {}", d.id);
    }
}

/// Closes the connection on every exit path, including panics.
struct ConnGuard {
    conn: Option<AudioPlaybackConnection>,
}

impl ConnGuard {
    fn close(&mut self) {
        if let Some(conn) = self.conn.take() {
            match conn.Close() {
                Ok(()) => println!("[6] Close() 成功"),
                Err(e) => println!("[!] Close() 失败: 0x{:08X} {e}", e.code().0 as u32),
            }
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.Close();
        }
    }
}

fn status_name(status: AudioPlaybackConnectionOpenResultStatus) -> &'static str {
    if status == AudioPlaybackConnectionOpenResultStatus::Success {
        "Success（成功）"
    } else if status == AudioPlaybackConnectionOpenResultStatus::RequestTimedOut {
        "RequestTimedOut（请求超时）"
    } else if status == AudioPlaybackConnectionOpenResultStatus::DeniedBySystem {
        "DeniedBySystem（被系统拒绝）"
    } else if status == AudioPlaybackConnectionOpenResultStatus::UnknownFailure {
        "UnknownFailure（未知失败）"
    } else {
        "Unrecognized（无法识别的状态码）"
    }
}

fn print_extended_error(result: &windows::Media::Audio::AudioPlaybackConnectionOpenResult) {
    match result.ExtendedError() {
        Ok(hr) => {
            let message = windows::core::Error::from_hresult(hr).message();
            println!("    ExtendedError = 0x{:08X} ({message})", hr.0 as u32);
        }
        Err(e) => println!("    读取 ExtendedError 失败: 0x{:08X} {e}", e.code().0 as u32),
    }
}

/// Blocks until Ctrl+C or the `--hold` deadline, while polling `State()`.
///
/// Polling matters because `Open()` can return Success before the A2DP stream
/// actually starts; `State` only becomes `Opened` once audio is streaming.
fn wait_for_exit(hold: Option<u64>, conn: Option<&AudioPlaybackConnection>) {
    let started = Instant::now();
    let mut last_state = conn.and_then(|c| c.State().ok()).map(|s| s.0);
    let mut last_poll = Instant::now();

    loop {
        if CTRL_C_PRESSED.load(Ordering::SeqCst) {
            println!("\n[·] 收到 Ctrl+C，准备断开");
            return;
        }
        if let Some(secs) = hold {
            if started.elapsed() >= Duration::from_secs(secs) {
                println!("\n[·] 到达 --hold 指定时长（{secs} 秒），自动断开");
                return;
            }
        }

        if let Some(conn) = conn {
            if last_poll.elapsed() >= Duration::from_millis(500) {
                last_poll = Instant::now();
                if let Ok(state) = conn.State() {
                    if last_state != Some(state.0) {
                        println!("[·] 状态变化: State = {state:?}");
                        last_state = Some(state.0);
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn run() -> Result<()> {
    let opts = match parse_args()? {
        Some(opts) => opts,
        None => return Ok(()),
    };

    // Best effort: make Chinese output readable in legacy conhost.
    unsafe {
        let _ = SetConsoleOutputCP(65001);
    }

    com::initialize_mta().context("初始化 MTA 失败")?;
    println!("[0] COM 已初始化（MTA）");

    let selector = AudioPlaybackConnection::GetDeviceSelector()
        .context("AudioPlaybackConnection::GetDeviceSelector() 失败（API 不可用？）")?;
    let selector_text = selector.to_string_lossy();
    println!("[1] Device selector:");
    println!("    {selector_text}");
    if selector_text.is_empty() {
        bail!("GetDeviceSelector() 返回空字符串");
    }

    let devices = enumerate_devices(&selector)?;
    println!("\n[2] 枚举到 {} 个设备", devices.len());
    print_devices(&devices);

    if !opts.has_target() {
        println!("\n未指定设备，退出。用 --index <N> 或 --id <device_id> 选择设备。");
        return Ok(());
    }
    if devices.is_empty() {
        bail!("设备列表为空，无法继续");
    }

    let target = if let Some(index) = opts.index {
        devices
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("--index {index} 越界（共 {} 个设备）", devices.len()))?
    } else {
        let id = opts.id.as_deref().unwrap_or_default();
        devices
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("找不到 Id 为 {id} 的设备"))?
    };

    println!(
        "\n[3] 选中设备: {}",
        if target.name.is_empty() { "(无名称)" } else { target.name.as_str() }
    );
    println!("    Id: {}", target.id);

    let conn = match AudioPlaybackConnection::TryCreateFromId(&HSTRING::from(target.id.as_str())) {
        Ok(conn) => conn,
        // windows-rs maps a null WinRT object to an error carrying S_OK (0).
        Err(e) if e.code() == windows::core::HRESULT(0) => {
            println!("[3] TryCreateFromId 返回 null → 该设备不支持音频流（无法作为 A2DP Sink）");
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow!(
                "TryCreateFromId 失败: 0x{:08X} {e}",
                e.code().0 as u32
            ));
        }
    };
    println!("[3] TryCreateFromId 返回非 null，继续");

    let mut guard = ConnGuard { conn: Some(conn) };

    {
        let conn = guard.conn.as_ref().expect("connection missing");
        conn.Start().context("Start() 失败")?;
        println!("[4] Start() 成功（仅向系统登记，此时还没有音频）");
        match conn.State() {
            Ok(state) => println!("    当前 State = {state:?}"),
            Err(e) => println!("    读取 State() 失败: 0x{:08X} {e}", e.code().0 as u32),
        }
    }

    println!("[5] 正在调用 Open()（可能阻塞数秒）...");
    let result = {
        let conn = guard.conn.as_ref().expect("connection missing");
        conn.Open().context("Open() 调用失败")?
    };

    let status = result.Status().context("读取 OpenResult.Status() 失败")?;
    println!("[5] Open() 返回 Status = {}", status_name(status));

    if status == AudioPlaybackConnectionOpenResultStatus::Success {
        if let Some(conn) = guard.conn.as_ref() {
            match conn.State() {
                Ok(state) => println!("    连接后 State = {state:?}"),
                Err(e) => println!("    读取 State() 失败: 0x{:08X} {e}", e.code().0 as u32),
            }
        }

        println!();
        println!("[✓] Status = Success：连接已建立。");
        println!("    请用手机播放音频测试，本程序无法判断声音是否真的输出。");

        if let Err(e) = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), true) } {
            println!("[!] 注册 Ctrl+C 处理器失败: {e}");
            println!("    Ctrl+C 将直接结束进程，可能来不及调用 Close()");
        }

        match opts.hold {
            Some(secs) => println!("    {secs} 秒后自动断开；按 Ctrl+C 可提前结束。"),
            None => println!("    按 Ctrl+C 结束。"),
        }

        wait_for_exit(opts.hold, guard.conn.as_ref());
        guard.close();
    } else {
        print_extended_error(&result);
        match status {
            s if s == AudioPlaybackConnectionOpenResultStatus::RequestTimedOut => {
                println!("    可能原因：手机未响应、不在附近，或蓝牙链路尚未就绪。");
            }
            s if s == AudioPlaybackConnectionOpenResultStatus::DeniedBySystem => {
                println!(
                    "    可能原因：蓝牙已关闭、设备未配对、被其他程序占用，或非打包应用缺少权限。"
                );
            }
            _ => {}
        }
        guard.close();
    }

    Ok(())
}
