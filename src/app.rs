//! 主窗口与应用状态：全屏覆盖（含任务栏）、搜索框、丝滑滑动翻页、空白退出、右键菜单、实时轨迹浮窗与子进程调度。

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateSolidBrush, DeleteObject, EndPaint, GetMonitorInfoW, InvalidateRect,
    MonitorFromPoint, SetBkColor, SetTextColor, UpdateWindow, AC_SRC_ALPHA, AC_SRC_OVER,
    BLENDFUNCTION, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::AttachThreadInput;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::Ime::{
    ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::catalog::{self, AppEntry, ScanResult, Source};
use crate::config::{CatalogCache, Settings, Store};
use crate::gesture::{self, Gesture};
use crate::hotkey::{self, HotkeySpec};
use crate::icons::{IconResult, IconWorker, WM_APP_ICON};
use crate::launch;
use crate::log;
use crate::render::{
    adaptive_grid, cell_visual_rect, compute_folder_layout, compute_layout, dot_rect,
    folder_dot_rect, folder_hit_test, hit_test, ContextCmd, ContextItem, ContextMenu, DragGhost,
    FolderHit, FolderLayout, Frame, Hit, Layout, OpenFolderFrame, Renderer, ViewItem, ViewItemKind,
    C_TEXT_HINT,
};
use crate::startup;
use crate::tray::{self, Tray};
use crate::ui::{
    draw_gesture_trail, draw_text, draw_transparent_text, make_font, measure_text, rgb, DibTarget,
    IconPixels, TextAlign,
};
use crate::wallpaper;
use crate::watcher::{AppWatcher, WATCH_FLAG_UWP, WM_APP_WATCH_EVENT};
use crate::win32::*;

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(uPeriod: u32) -> u32;
    fn timeEndPeriod(uPeriod: u32) -> u32;
}

pub const WM_APP_TRAY: u32 = WM_APP + 1;
pub const WM_APP_SCAN: u32 = WM_APP + 2;
pub const WM_APP_CMD: u32 = WM_APP + 4;
pub const WM_APP_LAUNCH: u32 = WM_APP + 6;
pub const WM_APP_FOCUS: u32 = WM_APP + 7;
pub const WM_APP_WALLPAPER: u32 = WM_APP + 8;
pub const WM_APP_UPDATE_EXIT: u32 = WM_APP + 10;
pub const WM_APP_AUTO_UPDATE: u32 = WM_APP + 12;
pub const WM_APP_TRAY_MENU_CLOSED: u32 = WM_APP + 13;

const TIMER_FADE: usize = 1;
const TIMER_SLIDE: usize = 2;
const TIMER_TOOLTIP: usize = 3;
const TIMER_AUTO_SCAN: usize = 4;
const TIMER_AUTO_UPDATE: usize = 5;
const TIMER_DRAG_EDGE: usize = 6;
const SHOW_SCAN_THROTTLE_SECS: u64 = 5;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FADE_DURATION_MS: f32 = 135.0;
const WM_POWERBROADCAST_MSG: u32 = 0x0218;
const PBT_APMRESUMESUSPEND_WPARAM: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC_WPARAM: usize = 0x0012;

/// 触发目录扫描的来源类型，用于控制状态提示可见性与图标失效策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanKind {
    Startup,
    Periodic,
    Show,
    Event { uwp_changed: bool },
    Manual,
}

impl ScanKind {
    pub fn label(self) -> &'static str {
        match self {
            ScanKind::Startup => "启动扫描",
            ScanKind::Periodic => "定时刷新",
            ScanKind::Show => "呼出刷新",
            ScanKind::Event { .. } => "事件驱动",
            ScanKind::Manual => "手动扫描",
        }
    }
}

/// 合并扫描进行期间新到达的挂起扫描请求，确保事件驱动与手动扫描不被降级。
fn merge_scan_kind(prev: Option<ScanKind>, next: ScanKind) -> ScanKind {
    match (prev, next) {
        (Some(ScanKind::Manual), _) | (_, ScanKind::Manual) => ScanKind::Manual,
        (Some(ScanKind::Event { uwp_changed: a }), ScanKind::Event { uwp_changed: b }) => {
            ScanKind::Event { uwp_changed: a || b }
        }
        (Some(ScanKind::Event { uwp_changed }), _) | (_, ScanKind::Event { uwp_changed }) => {
            ScanKind::Event { uwp_changed }
        }
        _ => next,
    }
}

fn local_hms() -> String {
    unsafe {
        let st = GetLocalTime();
        format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
    }
}

fn file_mtime(path: &str) -> Option<std::time::SystemTime> {
    if path.is_empty() || path.starts_with("shell:") {
        return None;
    }
    let exp = if path.contains('%') {
        catalog::expand_env(path)
    } else {
        path.to_string()
    };
    std::fs::metadata(Path::new(&exp)).and_then(|m| m.modified()).ok()
}

/// 检查桌面快捷方式或关联程序文件是否比本地图标磁盘缓存更新。
fn source_newer_than_cache(entry: &AppEntry, icons_dir: &Path) -> bool {
    if entry.icon_key.is_empty() || entry.icon_key.starts_with("shell:") {
        return false;
    }
    let Some(cache_mt) = IconWorker::disk_cache_mtime(icons_dir, &entry.icon_key) else {
        return false;
    };
    if let Some(key_mt) = file_mtime(&entry.icon_key) {
        if key_mt > cache_mt {
            return true;
        }
    }
    if entry.icon_key.to_lowercase().ends_with(".lnk") {
        if let Some(target_path) = entry.aliases.first() {
            if let Some(target_mt) = file_mtime(target_path) {
                if target_mt > cache_mt {
                    return true;
                }
            }
        }
    }
    false
}

fn entry_meta_eq(a: &AppEntry, b: &AppEntry) -> bool {
    a.id == b.id
        && a.name == b.name
        && a.source == b.source
        && a.target == b.target
        && a.args == b.args
        && a.icon_key == b.icon_key
        && a.folder_id == b.folder_id
        && a.folder_name == b.folder_name
}

#[derive(Clone, PartialEq, Eq)]
enum TipTarget {
    Cell(usize),
    NewFolder,
    Add,
    Settings,
    Dot(usize),
    FolderTitle,
    FolderClose,
    FolderCell(usize),
    FolderDot(usize),
}

#[derive(Clone, Debug)]
pub struct VisibleItem {
    pub entry: AppEntry,
    pub valid: bool,
}

#[derive(Clone, Debug)]
pub enum GridEntry {
    App(VisibleItem),
    Folder {
        id: String,
        name: String,
        apps: Vec<VisibleItem>,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum FilteredRef {
    View(usize),
    AllApp(usize),
}

enum GridEntryRef<'a> {
    App(&'a VisibleItem),
    Folder {
        id: &'a str,
        name: &'a str,
        apps: &'a [VisibleItem],
    },
}

#[derive(Clone, Debug)]
struct IconDrag {
    app_id: String,
    icon_key: String,
    name: String,
    from_folder: Option<String>,
    start_x: i32,
    start_y: i32,
    cur_x: i32,
    cur_y: i32,
    moved: bool,
    target_folder_id: Option<String>,
    outside_folder: bool,
    edge_dir: i32,
    edge_since: Option<Instant>,
}

fn inside_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 根据扫描目录与用户配置构建启动台主视图（独立应用按中英文拼音字母排序在前，文件夹按字母排序在后，文件夹内同样按字母排序）。
pub fn build_grid_view(scan_entries: &[AppEntry], settings: &Settings) -> (Vec<GridEntry>, Vec<VisibleItem>) {
    let mut all_apps: Vec<VisibleItem> =
        Vec::with_capacity(scan_entries.len() + settings.manual.len());
    for e in scan_entries.iter().chain(settings.manual.iter()) {
        if !catalog::is_entry_visible(e, settings) {
            continue;
        }
        let valid = catalog::is_entry_target_valid(e);
        all_apps.push(VisibleItem {
            entry: e.clone(),
            valid,
        });
    }
    all_apps.sort_by(|a, b| catalog::cmp_names(&a.entry.name, &b.entry.name));

    let mut app_to_folder: HashMap<&str, &str> = HashMap::new();
    for f in &settings.folders {
        for aid in &f.app_ids {
            app_to_folder.entry(aid.as_str()).or_insert(f.id.as_str());
        }
    }

    let mut standalone: Vec<VisibleItem> = Vec::new();
    let mut by_folder: HashMap<&str, Vec<VisibleItem>> = HashMap::new();
    for item in &all_apps {
        if let Some(&fid) = app_to_folder.get(item.entry.id.as_str()) {
            by_folder.entry(fid).or_default().push(item.clone());
        } else {
            standalone.push(item.clone());
        }
    }

    let mut folders: Vec<(String, String, Vec<VisibleItem>)> = settings
        .folders
        .iter()
        .map(|f| {
            let apps = by_folder.remove(f.id.as_str()).unwrap_or_default();
            (f.id.clone(), f.name.clone(), apps)
        })
        .collect();
    folders.sort_by(|a, b| catalog::cmp_names(&a.1, &b.1).then_with(|| a.0.cmp(&b.0)));

    let mut view: Vec<GridEntry> = Vec::with_capacity(standalone.len() + folders.len());
    for app in standalone {
        view.push(GridEntry::App(app));
    }
    for (id, name, apps) in folders {
        view.push(GridEntry::Folder { id, name, apps });
    }
    (view, all_apps)
}

/// 全局手势实时轨迹与动作提示透明分层浮窗（类似 Quicker：亮青顺滑轨迹 + 光标旁悬空文字，零点击拦截）。
struct GestureOverlay {
    hwnd: HWND,
    target: DibTarget,
    text_target: DibTarget,
    cov_buf: Vec<f32>,
    font: HFONT,
    scale: f32,
    visible: bool,
}

impl GestureOverlay {
    fn new() -> Self {
        Self {
            hwnd: HWND::default(),
            target: DibTarget::default(),
            text_target: DibTarget::default(),
            cov_buf: Vec::new(),
            font: HFONT::default(),
            scale: 1.0,
            visible: false,
        }
    }

    fn ensure_window(&mut self, hinst: HINSTANCE) -> bool {
        if !self.hwnd.is_invalid() {
            return true;
        }
        unsafe {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(overlay_proc),
                hInstance: hinst,
                hbrBackground: HBRUSH::default(),
                lpszClassName: w!("WinLaunchpadGestureOverlay"),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);
            self.hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT
                    | WS_EX_LAYERED,
                w!("WinLaunchpadGestureOverlay"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                hinst,
                None,
            )
            .unwrap_or_default();
        }
        !self.hwnd.is_invalid()
    }

    fn update(
        &mut self,
        hinst: HINSTANCE,
        snap: &gesture::TrailSnapshot,
        swap_lr: bool,
        launchpad_visible: bool,
    ) {
        if !snap.active || snap.pts.is_empty() {
            self.hide();
            return;
        }
        if !self.ensure_window(hinst) {
            return;
        }
        unsafe {
            let cx = snap.cursor.x;
            let cy = snap.cursor.y;
            let dpi = dpi_at_point(cx, cy);
            let scale = (dpi as f32 / 96.0).max(1.0);
            if (self.scale - scale).abs() > 0.01 || self.font.is_invalid() {
                if !self.font.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.font.0));
                }
                self.scale = scale;
                self.font = make_font((17.0 * scale).round() as i32, 600);
            }

            // 读取当前显示器范围，确保浮窗文字绝不贴边截断
            let mon = MonitorFromPoint(snap.cursor, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let mon_rc = if GetMonitorInfoW(mon, &mut mi).as_bool() {
                mi.rcMonitor
            } else {
                RECT {
                    left: 0,
                    top: 0,
                    right: GetSystemMetrics(SM_CXSCREEN).max(1920),
                    bottom: GetSystemMetrics(SM_CYSCREEN).max(1080),
                }
            };

            let mut min_x = cx;
            let mut max_x = cx;
            let mut min_y = cy;
            let mut max_y = cy;
            for p in &snap.pts {
                min_x = min_x.min(p.x);
                max_x = max_x.max(p.x);
                min_y = min_y.min(p.y);
                max_y = max_y.max(p.y);
            }
            let stroke_pad = (14.0 * scale).ceil() as i32;
            min_x -= stroke_pad;
            min_y -= stroke_pad;
            max_x += stroke_pad;
            max_y += stroke_pad;

            let label_info = snap.gesture.map(|g| {
                let lbl = g.hud_label(swap_lr, launchpad_visible);
                let _ = self.text_target.ensure_at_least(16, 16);
                let (tw, th) = self
                    .text_target
                    .mem_dc
                    .map(|dc| measure_text(dc, lbl, self.font))
                    .unwrap_or((120, 26));
                let lw = tw + (10.0 * scale).round() as i32;
                let lh = (th + (8.0 * scale).round() as i32).max((28.0 * scale).round() as i32);
                let gap = (26.0 * scale).round() as i32;
                let margin = (12.0 * scale).round() as i32;
                let lx = if cx + gap + lw <= mon_rc.right - margin {
                    cx + gap
                } else {
                    (cx - gap - lw).max(mon_rc.left + margin)
                };
                let ly = (cy - lh / 2)
                    .clamp(mon_rc.top + margin, (mon_rc.bottom - lh - margin).max(mon_rc.top + margin));
                (
                    lbl,
                    RECT {
                        left: lx,
                        top: ly,
                        right: lx + lw,
                        bottom: ly + lh,
                    },
                )
            });

            if let Some((_, ref lr)) = label_info {
                min_x = min_x.min(lr.left - 4);
                min_y = min_y.min(lr.top - 4);
                max_x = max_x.max(lr.right + 4);
                max_y = max_y.max(lr.bottom + 4);
            }

            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            let win_l = min_x.max(vx);
            let win_t = min_y.max(vy);
            let win_r = max_x.min(vx + vw);
            let win_b = max_y.min(vy + vh);
            let need_w = (win_r - win_l).max(1);
            let need_h = (win_b - win_t).max(1);
            if !self.target.ensure_at_least(need_w, need_h) {
                return;
            }
            let Some(dc) = self.target.mem_dc else { return };
            let Some(mut sf) = self.target.surface() else { return };
            sf.clear_rect_transparent(need_w, need_h);

            let local_pts: Vec<(f32, f32)> = snap
                .pts
                .iter()
                .map(|p| ((p.x - win_l) as f32, (p.y - win_t) as f32))
                .collect();
            let stroke_w = (4.2 * scale).max(2.6);
            // 亮青蓝色实时轨迹与浮动标签（复刻 Quicker 视觉）
            let cyan = rgb(0x00AEFF, 0.96);
            draw_gesture_trail(
                &mut sf,
                &local_pts,
                stroke_w,
                cyan,
                need_w,
                need_h,
                &mut self.cov_buf,
            );
            if let Some((lbl, lr)) = label_info {
                let local_rc = RECT {
                    left: lr.left - win_l,
                    top: lr.top - win_t,
                    right: lr.right - win_l,
                    bottom: lr.bottom - win_t,
                };
                draw_transparent_text(
                    &mut sf,
                    &mut self.text_target,
                    lbl,
                    self.font,
                    &local_rc,
                    rgb(0x00AEFF, 1.0),
                    scale,
                );
            }

            let dst_pt = POINT { x: win_l, y: win_t };
            let dst_sz = SIZE { cx: need_w, cy: need_h };
            let src_pt = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&dst_pt),
                Some(&dst_sz),
                dc,
                Some(&src_pt),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            if !self.visible {
                let _ = ShowWindow(self.hwnd, SW_SHOWNA);
                self.visible = true;
            }
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    fn hide(&mut self) {
        if self.visible && !self.hwnd.is_invalid() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
        self.target.release();
        self.text_target.release();
        self.cov_buf.clear();
        self.cov_buf.shrink_to_fit();
    }

