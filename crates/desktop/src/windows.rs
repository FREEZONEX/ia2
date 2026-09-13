//! Native Windows host. No child-process kill-on-drop or job-object teardown:
//! losing an IDE window must never implicitly stop a running controller.

use crate::support::*;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Icon, Theme, Window, WindowBuilder, WindowId};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, WaitForMultipleObjects,
    CREATE_NO_WINDOW, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows_sys::Win32::UI::Shell::{SetCurrentProcessExplicitAppUserModelID, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, SW_SHOWNORMAL,
};
use wry::{
    NewWindowResponse, PermissionResponse, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows, WebViewExtWindows,
};

const RGBA: &[u8] = include_bytes!("../assets/ia2.rgba");

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn message(detail: &str, error: bool) {
    // SAFETY: both UTF-16 strings stay alive for this synchronous dialog.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(detail).as_ptr(),
            wide("IA2").as_ptr(),
            MB_OK
                | if error {
                    MB_ICONERROR
                } else {
                    MB_ICONINFORMATION
                },
        );
    }
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    // SAFETY: writable output buffer of the specified length, system RNG.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(format!("Windows 随机数初始化失败：{status:#x}"));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this RAII owner closes its unique handle exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Instance {
    key: String,
    directory: PathBuf,
    _mutex: Handle,
    _activate: Handle,
    _shutdown: Handle,
}

