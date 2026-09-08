use crate::{
    config::Config,
    hardware::{self, HardwareInfo},
    render::{Cell, Renderer, RendererStats},
    taskbar_host::{self, TaskbarHost},
    telemetry::{Metric, MetricState, Snapshot, Telemetry},
};
#[path = "widget_stats.rs"]
mod widget_stats;
use std::{
    mem::size_of,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};
use widget_stats::WidgetStats;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, LibraryLoader::GetModuleHandleW, Registry::*, Threading::*},
        UI::{HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, Result, w},
};

const CLASS: PCWSTR = w!("RustTaskbarMonitor.Widget.1");
const CONTROL_CLASS: PCWSTR = w!("RustTaskbarMonitor.Controller.1");
const DATA_READY: u32 = WM_APP + 1;
const TRAY_EVENT: u32 = WM_APP + 2;
const REATTACH: u32 = WM_APP + 3;
const REPORT_NOW: u32 = WM_APP + 4;
const LABELS: [&str; 6] = ["CPU", "RAM", "GPU", "DISK", "NPU", "FAN"];

struct App {
    config: Config,
    telemetry: Telemetry,
    renderer: Option<Renderer>,
    light: bool,
    taskbar_created: u32,
    last_rect: RECT,
    visible: bool,
    dragging: Option<(i32, i32)>,
    start: Instant,
    end_after: Option<Duration>,
    report: Option<String>,
    paints: u64,
    draw_errors: u64,
    last_cpu_sample: Option<Instant>,
    diagnostics: Option<WidgetStats>,
    save_error: bool,
    menu_open: bool,
    last_cells: Vec<Cell>,
    inspect: bool,
    visible_ticks: u64,
    hidden_ticks: u64,
    hardware: HardwareInfo,
    hardware_rx: Option<Receiver<HardwareInfo>>,
    hardware_reload: bool,
    controller: HWND,
    widget: HWND,
    host: Option<TaskbarHost>,
    shutting_down: bool,
    attach_count: u64,
    attach_error: String,
    last_verified_host: serde_json::Value,
    retired_renderer_stats: RendererStats,
    last_software_fallback: Option<bool>,
}

impl App {
    fn retire_renderer(&mut self) {
        if let Some(renderer) = self.renderer.take() {
            self.retired_renderer_stats.accumulate(renderer.stats());
            self.last_software_fallback = Some(renderer.software_fallback());
        }
    }
}

pub fn run(args: &[String]) -> Result<()> {
    unsafe {
        let target = parse_control_target(args)
            .map_err(|message| windows::core::Error::new(E_INVALIDARG, message))?;
        for (argument, message) in [("--reattach", REATTACH), ("--report-now", REPORT_NOW)] {
            if args.iter().any(|s| s == argument) {
                match FindWindowW(CONTROL_CLASS, PCWSTR::null()) {
                    Ok(hwnd) => {
                        if let Some(target) = target {
                            verify_control_target(hwnd, target)?;
                            // The receiver checks the same identity again. HWND/PID reuse
                            // between this check and delivery cannot redirect the request.
                            PostMessageW(
                                Some(hwnd),
                                message,
                                WPARAM(target.process_id as usize),
                                LPARAM(target.created_100ns as isize),
                            )?;
                        } else {
                            PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0))?;
                        }
                    }
                    Err(error) if target.is_some() => return Err(error),
                    Err(_) => {}
                }
                return Ok(());
            }
        }
        if args.iter().any(|s| s == "--quit") {
            if let Ok(hwnd) = FindWindowW(CONTROL_CLASS, PCWSTR::null())
                .or_else(|_| FindWindowW(CLASS, PCWSTR::null()))
            {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            return Ok(());
        }
        let guard = CreateMutexW(None, false, w!("Local\\RustTaskbarMonitor.Widget.1"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(guard);
            return Ok(());
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let module = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let controller_class = WNDCLASSW {
            lpszClassName: CONTROL_CLASS,
            ..wc
        };
        if RegisterClassW(&controller_class) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let config = Config::load();
        let light = is_light(&config.theme);
        let start = Instant::now();
        let report = arg(args, "--report");
        let diagnostics = report.as_ref().map(|_| WidgetStats::new(start));
        let mut app = Box::new(App {
            config,
            telemetry: Telemetry::start(),
            renderer: None,
            light,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            last_rect: RECT::default(),
            visible: false,
            dragging: None,
            start,
            end_after: arg(args, "--seconds")
                .and_then(|s| s.parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.clamp(1, 86_400))),
            report,
            paints: 0,
            draw_errors: 0,
            last_cpu_sample: None,
            diagnostics,
            save_error: false,
            menu_open: false,
            last_cells: Vec::new(),
            inspect: args.iter().any(|s| s == "--inspect"),
            visible_ticks: 0,
            hidden_ticks: 0,
            hardware: HardwareInfo::default(),
            hardware_rx: None,
            hardware_reload: false,
            controller: HWND::default(),
            widget: HWND::default(),
            host: None,
            shutting_down: false,
            attach_count: 0,
            attach_error: String::new(),
            last_verified_host: serde_json::Value::Null,
            retired_renderer_stats: RendererStats::default(),
            last_software_fallback: None,
        });
        if let Some(stats) = &mut app.diagnostics {
            stats.sample_resources(true);
        }
        refresh_hardware(&mut app);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            CONTROL_CLASS,
            w!("Taskbar Monitor Controller"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            Some((&mut *app as *mut App).cast()),
        )?;
        app.controller = hwnd;
        let raw = hwnd.0 as usize;
        app.telemetry.set_notify(move || {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), DATA_READY, WPARAM(0), LPARAM(0));
        });
        tray(hwnd, NIM_ADD);
        maintain_attachment(&mut *app);
        SetTimer(Some(hwnd), 1, 500, None);
        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0).0;
            if result <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !app.shutting_down {
            write_report(&mut app, "message_loop_exited");
        }
        drop(app);
        CoUninitialize();
        let _ = CloseHandle(guard);
        Ok(())
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2).find(|s| s[0] == key).map(|s| s[1].clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControlTarget {
    process_id: u32,
    created_100ns: u64,
}

fn parse_control_target(
    args: &[String],
) -> std::result::Result<Option<ControlTarget>, &'static str> {
    let keys = ["--target-pid", "--target-start-time"];
    let present = keys.map(|key| {
        args.iter()
            .filter(|argument| argument.as_str() == key)
            .count()
    });
    if present == [0, 0] {
        return Ok(None);
    }
    if present != [1, 1]
        || args
            .iter()
            .filter(|argument| matches!(argument.as_str(), "--reattach" | "--report-now"))
            .count()
            != 1
        || args.iter().any(|argument| argument == "--quit")
    {
        return Err(
            "Target identity requires one control command and exactly one PID/start-time pair",
        );
    }
    let number = |key| -> std::result::Result<u64, &'static str> {
        let value = arg(args, key).ok_or("Missing target identity value")?;
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Target identity must contain positive decimal integers");
        }
        value
            .parse::<u64>()
            .map_err(|_| "Target identity is outside its integer range")
    };
    let process_id = number(keys[0])?;
    let created_100ns = number(keys[1])?;
    if process_id == 0
        || process_id > u64::from(u32::MAX)
        || created_100ns == 0
        || created_100ns > isize::MAX as u64
    {
        return Err("Target identity is outside the supported Windows x64 range");
    }
    Ok(Some(ControlTarget {
        process_id: process_id as u32,
        created_100ns,
    }))
}