    fn destroy(&mut self) {
        self.hide();
        unsafe {
            if !self.hwnd.is_invalid() {
                let _ = DestroyWindow(self.hwnd);
                self.hwnd = HWND::default();
            }
            if !self.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
                self.font = HFONT::default();
            }
        }
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT：彻底穿透鼠标事件
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub struct App {
    pub hinst: HINSTANCE,
    pub hwnd: HWND,
    pub edit: HWND,
    pub font_edit: HFONT,
    pub edit_brush: HBRUSH,
    pub edit_bg_colorref: COLORREF,
    pub edit_composing: bool,
    pub edit_target: DibTarget,
    pub store: Store,
    pub settings: Settings,
    pub scan: ScanResult,
    pub scanning: bool,
    pub wp_loading: bool,
    pub view: Vec<GridEntry>,
    pub all_apps: Vec<VisibleItem>,
    pub filtered: Vec<FilteredRef>,
    pub page: usize,
    pub selected: Option<usize>,
    pub hover: Option<usize>,
    pub hover_btn: Option<Hit>,
    pub pressed: Option<Hit>,
    pub pressed_cell: Option<usize>,
    pub layout: Layout,
    pub scale: f32,
    pub renderer: Renderer,
    pub icons: Option<std::sync::Arc<IconWorker>>,
    pub tray: Option<Tray>,
    pub hotkey: HotkeySpec,
    pub hotkey_active: bool,
    pub visible: bool,
    pub fading: bool,
    pub alpha: u8,
    pub modal: u32,
    pub in_menu: bool,
    pub context_menu: Option<ContextMenu>,
    menu_app_target: Option<VisibleItem>,
    menu_folder_target: Option<String>,
    pub taskbar_created_msg: u32,
    pub gesture: Option<gesture::GestureHandle>,
    gesture_overlay: GestureOverlay,
    pub watcher: Option<AppWatcher>,
    active_scan_kind: ScanKind,
    pending_scan_kind: Option<ScanKind>,
    pub last_scan_time_text: String,
    pub last_scan_kind: Option<ScanKind>,
    last_show_scan_at: Option<Instant>,
    pub settings_hwnd: HWND,
    pub add_hwnd: HWND,
    pub folder_hwnd: HWND,
    pub update_hwnd: HWND,
    // 文件夹展开态与拖拽图标状态
    open_folder_id: Option<String>,
    folder_page: usize,
    folder_hover: Option<FolderHit>,
    pressed_folder: Option<FolderHit>,
    folder_selected: Option<usize>,
    icon_drag: Option<IconDrag>,
    pub available_update: Option<crate::update::UpdateBundle>,
    auto_updating: bool,
    auto_notified_version: Option<String>,
    last_auto_update_at: Option<SystemTime>,
    pending_update_popup: bool,
    pub exiting: bool,
    // 开合淡入与高精度动画时钟状态
    fade_start: Option<Instant>,
    hi_res_timer: bool,
    // 水平滑动翻页状态
    drag_active: bool,
    drag_moved: bool,
    drag_start_x: i32,
    drag_start_y: i32,
    drag_origin_x: i32,
    drag_base_x: f32,
    drag_samples: VecDeque<(i32, Instant)>,
    slide_x: f32,
    slide_vx: f32,
    slide_anim: bool,
    slide_last_time: Instant,
    slide_neighbor: Option<(usize, i32)>,
    // 100ms Tooltip 状态
    tip_pending: Option<(TipTarget, RECT, String)>,
    tip_visible: Option<(RECT, String)>,
}

impl App {
    pub fn new(hinst: HINSTANCE) -> Result<Box<App>, String> {
        let data_dir = local_app_data().join("WinLaunchpad");
        let store = Store::new(data_dir.clone());
        log::init(&data_dir);
        let first_run = !store.settings_exists();
        let settings = store.load_settings();
        let scan = match store.load_catalog() {
            Some(c) => ScanResult {
                entries: c.entries,
                errors: Vec::new(),
                scanned_at: c.scanned_at,
            },
            None => ScanResult::default(),
        };
        let mut renderer = Renderer::new().map_err(|e| format!("渲染器初始化失败: {}", error_string(e)))?;
        let wp_cache = store.wallpaper_cache_path();
        if wp_cache.exists() {
            let _ = renderer.wallpaper.load_from_file(&wp_cache);
        }
        let font_edit = create_font(96, 13, false);
        let init_cr = COLORREF(0x003D_2B24);
        let edit_brush = unsafe { CreateSolidBrush(init_cr) };
        let taskbar_created_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let app = Box::new(App {
            hinst,
            hwnd: HWND::default(),
            edit: HWND::default(),
            font_edit,
            edit_brush,
            edit_bg_colorref: init_cr,
            edit_composing: false,
            edit_target: DibTarget::default(),
            store,
            settings,
            scan,
            scanning: false,
            wp_loading: false,
            view: Vec::new(),
            all_apps: Vec::new(),
            filtered: Vec::new(),
            page: 0,
            selected: None,
            hover: None,
            hover_btn: None,
            pressed: None,
            pressed_cell: None,
            layout: Layout::default(),
            scale: 1.0,
            renderer,
            icons: None,
            tray: None,
            hotkey: HotkeySpec::default(),
            hotkey_active: false,
            visible: false,
            fading: false,
            alpha: 255,
            modal: 0,
            in_menu: false,
            context_menu: None,
            menu_app_target: None,
            menu_folder_target: None,
            taskbar_created_msg,
            gesture: None,
            gesture_overlay: GestureOverlay::new(),
            watcher: None,
            active_scan_kind: ScanKind::Startup,
            pending_scan_kind: None,
            last_scan_time_text: String::new(),
            last_scan_kind: None,
            last_show_scan_at: None,
            settings_hwnd: HWND::default(),
            add_hwnd: HWND::default(),
            folder_hwnd: HWND::default(),
            update_hwnd: HWND::default(),
            open_folder_id: None,
            folder_page: 0,
            folder_hover: None,
            pressed_folder: None,
            folder_selected: None,
            icon_drag: None,
            available_update: None,
            auto_updating: false,
            auto_notified_version: None,
            last_auto_update_at: None,
            pending_update_popup: false,
            exiting: false,
            fade_start: None,
            hi_res_timer: false,
            drag_active: false,
            drag_moved: false,
            drag_start_x: 0,
            drag_start_y: 0,
            drag_origin_x: 0,
            drag_base_x: 0.0,
            drag_samples: VecDeque::with_capacity(16),
            slide_x: 0.0,
            slide_vx: 0.0,
            slide_anim: false,
            slide_last_time: Instant::now(),
            slide_neighbor: None,
            tip_pending: None,
            tip_visible: None,
        });
        if first_run && app.settings.start_with_windows {
            if let Err(e) = startup::set_enabled(true) {
                log::log(&format!("首次运行创建启动项失败: {e}"));
            }
        }
        Ok(app)
    }

    /// 窗口创建完成后调用：托盘、热键、图标与扫描调度、手势线程、事件监听线程。
    pub fn post_init(&mut self) {
        // 以管理员身份运行时放行外部命令行唤醒消息及 Explorer 任务栏重建广播
        allow_uipi_messages(self.hwnd, &[WM_APP_CMD, self.taskbar_created_msg]);
        self.hotkey = self.settings.hotkey;
        self.hotkey_active = hotkey::register(self.hwnd, self.hotkey);
        if !self.hotkey_active {
            log::log(&format!("全局热键 {} 注册失败（可能被占用）", self.hotkey.display()));
        }
        let mut tray = Tray::new(self.hwnd, self.hinst);
        tray.add();
        self.tray = Some(tray);
        self.icons = Some(IconWorker::start(self.hwnd, self.store.icons_dir()));
        gesture::set_threshold(self.settings.gesture_threshold);
        self.gesture = gesture::start(self.hwnd);
        if self.settings.gestures_enabled {
            if let Some(g) = &self.gesture {
                g.set_enabled(true);
            }
        }
        self.rebuild_view();
        self.sync_scan_timer();
        self.sync_auto_update_timer();
        self.sync_watcher();
        self.start_scan_with(ScanKind::Startup);
        self.ensure_wallpaper_async();
    }

    // ---------- 目录与视图 ----------

    pub fn rebuild_view(&mut self) {
        let (view, all_apps) = build_grid_view(&self.scan.entries, &self.settings);
        self.view = view;
        self.all_apps = all_apps;
        if let Some(fid) = &self.open_folder_id {
            if !self.settings.folders.iter().any(|f| &f.id == fid) {
                self.open_folder_id = None;
                self.folder_page = 0;
                self.folder_selected = None;
            }
        }
        // 添加/删除条目或更新设置后保持当前页码，仅在超出总页数时向前收窄
        self.rebuild_filtered(false);
        self.clamp_folder_page();
    }

    fn rebuild_filtered(&mut self, reset_page: bool) {
        let q = self.query().to_lowercase();
        let q = q.trim();
        self.filtered = if q.is_empty() {
            (0..self.view.len()).map(FilteredRef::View).collect()
        } else {
            let mut out: Vec<FilteredRef> = self
                .all_apps
                .iter()
                .enumerate()
                .filter(|(_, v)| {
                    v.entry.name.to_lowercase().contains(q)
                        || v.entry.aliases.iter().any(|a| a.contains(q))
                })
                .map(|(i, _)| FilteredRef::AllApp(i))
                .collect();
            for (i, ge) in self.view.iter().enumerate() {
                if let GridEntry::Folder { name, .. } = ge {
                    if name.to_lowercase().contains(q) {
                        out.push(FilteredRef::View(i));
                    }
                }
            }
            out
        };
        if reset_page {
            self.page = 0;
        }
        self.selected = None;
        self.hover = None;
        self.slide_x = 0.0;
        self.slide_vx = 0.0;
        self.slide_anim = false;
        self.slide_neighbor = None;
        self.sync_hi_res_timer();
        self.clamp_page();
        self.position_edit();
        self.request_page_icons();
    }

    fn entry_at_filtered(&self, pos: usize) -> Option<GridEntryRef<'_>> {
        match *self.filtered.get(pos)? {
            FilteredRef::View(vi) => match self.view.get(vi)? {
                GridEntry::App(v) => Some(GridEntryRef::App(v)),
                GridEntry::Folder { id, name, apps } => Some(GridEntryRef::Folder {
                    id: id.as_str(),
                    name: name.as_str(),
                    apps: apps.as_slice(),
                }),
            },
            FilteredRef::AllApp(ai) => self.all_apps.get(ai).map(GridEntryRef::App),
        }
    }

    fn filtered_id_at(&self, pos: usize) -> Option<&str> {
        match self.entry_at_filtered(pos)? {
            GridEntryRef::App(v) => Some(v.entry.id.as_str()),
            GridEntryRef::Folder { id, .. } => Some(id),
        }
    }

    fn open_folder_ref(&self) -> Option<(&str, &str, &[VisibleItem])> {
        let fid = self.open_folder_id.as_deref()?;
        for ge in &self.view {
            if let GridEntry::Folder { id, name, apps } = ge {
                if id == fid {
                    return Some((id.as_str(), name.as_str(), apps.as_slice()));
                }
            }
        }
        None
    }

    fn open_folder_layout(&self) -> Option<(FolderLayout, usize)> {
        let (_, _, apps) = self.open_folder_ref()?;
        let base_fl = compute_folder_layout(
            self.layout.w.max(1),
            self.layout.h.max(1),
            self.scale,
            self.settings.icon_size_dp,
            2,
        );
        let per = base_fl.cells.len().max(1);
        let pages = apps.len().div_ceil(per).max(1);
        let fl = compute_folder_layout(
            self.layout.w.max(1),
            self.layout.h.max(1),
            self.scale,
            self.settings.icon_size_dp,
            pages,
        );
        Some((fl, pages))
    }

    fn clamp_folder_page(&mut self) {
        if let Some((_, pages)) = self.open_folder_layout() {
            if self.folder_page >= pages {
                self.folder_page = pages - 1;
            }
        } else {
            self.folder_page = 0;
        }
    }

    fn open_folder_page_items(&self, per: usize, page: usize) -> Vec<ViewItem> {
        let Some((_, _, apps)) = self.open_folder_ref() else {
            return Vec::new();
        };
        let start = page * per;
        if start >= apps.len() {
            return Vec::new();
        }
        apps[start..apps.len().min(start + per)]
            .iter()
            .map(|v| ViewItem {
                id: v.entry.id.clone(),
                key: v.entry.icon_key.clone(),
                name: v.entry.name.clone(),
                valid: v.valid,
                kind: ViewItemKind::App,
            })
            .collect()
    }

    pub fn query(&self) -> String {
        unsafe {
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(self.edit, &mut buf);
            from_wide(&buf[..n as usize])
        }
    }

    fn per_page(&self) -> usize {
        self.layout.cells.len().max(1)
    }

    fn pages(&self) -> usize {
        self.filtered.len().div_ceil(self.per_page()).max(1)
    }

    fn clamp_page(&mut self) {
        if self.page >= self.pages() {
            self.page = self.pages() - 1;
        }
    }

    pub fn items_for_page(&self, page: usize) -> Vec<ViewItem> {
        let per = self.per_page();
        let start = page * per;
        if start >= self.filtered.len() {
            return Vec::new();
        }
        (start..self.filtered.len().min(start + per))
            .filter_map(|pos| match self.entry_at_filtered(pos)? {
                GridEntryRef::App(v) => Some(ViewItem {
                    id: v.entry.id.clone(),
                    key: v.entry.icon_key.clone(),
                    name: v.entry.name.clone(),
                    valid: v.valid,
                    kind: ViewItemKind::App,
                }),
                GridEntryRef::Folder { id, name, apps } => Some(ViewItem {
                    id: id.to_string(),
                    key: String::new(),
                    name: name.to_string(),
                    valid: true,
                    kind: ViewItemKind::Folder {
                        preview_keys: apps.iter().take(9).map(|a| a.entry.icon_key.clone()).collect(),
                    },
                }),
            })
            .collect()
    }

    pub fn page_items(&self) -> Vec<ViewItem> {
        self.items_for_page(self.page)
    }

    pub fn ensure_icon_loaded(&mut self, key: &str, size: u32) {
        if key.is_empty() || self.renderer.icons.contains(key) {
            return;
        }
        if let Some(icons) = &self.icons {
            if let Some((px, w, h)) = icons.read_disk_cache(key, size) {
                self.renderer.add_icon(key, &px, w, h);
                return;
            }
            icons.request(key, size);
        }
    }

    fn request_icons_for_items(&mut self, items: &[ViewItem], size: u32) {
        for item in items {
            match &item.kind {
                ViewItemKind::App => self.ensure_icon_loaded(&item.key, size),
                ViewItemKind::Folder { preview_keys } => {
                    for k in preview_keys {
                        self.ensure_icon_loaded(k, size);
                    }
                }
            }
        }
    }

    /// 优先加载当前可视页图标，确保启动台首帧零阻塞立即呈现。
    pub fn request_current_page_icons(&mut self) {
        let size = crate::icons::MASTER_ICON_SIZE;
        let items = self.page_items();
        self.request_icons_for_items(&items, size);
        self.request_open_folder_icons();
    }

    fn request_open_folder_icons(&mut self) {
        let Some((fl, pages)) = self.open_folder_layout() else {
            return;
        };
        let size = crate::icons::MASTER_ICON_SIZE;
        let per = fl.cells.len().max(1);
        let cur = self.open_folder_page_items(per, self.folder_page);
        self.request_icons_for_items(&cur, size);
        if self.folder_page + 1 < pages {
            let next = self.open_folder_page_items(per, self.folder_page + 1);
            self.request_icons_for_items(&next, size);
        }
        if self.folder_page > 0 {
            let prev = self.open_folder_page_items(per, self.folder_page - 1);
            self.request_icons_for_items(&prev, size);
        }
    }