fn instance_key(executable: &Path) -> String {
    // Stable per installation, independent of working directory or port.
    let mut hash = 0xcbf29ce484222325_u64;
    for unit in executable
        .as_os_str()
        .to_string_lossy()
        .to_lowercase()
        .encode_utf16()
    {
        for byte in unit.to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    format!("IA2-{hash:016x}")
}

fn object_name(key: &str, suffix: &str) -> Vec<u16> {
    // Global event/mutex objects bridge SSH/installer session 0 and the
    // interactive desktop; unlike file mappings, creating these does not
    // require SeCreateGlobalPrivilege. The per-user installation path scopes
    // the key; the default process-token DACL still controls access.
    wide(format!("Global\\{key}-{suffix}"))
}

fn open_event(key: &str, suffix: &str) -> Result<Handle, String> {
    // SAFETY: null-terminated name; returned handle is uniquely owned.
    let handle = unsafe {
        OpenEventW(
            EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            0,
            object_name(key, suffix).as_ptr(),
        )
    };
    if handle.is_null() {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(Handle(handle))
    }
}

fn signal(key: &str, suffix: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(event) = open_event(key, suffix) {
            // SAFETY: event is a live event handle opened with modify rights.
            if unsafe { SetEvent(event.0) } != 0 {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("IA2 正在启动或尚未响应，请稍后重试。".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

impl Instance {
    fn acquire(key: &str, data: &Path) -> Result<Option<Self>, String> {
        // SAFETY: default token DACL, no handle inheritance, terminated name.
        let mutex =
            unsafe { CreateMutexW(std::ptr::null(), 0, object_name(key, "instance").as_ptr()) };
        if mutex.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mutex = Handle(mutex);
        if existed {
            return Ok(None);
        }
        let create = |suffix| {
            // Auto-reset events retain one signal even before the waiter starts.
            let handle =
                unsafe { CreateEventW(std::ptr::null(), 0, 0, object_name(key, suffix).as_ptr()) };
            if handle.is_null() {
                Err(std::io::Error::last_os_error().to_string())
            } else {
                Ok(Handle(handle))
            }
        };
        let directory = data.join("instances").join(key);
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            key: key.into(),
            directory,
            _mutex: mutex,
            _activate: create("activate")?,
            _shutdown: create("shutdown")?,
        }))
    }

    fn watch(&self, proxy: EventLoopProxy<DesktopEvent>) -> Result<(), String> {
        let key = self.key.clone();
        thread::Builder::new()
            .name("ia2-desktop-ipc".into())
            .spawn(move || {
                // Open independent owning handles inside this thread; never wait on
                // handles that another thread can close underneath us.
                let (Ok(activate), Ok(shutdown)) =
                    (open_event(&key, "activate"), open_event(&key, "shutdown"))
                else {
                    return;
                };
                let handles = [activate.0, shutdown.0];
                loop {
                    let result =
                        unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX) };
                    let event = match result {
                        WAIT_OBJECT_0 => DesktopEvent::Activate,
                        value if value == WAIT_OBJECT_0 + 1 => DesktopEvent::ShutdownCommand,
                        _ => break,
                    };
                    if proxy.send_event(event).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn shutdown_existing(key: &str, data: &Path) -> Result<(), String> {
    let handle = unsafe {
        OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            0,
            object_name(key, "instance").as_ptr(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            println!("{{\"ok\":true,\"status\":\"not_running\"}}");
            return Ok(());
        }
        return Err(format!(
            "无法确认 IA2 后台状态：{error}。没有强制终止控制器。"
        ));
    }
    let _handle = Handle(handle);
    let directory = data.join("instances").join(key);
    let id = random_id()?;
    // Cleanup runs on success, refusal, signalling failure and timeout.
    let _request = PendingShutdownRequest::publish(&directory, &id).map_err(|e| e.to_string())?;
    signal(key, "shutdown")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(bytes) = fs::read(directory.join(format!("shutdown-{id}.json"))) {
            if let Ok(result) = serde_json::from_slice::<ShutdownResponse>(&bytes) {
                if result.id == id {
                    let _ = fs::remove_file(directory.join(format!("shutdown-{id}.json")));
                    if result.ok {
                        println!("{{\"ok\":true,\"status\":\"stopped\"}}");
                        return Ok(());
                    }
                    return Err(result.detail);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("后台退出未获确认；没有强制终止控制器。".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

struct Backend {
    child: Child,
    origin: Origin,
    token: String,
    log: PathBuf,
    startup_error: Option<String>,
}

fn append_log(file: &Path, detail: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(file, "{detail}");
    }
}

fn start_backend(layout: &Layout, port: u16, log_dir: &Path) -> Result<Backend, String> {
    layout.validate()?;
    // Fail explicitly instead of attaching to an arbitrary existing listener.
    // This is only an early diagnostic: the server's own bind remains the
    // authority if another process races this check.
    let origin = Origin::new(port);
    let check = std::net::TcpListener::bind(origin.address()).map_err(|e| format!("本机端口 {port} 已被占用或不可用：{e}。请先退出原有 IA2 后台，或使用 IA2.exe --port 其他端口。"))?;
    drop(check);
    let token = random_id()?;
    fs::create_dir_all(log_dir).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let log = log_dir.join(format!("server-{stamp}.log"));
    let stderr = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log)
        .map_err(|e| e.to_string())?;
    let mut child = Command::new(&layout.server)
        .current_dir(&layout.root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--static-dir")
        .arg(&layout.web)
        .arg("--library-dir")
        .arg(&layout.library)
        .env("IA2_DESKTOP_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("无法启动 {}：{e}", layout.server.display()))?;
    let stdout = child.stdout.take().ok_or("无法读取后台启动状态")?;
    let (sender, receiver) = mpsc::channel();
    let log_copy = log.clone();
    let expected_origin = origin.clone();
    let expected_token = token.clone();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.starts_with("IA2_READY=") => {
                    // The readiness token is never written to the log.
                    let _ = sender.send(parse_ready(&line, &expected_origin, &expected_token));
                }
                Ok(line) => append_log(&log_copy, &line),
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    let startup_error = match receiver.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(true)) => None,
        result => {
            let state = child
                .try_wait()
                .ok()
                .flatten()
                .map(|status| status.to_string())
                .unwrap_or_else(|| "后台状态未知，未强制终止".into());
            Some(format!(
                "IA2 后台未完成启动：{result:?}（{state}）。\n日志：{}",
                log.display()
            ))
        }
    };
    // Retain even a not-ready process so a user can request its protected
    // shutdown. Never discard ownership and leave an unmanageable server.
    Ok(Backend {
        child,
        origin,
        token,
        log,
        startup_error,
    })
}

enum DesktopEvent {
    Activate,
    OpenWindow(String),
    External(String),
    Theme(WindowId, DesktopTheme),
    BackendReady(Result<Backend, String>),
    Exit(Option<String>),
    ShutdownCommand,
    ShutdownResult(Option<String>, Result<(), String>),
}

struct View {
    // WebView must be released before its parent native window.
    webview: WebView,
    window: Window,
    theme: DesktopTheme,
}

impl View {
    fn set_theme(&mut self, theme: DesktopTheme) -> Result<(), wry::Error> {
        self.theme = theme;
        let (native, browser) = match theme {
            DesktopTheme::Light => (Theme::Light, wry::Theme::Light),
            DesktopTheme::Dark => (Theme::Dark, wry::Theme::Dark),
        };
        self.window.set_theme(Some(native));
        self.window.set_background_color(Some(theme.background()));
        self.webview.set_background_color(theme.background())?;
        self.webview.set_theme(browser)
    }

    fn status(&self, title: &str, detail: &str) {
        let _ = self
            .webview
            .load_html(&status_page(title, detail, self.theme));
    }
}

fn make_view(
    target: &EventLoopWindowTarget<DesktopEvent>,
    context: &mut WebContext,
    proxy: EventLoopProxy<DesktopEvent>,
    origin: &Origin,
    url: Option<&str>,
) -> Result<View, String> {
    let window = WindowBuilder::new()
        .with_title("IA2")
        .with_theme(Some(Theme::Light))
        .with_background_color(DesktopTheme::Light.background())
        .with_inner_size(tao::dpi::LogicalSize::new(1360., 900.))
        .with_min_inner_size(tao::dpi::LogicalSize::new(960., 600.))
        .with_window_icon(Some(
            Icon::from_rgba(RGBA.to_vec(), 32, 32).map_err(|e| e.to_string())?,
        ))
        .with_visible(false)
        .build(target)
        .map_err(|e| e.to_string())?;
    let navigation_origin = origin.clone();
    let navigation_proxy = proxy.clone();
    let popup_origin = origin.clone();
    let theme_origin = origin.clone();
    let theme_proxy = proxy.clone();
    let window_id = window.id();
    let builder = WebViewBuilder::new_with_web_context(context)
        .with_focused(false)
        .with_background_color(DesktopTheme::Light.background())
        .with_theme(wry::Theme::Light)
        .with_initialization_script(include_str!("../assets/theme.js"))
        .with_ipc_handler(move |request| {
            if let Some(theme) =
                page_theme(&theme_origin, &request.uri().to_string(), request.body())
            {
                let _ = theme_proxy.send_event(DesktopEvent::Theme(window_id, theme));
            }
        })
        .with_devtools(cfg!(debug_assertions))
        .with_clipboard(true)
        .with_permission_handler(|_| PermissionResponse::Deny)
        .with_navigation_handler(move |url| {
            if url == "about:blank" || navigation_origin.allows(&url) {
                return true;
            }
            if external_url(&url) {
                let _ = navigation_proxy.send_event(DesktopEvent::External(url));
            }
            false
        })
        .with_new_window_req_handler(move |url, _features| {
            if popup_origin.allows(&url) {
                let _ = proxy.send_event(DesktopEvent::OpenWindow(url));
            } else if external_url(&url) {
                let _ = proxy.send_event(DesktopEvent::External(url));
            }
            NewWindowResponse::Deny
        });
    let builder = if let Some(url) = url {
        builder.with_url(url)
    } else {
        builder.with_html(status_page(
            "正在启动",
            "正在准备本机控制器和工作区。\n关闭窗口后，后台将继续运行；可从系统托盘重新打开 IA2。",
            DesktopTheme::Light,
        ))
    };
    let webview = builder
        .build(&window)
        .map_err(|e| format!("WebView2 窗口初始化失败：{e}"))?;
    window.set_visible(true);
    window.set_focus();
    Ok(View {
        webview,
        window,
        theme: DesktopTheme::Light,
    })
}

fn show(view: &View) {
    view.window.set_visible(true);
    view.window.set_minimized(false);
    view.window.set_focus();
}

fn external(candidate: &str) {
    if !external_url(candidate) {
        return;
    }
    let Ok(url) = url::Url::parse(candidate) else {
        return;
    };
    // Only validated http(s) URLs, never shell commands or file/custom schemes.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(url.as_str()).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn respond(directory: &Path, id: Option<&str>, result: Result<(), &str>) {
    if let Some(id) = id {
        let response = ShutdownResponse {
            id: id.into(),
            ok: result.is_ok(),
            detail: result.err().unwrap_or("后台已退出").into(),
        };
        if let Ok(json) = serde_json::to_vec(&response) {
            let _ = fs::write(directory.join(format!("shutdown-{id}.json")), json);
        }
    }
}

fn launch(executable: PathBuf, data: PathBuf, port: u16) -> Result<(), String> {
    let key = instance_key(&executable);
    let Some(instance) = Instance::acquire(&key, &data)? else {
        return signal(&key, "activate");
    };
    let version = wry::webview_version().map_err(|e| format!("缺少或无法启动 Microsoft Edge WebView2 Runtime：{e}\n请安装运行时后重新打开 IA2：\n{WEBVIEW_INSTALL}"))?;
    let layout = Layout::from_executable(&executable)?;
    layout.validate()?;
    let log_dir = data.join("logs");
    fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    append_log(
        &log_dir.join("desktop.log"),
        &format!(
            "IA2 {} starting; WebView2 {version}; install {}",
            env!("CARGO_PKG_VERSION"),
            layout.root.display()
        ),
    );
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(wide("IA2").as_ptr());
    }
    let event_loop = EventLoopBuilder::<DesktopEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let profile = data.join("WebView2").join(&key);
    fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let mut context = WebContext::new(Some(profile));
    let origin = Origin::new(port);
    let primary = make_view(&event_loop, &mut context, proxy.clone(), &origin, None)?;
    let primary_id = primary.window.id();
    let mut views = HashMap::from([(primary_id, primary)]);
    let open_item = MenuItem::new("打开 IA2", true, None);
    let exit_item = MenuItem::new("退出 IA2", true, None);
    let open_id = open_item.id().clone();
    let exit_id = exit_item.id().clone();
    let menu = Menu::new();
    menu.append_items(&[&open_item, &exit_item])
        .map_err(|e| e.to_string())?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("IA2 — 关闭窗口后控制器继续运行")
        .with_icon(tray_icon::Icon::from_rgba(RGBA.to_vec(), 32, 32).map_err(|e| e.to_string())?)
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| e.to_string())?;
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            let _ = menu_proxy.send_event(DesktopEvent::Activate);
        } else if event.id == exit_id {
            let _ = menu_proxy.send_event(DesktopEvent::Exit(None));
        }
    }));
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(event, TrayIconEvent::DoubleClick { .. }) {
            let _ = tray_proxy.send_event(DesktopEvent::Activate);
        }
    }));
    instance.watch(proxy.clone())?;
    let startup_proxy = proxy.clone();
    thread::spawn(move || {
        let _ = startup_proxy.send_event(DesktopEvent::BackendReady(start_backend(
            &layout, port, &log_dir,
        )));
    });
    let mut backend: Option<Backend> = None;
    let mut startup_pending = true;
    let mut shutdown_pending = false;
    let mut shutdown_wait: Option<(Option<String>, Instant)> = None;
    let mut close_notice = !data.join("close-notice-shown").is_file();
    event_loop.run(move |event, target, control| {
        // Retain native resources until the event loop exits.
        let _keep_alive = (&instance, &tray);
        *control = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250));
        match event {
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
                if window_id == primary_id {
                    if close_notice {
                        message("IA2 已收起到系统托盘，后台控制器继续运行。\n双击 IA2 或使用托盘“打开 IA2”可恢复窗口。\n需要退出时，请先停止控制器，再选择托盘“退出 IA2”。", false);
                        close_notice = false;
                        let _ = fs::write(data.join("close-notice-shown"), b"1");
                    }
                    if let Some(view) = views.get(&primary_id) { view.window.set_visible(false); }
                } else { views.remove(&window_id); }
            }
            Event::UserEvent(DesktopEvent::Activate) => { if let Some(view) = views.get(&primary_id) { show(view); } }
            Event::UserEvent(DesktopEvent::OpenWindow(url)) => {
                match make_view(target, &mut context, proxy.clone(), &origin, Some(&url)) {
                    Ok(view) => { views.insert(view.window.id(), view); }
                    Err(error) => message(&error, true),
                }
            }
            Event::UserEvent(DesktopEvent::External(url)) => external(&url),
            Event::UserEvent(DesktopEvent::Theme(window_id, theme)) => {
                if let Some(view) = views.get_mut(&window_id) {
                    if let Err(error) = view.set_theme(theme) {
                        append_log(&data.join("logs/desktop.log"), &format!("Cannot update desktop theme: {error}"));
                    }
                }
            }
            Event::UserEvent(DesktopEvent::BackendReady(result)) => {
                startup_pending = false;
                match result {
                    Ok(mut ready) => {
                        if let Some(view) = views.get(&primary_id) {
                            if let Some(error) = ready.startup_error.take() {
                                view.status("后台未能启动", &error);
                                show(view);
                                message(&error, true);
                            } else if let Err(error) = view.webview.load_url(ready.origin.url()) { message(&format!("无法打开工作区：{error}"), true); }
                        }
                        backend = Some(ready);
                    }
                    Err(error) => {
                        if let Some(view) = views.get(&primary_id) { view.status("后台未能启动", &error); show(view); }
                        message(&error, true);
                    }
                }
            }
            Event::UserEvent(DesktopEvent::ShutdownCommand) => {
                match take_shutdown_requests(&instance.directory) {
                    Ok(requests) => {
                        if requests.is_empty() { return; }
                        if !startup_pending && !shutdown_pending && backend.is_none() {
                            // No backend exists; reply to the entire batch before
                            // exiting, rather than abandoning later queued clients.
                            for request in requests { respond(&instance.directory, Some(&request.id), Ok(())); }
                            *control = ControlFlow::Exit;
                        } else {
                            for request in requests { let _ = proxy.send_event(DesktopEvent::Exit(Some(request.id))); }
                        }
                    }
                    Err(error) => append_log(&data.join("logs/desktop.log"), &format!("Cannot read desktop shutdown requests: {error}")),
                }
            }
            Event::UserEvent(DesktopEvent::Exit(id)) => {
                if startup_pending || shutdown_pending {
                    respond(&instance.directory, id.as_deref(), Err("IA2 正在启动或退出，请稍后重试。"));
                    if id.is_none() { message("IA2 正在启动或退出，请稍后重试。", false); }
                } else if let Some(backend) = &backend {
                    shutdown_pending = true;
                    let (origin, token, proxy) = (backend.origin.clone(), backend.token.clone(), proxy.clone());
                    thread::spawn(move || {
                        let result = request(&origin, "POST", "/api/desktop/shutdown", Some(&token)).and_then(|(code, body)| match code {
                            202 => Ok(()),
                            409 => Err("控制器仍在运行，IA2 保持开启。请先在工作区停止运行，再退出。".into()),
                            _ => Err(format!("后台拒绝退出（HTTP {code}）：{body}\n未强制终止控制器。")),
                        });
                        let _ = proxy.send_event(DesktopEvent::ShutdownResult(id, result));
                    });
                } else {
                    respond(&instance.directory, id.as_deref(), Ok(()));
                    *control = ControlFlow::Exit;
                }
            }
            Event::UserEvent(DesktopEvent::ShutdownResult(id, result)) => match result {
                Ok(()) => shutdown_wait = Some((id, Instant::now() + Duration::from_secs(15))),
                Err(error) => {
                    shutdown_pending = false;
                    respond(&instance.directory, id.as_deref(), Err(&error));
                    if id.is_none() { if let Some(view) = views.get(&primary_id) { show(view); } message(&error, true); }
                }
            },
            Event::MainEventsCleared => {
                if let Some(active) = &mut backend {
                    if let Ok(Some(status)) = active.child.try_wait() {
                        // HTTP 202 and child exit can race the worker's event.
                        // Wait for the shutdown response before classifying it.
                        if shutdown_pending && shutdown_wait.is_none() { return; }
                        if let Some((id, _)) = shutdown_wait.take() {
                            respond(&instance.directory, id.as_deref(), Ok(()));
                            *control = ControlFlow::Exit;
                        } else {
                            let error = format!("后台已退出（{status}）。请检查设备状态。\n日志：{}\n请从托盘退出 IA2 后重新打开。", active.log.display());
                            for view in views.values() { view.status("后台连接已中断", &error); }
                            if let Some(view) = views.get(&primary_id) { show(view); }
                            message(&error, true);
                        }
                        backend = None;
                    }
                }
                if shutdown_wait.as_ref().is_some_and(|(_, deadline)| Instant::now() >= *deadline) {
                    let (id, _) = shutdown_wait.take().unwrap();
                    let error = "后台退出尚未确认。IA2 没有强制终止控制器。";
                    respond(&instance.directory, id.as_deref(), Err(error));
                    if id.is_none() { message(error, true); }
                    shutdown_pending = false;
                }
            }
            _ => {}
        }
    });
}