unsafe fn process_creation_time(process: HANDLE) -> Result<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user)?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

unsafe fn verify_control_target(hwnd: HWND, target: ControlTarget) -> Result<()> {
    let mut process_id = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    if process_id != target.process_id {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            "Controller belongs to a different process",
        ));
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)?;
    let created = process_creation_time(process);
    let _ = CloseHandle(process);
    if created? != target.created_100ns {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            "Controller process creation time differs",
        ));
    }
    Ok(())
}

fn control_message_matches(
    current: ControlTarget,
    process_id: usize,
    created_100ns: isize,
) -> bool {
    (process_id == 0 && created_100ns == 0)
        || (process_id == current.process_id as usize
            && created_100ns > 0
            && created_100ns as u64 == current.created_100ns)
}

unsafe fn accept_control_message(wp: WPARAM, lp: LPARAM) -> bool {
    if wp.0 == 0 && lp.0 == 0 {
        return true;
    }
    process_creation_time(GetCurrentProcess()).is_ok_and(|created_100ns| {
        control_message_matches(
            ControlTarget {
                process_id: GetCurrentProcessId(),
                created_100ns,
            },
            wp.0,
            lp.0,
        )
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    // A popup menu runs a nested message loop. Do not retain any reference into App across it.
    if msg == WM_CONTEXTMENU
        || (msg == TRAY_EVENT && (lp.0 as u32 == WM_RBUTTONUP || lp.0 as u32 == WM_LBUTTONUP))
    {
        open_menu((*ptr).controller, ptr);
        return LRESULT(0);
    }
    if msg == WM_SIZE && hwnd == (*ptr).widget {
        if let Some(renderer) = (*ptr).renderer.as_mut() {
            let width = (lp.0 as u32) & 0xffff;
            let height = (lp.0 as u32 >> 16) & 0xffff;
            if width > 0 && height > 0 {
                if let Err(error) = renderer.resize(width, height) {
                    retire_renderer(ptr);
                    (*ptr).draw_errors += 1;
                    if let Some(stats) = &mut (*ptr).diagnostics {
                        stats.record_error("resize", &error);
                    }
                }
            }
        }
        return LRESULT(0);
    }
    match msg {
        REATTACH => {
            if !accept_control_message(wp, lp) {
                return LRESULT(0);
            }
            if !(*ptr).menu_open {
                discard_widget(ptr);
                maintain_attachment(ptr);
            }
            LRESULT(0)
        }
        REPORT_NOW => {
            if !accept_control_message(wp, lp) {
                return LRESULT(0);
            }
            write_report(&mut *ptr, "on_demand");
            LRESULT(0)
        }
        WM_PAINT if hwnd == (*ptr).widget => {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut ps);
            let app = &mut *ptr;
            let snapshot = app.telemetry.snapshot();
            let cells = cells(&snapshot, &app.config, &app.hardware);
            if app.renderer.is_none() {
                match Renderer::new(hwnd) {
                    Ok(renderer) => {
                        app.renderer = Some(renderer);
                        if let Some(stats) = &mut app.diagnostics {
                            stats.renderer_created();
                        }
                    }
                    Err(error) => {
                        if let Some(stats) = &mut app.diagnostics {
                            stats.record_error("renderer_init", &error);
                        }
                    }
                }
            }
            let drawn = if let Some(renderer) = app.renderer.as_mut() {
                match renderer.draw(&cells, app.light, &app.config.style) {
                    Ok(()) => true,
                    Err(error) => {
                        app.retire_renderer();
                        if let Some(stats) = &mut app.diagnostics {
                            stats.record_error("paint", &error);
                        }
                        false
                    }
                }
            } else {
                false
            };
            app.paints += 1;
            if drawn {
                app.last_cells = cells;
            } else {
                app.draw_errors += 1;
            }
            if let Some(stats) = &mut app.diagnostics {
                stats.paint_finished(drawn);
            }
            if app.diagnostics.is_some()
                && drawn
                && snapshot.cpu.state == MetricState::Ready
                && app.last_cpu_sample != Some(snapshot.cpu.sampled_at)
            {
                app.last_cpu_sample = Some(snapshot.cpu.sampled_at);
                if let Some(stats) = &mut app.diagnostics {
                    stats.record_latency(snapshot.cpu.sampled_at, Instant::now());
                }
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        DATA_READY => {
            redraw_widget(ptr);
            LRESULT(0)
        }
        WM_TIMER => {
            poll_hardware(&mut *ptr);
            if !(*ptr).menu_open {
                maintain_attachment(ptr);
            }
            if (*ptr).visible {
                (*ptr).visible_ticks += 1;
            } else {
                (*ptr).hidden_ticks += 1;
            }
            if let Some(stats) = &mut (*ptr).diagnostics {
                stats.sample_resources(false);
            }
            if (*ptr)
                .end_after
                .is_some_and(|d| (*ptr).start.elapsed() >= d)
            {
                let _ = PostMessageW(Some((*ptr).controller), WM_CLOSE, WPARAM(0), LPARAM(0));
            } else {
                // Also redraw stalled collectors so stale state becomes visible.
                if (*ptr).visible
                    && ((*ptr).renderer.is_none()
                        || cells(
                            &(*ptr).telemetry.snapshot(),
                            &(*ptr).config,
                            &(*ptr).hardware,
                        ) != (*ptr).last_cells)
                {
                    redraw_widget(ptr);
                }
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            (*ptr).light = is_light(&(*ptr).config.theme);
            redraw_widget(ptr);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_DPICHANGED_AFTERPARENT => {
            (*ptr).last_rect = RECT::default();
            // Reposition on the timer to avoid holding App across nested window messages.
            let _ = PostMessageW(Some((*ptr).controller), WM_TIMER, WPARAM(1), LPARAM(0));
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            // PBT_APMRESUMECRITICAL / PBT_APMRESUMESUSPEND / PBT_APMRESUMEAUTOMATIC
            if [6, 7, 18].contains(&wp.0) {
                (*ptr).telemetry.reset_baselines();
            }
            LRESULT(1)
        }
        WM_DEVICECHANGE => {
            (*ptr).telemetry.reset_baselines();
            refresh_hardware(&mut *ptr);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            (*ptr).dragging = Some((cursor.x, (*ptr).config.offset_dip));
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some((start, offset)) = (*ptr).dragging {
                let mut cursor = POINT::default();
                let _ = GetCursorPos(&mut cursor);
                let scale = GetDpiForWindow(hwnd).max(96) as f64 / 96.0;
                (*ptr).config.offset_dip =
                    (offset + ((cursor.x - start) as f64 / scale).round() as i32).max(0);
                maintain_attachment(ptr);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if (*ptr).dragging.take().is_some() {
                let _ = ReleaseCapture();
                save(ptr);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            (*ptr).dragging = None;
            LRESULT(0)
        }
        WM_CLOSE => {
            if hwnd == (*ptr).controller {
                // Preserve live HWND/resource state before releasing the renderer.
                write_report(&mut *ptr, "before_shutdown");
                // Clear the callback while its controller HWND still belongs to us.
                (*ptr).telemetry.request_stop();
                (*ptr).shutting_down = true;
                discard_widget(ptr);
                let _ = DestroyWindow(hwnd);
            } else {
                let _ = PostMessageW(Some((*ptr).controller), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if hwnd == (*ptr).controller {
                (*ptr).telemetry.set_notify(|| {});
                tray(hwnd, NIM_DELETE);
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
            } else if hwnd == (*ptr).widget {
                // Explorer may destroy its child. The controller and collectors survive.
                if let Some(stats) = &mut (*ptr).diagnostics {
                    stats.external_widget_destroyed();
                }
                retire_renderer(ptr);
                (*ptr).widget = HWND::default();
                (*ptr).dragging = None;
                (*ptr).host = None;
                (*ptr).visible = false;
                (*ptr).last_rect = RECT::default();
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ if msg == (*ptr).taskbar_created => {
            tray((*ptr).controller, NIM_ADD);
            (*ptr).last_rect = RECT::default();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn redraw_widget(ptr: *mut App) {
    if !(*ptr).widget.0.is_null() {
        let _ = InvalidateRect(Some((*ptr).widget), None, false);
    }
}

unsafe fn retire_renderer(ptr: *mut App) {
    (*ptr).retire_renderer();
}
unsafe fn discard_widget(ptr: *mut App) {
    let old = (*ptr).widget;
    // Release DirectComposition before destroying its own HWND. No Rust borrow
    // into App may survive DestroyWindow's synchronous nested messages.
    retire_renderer(ptr);
    (*ptr).widget = HWND::default();
    (*ptr).dragging = None;
    (*ptr).host = None;
    (*ptr).visible = false;
    (*ptr).last_rect = RECT::default();
    (*ptr).last_cells.clear();
    if !old.0.is_null() && IsWindow(Some(old)).as_bool() {
        let _ = DestroyWindow(old);
    }
}

unsafe fn maintain_attachment(ptr: *mut App) {
    if (*ptr).shutting_down {
        return;
    }
    let host = taskbar_host::find();
    let connected = host.is_some()
        && host == (*ptr).host
        && IsWindow(Some((*ptr).widget)).as_bool()
        && GetParent((*ptr).widget).ok() == host.map(|h| h.hwnd);
    if !connected {
        discard_widget(ptr);
        let Some(host) = host else {
            (*ptr).attach_error = "작업표시줄 연결 대기".into();
            return;
        };
        let Some(rect) = host.bounds((*ptr).config.width(), 40, (*ptr).config.offset_dip) else {
            (*ptr).attach_error = "가로 작업표시줄 영역 확인 대기".into();
            return;
        };
        let dpi_context = host.dpi_context();
        if dpi_context.0.is_null() {
            return;
        }
        let previous_dpi = SetThreadDpiAwarenessContext(dpi_context);
        if previous_dpi.0.is_null() {
            (*ptr).attach_error = "작업표시줄 DPI 설정을 맞추지 못했습니다".into();
            return;
        }
        if let Some(stats) = &mut (*ptr).diagnostics {
            stats.attach_attempted();
        }
        let created = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE | WS_EX_NOPARENTNOTIFY,
            CLASS,
            w!("Taskbar Monitor"),
            WS_CHILD | WS_CLIPSIBLINGS,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            Some(host.hwnd),
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            Some(ptr.cast()),
        );
        SetThreadDpiAwarenessContext(previous_dpi);
        let child = match created {
            Ok(child) => child,
            Err(error) => {
                if let Some(stats) = &mut (*ptr).diagnostics {
                    stats.record_error("attach", &error);
                }
                (*ptr).attach_error = format!("작업표시줄 연결 실패: {error}");
                return;
            }
        };
        if !host.is_current() || GetParent(child).ok() != Some(host.hwnd) {
            let _ = DestroyWindow(child);
            (*ptr).attach_error = "작업표시줄이 변경되어 다시 연결합니다".into();
            return;
        }
        (*ptr).widget = child;
        (*ptr).host = Some(host);
        match Renderer::new(child) {
            Ok(renderer) => {
                (*ptr).renderer = Some(renderer);
                if let Some(stats) = &mut (*ptr).diagnostics {
                    stats.renderer_created();
                }
            }
            Err(error) => {
                if let Some(stats) = &mut (*ptr).diagnostics {
                    stats.record_error("renderer_init", &error);
                }
                (*ptr).attach_error = format!("그리기 초기화 실패: {error}");
                discard_widget(ptr);
                return;
            }
        }
        (*ptr).attach_count += 1;
        (*ptr).attach_error.clear();
    }
    let Some(host) = (*ptr).host else {
        return;
    };
    let Some(rect) = host.bounds((*ptr).config.width(), 40, (*ptr).config.offset_dip) else {
        discard_widget(ptr);
        (*ptr).attach_error = "가로 작업표시줄 영역 확인 대기".into();
        return;
    };
    if rect != (*ptr).last_rect {
        // HWND_TOP affects siblings inside the taskbar only. There is no system
        // TOPMOST window or independent foreground/auto-hide policy anymore.
        if SetWindowPos(
            (*ptr).widget,
            Some(HWND_TOP),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        .is_ok()
        {
            (*ptr).last_rect = rect;
            redraw_widget(ptr);
        } else {
            (*ptr).attach_error = "작업표시줄 안의 배치를 다시 확인합니다".into();
            discard_widget(ptr);
            return;
        }
    }
    (*ptr).visible = IsWindowVisible((*ptr).widget).as_bool();
    // Native attachment checks above are always active. The detailed JSON is
    // diagnostic-only and would otherwise allocate on every maintenance tick.
    if (*ptr).report.is_some() {
        verify_host(ptr, host);
    }
}

unsafe fn verify_host(ptr: *mut App, host: TaskbarHost) {
    let child = (*ptr).widget;
    let parent = GetParent(child).unwrap_or_default();
    let root = GetAncestor(child, GA_ROOT);
    let style = GetWindowLongPtrW(child, GWL_STYLE) as u32;
    let extended = GetWindowLongPtrW(child, GWL_EXSTYLE) as u32;
    let mut child_rect = RECT::default();
    let mut parent_rect = RECT::default();
    let _ = GetWindowRect(child, &mut child_rect);
    let _ = GetWindowRect(host.hwnd, &mut parent_rect);
    (*ptr).last_verified_host = serde_json::json!({
        "observed_at_uptime_seconds":(*ptr).start.elapsed().as_secs_f64(),
        "child_hwnd":child.0 as usize,"parent_hwnd":parent.0 as usize,"taskbar_hwnd":host.hwnd.0 as usize,
        "parent_is_taskbar":parent==host.hwnd,"root_is_taskbar":root==host.hwnd,
        "child_process_id":taskbar_host::process_id(child),"host_process_id":host.process_id,
        "ws_child":style & WS_CHILD.0 != 0,"ws_popup":style & WS_POPUP.0 != 0,
        "ws_ex_topmost":extended & WS_EX_TOPMOST.0 != 0,
        "child_dpi":GetDpiForWindow(child),"host_dpi":GetDpiForWindow(host.hwnd),
        "dpi_contexts_equal":AreDpiAwarenessContextsEqual(GetWindowDpiAwarenessContext(child),host.dpi_context()).as_bool(),
        "child_screen_rect":[child_rect.left,child_rect.top,child_rect.right,child_rect.bottom],
        "host_screen_rect":[parent_rect.left,parent_rect.top,parent_rect.right,parent_rect.bottom]
    });
}

fn metrics(s: &Snapshot) -> [&Metric; 6] {
    [&s.cpu, &s.ram, &s.gpu, &s.disk, &s.npu, &s.fan]
}

fn refresh_hardware(app: &mut App) {
    // Disk numbers can be reused after a USB change; never keep an old model mapping.
    app.hardware.disks.clear();
    app.hardware.disk_detail = "연결된 디스크 모델을 확인 중입니다".into();
    if app.hardware_rx.is_some() {
        app.hardware_reload = true;
        return;
    }
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new()
        .name("hardware-inventory".into())
        .spawn(move || {
            let _ = tx.send(hardware::load());
        })
        .is_ok()
    {
        app.hardware_rx = Some(rx);
    }
}

fn poll_hardware(app: &mut App) {
    let result = app.hardware_rx.as_ref().map(Receiver::try_recv);
    match result {
        Some(Ok(info)) => {
            app.hardware_rx = None;
            if app.hardware_reload {
                app.hardware_reload = false;
                refresh_hardware(app);
            } else {
                app.hardware = info;
            }
        }
        Some(Err(TryRecvError::Disconnected)) => {
            app.hardware_rx = None;
            if app.hardware_reload {
                app.hardware_reload = false;
                refresh_hardware(app);
            }
        }
        _ => {}
    }
}

fn active_disk_index(detail: &str) -> Option<u32> {
    // Parse only our collector's exact prefix, never an arbitrary number in an error.
    let rest = detail.strip_prefix("가장 바쁜 물리 디스크 ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if !rest[digits.len()..].starts_with([' ', ':']) {
        return None;
    }
    digits.parse().ok()
}

fn hardware_text(snapshot: &Snapshot, h: &HardwareInfo, i: usize) -> (String, String) {
    match i {
        0 => (h.cpu.clone(), h.cpu_meta.clone()),
        1 => (h.ram.clone(), h.ram_meta.clone()),
        2 => (h.gpu.clone(), h.gpu_meta.clone()),
        3 => {
            let index = active_disk_index(&snapshot.disk.detail);
            if let Some(disk) = index.and_then(|n| h.disks.iter().find(|d| d.index == n)) {
                let size = if disk.capacity_bytes >= 1_000_000_000_000 {
                    format!("{:.1} TB", disk.capacity_bytes as f64 / 1e12)
                } else if disk.capacity_bytes > 0 {
                    format!("{:.0} GB", disk.capacity_bytes as f64 / 1e9)
                } else {
                    String::new()
                };
                (disk.name.clone(), size)
            } else {
                (
                    index
                        .map(|n| format!("Disk {n}"))
                        .unwrap_or_else(|| "물리 디스크".into()),
                    "모델 확인 중".into(),
                )
            }
        }
        4 => ("AI 가속기".into(), String::new()),
        _ => ("팬 속도".into(), String::new()),
    }
}

fn cells(snapshot: &Snapshot, config: &Config, hardware: &HardwareInfo) -> Vec<Cell> {
    metrics(snapshot)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| config.visible[*i])
        .map(|(i, m)| {
            let ready = m.state == MetricState::Ready;
            let (name, mut extra) = hardware_text(snapshot, hardware, i);
            let (value, unit) = if ready {
                match m.value.filter(|v| v.is_finite()) {
                    Some(v) => (format!("{v:.0}"), if i == 5 { "rpm" } else { "%" }.into()),
                    None => ("—".into(), String::new()),
                }
            } else {
                extra = match m.state {
                    MetricState::NotPresent => "장치 없음",
                    MetricState::Unsupported => "센서 미지원",
                    MetricState::WarmingUp => "준비 중",
                    MetricState::Stale => "갱신 지연",
                    _ => "조회 오류",
                }
                .into();
                ("—".into(), String::new())
            };
            Cell {
                label: LABELS[i].into(),
                value,
                unit,
                name,
                extra,
                progress: if ready && i != 5 {
                    m.value.map(|v| (v / 100.0) as f32)
                } else {
                    None
                },
                muted: !ready,
            }
        })
        .collect()
}

unsafe fn is_light(theme: &str) -> bool {
    if theme == "light" {
        return true;
    }
    if theme == "dark" {
        return false;
    }
    let mut value = 0u32;
    let mut len = 4u32;
    let status = RegGetValueW(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("SystemUsesLightTheme"),
        RRF_RT_REG_DWORD,
        None,
        Some((&mut value as *mut u32).cast()),
        Some(&mut len),
    );
    status == ERROR_SUCCESS && value != 0
}

unsafe fn tray(hwnd: HWND, action: NOTIFY_ICON_MESSAGE) {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_TIP | NIF_ICON,
        uCallbackMessage: TRAY_EVENT,
        ..Default::default()
    };
    data.hIcon = LoadIconW(
        GetModuleHandleW(None).ok().map(Into::into),
        PCWSTR(101usize as *const u16),
    )
    .unwrap_or_default();
    for (d, c) in data
        .szTip
        .iter_mut()
        .zip("Taskbar Monitor · 우클릭으로 설정".encode_utf16())
    {
        *d = c;
    }
    let _ = Shell_NotifyIconW(action, &data);
}

unsafe fn item(menu: HMENU, id: usize, label: &str, checked: bool, disabled: bool) {
    let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    if disabled {
        flags |= MF_GRAYED;
    }
    let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
}
unsafe fn separator(menu: HMENU) {
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
}
unsafe fn submenu(parent: HMENU, menu: HMENU, label: &str) {
    let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
    let _ = AppendMenuW(
        parent,
        MF_POPUP | MF_STRING,
        menu.0 as usize,
        PCWSTR(text.as_ptr()),
    );
}

unsafe fn open_menu(hwnd: HWND, ptr: *mut App) {
    if (*ptr).menu_open {
        return;
    }
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    {
        let app = &*ptr;
        let snapshot = app.telemetry.snapshot();
        item(
            menu,
            0,
            concat!("Taskbar Monitor  ", env!("CARGO_PKG_VERSION")),
            false,
            true,
        );
        if !app.attach_error.is_empty() {
            item(menu, 0, &app.attach_error.replace('&', "&&"), false, true);
        }
        for (i, m) in metrics(&snapshot).iter().enumerate() {
            let summary = if m.state == MetricState::Ready {
                format!(
                    "{}  {:.1} {}",
                    LABELS[i],
                    m.value.unwrap_or_default(),
                    if i == 5 { "RPM" } else { "%" }
                )
            } else {
                format!(
                    "{}  {}",
                    LABELS[i],
                    match m.state {
                        MetricState::NotPresent => "장치 없음",
                        MetricState::Unsupported => "센서 미지원",
                        MetricState::WarmingUp => "준비 중",
                        MetricState::Stale => "갱신 지연",
                        _ => "조회 오류",
                    }
                )
            };
            if let Ok(detail) = CreatePopupMenu() {
                let model_detail = match i {
                    0 => &app.hardware.cpu_detail,
                    1 => &app.hardware.ram_detail,
                    2 => &app.hardware.gpu_detail,
                    3 => &app.hardware.disk_detail,
                    _ => "",
                };
                for line in model_detail.lines() {
                    item(detail, 0, &line.replace('&', "&&"), false, true);
                }
                item(detail, 0, &m.detail.replace('&', "&&"), false, true);
                if i == 1 {
                    item(
                        detail,
                        0,
                        &format!(
                            "{:.2} / {:.2} GiB",
                            snapshot.memory_used_bytes as f64 / 1073741824.0,
                            snapshot.memory_total_bytes as f64 / 1073741824.0
                        ),
                        false,
                        true,
                    );
                }
                if i == 3 {
                    if let Some(disk) = active_disk_index(&snapshot.disk.detail)
                        .and_then(|index| app.hardware.disks.iter().find(|d| d.index == index))
                    {
                        item(
                            detail,
                            0,
                            &format!("현재 게이지: {}", disk.detail).replace('&', "&&"),
                            false,
                            true,
                        );
                    }
                    item(
                        detail,
                        0,
                        "DISK = 물리 디스크 중 가장 높은 활성 시간",
                        false,
                        true,
                    );
                }
                submenu(menu, detail, &summary);
            }
        }
        separator(menu);
        if let Ok(sub) = CreatePopupMenu() {
            for (i, label) in [
                "HUD · 원형 계기",
                "EVA · 분절 게이지",
                "Minimal · 얇은 원형",
            ]
            .iter()
            .enumerate()
            {
                item(
                    sub,
                    40 + i,
                    label,
                    app.config.style == ["hud", "eva", "minimal"][i],
                    false,
                );
            }
            submenu(menu, sub, "게이지 디자인");
        }
        if let Ok(sub) = CreatePopupMenu() {
            for (i, label) in ["Windows 테마", "어두운 테마", "밝은 테마"]
                .iter()
                .enumerate()
            {
                item(
                    sub,
                    10 + i,
                    label,
                    app.config.theme == ["auto", "dark", "light"][i],
                    false,
                );
            }
            submenu(menu, sub, "테마");
        }
        if let Ok(sub) = CreatePopupMenu() {
            for (i, label) in LABELS.iter().enumerate() {
                item(sub, 20 + i, label, app.config.visible[i], false);
            }
            submenu(menu, sub, "표시 항목");
        }
        if let Ok(sub) = CreatePopupMenu() {
            for (id, label) in [
                (31, "조금 왼쪽으로"),
                (32, "조금 오른쪽으로"),
                (33, "폭 줄이기"),
                (34, "폭 늘리기"),
                (35, "기본 배치로 되돌리기"),
            ] {
                item(sub, id, label, false, false);
            }
            item(sub, 0, "왼쪽 버튼으로 끌어서 위치 이동", false, true);
            submenu(menu, sub, "위치 · 크기");
        }
        if app.save_error {
            item(menu, 0, "설정 저장 실패: 폴더 쓰기 권한 확인", false, true);
        }
        separator(menu);
        item(menu, 90, "센서 다시 확인", false, false);
        item(menu, 91, "작업표시줄 다시 연결", false, false);
        item(menu, 99, "종료", false, false);
    }
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let previous = GetForegroundWindow();
    (*ptr).menu_open = true;
    let _ = SetForegroundWindow(hwnd);
    let choice = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        None,
        hwnd,
        None,
    )
    .0;
    let _ = DestroyMenu(menu);
    (*ptr).menu_open = false;
    if GetForegroundWindow() == hwnd && IsWindow(Some(previous)).as_bool() {
        let _ = SetForegroundWindow(previous);
    }
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    match choice {
        10..=12 => {
            (*ptr).config.theme = ["auto", "dark", "light"][(choice - 10) as usize].into();
        }
        20..=25 => {
            let i = (choice - 20) as usize;
            if !(*ptr).config.visible[i] || (*ptr).config.visible.iter().filter(|v| **v).count() > 1
            {
                (*ptr).config.visible[i] = !(*ptr).config.visible[i];
            }
        }
        31 => {
            (*ptr).config.offset_dip = ((*ptr).config.offset_dip - 12).max(0);
        }
        32 => {
            (*ptr).config.offset_dip += 12;
        }
        33 => {
            (*ptr).config.column_dip = (*ptr).config.column_dip.saturating_sub(4).max(88);
        }
        34 => {
            (*ptr).config.column_dip = ((*ptr).config.column_dip + 4).min(116);
        }
        35 => {
            (*ptr).config = Config::default();
        }
        90 => {
            (*ptr).telemetry.reset_baselines();
            refresh_hardware(&mut *ptr);
        }
        91 => {
            discard_widget(ptr);
            maintain_attachment(ptr);
        }
        40..=42 => {
            (*ptr).config.style = ["hud", "eva", "minimal"][(choice - 40) as usize].into();
        }
        99 => {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        _ => {}
    }
    if (10..=42).contains(&choice) {
        (*ptr).config.normalize();
        (*ptr).light = is_light(&(*ptr).config.theme);
        save(ptr);
        (*ptr).last_rect = RECT::default();
        let _ = PostMessageW(Some(hwnd), WM_TIMER, WPARAM(1), LPARAM(0));
        redraw_widget(ptr);
    }
}

unsafe fn save(ptr: *mut App) {
    (*ptr).save_error = (*ptr).config.save().is_err();
}

fn write_report(app: &mut App, phase: &str) {
    if app.report.is_none() {
        return;
    }
    if let Some(stats) = &mut app.diagnostics {
        stats.sample_resources(true);
    }
    if let Some(path) = &app.report {
        let mut renderer_stats = app.retired_renderer_stats;
        if let Some(renderer) = &app.renderer {
            renderer_stats.accumulate(renderer.stats());
        }
        let widget_state = unsafe {
            let exists = !app.widget.0.is_null() && IsWindow(Some(app.widget)).as_bool();
            let parent = if exists {
                GetParent(app.widget).ok()
            } else {
                None
            };
            serde_json::json!({
                "widget_exists":exists,
                "host_is_current":app.host.is_some_and(|host| host.is_current()),
                "parent_is_expected_taskbar":parent.is_some() && parent == app.host.map(|host| host.hwnd),
                "win32_visible":exists && IsWindowVisible(app.widget).as_bool(),
                "last_policy_visible":app.visible,
                "visibility_caveat":"IsWindowVisible is a window/ancestor style check, not proof of unclipped, unoccluded or physically visible pixels",
                "menu_open":app.menu_open,"renderer_available":app.renderer.is_some(),
                "shutting_down":app.shutting_down,
                "client_rect":[app.last_rect.left,app.last_rect.top,app.last_rect.right,app.last_rect.bottom]
            })
        };
        let report = serde_json::json!({"elapsed_seconds":app.start.elapsed().as_secs_f64(),"paints":app.paints,"draw_errors":app.draw_errors,
            "schema_version":2,"mode":"live_widget_diagnostics","report_phase":phase,
            "uptime_clock":"monotonic Instant elapsed since GUI initialization; timer gaps are not active display time",
            "hosting":"taskbar-child","attach_count":app.attach_count,"attach_error":app.attach_error,
            "successful_reattachments":app.attach_count.saturating_sub(1),"widget_state":widget_state,
            "lifecycle":app.diagnostics.as_ref().map(WidgetStats::lifecycle_report),
            "last_verified_host":app.last_verified_host,"inspection_changes_hosting":false,
            "inspection_mode":app.inspect,"visible_policy_ticks":app.visible_ticks,"hidden_policy_ticks":app.hidden_ticks,
            "visibility_tick_caveat":"500 ms maintenance observations, including posted maintenance messages; not elapsed visible/hidden duration",
            "cpu_sample_to_present_return_ms":app.diagnostics.as_ref().map(WidgetStats::latency_report),
            "process_resources":app.diagnostics.as_ref().map(WidgetStats::resource_report),
            "display_completion_caveat":"CPU sample to EndDraw and DXGI Present return; not physical screen scanout",
            "composition":"DirectComposition per-pixel premultiplied alpha",
            "software_fallback":app.renderer.as_ref().map(Renderer::software_fallback).or(app.last_software_fallback),
            "renderer_stats":renderer_stats,"renderer_stats_scope":"all renderer instances since GUI initialization, including retired instances",
            "version":env!("CARGO_PKG_VERSION"),
            "hardware":{"cpu":app.hardware.cpu_detail,"ram":app.hardware.ram_detail,"gpu":app.hardware.gpu_detail,"disk":app.hardware.disk_detail},
            "widget_rect_coordinates":"taskbar client pixels; use last_verified_host for screen coordinates",
            "config":app.config});
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn targeted_control_requires_a_complete_strict_identity_pair() {
        let parse = |args: &[&str]| {
            parse_control_target(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(parse(&["app", "--reattach"]), Ok(None));
        assert_eq!(
            parse(&[
                "app",
                "--report-now",
                "--target-pid",
                "42",
                "--target-start-time",
                "123456"
            ]),
            Ok(Some(ControlTarget {
                process_id: 42,
                created_100ns: 123456
            }))
        );
        for args in [
            vec!["--reattach", "--target-pid", "42"],
            vec!["--target-pid", "42", "--target-start-time", "123456"],
            vec![
                "--quit",
                "--target-pid",
                "42",
                "--target-start-time",
                "123456",
            ],
            vec![
                "--reattach",
                "--target-pid",
                "0",
                "--target-start-time",
                "123456",
            ],
            vec![
                "--reattach",
                "--target-pid",
                "+42",
                "--target-start-time",
                "123456",
            ],
            vec![
                "--reattach",
                "--target-pid",
                "4294967296",
                "--target-start-time",
                "123456",
            ],
            vec![
                "--reattach",
                "--target-pid",
                "42",
                "--target-start-time",
                "18446744073709551615",
            ],
            vec![
                "--reattach",
                "--target-pid",
                "42",
                "--target-start-time",
                "123456",
                "--target-pid",
                "42",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn targeted_messages_reject_reused_pid_or_hwnd_but_keep_legacy_commands() {
        let current = ControlTarget {
            process_id: 42,
            created_100ns: 123456,
        };
        assert!(control_message_matches(current, 0, 0));
        assert!(control_message_matches(current, 42, 123456));
        for (pid, time) in [(41, 123456), (42, 123455), (0, 123456), (42, 0), (42, -1)] {
            assert!(!control_message_matches(current, pid, time));
        }
    }
    #[test]
    fn disk_model_follows_reported_physical_index() {
        assert_eq!(
            active_disk_index("가장 바쁜 물리 디스크 1 D:: 100 - % Idle Time"),
            Some(1)
        );
        assert_eq!(
            active_disk_index("가장 바쁜 물리 디스크 0: 100 - % Idle Time"),
            Some(0)
        );
        assert_eq!(active_disk_index("가장 바쁜 물리 디스크 1bad: 100"), None);
        assert_eq!(active_disk_index("조회 오류: 장치 1"), None);
        let mut snapshot = Snapshot::default();
        snapshot.disk.detail = "가장 바쁜 물리 디스크 1 D:: 100 - % Idle Time".into();
        let mut hardware = HardwareInfo::default();
        hardware.disks = vec![
            hardware::HardwareDisk {
                index: 0,
                name: "Internal".into(),
                detail: String::new(),
                capacity_bytes: 2_000_000_000_000,
            },
            hardware::HardwareDisk {
                index: 1,
                name: "USB".into(),
                detail: String::new(),
                capacity_bytes: 500_000_000_000,
            },
        ];
        assert_eq!(
            hardware_text(&snapshot, &hardware, 3),
            ("USB".into(), "500 GB".into())
        );
        hardware.disks.clear();
        assert_eq!(hardware_text(&snapshot, &hardware, 3).0, "Disk 1");
    }
}