    /// 预取左右相邻页图标，保证滑动翻页瞬间即可显示。
    pub fn prefetch_neighbor_icons(&mut self) {
        let size = crate::icons::MASTER_ICON_SIZE;
        if self.page + 1 < self.pages() {
            let items = self.items_for_page(self.page + 1);
            self.request_icons_for_items(&items, size);
        }
        if self.page > 0 {
            let items = self.items_for_page(self.page - 1);
            self.request_icons_for_items(&items, size);
        }
    }

    pub fn request_page_icons(&mut self) {
        self.request_current_page_icons();
        self.prefetch_neighbor_icons();
    }

    pub fn request_icon_async(&mut self, key: &str) {
        self.ensure_icon_loaded(key, crate::icons::MASTER_ICON_SIZE);
    }

    /// 打开外部链接或目录前收起启动台，避免置顶窗口与待弹出提醒遮挡新窗口。
    pub fn dismiss_for_external(&mut self) {
        self.pending_update_popup = false;
        self.hide();
    }

    /// 供设置页应用新的图标大小后即时刷新主面板布局。
    pub fn apply_icon_size(&mut self) {
        self.renderer.icons.clear_sprites();
        self.relayout();
    }

    /// 返回当前光标所在显示器的自适应推荐行列数，用于在设置界面标注「自动 (C×R)」。
    pub fn recommended_grid(&self) -> (i32, i32) {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let rc = mi.rcMonitor;
                let w = (rc.right - rc.left).max(1);
                let h = (rc.bottom - rc.top).max(1);
                let scale = dpi_at_point(pt.x, pt.y) as f32 / 96.0;
                return adaptive_grid(w, h, scale);
            }
        }
        (self.layout.cols.max(3), self.layout.rows.max(2))
    }

    /// 同步后台定时自动扫描计时器（0 秒表示关闭定时轮询）。
    pub fn sync_scan_timer(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_AUTO_SCAN);
            if !self.exiting && self.settings.scan_interval_secs > 0 {
                let ms = self.settings.scan_interval_secs.saturating_mul(1000).max(1000);
                let _ = SetTimer(self.hwnd, TIMER_AUTO_SCAN, ms, None);
            }
        }
    }

    /// 同步后台自动检测更新计时器：开启时启动后延迟检测一次，此后按固定周期轮询。
    pub fn sync_auto_update_timer(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_AUTO_UPDATE);
            if self.exiting || !self.settings.auto_check_update || self.auto_updating {
                return;
            }
            let interval = Duration::from_secs(crate::config::AUTO_UPDATE_INTERVAL_SECS);
            let startup_delay = crate::config::AUTO_UPDATE_STARTUP_DELAY_MS;
            let delay_ms = match self.last_auto_update_at {
                None => startup_delay,
                Some(last) => match last.elapsed() {
                    Ok(elapsed) if elapsed >= interval => startup_delay,
                    Ok(elapsed) => (interval - elapsed)
                        .as_millis()
                        .clamp(u128::from(startup_delay), i32::MAX as u128)
                        as u32,
                    Err(_) => interval
                        .as_millis()
                        .clamp(u128::from(startup_delay), i32::MAX as u128)
                        as u32,
                },
            };
            let _ = SetTimer(self.hwnd, TIMER_AUTO_UPDATE, delay_ms, None);
        }
    }

    /// 在后台线程静默检测是否有新版本；仅在启用自动检测且未在检测中时触发。
    fn start_auto_update_check(&mut self) {
        if self.exiting || !self.settings.auto_check_update || self.auto_updating {
            return;
        }
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_AUTO_UPDATE);
        }
        self.auto_updating = true;
        let hwnd = self.hwnd.0 as isize;
        let spawn = std::thread::Builder::new()
            .name("winlaunchpad-auto-update".into())
            .spawn(move || {
                let result = crate::update::check();
                let hwnd = HWND(hwnd as *mut _);
                unsafe {
                    let _ = PostMessageW(
                        hwnd,
                        WM_APP_AUTO_UPDATE,
                        WPARAM(0),
                        LPARAM(Box::into_raw(Box::new(result)) as isize),
                    );
                }
            });
        if let Err(err) = spawn {
            self.auto_updating = false;
            log::log(&format!("启动后台自动检测更新线程失败：{err}"));
            self.sync_auto_update_timer();
        }
    }

    /// 通知已打开的设置窗口同步最新的可用更新状态。
    fn notify_settings_update(&self) {
        unsafe {
            if !self.settings_hwnd.is_invalid() && IsWindow(self.settings_hwnd).as_bool() {
                let _ = PostMessageW(
                    self.settings_hwnd,
                    WM_APP_AUTO_UPDATE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    /// 清除等待启动台收起后再弹出的自动更新提醒标记。
    pub fn clear_pending_update_popup(&mut self) {
        self.pending_update_popup = false;
    }

    /// 同步手动检测更新弹窗所得结果：刷新可用更新缓存、自动检测计时并通知设置窗口。
    pub fn sync_manual_update_result(&mut self, bundle: Option<crate::update::UpdateBundle>) {
        self.last_auto_update_at = Some(SystemTime::now());
        self.sync_auto_update_timer();
        if let Some(ref b) = bundle {
            self.auto_notified_version = Some(b.latest.version.clone());
        }
        self.available_update = bundle;
        self.notify_settings_update();
    }

    /// 处理后台自动更新检测结果：静默忽略错误，发现新版本时触发弹窗或等待启动台收起后弹出。
    fn on_auto_update_done(
        &mut self,
        result: Result<(Option<crate::update::UpdateBundle>, crate::update::Channel), String>,
    ) {
        self.auto_updating = false;
        self.last_auto_update_at = Some(SystemTime::now());
        self.sync_auto_update_timer();
        if self.exiting || !self.settings.auto_check_update {
            return;
        }
        match result {
            Ok((Some(bundle), _)) => {
                let version = bundle.latest.version.clone();
                self.available_update = Some(bundle.clone());
                self.notify_settings_update();
                let popup_open = unsafe {
                    !self.update_hwnd.is_invalid() && IsWindow(self.update_hwnd).as_bool()
                };
                // 同一会话内同一版本仅主动弹窗提醒一次，避免周期检测重复打扰。
                if self.auto_notified_version.as_deref() != Some(version.as_str()) && !popup_open {
                    self.auto_notified_version = Some(version);
                    if self.visible {
                        self.pending_update_popup = true;
                    } else {
                        crate::update_dialog::open(self, bundle);
                    }
                }
            }
            Ok((None, _)) => {
                self.available_update = None;
                self.notify_settings_update();
            }
            Err(err) => {
                log::log(&format!("自动检测更新失败：{err}"));
            }
        }
    }

    /// 同步安装/卸载事件驱动监听线程启停状态。
    pub fn sync_watcher(&mut self) {
        if !self.exiting && self.settings.event_watch_enabled {
            if self.watcher.is_none() && !self.hwnd.is_invalid() {
                self.watcher = AppWatcher::start(self.hwnd);
            }
        } else if let Some(mut w) = self.watcher.take() {
            w.stop();
        }
    }

    /// 清空内存与磁盘图标缓存并立即重新提取当前页图标。
    pub fn rebuild_all_icons(&mut self) {
        self.renderer.icons.clear_all();
        if let Some(icons) = &self.icons {
            icons.clear();
        }
        IconWorker::clear_disk_cache(&self.store.icons_dir());
        self.request_page_icons();
        if self.visible {
            self.invalidate();
        }
        unsafe {
            if !self.settings_hwnd.is_invalid() && IsWindow(self.settings_hwnd).as_bool() {
                let _ = InvalidateRect(self.settings_hwnd, None, false);
            }
        }
    }

    pub fn start_scan(&mut self) {
        self.start_scan_with(ScanKind::Manual);
    }

    pub fn start_scan_with(&mut self, kind: ScanKind) {
        if self.exiting {
            return;
        }
        if self.scanning {
            // 扫描进行中时合并挂起请求（定时轮询不额外排队，事件/手动优先保留）
            if !matches!(kind, ScanKind::Periodic) {
                self.pending_scan_kind = Some(merge_scan_kind(self.pending_scan_kind, kind));
            }
            return;
        }
        self.scanning = true;
        self.active_scan_kind = kind;
        let show_status = matches!(kind, ScanKind::Manual | ScanKind::Startup) || self.view.is_empty();
        let hwnd = self.hwnd.0 as isize;
        let out_file = self.store.dir.join(format!("scan_{}.tmp.json", std::process::id()));
        std::thread::Builder::new()
            .name("catalog-scan".into())
            .spawn(move || {
                let mut parsed: Option<ScanResult> = None;
                let exe = exe_path();
                if let Ok(mut child) = std::process::Command::new(exe)
                    .arg("--worker-scan")
                    .arg(&out_file)
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                {
                    let start = Instant::now();
                    loop {
                        match child.try_wait() {
                            Ok(Some(st)) => {
                                if st.success() {
                                    if let Ok(text) = std::fs::read_to_string(&out_file) {
                                        parsed = serde_json::from_str::<ScanResult>(&text).ok();
                                    }
                                }
                                break;
                            }
                            Ok(None) => {
                                if start.elapsed() > Duration::from_secs(20) {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    log::log("目录扫描子进程超时已终止");
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(30));
                            }
                            Err(_) => break,
                        }
                    }
                }
                let _ = std::fs::remove_file(&out_file);
                let result = parsed.unwrap_or_default();
                let hwnd = HWND(hwnd as *mut _);
                unsafe {
                    let _ = PostMessageW(
                        hwnd,
                        WM_APP_SCAN,
                        WPARAM(0),
                        LPARAM(Box::into_raw(Box::new(result)) as isize),
                    );
                }
            })
            .ok();
        if show_status && self.visible {
            self.invalidate();
        }
        unsafe {
            if !self.settings_hwnd.is_invalid() && IsWindow(self.settings_hwnd).as_bool() {
                let _ = InvalidateRect(self.settings_hwnd, None, false);
            }
        }
    }

    pub fn ensure_wallpaper_async(&mut self) {
        if !self.settings.use_wallpaper_blur || self.wp_loading {
            return;
        }
        let Some((_p, sig)) = wallpaper::current_wallpaper_sig() else {
            return;
        };
        if self.renderer.wallpaper.is_ready() && self.renderer.wallpaper.sig == sig {
            return;
        }
        self.wp_loading = true;
        let hwnd = self.hwnd.0 as isize;
        let cache_path = self.store.wallpaper_cache_path();
        std::thread::Builder::new()
            .name("wallpaper-worker".into())
            .spawn(move || {
                let exe = exe_path();
                let mut ok = false;
                if let Ok(mut child) = std::process::Command::new(exe)
                    .arg("--worker-wallpaper")
                    .arg(&cache_path)
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                {
                    let start = Instant::now();
                    loop {
                        match child.try_wait() {
                            Ok(Some(st)) => {
                                ok = st.success();
                                break;
                            }
                            Ok(None) => {
                                if start.elapsed() > Duration::from_secs(12) {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(25));
                            }
                            Err(_) => break,
                        }
                    }
                }
                let hwnd = HWND(hwnd as *mut _);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_WALLPAPER, WPARAM(ok as usize), LPARAM(0));
                }
            })
            .ok();
    }

    fn on_wallpaper_done(&mut self, ok: bool) {
        self.wp_loading = false;
        if ok {
            let p = self.store.wallpaper_cache_path();
            if self.renderer.wallpaper.load_from_file(&p) {
                self.refresh_search_brush();
                if self.visible {
                    self.invalidate();
                }
            }
        }
    }

    fn refresh_search_brush(&mut self) {
        let cr = self
            .renderer
            .compute_search_colorref(&self.layout, self.settings.use_wallpaper_blur);
        if cr != self.edit_bg_colorref {
            self.edit_bg_colorref = cr;
            unsafe {
                if !self.edit_brush.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.edit_brush.0));
                }
                self.edit_brush = CreateSolidBrush(cr);
                if !self.edit.is_invalid() {
                    let _ = InvalidateRect(self.edit, None, true);
                }
            }
        }
    }

    fn on_scan_done(&mut self, r: ScanResult) {
        let kind = self.active_scan_kind;
        let prev_sel = self
            .selected
            .and_then(|pos| self.filtered_id_at(pos))
            .map(str::to_string);
        self.scanning = false;
        let usable = !r.entries.is_empty() || self.scan.entries.is_empty();
        if usable {
            self.last_scan_time_text = local_hms();
            self.last_scan_kind = Some(kind);
        }

        let icons_dir = self.store.icons_dir();
        let mut entries_changed = false;
        let mut changed_keys: Vec<String> = Vec::new();
        let mut removed_keys: Vec<String> = Vec::new();

        if usable {
            let old_map: HashMap<&str, &AppEntry> =
                self.scan.entries.iter().map(|e| (e.id.as_str(), e)).collect();
            let new_map: HashMap<&str, &AppEntry> =
                r.entries.iter().map(|e| (e.id.as_str(), e)).collect();

            if old_map.len() != new_map.len() {
                entries_changed = true;
            }
            for e in &r.entries {
                match old_map.get(e.id.as_str()) {
                    None => {
                        entries_changed = true;
                        if !e.icon_key.is_empty() {
                            changed_keys.push(e.icon_key.clone());
                        }
                    }
                    Some(prev) => {
                        if !entry_meta_eq(prev, e) {
                            entries_changed = true;
                            if !e.icon_key.is_empty() {
                                changed_keys.push(e.icon_key.clone());
                            }
                            if !prev.icon_key.is_empty() && prev.icon_key != e.icon_key {
                                removed_keys.push(prev.icon_key.clone());
                            }
                        }
                    }
                }
            }
            for old in &self.scan.entries {
                if !new_map.contains_key(old.id.as_str()) {
                    entries_changed = true;
                    if !old.icon_key.is_empty() {
                        removed_keys.push(old.icon_key.clone());
                    }
                }
            }

            let active_keys: HashSet<&str> = r
                .entries
                .iter()
                .chain(self.settings.manual.iter())
                .map(|e| e.icon_key.as_str())
                .filter(|k| !k.is_empty())
                .collect();
            removed_keys.retain(|k| !active_keys.contains(k.as_str()));

            // 在事件驱动/手动/启动扫描或目录变化时，校验源文件修改时间并失效旧图标缓存
            let check_mtime = self.settings.refresh_changed_icons
                && (entries_changed
                    || matches!(kind, ScanKind::Event { .. } | ScanKind::Manual | ScanKind::Startup));
            if check_mtime {
                for e in r.entries.iter().chain(self.settings.manual.iter()) {
                    if source_newer_than_cache(e, &icons_dir) && !e.icon_key.is_empty() {
                        changed_keys.push(e.icon_key.clone());
                    }
                }
            }
        }

        let mut invalidated_set: HashSet<String> = HashSet::new();
        for k in removed_keys {
            if invalidated_set.insert(k.clone()) {
                self.renderer.icons.remove(&k);
                IconWorker::remove_disk_cache_for_key(&icons_dir, &k);
            }
        }
        let mut refresh_targets: Vec<String> = Vec::new();
        for k in changed_keys {
            if invalidated_set.insert(k.clone()) {
                self.renderer.icons.remove(&k);
                IconWorker::remove_disk_cache_for_key(&icons_dir, &k);
                refresh_targets.push(k);
            }
        }
        let icons_invalidated = !invalidated_set.is_empty();

        if usable && entries_changed {
            self.scan = r;
            let cache = CatalogCache {
                version: 1,
                scanned_at: self.scan.scanned_at,
                entries: self.scan.entries.clone(),
            };
            self.store.save_catalog(&cache);
        } else if usable {
            self.scan.scanned_at = r.scanned_at;
            self.scan.errors = r.errors;
        }

        let validity_changed = self
            .all_apps
            .iter()
            .any(|v| catalog::is_entry_target_valid(&v.entry) != v.valid);

        if entries_changed || validity_changed || icons_invalidated {
            self.rebuild_view();
            if let Some(id) = prev_sel {
                if let Some(pos) = (0..self.filtered.len()).position(|p| self.filtered_id_at(p) == Some(id.as_str())) {
                    self.selected = Some(pos);
                    self.page = pos / self.per_page();
                }
            }
            self.request_page_icons();
            // 预先提取新增或更新条目的图标，确保翻页立即可见
            for k in refresh_targets.iter().take(24) {
                self.request_icon_async(k);
            }
            if self.visible {
                self.invalidate();
            }
        } else if matches!(kind, ScanKind::Startup | ScanKind::Manual) {
            self.request_page_icons();
            if self.visible {
                self.invalidate();
            }
        }

        if !self.scan.errors.is_empty() && matches!(kind, ScanKind::Startup | ScanKind::Manual) {
            log::log(&format!(
                "扫描完成，{} 个条目，{} 个来源问题",
                self.scan.entries.len(),
                self.scan.errors.len()
            ));
        }
        unsafe {
            if !self.settings_hwnd.is_invalid() && IsWindow(self.settings_hwnd).as_bool() {
                let _ = PostMessageW(self.settings_hwnd, WM_APP_SCAN, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(next_kind) = self.pending_scan_kind.take() {
            self.start_scan_with(next_kind);
        }
    }

    fn on_icon(&mut self, r: IconResult) {
        let Some(px) = r.pixels else { return };
        self.renderer.add_icon(&r.key, &px, r.w, r.h);
        if self.visible {
            let hit_main = self.page_items().iter().any(|i| {
                i.key == r.key
                    || matches!(&i.kind, ViewItemKind::Folder { preview_keys } if preview_keys.iter().any(|k| k == &r.key))
            });
            let hit_folder = if let Some((fl, _)) = self.open_folder_layout() {
                let per = fl.cells.len().max(1);
                self.open_folder_page_items(per, self.folder_page)
                    .iter()
                    .any(|i| i.key == r.key)
            } else {
                false
            };
            if hit_folder {
                self.invalidate();
            } else if hit_main {
                self.invalidate_grid();
            }
        }
        unsafe {
            if !self.settings_hwnd.is_invalid() && IsWindow(self.settings_hwnd).as_bool() {
                let _ = InvalidateRect(self.settings_hwnd, None, false);
            }
            if !self.add_hwnd.is_invalid() && IsWindow(self.add_hwnd).as_bool() {
                let _ = InvalidateRect(self.add_hwnd, None, false);
            }
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    /// 仅重绘图标网格及底部分页圆点区，避免翻页滑动时波及顶部搜索框引发闪烁。
    fn invalidate_grid(&self) {
        let rc = RECT {
            left: 0,
            top: self.layout.grid_area.top,
            right: self.layout.w.max(1),
            bottom: self.layout.h.max(1),
        };
        if rc.bottom <= rc.top {
            self.invalidate();
            return;
        }
        unsafe {
            let _ = InvalidateRect(self.hwnd, Some(&rc), false);
        }
    }

    // ---------- 显隐与全屏布局 ----------

    pub fn toggle(&mut self) {
        if self.visible {
            self.hide();
        } else {
            self.show();
        }
    }

    /// 仅在淡入或滑页动画进行期间申请 1ms 系统时钟精度，空闲时立即归还以保持零功耗。
    fn sync_hi_res_timer(&mut self) {
        let need = self.visible && (self.fading || self.slide_anim);
        if need != self.hi_res_timer {
            self.hi_res_timer = need;
            unsafe {
                if need {
                    timeBeginPeriod(1);
                } else {
                    timeEndPeriod(1);
                }
            }
        }
    }

    pub fn show(&mut self) {
        if self.exiting {
            return;
        }
        if let Some(t) = &mut self.tray {
            t.close_menu();
        }
        self.in_menu = false;
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(mon, &mut mi);
            // 全屏覆盖显示器（包含任务栏区域，复刻 macOS 原生全屏启动台）
            let rc = mi.rcMonitor;
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            let dpi = dpi_at_point(pt.x, pt.y);
            self.scale = dpi as f32 / 96.0;
            self.layout = compute_layout(
                w,
                h,
                self.scale,
                self.settings.icon_size_dp,
                self.settings.grid_cols,
                self.settings.grid_rows,
            );
            self.sync_edit_font();
            self.refresh_search_brush();
            self.renderer.ensure_target(self.hwnd, w as u32, h as u32, self.scale);
            self.visible = true;
            self.alpha = 0;
            self.fading = false;
            self.fade_start = None;
            self.slide_x = 0.0;
            self.slide_vx = 0.0;
            self.slide_anim = false;
            self.slide_neighbor = None;
            self.context_menu = None;
            self.menu_app_target = None;
            self.menu_folder_target = None;
            self.open_folder_id = None;
            self.folder_page = 0;
            self.folder_hover = None;
            self.pressed_folder = None;
            self.folder_selected = None;
            self.icon_drag = None;
            self.tip_pending = None;
            self.tip_visible = None;
            self.selected = None;
            self.hover = None;
            self.pressed = None;
            self.pressed_cell = None;
            self.clamp_page();
            self.request_current_page_icons();

            // 先置全透明并同步绘完首帧，再以时间基准缓动淡入，彻底消除打开瞬间的卡顿与跳帧
            let _ = SetLayeredWindowAttributes(self.hwnd, COLORREF(0), 0, LWA_ALPHA);
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                rc.left,
                rc.top,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            self.position_edit();
            self.invalidate();
            let _ = UpdateWindow(self.hwnd);

            self.fading = true;
            self.fade_start = Some(Instant::now());
            self.sync_hi_res_timer();
            let _ = SetTimer(self.hwnd, TIMER_FADE, 8, None);

            self.force_foreground();
            // 抢焦点后再确认一次 HWND_TOPMOST，压住系统任务栏层级
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        self.ensure_wallpaper_async();
        self.prefetch_neighbor_icons();
        if self.settings.scan_on_show {
            let allow = self
                .last_show_scan_at
                .map(|t| t.elapsed() >= Duration::from_secs(SHOW_SCAN_THROTTLE_SECS))
                .unwrap_or(true);
            if allow {
                self.last_show_scan_at = Some(Instant::now());
                self.start_scan_with(ScanKind::Show);
            }
        }
    }

    fn sync_edit_font(&mut self) {
        let px_pt = 12;
        let dpi = (self.scale * 96.0).round() as u32;
        let new_f = create_font(dpi.max(96), px_pt, false);
        unsafe {
            if !self.edit.is_invalid() {
                set_control_font(self.edit, new_f);
            }
            if !self.font_edit.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font_edit.0));
            }
        }
        self.font_edit = new_f;
    }

    fn force_foreground(&mut self) {
        unsafe {
            let cur = windows::Win32::System::Threading::GetCurrentThreadId();
            let fg = GetForegroundWindow();
            let fg_thread = if fg.is_invalid() {
                0
            } else {
                GetWindowThreadProcessId(fg, None)
            };
            let attached =
                fg_thread != 0 && fg_thread != cur && AttachThreadInput(cur, fg_thread, true).as_bool();
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetActiveWindow(self.hwnd);
            if !GetForegroundWindow().eq(&self.hwnd) {
                let mut down = INPUT::default();
                down.r#type = INPUT_KEYBOARD;
                down.Anonymous.ki = KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x12),
                    wScan: 0x38,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                };
                let mut up = down;
                up.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
                SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32);
                let _ = SetForegroundWindow(self.hwnd);
                let _ = SetActiveWindow(self.hwnd);
            }
            if attached {
                let _ = AttachThreadInput(cur, fg_thread, false);
            }
            let _ = PostMessageW(self.hwnd, WM_APP_FOCUS, WPARAM(0), LPARAM(0));
        }
    }

    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_FADE);
            let _ = KillTimer(self.hwnd, TIMER_SLIDE);
            let _ = KillTimer(self.hwnd, TIMER_TOOLTIP);
            let _ = KillTimer(self.hwnd, TIMER_DRAG_EDGE);
            if self.drag_active || self.icon_drag.is_some() {
                self.drag_active = false;
                self.drag_moved = false;
                self.icon_drag = None;
                let _ = ReleaseCapture();
            }
            self.fading = false;
            self.fade_start = None;
            self.visible = false;
            self.alpha = 255;
            self.context_menu = None;
            self.menu_app_target = None;
            self.menu_folder_target = None;
            self.open_folder_id = None;
            self.folder_page = 0;
            self.folder_hover = None;
            self.pressed_folder = None;
            self.folder_selected = None;
            self.tip_pending = None;
            self.tip_visible = None;
            self.slide_x = 0.0;
            self.slide_vx = 0.0;
            self.slide_anim = false;
            self.slide_neighbor = None;
            self.drag_base_x = 0.0;
            self.drag_samples.clear();
            self.sync_hi_res_timer();
            let _ = SetLayeredWindowAttributes(self.hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            if !self.query().is_empty() {
                let _ = SetWindowTextW(self.edit, w!(""));
            }
            self.edit_composing = false;
            self.selected = None;
            self.hover = None;
            self.pressed = None;
            self.pressed_cell = None;
            if let Some(icons) = &self.icons {
                icons.clear();
            }
            self.edit_target.release();
            self.renderer.release_target();
        }
        if self.pending_update_popup {
            self.pending_update_popup = false;
            if self.settings.auto_check_update {
                if let Some(bundle) = self.available_update.clone() {
                    crate::update_dialog::open(self, bundle);
                }
            }
        }
    }

    /// 始终保持搜索 EDIT 全宽，防止首字滚动截断并确保鼠标悬停显示竖线光标。
    fn position_edit(&mut self) {
        if self.edit.is_invalid() {
            return;
        }
        let r = self.layout.edit;
        unsafe {
            let _ = SetWindowPos(
                self.edit,
                HWND_TOP,
                r.left,
                r.top,
                (r.right - r.left).max(1),
                (r.bottom - r.top).max(1),
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOZORDER,
            );
        }
    }

    fn relayout(&mut self) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w <= 0 || h <= 0 {
            return;
        }
        self.layout = compute_layout(
            w,
            h,
            self.scale,
            self.settings.icon_size_dp,
            self.settings.grid_cols,
            self.settings.grid_rows,
        );
        self.sync_edit_font();
        self.refresh_search_brush();
        self.position_edit();
        self.clamp_page();
        if self.visible {
            self.renderer.ensure_target(self.hwnd, w as u32, h as u32, self.scale);
            self.request_page_icons();
            self.invalidate();
        }
    }

    // ---------- 交互操作 ----------

    fn page_cell_to_filtered(&self, i: usize) -> Option<usize> {
        let pos = self.page * self.per_page() + i;
        if pos < self.filtered.len() {
            Some(pos)
        } else {
            None
        }
    }

    fn launch_visible_item(&mut self, item: &VisibleItem) {
        let entry = item.entry.clone();
        let valid = item.valid;
        if !valid {
            let name = entry.name.clone();
            self.hide();
            self.show_error(&format!(
                "找不到「{name}」的目标文件，可能已被卸载。\n可以在设置「图标管理 → 检测过期图标」中清理。"
            ));
            return;
        }
        let hwnd = self.hwnd.0 as isize;
        self.hide();
        launch::launch_async(entry.clone(), move |r| {
            let hwnd = HWND(hwnd as *mut _);
            if let Err(e) = r {
                let msg = format!("无法启动「{}」：{}", entry.name, e);
                unsafe {
                    let _ = PostMessageW(
                        hwnd,
                        WM_APP_LAUNCH,
                        WPARAM(0),
                        LPARAM(Box::into_raw(Box::new(msg)) as isize),
                    );
                }
            }
        });
    }

    fn open_folder(&mut self, folder_id: String) {
        self.open_folder_id = Some(folder_id);
        self.folder_page = 0;
        self.folder_hover = None;
        self.pressed_folder = None;
        self.folder_selected = None;
        self.cancel_tooltip();
        self.request_open_folder_icons();
        self.invalidate();
    }

    fn close_open_folder(&mut self) {
        if self.open_folder_id.is_some() {
            self.open_folder_id = None;
            self.folder_page = 0;
            self.folder_hover = None;
            self.pressed_folder = None;
            self.folder_selected = None;
            self.cancel_tooltip();
            self.invalidate();
        }
    }

    fn launch_selected(&mut self) {
        if let Some((_, _, apps)) = self.open_folder_ref() {
            let idx = self
                .folder_selected
                .or(if apps.is_empty() { None } else { Some(0) });
            if let Some(item) = idx.and_then(|i| apps.get(i).cloned()) {
                self.launch_visible_item(&item);
            }
            return;
        }
        let idx = self.selected.or(if self.filtered.is_empty() { None } else { Some(0) });
        if let Some(pos) = idx {
            match self.entry_at_filtered(pos) {
                Some(GridEntryRef::App(v)) => {
                    let item = v.clone();
                    self.launch_visible_item(&item);
                }
                Some(GridEntryRef::Folder { id, .. }) => {
                    let fid = id.to_string();
                    self.open_folder(fid);
                }
                None => {}
            }
        }
    }

    fn show_error(&self, msg: &str) {
        unsafe {
            let m = wide(msg);
            let t = wide("WinLaunchpad");
            let _ = MessageBoxW(
                None,
                PCWSTR(m.as_ptr()),
                PCWSTR(t.as_ptr()),
                MB_OK | MB_ICONWARNING | MB_SETFOREGROUND,
            );
        }
    }

    fn remove_app_entry(&mut self, entry: &AppEntry) {
        let id = entry.id.clone();
        if entry.source == Source::Manual {
            self.settings.manual.retain(|m| m.id != id);
        } else {
            self.settings.app_overrides.insert(id.clone(), false);
        }
        self.settings.remove_app_from_folders(&id);
        if !self.store.save_settings(&self.settings) {
            self.show_error("保存设置失败，本次移除在重启后可能失效。");
        }
        self.rebuild_view();
        self.invalidate();
    }

    fn open_location_entry(&mut self, entry: &AppEntry) {
        let target = entry.target.clone();
        if target.starts_with("shell:") {
            return;
        }
        self.hide();
        open_shell_path("open", "explorer.exe", Some(&format!("/select,\"{target}\"")));
    }

    /// 新建文件夹并自动跳转到该文件夹所在分页。
    pub fn create_folder_and_focus(&mut self, name: &str) {
        let Some(fid) = self.settings.create_folder(name) else {
            return;
        };
        if !self.store.save_settings(&self.settings) {
            self.show_error("保存文件夹设置失败。");
        }
        if !self.query().is_empty() {
            unsafe {
                let _ = SetWindowTextW(self.edit, w!(""));
            }
        }
        self.open_folder_id = None;
        self.rebuild_view();
        self.focus_folder_page(&fid);
        self.request_page_icons();
        self.invalidate();
    }

    /// 重命名文件夹并保持或定位到目标分页。
    pub fn rename_folder_and_focus(&mut self, folder_id: &str, name: &str) {
        if !self.settings.rename_folder(folder_id, name) {
            return;
        }
        if !self.store.save_settings(&self.settings) {
            self.show_error("保存文件夹设置失败。");
        }
        self.rebuild_view();
        if self.open_folder_id.as_deref() != Some(folder_id) {
            self.focus_folder_page(folder_id);
        }
        self.request_page_icons();
        self.invalidate();
    }

    fn delete_folder(&mut self, folder_id: &str) {
        if !self.settings.delete_folder(folder_id) {
            return;
        }
        if self.open_folder_id.as_deref() == Some(folder_id) {
            self.open_folder_id = None;
            self.folder_page = 0;
            self.folder_selected = None;
        }
        if !self.store.save_settings(&self.settings) {
            self.show_error("保存文件夹设置失败。");
        }
        self.rebuild_view();
        self.request_page_icons();
        self.invalidate();
    }

    fn focus_folder_page(&mut self, folder_id: &str) {
        if let Some(pos) = (0..self.filtered.len()).position(|p| self.filtered_id_at(p) == Some(folder_id)) {
            let target_page = pos / self.per_page();
            self.selected = Some(pos);
            if self.visible && target_page != self.page {
                self.goto_page(target_page);
            } else {
                self.page = target_page;
            }
        }
    }

    fn find_drop_folder_on_page(&self, x: i32, y: i32) -> Option<String> {
        let count = self.page_items().len();
        for (i, cell) in self.layout.cells.iter().enumerate().take(count) {
            if inside_rect(cell, x, y) {
                if let Some(pos) = self.page_cell_to_filtered(i) {
                    if let Some(GridEntryRef::Folder { id, .. }) = self.entry_at_filtered(pos) {
                        return Some(id.to_string());
                    }
                }
            }
        }
        None
    }

    fn folder_name_by_id(&self, folder_id: &str) -> Option<&str> {
        self.settings
            .folders
            .iter()
            .find(|f| f.id == folder_id)
            .map(|f| f.name.as_str())
    }

    fn move_folder_selection(&mut self, dx: i32, dy: i32) {
        let Some((fl, pages)) = self.open_folder_layout() else {
            return;
        };
        let Some((_, _, apps)) = self.open_folder_ref() else {
            return;
        };
        let total = apps.len();
        if total == 0 {
            return;
        }
        let cols = fl.cols.max(1);
        let per = fl.cells.len().max(1);
        if self.folder_selected.is_none() {
            let start = self.folder_page * per;
            if start < total {
                self.folder_selected = Some(start);
                self.invalidate();
            }
            return;
        }
        let cur = self.folder_selected.unwrap() as i32;
        let next = (cur + dy * cols + dx).clamp(0, total as i32 - 1) as usize;
        self.folder_selected = Some(next);
        self.folder_page = (next / per).min(pages.saturating_sub(1));
        self.request_open_folder_icons();
        self.invalidate();
    }

    fn change_folder_page(&mut self, delta: i32) {
        let Some((_, pages)) = self.open_folder_layout() else {
            return;
        };
        if pages <= 1 {
            return;
        }
        let next = (self.folder_page as i32 + delta).clamp(0, pages as i32 - 1) as usize;
        if next != self.folder_page {
            self.folder_page = next;
            self.folder_hover = None;
            self.pressed_folder = None;
            self.cancel_tooltip();
            self.request_open_folder_icons();
            self.invalidate();
        }
    }

    fn move_selection(&mut self, dx: i32, dy: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let cols = self.layout.cols.max(1) as i32;
        if self.selected.is_none() {
            let start = self.page * self.per_page();
            if start < self.filtered.len() {
                self.selected = Some(start);
                self.invalidate();
            }
            return;
        }
        let cur = self.selected.unwrap() as i32;
        if dy < 0 {
            let page_start = (self.page * self.per_page()) as i32;
            if cur >= page_start && cur < page_start + cols && !self.query().is_empty() {
                self.selected = None;
                self.invalidate();
                return;
            }
        }
        let mut next = cur + dy * cols + dx;
        if next < 0 {
            next = 0;
        }
        if next >= self.filtered.len() as i32 {
            next = self.filtered.len() as i32 - 1;
        }
        let next = next as usize;
        let target_page = next / self.per_page();
        self.selected = Some(next);
        if target_page != self.page {
            self.goto_page(target_page);
        } else {
            self.request_page_icons();
            self.invalidate();
        }
    }

    fn change_page(&mut self, delta: i32) {
        if self.pages() <= 1 {
            return;
        }
        let p = (self.page as i32 + delta).clamp(0, self.pages() as i32 - 1) as usize;
        self.goto_page(p);
    }

    fn goto_page(&mut self, target: usize) {
        let max_p = self.pages().saturating_sub(1);
        let target = target.min(max_p);
        if target == self.page {
            return;
        }
        let old = self.page;
        let span = self.layout.grid_span_w.max(1);
        self.page = target;
        self.hover = None;
        self.pressed_cell = None;
        self.cancel_tooltip();
        if target > old {
            self.slide_neighbor = Some((old, -span));
            self.slide_x = span as f32;
        } else {
            self.slide_neighbor = Some((old, span));
            self.slide_x = -(span as f32);
        }
        self.slide_vx = 0.0;
        self.request_page_icons();
        self.start_slide_anim();
        self.invalidate_grid();
    }

    fn start_slide_anim(&mut self) {
        self.slide_anim = true;
        self.slide_last_time = Instant::now();
        self.sync_hi_res_timer();
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_SLIDE, 8, None);
        }
    }

    /// 复刻 macOS 边缘渐进橡皮筋阻尼，越拖越重且过零平滑无突变。
    fn rubber_band(over: f32, span: f32) -> f32 {
        let d = over.abs();
        let limit = (span * 0.30).max(120.0);
        let res = (limit * d) / (limit / 0.45 + d);
        res.copysign(over)
    }

    /// 基于近期 85ms 滑动窗口计算精确脱手水平速度，静止超过 45ms 则速度归零防误翻。
    fn compute_drag_vx(&mut self) -> f32 {
        let now = Instant::now();
        while self.drag_samples.len() > 2
            && now.duration_since(self.drag_samples.front().unwrap().1) > Duration::from_millis(85)
        {
            self.drag_samples.pop_front();
        }
        let Some(&(last_x, last_t)) = self.drag_samples.back() else {
            return 0.0;
        };
        if now.duration_since(last_t) > Duration::from_millis(45) {
            return 0.0;
        }
        let &(first_x, first_t) = self.drag_samples.front().unwrap();
        let dt = last_t.duration_since(first_t).as_secs_f32();
        if dt < 0.002 {
            return 0.0;
        }
        (last_x - first_x) as f32 / dt
    }

    fn cancel_tooltip(&mut self) {
        let had = self.tip_visible.is_some() || self.tip_pending.is_some();
        self.tip_pending = None;
        self.tip_visible = None;
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TOOLTIP);
        }
        if had {
            self.invalidate();
        }
    }

    fn set_pending_tooltip(&mut self, next: Option<(TipTarget, RECT, String)>) {
        let same = match (&self.tip_pending, &next) {
            (Some((a, _, _)), Some((b, _, _))) => a == b,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.tip_visible = None;
        self.tip_pending = next;
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TOOLTIP);
            if self.tip_pending.is_some() {
                let _ = SetTimer(self.hwnd, TIMER_TOOLTIP, 100, None);
            }
        }
        self.invalidate();
    }

    fn update_tooltip_target(&mut self, hit: Hit) {
        let next: Option<(TipTarget, RECT, String)> = match hit {
            Hit::NewFolder => Some((TipTarget::NewFolder, self.layout.new_folder_btn, "新建文件夹".into())),
            Hit::Add => Some((TipTarget::Add, self.layout.add_btn, "添加应用".into())),
            Hit::Settings => Some((TipTarget::Settings, self.layout.settings_btn, "打开设置".into())),
            Hit::Dot(i) => Some((
                TipTarget::Dot(i),
                dot_rect(&self.layout, i, self.pages(), self.scale),
                format!("第 {} 页", i + 1),
            )),
            Hit::Cell(i) => {
                if let Some(pos) = self.page_cell_to_filtered(i) {
                    match self.entry_at_filtered(pos) {
                        Some(GridEntryRef::App(v)) => {
                            if self.renderer.is_label_truncated(&v.entry.name, &self.layout) || !v.valid {
                                let vr = cell_visual_rect(&self.layout.cells[i], self.layout.icon_size, self.scale);
                                let tip = if v.valid {
                                    v.entry.name.clone()
                                } else {
                                    format!("{}（目标文件已失效）", v.entry.name)
                                };
                                Some((TipTarget::Cell(i), vr, tip))
                            } else {
                                None
                            }
                        }
                        Some(GridEntryRef::Folder { name, .. }) => {
                            if self.renderer.is_label_truncated(name, &self.layout) {
                                let vr = cell_visual_rect(&self.layout.cells[i], self.layout.icon_size, self.scale);
                                Some((TipTarget::Cell(i), vr, name.to_string()))
                            } else {
                                None
                            }
                        }
                        None => None,
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        self.set_pending_tooltip(next);
    }

    fn update_folder_tooltip_target(&mut self, hit: FolderHit) {
        let Some((fl, pages)) = self.open_folder_layout() else {
            self.set_pending_tooltip(None);
            return;
        };
        let next: Option<(TipTarget, RECT, String)> = match hit {
            FolderHit::Title => Some((TipTarget::FolderTitle, fl.title_rc, "点击重命名文件夹".into())),
            FolderHit::Close => Some((TipTarget::FolderClose, fl.close_btn, "关闭文件夹".into())),
            FolderHit::Dot(i) => Some((
                TipTarget::FolderDot(i),
                folder_dot_rect(&fl, i, pages, self.scale),
                format!("第 {} 页", i + 1),
            )),
            FolderHit::Cell(i) => {
                let per = fl.cells.len().max(1);
                let idx = self.folder_page * per + i;
                if let Some((_, _, apps)) = self.open_folder_ref() {
                    if let Some(v) = apps.get(idx) {
                        let cell_w = fl.cells.first().map(|c| c.right - c.left).unwrap_or(110);
                        if self.renderer.is_label_truncated_in_cell(&v.entry.name, cell_w) || !v.valid {
                            let vr = cell_visual_rect(&fl.cells[i], fl.icon_size, self.scale);
                            let tip = if v.valid {
                                v.entry.name.clone()
                            } else {
                                format!("{}（目标文件已失效）", v.entry.name)
                            };
                            Some((TipTarget::FolderCell(i), vr, tip))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        self.set_pending_tooltip(next);
    }

    fn open_context_menu(&mut self, hit: Hit, pt: POINT) {
        self.cancel_tooltip();
        self.menu_app_target = None;
        self.menu_folder_target = None;
        let mut items = Vec::new();
        if let Hit::Cell(i) = hit {
            if let Some(pos) = self.page_cell_to_filtered(i) {
                match self.entry_at_filtered(pos) {
                    Some(GridEntryRef::App(v)) => {
                        let v = v.clone();
                        items.push(ContextItem { cmd: ContextCmd::Open, label: "打开", danger: false });
                        if !v.entry.target.starts_with("shell:") {
                            items.push(ContextItem { cmd: ContextCmd::OpenLocation, label: "打开文件位置", danger: false });
                        }
                        items.push(ContextItem { cmd: ContextCmd::Remove, label: "从启动台移除", danger: true });
                        self.menu_app_target = Some(v);
                    }
                    Some(GridEntryRef::Folder { id, .. }) => {
                        self.menu_folder_target = Some(id.to_string());
                        items.push(ContextItem { cmd: ContextCmd::OpenFolder, label: "打开文件夹", danger: false });
                        items.push(ContextItem { cmd: ContextCmd::RenameFolder, label: "重命名", danger: false });
                        items.push(ContextItem { cmd: ContextCmd::DeleteFolder, label: "删除文件夹", danger: true });
                    }
                    None => {}
                }
            }
        }
        if items.is_empty() {
            items.push(ContextItem { cmd: ContextCmd::Add, label: "添加应用", danger: false });
            items.push(ContextItem { cmd: ContextCmd::NewFolder, label: "新建文件夹", danger: false });
            items.push(ContextItem { cmd: ContextCmd::Rescan, label: "刷新应用图标", danger: false });
            items.push(ContextItem { cmd: ContextCmd::Settings, label: "设置", danger: false });
        }
        self.context_menu = Some(ContextMenu::build(
            pt.x,
            pt.y,
            self.layout.w,
            self.layout.h,
            items,
            self.scale,
        ));
        self.invalidate();
    }

    fn open_folder_context_menu(&mut self, hit: FolderHit, pt: POINT) {
        self.cancel_tooltip();
        self.menu_app_target = None;
        self.menu_folder_target = self.open_folder_id.clone();
        let mut items = Vec::new();
        if let FolderHit::Cell(i) = hit {
            if let Some((fl, _)) = self.open_folder_layout() {
                let per = fl.cells.len().max(1);
                let idx = self.folder_page * per + i;
                if let Some((_, _, apps)) = self.open_folder_ref() {
                    if let Some(v) = apps.get(idx).cloned() {
                        items.push(ContextItem { cmd: ContextCmd::Open, label: "打开", danger: false });
                        if !v.entry.target.starts_with("shell:") {
                            items.push(ContextItem { cmd: ContextCmd::OpenLocation, label: "打开文件位置", danger: false });
                        }
                        items.push(ContextItem { cmd: ContextCmd::RemoveFromFolder, label: "移出文件夹", danger: false });
                        items.push(ContextItem { cmd: ContextCmd::Remove, label: "从启动台移除", danger: true });
                        self.menu_app_target = Some(v);
                    }
                }
            }
        }
        if items.is_empty() {
            items.push(ContextItem { cmd: ContextCmd::RenameFolder, label: "重命名文件夹", danger: false });
            items.push(ContextItem { cmd: ContextCmd::CloseFolder, label: "关闭文件夹", danger: false });
        }
        self.context_menu = Some(ContextMenu::build(
            pt.x,
            pt.y,
            self.layout.w,
            self.layout.h,
            items,
            self.scale,
        ));
        self.invalidate();
    }

    fn exec_context_cmd(&mut self, cmd: ContextCmd) {
        self.context_menu = None;
        match cmd {
            ContextCmd::Open => {
                if let Some(v) = self.menu_app_target.take() {
                    self.launch_visible_item(&v);
                }
            }
            ContextCmd::OpenLocation => {
                if let Some(v) = self.menu_app_target.take() {
                    self.open_location_entry(&v.entry);
                }
            }
            ContextCmd::Remove => {
                if let Some(v) = self.menu_app_target.take() {
                    self.remove_app_entry(&v.entry);
                }
            }
            ContextCmd::RemoveFromFolder => {
                if let Some(v) = self.menu_app_target.take() {
                    if self.settings.remove_app_from_folders(&v.entry.id) {
                        self.store.save_settings(&self.settings);
                        self.rebuild_view();
                        self.request_page_icons();
                    }
                    self.invalidate();
                }
            }
            ContextCmd::Add => crate::add::open(self),
            ContextCmd::NewFolder => crate::folder_dialog::open_create(self),
            ContextCmd::Rescan => self.start_scan(),
            ContextCmd::Settings => crate::settings::open(self),
            ContextCmd::OpenFolder => {
                if let Some(fid) = self.menu_folder_target.take() {
                    self.open_folder(fid);
                }
            }
            ContextCmd::RenameFolder => {
                if let Some(fid) = self.menu_folder_target.take() {
                    if let Some(name) = self.folder_name_by_id(&fid).map(str::to_string) {
                        crate::folder_dialog::open_rename(self, fid, name);
                    }
                }
            }
            ContextCmd::DeleteFolder => {
                if let Some(fid) = self.menu_folder_target.take() {
                    self.delete_folder(&fid);
                }
            }
            ContextCmd::CloseFolder => {
                self.close_open_folder();
            }
        }
    }

    fn on_menu_command(&mut self, id: u32) {
        match id {
            tray::CMD_TOGGLE => self.toggle(),
            tray::CMD_RESCAN => self.start_scan(),
            tray::CMD_CHECK_UPDATE => crate::update_dialog::open_check(self),
            tray::CMD_SETTINGS => crate::settings::open(self),
            tray::CMD_EXIT => self.exit(),
            _ => {}
        }
    }

    fn on_tray(&mut self, msg: u32) {
        match msg {
            WM_LBUTTONUP => {
                // 左键呼出/收起前先关闭可能残留的自绘托盘菜单
                if let Some(t) = &mut self.tray {
                    t.close_menu();
                }
                self.in_menu = false;
                self.toggle();
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                // 菜单弹出期间保持 in_menu 标记，防止主窗口失焦时误隐藏启动台
                self.in_menu = true;
                let opened = if let Some(t) = &mut self.tray {
                    t.show_menu(self.visible)
                } else {
                    false
                };
                if !opened {
                    self.in_menu = false;
                }
            }
            _ => {}
        }
    }

    pub fn apply_hotkey(&mut self, spec: HotkeySpec) -> Result<(), String> {
        if spec == self.hotkey && self.hotkey_active {
            return Ok(());
        }
        hotkey::unregister(self.hwnd);
        let ok = hotkey::register(self.hwnd, spec);
        if !ok {
            let restored = hotkey::register(self.hwnd, self.hotkey);
            self.hotkey_active = restored;
            return Err(format!("快捷键 {} 已被其他程序占用或系统不允许。", spec.display()));
        }
        self.hotkey = spec;
        self.hotkey_active = true;
        self.settings.hotkey = spec;
        Ok(())
    }

    pub fn apply_gestures(&mut self, enabled: bool) {
        self.settings.gestures_enabled = enabled;
        if !enabled {
            self.gesture_overlay.hide();
        }
        if let Some(g) = &self.gesture {
            g.set_enabled(enabled);
        } else if enabled {
            log::log("手势线程不可用，无法启用手势");
        }
    }

    /// 实时更新手势识别最低匹配度百分比并同步到底层钩子线程。
    pub fn apply_gesture_threshold(&mut self, pct: u8) {
        let clamped = crate::config::clamp_gesture_threshold(pct);
        self.settings.gesture_threshold = clamped;
        gesture::set_threshold(clamped);
    }

    /// 供非提权自有窗口在鼠标移动时触发低级手势钩子自愈检测。
    pub fn ensure_gesture_alive(&self) {
        if self.settings.gestures_enabled {
            if let Some(g) = &self.gesture {
                g.ensure_alive();
            }
        }
    }

    pub fn exit(&mut self) {
        if self.exiting {
            return;
        }
        self.exiting = true;
        self.fading = false;
        self.slide_anim = false;
        self.sync_hi_res_timer();
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_AUTO_SCAN);
            let _ = KillTimer(self.hwnd, TIMER_AUTO_UPDATE);
            let _ = KillTimer(self.hwnd, TIMER_DRAG_EDGE);
        }
        if let Some(mut w) = self.watcher.take() {
            w.stop();
        }
        self.gesture_overlay.destroy();
        self.store.save_settings(&self.settings);
        hotkey::unregister(self.hwnd);
        if let Some(t) = &mut self.tray {
            t.remove();
        }
        if let Some(g) = &self.gesture {
            g.set_enabled(false);
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    // ---------- 绘制 ----------

    fn paint(&mut self) {
        if !self.visible {
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(self.hwnd, &mut ps);
                let _ = EndPaint(self.hwnd, &ps);
                let _ = hdc;
            }
            return;
        }
        self.renderer.scale = self.scale;
        let items = self.page_items();
        let (neighbor_items, neighbor_offset) = match self.slide_neighbor {
            Some((np, off)) if np != self.page => (self.items_for_page(np), off),
            _ => (Vec::new(), 0),
        };
        let show_scanning = self.scanning
            && (matches!(self.active_scan_kind, ScanKind::Manual | ScanKind::Startup)
                || self.view.is_empty());
        let status = if show_scanning {
            "正在扫描已安装的应用".to_string()
        } else if !self.scan.errors.is_empty() {
            format!("{} 个来源读取失败（详见日志）", self.scan.errors.len())
        } else {
            String::new()
        };
        let q = self.query();
        let empty_text = if self.scanning && self.view.is_empty() {
            "正在扫描应用".to_string()
        } else if self.view.is_empty() {
            "暂无展示的应用，右键点击可添加或前往设置勾选文件夹".to_string()
        } else if !q.trim().is_empty() {
            format!("没有找到与「{}」匹配的应用", q.trim())
        } else {
            String::new()
        };
        let tip = self.tip_visible.as_ref().map(|(r, s)| (*r, s.as_str()));
        let hidden_app_id = self
            .icon_drag
            .as_ref()
            .filter(|d| d.moved)
            .map(|d| d.app_id.as_str());
        let drop_folder_id = self
            .icon_drag
            .as_ref()
            .filter(|d| d.moved)
            .and_then(|d| d.target_folder_id.as_deref());
        let drag_hint: Option<String> = self.icon_drag.as_ref().filter(|d| d.moved).and_then(|d| {
            if d.from_folder.is_some() {
                if d.outside_folder {
                    Some("移出文件夹".to_string())
                } else {
                    None
                }
            } else {
                d.target_folder_id
                    .as_deref()
                    .and_then(|fid| self.folder_name_by_id(fid))
                    .map(|fname| format!("放入「{fname}」"))
            }
        });
        let drag_ghost = self.icon_drag.as_ref().filter(|d| d.moved).map(|d| DragGhost {
            icon_key: &d.icon_key,
            name: &d.name,
            x: d.cur_x,
            y: d.cur_y,
            hint: drag_hint.as_deref(),
        });
        let open_folder_info = self.open_folder_layout().and_then(|(fl, pages)| {
            let (_, name, _) = self.open_folder_ref()?;
            let per = fl.cells.len().max(1);
            let folder_items = self.open_folder_page_items(per, self.folder_page);
            let outside_drag = self
                .icon_drag
                .as_ref()
                .is_some_and(|d| d.moved && d.from_folder.is_some() && d.outside_folder);
            Some((fl, pages, name.to_string(), folder_items, outside_drag))
        });
        let open_folder = open_folder_info.as_ref().map(|(fl, pages, title, folder_items, outside_drag)| {
            let per = fl.cells.len().max(1);
            OpenFolderFrame {
                layout: fl,
                title,
                items: folder_items,
                page: self.folder_page,
                pages: *pages,
                hover: self.folder_hover,
                pressed: self.pressed_folder,
                selected: self
                    .folder_selected
                    .map(|s| s.saturating_sub(self.folder_page * per))
                    .filter(|s| *s < per),
                hidden_app_id,
                outside_drag: *outside_drag,
            }
        });
        let frame = Frame {
            items: &items,
            neighbor_items: &neighbor_items,
            neighbor_offset,
            slide_x: self.slide_x,
            hover: if self.open_folder_id.is_some() { None } else { self.hover },
            pressed_cell: if self.open_folder_id.is_some() { None } else { self.pressed_cell },
            selected: if self.open_folder_id.is_some() {
                None
            } else {
                self.selected
                    .map(|s| s.saturating_sub(self.page * self.per_page()))
                    .filter(|s| *s < self.per_page())
            },
            page: self.page,
            pages: self.pages(),
            query_empty: q.is_empty() && !self.edit_composing,
            status: &status,
            empty_text: &empty_text,
            hover_btn: if self.open_folder_id.is_some() { None } else { self.hover_btn },
            pressed_btn: if self.open_folder_id.is_some() { None } else { self.pressed },
            tooltip: tip,
            context_menu: self.context_menu.as_ref(),
            use_wallpaper: self.settings.use_wallpaper_blur,
            icon_roundness: self.settings.icon_roundness,
            hidden_app_id,
            drop_folder_id,
            open_folder,
            drag_ghost,
        };
        if !self.renderer.paint(self.hwnd, &frame, &self.layout) {
            self.request_page_icons();
            self.invalidate();
        }
    }

    // ---------- 消息分发 ----------

    pub fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == self.taskbar_created_msg && self.taskbar_created_msg != 0 {
            if let Some(t) = &mut self.tray {
                t.add();
            }
            return LRESULT(0);
        }
        match msg {
            WM_NCCREATE => LRESULT(1),
            WM_CREATE => {
                unsafe {
                    self.edit = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("EDIT"),
                        w!(""),
                        WS_CHILD
                            | WS_VISIBLE
                            | WINDOW_STYLE(ES_LEFT as u32 | ES_AUTOHSCROLL as u32 | ES_NOHIDESEL as u32),
                        0,
                        0,
                        10,
                        10,
                        self.hwnd,
                        None,
                        self.hinst,
                        None,
                    )
                    .unwrap_or_default();
                    if !self.edit.is_invalid() {
                        let _ = SetWindowSubclass(self.edit, Some(edit_proc), 1, 0);
                        set_control_font(self.edit, self.font_edit);
                        let _ = SetWindowTextW(self.edit, w!(""));
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                self.fading = false;
                self.slide_anim = false;
                self.sync_hi_res_timer();
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_AUTO_SCAN);
                    let _ = KillTimer(self.hwnd, TIMER_AUTO_UPDATE);
                }
                if let Some(mut w) = self.watcher.take() {
                    w.stop();
                }
                self.gesture_overlay.destroy();
                self.edit_target.release();
                unsafe {
                    if !self.edit_brush.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(self.edit_brush.0));
                    }
                    if !self.font_edit.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(self.font_edit.0));
                    }
                    let _ = PostQuitMessage(0);
                }
                LRESULT(0)
            }
            WM_POWERBROADCAST_MSG => {
                if matches!(wparam.0, PBT_APMRESUMESUSPEND_WPARAM | PBT_APMRESUMEAUTOMATIC_WPARAM) {
                    if self.settings.gestures_enabled {
                        if let Some(g) = &self.gesture {
                            g.force_reinstall();
                        }
                    }
                    if self.settings.event_watch_enabled {
                        self.start_scan_with(ScanKind::Event { uwp_changed: false });
                    }
                    if self.settings.auto_check_update {
                        self.sync_auto_update_timer();
                    }
                }
                LRESULT(1)
            }
            WM_CLOSE => {
                self.hide();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                self.paint();
                LRESULT(0)
            }
            WM_SETCURSOR => {
                unsafe {
                    if self.open_folder_id.is_none() && self.icon_drag.is_none() {
                        let mut pt = POINT::default();
                        if GetCursorPos(&mut pt).is_ok() {
                            let mut cli = pt;
                            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut cli);
                            let sr = self.layout.search;
                            if cli.x >= sr.left && cli.x < sr.right && cli.y >= sr.top && cli.y < sr.bottom {
                                let _ = SetCursor(LoadCursorW(None, IDC_IBEAM).unwrap_or_default());
                                return LRESULT(1);
                            }
                        }
                    }
                    DefWindowProcW(self.hwnd, msg, wparam, lparam)
                }
            }
            WM_SIZE | WM_DISPLAYCHANGE => {
                self.relayout();
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let new = lparam.0 as *const RECT;
                unsafe {
                    let r = *new;
                    let dpi = hiword(wparam.0) as u32;
                    self.scale = dpi as f32 / 96.0;
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.relayout();
                LRESULT(0)
            }
            WM_ACTIVATE => {
                if loword(wparam.0) as u32 == WA_INACTIVE && self.visible && self.modal == 0 && !self.in_menu {
                    self.hide();
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                if wparam.0 as i32 != hotkey::HOTKEY_ID {
                    return LRESULT(0);
                }
                self.toggle();
                LRESULT(0)
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_FADE if self.fading => {
                        let p = self
                            .fade_start
                            .map(|t| (t.elapsed().as_secs_f32() * 1000.0 / FADE_DURATION_MS).clamp(0.0, 1.0))
                            .unwrap_or(1.0);
                        let inv = 1.0 - p;
                        let ease = 1.0 - inv * inv * inv;
                        self.alpha = (ease * 255.0).round().clamp(0.0, 255.0) as u8;
                        unsafe {
                            let _ = SetLayeredWindowAttributes(
                                self.hwnd,
                                COLORREF(0),
                                self.alpha,
                                LWA_ALPHA,
                            );
                            if p >= 1.0 || self.alpha >= 255 {
                                self.alpha = 255;
                                self.fading = false;
                                self.fade_start = None;
                                let _ = KillTimer(self.hwnd, TIMER_FADE);
                            }
                        }
                        if !self.fading {
                            self.sync_hi_res_timer();
                        }
                    }
                    TIMER_SLIDE => {
                        if self.slide_anim {
                            let now = Instant::now();
                            let dt = (now - self.slide_last_time).as_secs_f32().clamp(0.001, 0.05);
                            self.slide_last_time = now;
                            // 临界阻尼弹簧（ω = 17 rad/s）：零过冲并平滑衔接鼠标脱手初速度
                            let omega = 17.0f32;
                            let steps = ((dt / 0.002).ceil() as usize).clamp(1, 25);
                            let h = dt / steps as f32;
                            for _ in 0..steps {
                                let a = -(omega * omega) * self.slide_x - (2.0 * omega) * self.slide_vx;
                                self.slide_vx += a * h;
                                self.slide_x += self.slide_vx * h;
                            }
                            if self.slide_x.abs() < 0.6 && self.slide_vx.abs() < 15.0 * self.scale {
                                self.slide_x = 0.0;
                                self.slide_vx = 0.0;
                                self.slide_anim = false;
                                self.slide_neighbor = None;
                                unsafe {
                                    let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                                }
                                self.sync_hi_res_timer();
                            }
                        } else {
                            self.slide_x = 0.0;
                            self.slide_vx = 0.0;
                            self.slide_neighbor = None;
                            unsafe {
                                let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                            }
                            self.sync_hi_res_timer();
                        }
                        self.invalidate_grid();
                    }
                    TIMER_TOOLTIP => {
                        unsafe {
                            let _ = KillTimer(self.hwnd, TIMER_TOOLTIP);
                        }
                        if let Some((_, rect, text)) = &self.tip_pending {
                            self.tip_visible = Some((*rect, text.clone()));
                            self.invalidate();
                        }
                    }
                    TIMER_AUTO_SCAN => {
                        self.start_scan_with(ScanKind::Periodic);
                    }
                    TIMER_AUTO_UPDATE => {
                        self.start_auto_update_check();
                    }
                    TIMER_DRAG_EDGE => {
                        let flip_dir = self.icon_drag.as_ref().and_then(|d| {
                            if d.moved
                                && d.from_folder.is_none()
                                && d.edge_dir != 0
                                && d.edge_since.is_some_and(|t| t.elapsed() >= Duration::from_millis(450))
                            {
                                Some(d.edge_dir)
                            } else {
                                None
                            }
                        });
                        if let Some(dir) = flip_dir {
                            self.change_page(dir);
                            if let Some(d) = self.icon_drag.as_mut() {
                                d.edge_since = Some(Instant::now());
                            }
                            if let Some(d) = self.icon_drag.as_ref() {
                                let next_target = self.find_drop_folder_on_page(d.cur_x, d.cur_y);
                                if let Some(dm) = self.icon_drag.as_mut() {
                                    dm.target_folder_id = next_target;
                                }
                            }
                            self.invalidate();
                        } else if self.icon_drag.as_ref().is_none_or(|d| d.edge_dir == 0) {
                            unsafe {
                                let _ = KillTimer(self.hwnd, TIMER_DRAG_EDGE);
                            }
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_TRAY => {
                let m = loword(lparam.0 as usize) as u32;
                self.on_tray(m);
                LRESULT(0)
            }
            WM_APP_TRAY_MENU_CLOSED => {
                // 仅当销毁的窗口句柄与当前记录一致时才解除菜单阻塞标志
                let closed_hwnd = HWND(wparam.0 as *mut _);
                if let Some(t) = &mut self.tray {
                    t.on_menu_closed(closed_hwnd);
                    self.in_menu = t.is_menu_open();
                } else {
                    self.in_menu = false;
                }
                LRESULT(0)
            }
            WM_APP_SCAN => {
                let r = unsafe { *Box::from_raw(lparam.0 as *mut ScanResult) };
                self.on_scan_done(r);
                LRESULT(0)
            }
            WM_APP_WATCH_EVENT => {
                if self.settings.event_watch_enabled {
                    let uwp_changed = (wparam.0 & WATCH_FLAG_UWP) != 0;
                    self.start_scan_with(ScanKind::Event { uwp_changed });
                }
                LRESULT(0)
            }
            WM_APP_ICON => {
                let r = unsafe { *Box::from_raw(lparam.0 as *mut IconResult) };
                self.on_icon(r);
                LRESULT(0)
            }
            WM_APP_WALLPAPER => {
                self.on_wallpaper_done(wparam.0 != 0);
                LRESULT(0)
            }
            WM_APP_UPDATE_EXIT => {
                // 替换进程就绪后沿用托盘退出流程，确保设置已保存且全局钩子已释放。
                self.exit();
                LRESULT(0)
            }
            WM_APP_AUTO_UPDATE => {
                let res = unsafe {
                    *Box::from_raw(
                        lparam.0
                            as *mut Result<
                                (Option<crate::update::UpdateBundle>, crate::update::Channel),
                                String,
                            >,
                    )
                };
                self.on_auto_update_done(res);
                LRESULT(0)
            }
            WM_APP_CMD => {
                match wparam.0 {
                    2 => self.show(),
                    3 => crate::settings::open(self),
                    4 => crate::add::open(self),
                    5 => crate::settings::open_gestures(self),
                    _ => self.toggle(),
                }
                LRESULT(0)
            }
            WM_APP_FOCUS => {
                unsafe {
                    let _ = SetFocus(self.edit);
                }
                LRESULT(0)
            }
            gesture::WM_APP_GESTURE_HUD => {
                let snap = gesture::take_trail_snapshot();
                self.gesture_overlay
                    .update(self.hinst, &snap, self.settings.gesture_swap_lr, self.visible);
                LRESULT(0)
            }
            gesture::WM_APP_GESTURE => {
                self.gesture_overlay.hide();
                if let Some(g) = Gesture::from_id(wparam.0) {
                    if g == Gesture::UpDown {
                        self.toggle();
                    } else {
                        gesture::perform_async(g, self.settings.gesture_swap_lr);
                    }
                }
                LRESULT(0)
            }
            WM_APP_LAUNCH => {
                let m = unsafe { *Box::from_raw(lparam.0 as *mut String) };
                self.show_error(&m);
                LRESULT(0)
            }
            WM_COMMAND => {
                if lparam.0 == 0 {
                    self.on_menu_command(loword(wparam.0) as u32);
                    return LRESULT(0);
                }
                if lparam.0 as isize == self.edit.0 as isize && hiword(wparam.0) as u32 == EN_CHANGE {
                    if !self.query().trim().is_empty() {
                        self.open_folder_id = None;
                        self.folder_page = 0;
                        self.folder_hover = None;
                        self.pressed_folder = None;
                        self.folder_selected = None;
                    }
                    self.rebuild_filtered(true);
                    if self.visible {
                        self.invalidate();
                        unsafe {
                            let _ = InvalidateRect(self.edit, None, false);
                        }
                    }
                    return LRESULT(0);
                }
                LRESULT(0)
            }
            WM_CTLCOLOREDIT => {
                if lparam.0 as isize == self.edit.0 as isize {
                    unsafe {
                        let hdc = HDC(wparam.0 as *mut _);
                        SetTextColor(hdc, COLORREF(0x00FF_FFFF));
                        SetBkColor(hdc, self.edit_bg_colorref);
                    }
                    return LRESULT(self.edit_brush.0 as isize);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                self.ensure_gesture_alive();
                let pt = POINT {
                    x: get_x_lparam(lparam.0),
                    y: get_y_lparam(lparam.0),
                };
                if let Some(menu) = &mut self.context_menu {
                    let nh = menu.hit(pt.x, pt.y);
                    if nh != menu.hover {
                        menu.hover = nh;
                        self.track_mouse_leave();
                        self.invalidate();
                    }
                    return LRESULT(0);
                }
                if self.icon_drag.is_some() {
                    let thresh = (6.0 * self.scale).round() as i32;
                    let mut became_moved = false;
                    let mut from_folder = false;
                    if let Some(drag) = self.icon_drag.as_mut() {
                        drag.cur_x = pt.x;
                        drag.cur_y = pt.y;
                        if !drag.moved
                            && (pt.x - drag.start_x).abs().max((pt.y - drag.start_y).abs()) >= thresh
                        {
                            drag.moved = true;
                            became_moved = true;
                        }
                        from_folder = drag.from_folder.is_some();
                    }
                    if became_moved {
                        self.pressed = None;
                        self.pressed_cell = None;
                        self.pressed_folder = None;
                        self.hover = None;
                        self.hover_btn = None;
                        self.folder_hover = None;
                        self.cancel_tooltip();
                    }
                    if self.icon_drag.as_ref().is_some_and(|d| d.moved) {
                        if from_folder {
                            let outside = self
                                .open_folder_layout()
                                .is_some_and(|(fl, _)| !inside_rect(&fl.card, pt.x, pt.y));
                            if let Some(drag) = self.icon_drag.as_mut() {
                                drag.outside_folder = outside;
                                drag.target_folder_id = None;
                            }
                        } else {
                            let target_fid = self.find_drop_folder_on_page(pt.x, pt.y);
                            let edge_margin = (48.0 * self.scale).round() as i32;
                            let new_edge = if pt.x < edge_margin && self.page > 0 {
                                -1
                            } else if pt.x > self.layout.w - edge_margin && self.page + 1 < self.pages() {
                                1
                            } else {
                                0
                            };
                            let mut start_edge_timer = false;
                            let mut stop_edge_timer = false;
                            if let Some(drag) = self.icon_drag.as_mut() {
                                drag.target_folder_id = target_fid;
                                drag.outside_folder = false;
                                if new_edge != drag.edge_dir {
                                    drag.edge_dir = new_edge;
                                    if new_edge != 0 {
                                        drag.edge_since = Some(Instant::now());
                                        start_edge_timer = true;
                                    } else {
                                        drag.edge_since = None;
                                        stop_edge_timer = true;
                                    }
                                }
                            }
                            unsafe {
                                if start_edge_timer {
                                    let _ = SetTimer(self.hwnd, TIMER_DRAG_EDGE, 120, None);
                                } else if stop_edge_timer {
                                    let _ = KillTimer(self.hwnd, TIMER_DRAG_EDGE);
                                }
                            }
                        }
                        self.invalidate();
                    }
                    return LRESULT(0);
                }
                if self.drag_active {
                    let dx = pt.x - self.drag_start_x;
                    let dy = pt.y - self.drag_start_y;
                    let thresh = (6.0 * self.scale).round() as i32;
                    if !self.drag_moved && dx.abs() >= thresh && dx.abs() >= dy.abs() {
                        self.drag_moved = true;
                        // 以越过阈值瞬间的坐标为起点，彻底消除拖页起步瞬间的跳变
                        self.drag_origin_x = pt.x;
                        self.pressed = None;
                        self.pressed_cell = None;
                        self.hover = None;
                        self.hover_btn = None;
                        self.slide_anim = false;
                        self.slide_vx = 0.0;
                        self.cancel_tooltip();
                        unsafe {
                            let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                        }
                        self.sync_hi_res_timer();
                        self.request_page_icons();
                    }
                    if self.drag_moved {
                        let now = Instant::now();
                        self.drag_samples.push_back((pt.x, now));
                        while self.drag_samples.len() > 2
                            && now.duration_since(self.drag_samples.front().unwrap().1)
                                > Duration::from_millis(85)
                        {
                            self.drag_samples.pop_front();
                        }

                        let pages = self.pages();
                        let span = self.layout.grid_span_w.max(1);
                        let span_f = span as f32;
                        let raw = self.drag_base_x + (pt.x - self.drag_origin_x) as f32;
                        let off = if (raw > 0.0 && self.page == 0) || (raw < 0.0 && self.page + 1 >= pages) {
                            Self::rubber_band(raw, span_f)
                        } else {
                            raw.clamp(-span_f, span_f)
                        };
                        self.slide_x = off;
                        self.slide_neighbor = if off > 0.5 && self.page > 0 {
                            Some((self.page - 1, -span))
                        } else if off < -0.5 && self.page + 1 < pages {
                            Some((self.page + 1, span))
                        } else {
                            None
                        };
                        self.invalidate_grid();
                    }
                    return LRESULT(0);
                }
                if let Some((fl, pages)) = self.open_folder_layout() {
                    let per = fl.cells.len().max(1);
                    let fcount = self.open_folder_page_items(per, self.folder_page).len();
                    let fhit = folder_hit_test(&fl, pt.x, pt.y, fcount, pages, self.scale);
                    self.update_folder_tooltip_target(fhit);
                    if self.folder_hover != Some(fhit) {
                        self.folder_hover = Some(fhit);
                        self.track_mouse_leave();
                        self.invalidate();
                    }
                    return LRESULT(0);
                }
                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                let hover = match hit {
                    Hit::Cell(i) => Some(i),
                    _ => None,
                };
                let hover_btn = match hit {
                    Hit::NewFolder | Hit::Add | Hit::Settings | Hit::Dot(_) => Some(hit),
                    _ => None,
                };
                self.update_tooltip_target(hit);
                if hover != self.hover || hover_btn != self.hover_btn {
                    self.hover = hover;
                    self.hover_btn = hover_btn;
                    self.track_mouse_leave();
                    self.invalidate();
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                self.cancel_tooltip();
                if self.hover.is_some() || self.hover_btn.is_some() || self.folder_hover.is_some() {
                    self.hover = None;
                    self.hover_btn = None;
                    self.folder_hover = None;
                    self.invalidate();
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                if self.context_menu.is_some() {
                    self.context_menu = None;
                    self.invalidate();
                    return LRESULT(0);
                }
                let delta = get_wheel_delta(wparam.0);
                if self.open_folder_id.is_some() {
                    self.change_folder_page(if delta > 0 { -1 } else { 1 });
                } else {
                    self.change_page(if delta > 0 { -1 } else { 1 });
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let pt = POINT {
                    x: get_x_lparam(lparam.0),
                    y: get_y_lparam(lparam.0),
                };
                self.cancel_tooltip();
                if let Some(menu) = &self.context_menu {
                    if let Some(idx) = menu.hit(pt.x, pt.y) {
                        let cmd = menu.items[idx].cmd;
                        self.exec_context_cmd(cmd);
                    } else {
                        self.context_menu = None;
                        self.invalidate();
                    }
                    return LRESULT(0);
                }
                if let Some((fl, pages)) = self.open_folder_layout() {
                    let per = fl.cells.len().max(1);
                    let fcount = self.open_folder_page_items(per, self.folder_page).len();
                    let fhit = folder_hit_test(&fl, pt.x, pt.y, fcount, pages, self.scale);
                    self.pressed_folder = Some(fhit);
                    if let FolderHit::Cell(i) = fhit {
                        let idx = self.folder_page * per + i;
                        if let Some((fid, _, apps)) = self.open_folder_ref() {
                            if let Some(v) = apps.get(idx) {
                                self.icon_drag = Some(IconDrag {
                                    app_id: v.entry.id.clone(),
                                    icon_key: v.entry.icon_key.clone(),
                                    name: v.entry.name.clone(),
                                    from_folder: Some(fid.to_string()),
                                    start_x: pt.x,
                                    start_y: pt.y,
                                    cur_x: pt.x,
                                    cur_y: pt.y,
                                    moved: false,
                                    target_folder_id: None,
                                    outside_folder: false,
                                    edge_dir: 0,
                                    edge_since: None,
                                });
                                unsafe {
                                    let _ = SetCapture(self.hwnd);
                                }
                            }
                        }
                    }
                    unsafe {
                        let _ = SetFocus(self.edit);
                    }
                    self.invalidate();
                    return LRESULT(0);
                }
                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                self.pressed = Some(hit);
                self.pressed_cell = match hit {
                    Hit::Cell(i) => Some(i),
                    _ => None,
                };
                let start_app_drag = if let Hit::Cell(i) = hit {
                    if !self.settings.folders.is_empty() {
                        if let Some(pos) = self.page_cell_to_filtered(i) {
                            if let Some(GridEntryRef::App(v)) = self.entry_at_filtered(pos) {
                                Some(v.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(v) = start_app_drag {
                    if self.slide_anim {
                        self.slide_anim = false;
                        self.slide_vx = 0.0;
                        unsafe {
                            let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                        }
                        self.sync_hi_res_timer();
                    }
                    self.icon_drag = Some(IconDrag {
                        app_id: v.entry.id.clone(),
                        icon_key: v.entry.icon_key.clone(),
                        name: v.entry.name.clone(),
                        from_folder: None,
                        start_x: pt.x,
                        start_y: pt.y,
                        cur_x: pt.x,
                        cur_y: pt.y,
                        moved: false,
                        target_folder_id: None,
                        outside_folder: false,
                        edge_dir: 0,
                        edge_since: None,
                    });
                    unsafe {
                        let _ = SetCapture(self.hwnd);
                    }
                } else if matches!(hit, Hit::Cell(_) | Hit::Blank) {
                    // 若在滑页动画途中按下鼠标，立即接管当前偏移量实现零中断抓停
                    if self.slide_anim {
                        self.slide_anim = false;
                        self.slide_vx = 0.0;
                        unsafe {
                            let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                        }
                        self.sync_hi_res_timer();
                    }
                    self.drag_active = true;
                    self.drag_moved = false;
                    self.drag_start_x = pt.x;
                    self.drag_start_y = pt.y;
                    self.drag_origin_x = pt.x;
                    self.drag_base_x = self.slide_x;
                    self.drag_samples.clear();
                    self.drag_samples.push_back((pt.x, Instant::now()));
                    unsafe {
                        let _ = SetCapture(self.hwnd);
                    }
                }
                unsafe {
                    let _ = SetFocus(self.edit);
                }
                self.invalidate();
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let pt = POINT {
                    x: get_x_lparam(lparam.0),
                    y: get_y_lparam(lparam.0),
                };
                if let Some(drag) = self.icon_drag.take() {
                    unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_DRAG_EDGE);
                        let _ = ReleaseCapture();
                    }
                    if drag.moved {
                        self.pressed = None;
                        self.pressed_cell = None;
                        self.pressed_folder = None;
                        if drag.from_folder.is_some() {
                            if drag.outside_folder && self.settings.remove_app_from_folders(&drag.app_id) {
                                self.store.save_settings(&self.settings);
                                self.rebuild_view();
                                self.request_page_icons();
                            }
                        } else if let Some(fid) = drag.target_folder_id {
                            if self.settings.move_app_to_folder(&drag.app_id, &fid) {
                                self.store.save_settings(&self.settings);
                                self.rebuild_view();
                                self.request_page_icons();
                            }
                        }
                        self.invalidate();
                        return LRESULT(0);
                    }
                }
                if self.drag_active {
                    self.drag_active = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    if self.drag_moved {
                        self.drag_moved = false;
                        let pages = self.pages();
                        let span = self.layout.grid_span_w.max(1);
                        let span_f = span as f32;
                        let vx = self.compute_drag_vx().clamp(-6000.0 * self.scale, 6000.0 * self.scale);
                        let projected = self.slide_x + vx * 0.14;
                        let dist_thresh = (span_f * 0.25).max(80.0 * self.scale);
                        let go_next = self.page + 1 < pages
                            && (self.slide_x < -dist_thresh
                                || (projected < -dist_thresh && self.slide_x < -12.0 * self.scale));
                        let go_prev = self.page > 0
                            && (self.slide_x > dist_thresh
                                || (projected > dist_thresh && self.slide_x > 12.0 * self.scale));
                        if go_next {
                            let old = self.page;
                            self.page += 1;
                            self.selected = None;
                            self.slide_neighbor = Some((old, -span));
                            self.slide_x += span_f;
                            self.request_page_icons();
                        } else if go_prev {
                            let old = self.page;
                            self.page -= 1;
                            self.selected = None;
                            self.slide_neighbor = Some((old, span));
                            self.slide_x -= span_f;
                            self.request_page_icons();
                        }
                        self.slide_vx = vx;
                        self.drag_base_x = 0.0;
                        self.start_slide_anim();
                        self.invalidate_grid();
                        return LRESULT(0);
                    } else if self.drag_base_x.abs() > 8.0 * self.scale {
                        // 中途抓停滑页但未再拖动时松开，直接平滑归位而不触发点击空白关闭
                        self.drag_base_x = 0.0;
                        self.pressed = None;
                        self.pressed_cell = None;
                        self.start_slide_anim();
                        return LRESULT(0);
                    }
                    self.drag_base_x = 0.0;
                }

                if let Some((fl, pages)) = self.open_folder_layout() {
                    let per = fl.cells.len().max(1);
                    let fcount = self.open_folder_page_items(per, self.folder_page).len();
                    let fhit = folder_hit_test(&fl, pt.x, pt.y, fcount, pages, self.scale);
                    let pressed = self.pressed_folder.take();
                    if pressed == Some(fhit) {
                        match fhit {
                            FolderHit::Close | FolderHit::Outside => {
                                self.close_open_folder();
                                return LRESULT(0);
                            }
                            FolderHit::Title => {
                                if let Some((fid, name, _)) = self.open_folder_ref() {
                                    let (fid, name) = (fid.to_string(), name.to_string());
                                    self.invalidate();
                                    crate::folder_dialog::open_rename(self, fid, name);
                                    return LRESULT(0);
                                }
                            }
                            FolderHit::Dot(p) => {
                                if p != self.folder_page && p < pages {
                                    self.folder_page = p;
                                    self.request_open_folder_icons();
                                }
                            }
                            FolderHit::Cell(i) => {
                                let idx = self.folder_page * per + i;
                                self.folder_selected = Some(idx);
                                if let Some(item) = self.open_folder_ref().and_then(|(_, _, apps)| apps.get(idx).cloned()) {
                                    self.launch_visible_item(&item);
                                    return LRESULT(0);
                                }
                            }
                            FolderHit::Inside => {}
                        }
                    }
                    self.invalidate();
                    return LRESULT(0);
                }

                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                let pressed = self.pressed.take();
                self.pressed_cell = None;
                if pressed.is_some() && pressed == Some(hit) {
                    match hit {
                        Hit::Cell(i) => {
                            if let Some(pos) = self.page_cell_to_filtered(i) {
                                self.selected = Some(pos);
                                match self.entry_at_filtered(pos) {
                                    Some(GridEntryRef::App(v)) => {
                                        let item = v.clone();
                                        self.launch_visible_item(&item);
                                    }
                                    Some(GridEntryRef::Folder { id, .. }) => {
                                        let fid = id.to_string();
                                        self.open_folder(fid);
                                    }
                                    None => {}
                                }
                            }
                        }
                        Hit::NewFolder => {
                            self.invalidate();
                            crate::folder_dialog::open_create(self);
                        }
                        Hit::Add => {
                            self.invalidate();
                            crate::add::open(self);
                        }
                        Hit::Settings => {
                            self.invalidate();
                            crate::settings::open(self);
                        }
                        Hit::Dot(p) => self.goto_page(p),
                        Hit::Search => unsafe {
                            let _ = SetFocus(self.edit);
                        },
                        Hit::Blank => {
                            self.hide();
                        }
                    }
                } else {
                    self.invalidate();
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                self.ensure_gesture_alive();
                let pt = POINT {
                    x: get_x_lparam(lparam.0),
                    y: get_y_lparam(lparam.0),
                };
                if let Some((fl, pages)) = self.open_folder_layout() {
                    let per = fl.cells.len().max(1);
                    let fcount = self.open_folder_page_items(per, self.folder_page).len();
                    let fhit = folder_hit_test(&fl, pt.x, pt.y, fcount, pages, self.scale);
                    if fhit == FolderHit::Outside {
                        self.close_open_folder();
                    } else {
                        if let FolderHit::Cell(i) = fhit {
                            self.folder_selected = Some(self.folder_page * per + i);
                        }
                        self.open_folder_context_menu(fhit, pt);
                    }
                    return LRESULT(0);
                }
                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                if let Hit::Cell(i) = hit {
                    self.selected = self.page_cell_to_filtered(i);
                }
                self.open_context_menu(hit, pt);
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(self.hwnd, msg, wparam, lparam) },
        }
    }

    pub fn cached_icon(&self, key: &str) -> Option<IconPixels> {
        self.renderer.icons.get(key)
    }

    fn track_mouse_leave(&self) {
        unsafe {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
        }
    }

    /// 编辑框按键与输入法事件处理（子类化）。在有搜索文本且未选中图标时保留 ←/→ 光标移动能力。
    pub fn edit_key(&mut self, msg: u32, wparam: WPARAM, _lparam: LPARAM) -> bool {
        unsafe {
            if msg == WM_IME_STARTCOMPOSITION {
                self.edit_composing = true;
                self.invalidate();
                return false;
            }
            if msg == WM_IME_ENDCOMPOSITION {
                self.edit_composing = false;
                self.invalidate();
                return false;
            }
            let himc = ImmGetContext(self.edit);
            let composing = !himc.is_invalid() && ImmGetCompositionStringW(himc, GCS_COMPSTR, None, 0) > 0;
            if !himc.is_invalid() {
                let _ = ImmReleaseContext(self.edit, himc);
            }
            if composing {
                return false;
            }
            match msg {
                WM_CHAR => {
                    let ch = wparam.0 as u16;
                    match ch {
                        0x0D => {
                            if self.context_menu.is_some() {
                                self.context_menu = None;
                                self.invalidate();
                            } else {
                                self.launch_selected();
                            }
                            true
                        }
                        0x1B => {
                            if self.context_menu.is_some() {
                                self.context_menu = None;
                                self.invalidate();
                            } else if self.open_folder_id.is_some() {
                                self.close_open_folder();
                            } else {
                                self.hide();
                            }
                            true
                        }
                        _ => false,
                    }
                }
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    let vk = wparam.0 as u32;
                    let editing_text = self.open_folder_id.is_none()
                        && self.selected.is_none()
                        && !self.query().is_empty();
                    match vk {
                        x if x == VK_LEFT.0 as u32 => {
                            if editing_text {
                                return false;
                            }
                            if self.open_folder_id.is_some() {
                                self.move_folder_selection(-1, 0);
                            } else {
                                self.move_selection(-1, 0);
                            }
                        }
                        x if x == VK_RIGHT.0 as u32 => {
                            if editing_text {
                                return false;
                            }
                            if self.open_folder_id.is_some() {
                                self.move_folder_selection(1, 0);
                            } else {
                                self.move_selection(1, 0);
                            }
                        }
                        x if x == VK_DOWN.0 as u32 => {
                            if self.open_folder_id.is_some() {
                                self.move_folder_selection(0, 1);
                            } else {
                                self.move_selection(0, 1);
                            }
                        }
                        x if x == VK_UP.0 as u32 => {
                            if self.open_folder_id.is_some() {
                                self.move_folder_selection(0, -1);
                            } else {
                                self.move_selection(0, -1);
                            }
                        }
                        x if x == VK_PRIOR.0 as u32 => {
                            if self.open_folder_id.is_some() {
                                self.change_folder_page(-1);
                            } else {
                                self.change_page(-1);
                            }
                        }
                        x if x == VK_NEXT.0 as u32 => {
                            if self.open_folder_id.is_some() {
                                self.change_folder_page(1);
                            } else {
                                self.change_page(1);
                            }
                        }
                        x if x == VK_ESCAPE.0 as u32 => {
                            if self.context_menu.is_some() {
                                self.context_menu = None;
                                self.invalidate();
                            } else if self.open_folder_id.is_some() {
                                self.close_open_folder();
                            } else {
                                self.hide();
                            }
                            return true;
                        }
                        _ => return false,
                    }
                    true
                }
                _ => false,
            }
        }
    }
}

pub static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        let ptr = (*cs).lpCreateParams as isize;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr);
        if ptr != 0 {
            (*(ptr as *mut App)).hwnd = hwnd;
        }
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let app = &mut *ptr;
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.handle(msg, wparam, lparam)));
    match r {
        Ok(v) => v,
        Err(_) => {
            log::log(&format!("处理消息 0x{msg:04X} 时发生 panic"));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid: usize,
    _ref: usize,
) -> LRESULT {
    let parent = GetParent(hwnd).unwrap_or_default();
    let ptr = if !parent.is_invalid() {
        GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut App
    } else {
        std::ptr::null_mut()
    };

    if msg == WM_SETCURSOR {
        let _ = SetCursor(LoadCursorW(None, IDC_IBEAM).unwrap_or_default());
        return LRESULT(1);
    }

    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }

    if msg == WM_PAINT && !ptr.is_null() {
        let app = &mut *ptr;
        // 仅在搜索框为空且未输入法组词时，用双缓冲直接绘制居中的「🔍 搜索」，避免二次覆盖导致闪烁
        if GetWindowTextLengthW(hwnd) == 0 && !app.edit_composing {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if !hdc.is_invalid() {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let ew = rc.right - rc.left;
                let eh = rc.bottom - rc.top;
                if ew > 0 && eh > 0 && app.edit_target.ensure(ew, eh) {
                    if let Some(mdc) = app.edit_target.mem_dc {
                        let bg = app.renderer.search_bg_rgb;
                        let s = app.scale;
                        let tw = measure_text(mdc, "搜索", app.font_edit).0 as f32;
                        let icon_w = 13.0 * s;
                        let gap = 6.0 * s;
                        let total_w = icon_w + gap + tw;
                        let cap_cx = (app.layout.search.left + app.layout.search.right) as f32 * 0.5
                            - app.layout.edit.left as f32;
                        let start_x = (cap_cx - total_w * 0.5).max(2.0 * s);
                        if let Some(mut sf) = app.edit_target.surface() {
                            sf.clear((bg.0, bg.1, bg.2, 1.0));
                            let mcx = start_x + 5.2 * s;
                            let mcy = eh as f32 * 0.5;
                            let icon_col = C_TEXT_HINT;
                            sf.stroke_circle(mcx, mcy - 1.0 * s, 5.2 * s, (1.5 * s).max(1.0), icon_col);
                            sf.thick_line(
                                mcx + 3.6 * s,
                                mcy + 2.6 * s,
                                mcx + 7.4 * s,
                                mcy + 6.4 * s,
                                (1.6 * s).max(1.0),
                                icon_col,
                            );
                        }
                        let text_col = (bg.0 * 0.52 + 0.48, bg.1 * 0.52 + 0.48, bg.2 * 0.52 + 0.48, 1.0);
                        let tr = RECT {
                            left: (start_x + icon_w + gap).round() as i32,
                            top: 0,
                            right: ew,
                            bottom: eh,
                        };
                        draw_text(mdc, "搜索", app.font_edit, &tr, text_col, TextAlign::Left, false, false);
                        let _ = BitBlt(hdc, 0, 0, ew, eh, mdc, 0, 0, SRCCOPY);
                    }
                }
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        return DefSubclassProc(hwnd, msg, wparam, lparam);
    }

    if matches!(
        msg,
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_CHAR | WM_IME_STARTCOMPOSITION | WM_IME_ENDCOMPOSITION
    ) && !ptr.is_null()
    {
        let app = &mut *ptr;
        let handled = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.edit_key(msg, wparam, lparam)
        })) {
            Ok(h) => h,
            Err(_) => false,
        };
        if handled {
            return LRESULT(0);
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_app(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.into(),
            name: name.into(),
            source: Source::AppsFolder,
            target: format!("shell:AppsFolder\\{id}"),
            args: String::new(),
            aliases: Vec::new(),
            icon_key: String::new(),
            folder_id: "apps_store".into(),
            folder_name: "Microsoft Store 与常用应用".into(),
        }
    }

    #[test]
    fn grid_view_sorts_mixed_names_puts_folders_last_and_sorts_inside_folders() {
        let entries = vec![
            dummy_app("uwp:wechat", "微信"),
            dummy_app("uwp:chrome", "Chrome"),
            dummy_app("uwp:dingding", "钉钉"),
            dummy_app("uwp:zoom", "Zoom"),
            dummy_app("uwp:aqy", "爱奇艺"),
            dummy_app("uwp:calc", "计算器"),
        ];
        let mut settings = Settings::default();
        let f_work = settings.create_folder("办公").unwrap();
        let f_alpha = settings.create_folder("DevTools").unwrap();
        let f_ent = settings.create_folder("娱乐").unwrap();

        assert!(settings.move_app_to_folder("uwp:wechat", &f_work));
        assert!(settings.move_app_to_folder("uwp:chrome", &f_work));
        assert!(settings.move_app_to_folder("uwp:dingding", &f_work));

        let (view, all_apps) = build_grid_view(&entries, &settings);

        // 全部应用集合应按中英文拼音字母混合顺序排列
        let all_names: Vec<&str> = all_apps.iter().map(|v| v.entry.name.as_str()).collect();
        assert_eq!(all_names, ["爱奇艺", "Chrome", "钉钉", "计算器", "微信", "Zoom"]);

        // 独立图标排在文件夹之前，且按字母顺序排列
        assert_eq!(view.len(), 6);
        match (&view[0], &view[1], &view[2]) {
            (GridEntry::App(a), GridEntry::App(b), GridEntry::App(c)) => {
                assert_eq!(
                    [a.entry.name.as_str(), b.entry.name.as_str(), c.entry.name.as_str()],
                    ["爱奇艺", "计算器", "Zoom"]
                );
            }
            _ => panic!("前 3 项应为独立图标"),
        }

        // 文件夹排在独立图标之后，且文件夹之间按中英文拼音字母顺序排列：办公(ban) < DevTools(dev) < 娱乐(yu)
        let folder_names: Vec<&str> = view[3..]
            .iter()
            .map(|g| match g {
                GridEntry::Folder { name, .. } => name.as_str(),
                _ => panic!("后 3 项应为文件夹"),
            })
            .collect();
        assert_eq!(folder_names, ["办公", "DevTools", "娱乐"]);

        // 文件夹内部应用同样按中英文拼音字母顺序排列：Chrome < 钉钉 < 微信
        match &view[3] {
            GridEntry::Folder { id, apps, .. } => {
                assert_eq!(id, &f_work);
                let inside: Vec<&str> = apps.iter().map(|a| a.entry.name.as_str()).collect();
                assert_eq!(inside, ["Chrome", "钉钉", "微信"]);
            }
            _ => unreachable!(),
        }
        assert!(matches!(&view[4], GridEntry::Folder { id, apps, .. } if id == &f_alpha && apps.is_empty()));
        assert!(matches!(&view[5], GridEntry::Folder { id, apps, .. } if id == &f_ent && apps.is_empty()));
    }
}