pub fn entry() -> i32 {
    let mode = match arguments(std::env::args().skip(1)) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("{error}");
            message(&error, true);
            return 2;
        }
    };
    match mode {
        Mode::Help => {
            println!("{HELP}");
            return 0;
        }
        Mode::Version => {
            println!("IA2 {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Mode::CheckRuntime => {
            return match wry::webview_version() {
                Ok(version) => {
                    println!(
                        "{}",
                        serde_json::json!({"ok":true,"webview2_version":version})
                    );
                    0
                }
                Err(error) => {
                    eprintln!("Microsoft Edge WebView2 Runtime is unavailable: {error}\nInstall: {WEBVIEW_INSTALL}");
                    3
                }
            };
        }
        _ => {}
    }
    let result = (|| {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?;
        let data = PathBuf::from(local).join("IA2Desktop");
        match mode {
            Mode::Launch(port) => launch(executable, data, port),
            Mode::Shutdown => shutdown_existing(&instance_key(&executable), &data),
            _ => unreachable!(),
        }
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            if mode != Mode::Shutdown {
                message(&error, true);
            }
            3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_lock_excludes_second_owner_and_releases_on_drop() {
        let data = tempfile::tempdir().unwrap();
        let key = format!("IA2-test-{}", random_id().unwrap());
        let first = Instance::acquire(&key, data.path()).unwrap().unwrap();
        assert!(Instance::acquire(&key, data.path()).unwrap().is_none());
        signal(&key, "activate").unwrap();
        drop(first);
        assert!(Instance::acquire(&key, data.path()).unwrap().is_some());
    }

    #[test]
    fn installation_identity_is_case_insensitive_and_keeps_unicode() {
        assert_eq!(
            instance_key(Path::new(r"C:\Users\测试\IA2\bin\IA2.exe")),
            instance_key(Path::new(r"c:\users\测试\ia2\BIN\ia2.EXE"))
        );
        assert_ne!(
            instance_key(Path::new(r"C:\Users\测试\IA2\bin\IA2.exe")),
            instance_key(Path::new(r"C:\Users\其他\IA2\bin\IA2.exe"))
        );
    }

    #[test]
    fn desktop_tokens_are_random_64_hex_bytes() {
        let first = random_id().unwrap();
        let second = random_id().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
