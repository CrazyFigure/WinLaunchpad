//! 主窗口与应用状态：全屏覆盖（含任务栏）、搜索框、丝滑滑动翻页、空白退出、右键菜单、手势 HUD 与子进程调度。

use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateSolidBrush, DeleteObject, EndPaint, GetMonitorInfoW,
    InvalidateRect, MonitorFromPoint, SetBkColor, SetTextColor, HBRUSH, HDC, HFONT, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
};
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
    adaptive_grid, cell_visual_rect, compute_layout, dot_rect, hit_test, ContextCmd, ContextItem,
    ContextMenu, Frame, Hit, Layout, Renderer, ViewItem, C_TEXT_HINT,
};
use crate::startup;
use crate::tray::{self, Tray};
use crate::ui::{draw_text, make_font, measure_text, rgb, DibTarget, IconPixels, TextAlign};
use crate::wallpaper;
use crate::win32::*;

pub const WM_APP_TRAY: u32 = WM_APP + 1;
pub const WM_APP_SCAN: u32 = WM_APP + 2;
pub const WM_APP_CMD: u32 = WM_APP + 4;
pub const WM_APP_LAUNCH: u32 = WM_APP + 6;
pub const WM_APP_FOCUS: u32 = WM_APP + 7;
pub const WM_APP_WALLPAPER: u32 = WM_APP + 8;
pub const WM_APP_UPDATE_EXIT: u32 = WM_APP + 10;

const TIMER_FADE: usize = 1;
const TIMER_SLIDE: usize = 2;
const TIMER_TOOLTIP: usize = 3;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SLIDE_DURATION_MS: f32 = 210.0;

#[derive(Clone, PartialEq, Eq)]
enum TipTarget {
    Cell(usize),
    Add,
    Settings,
    Dot(usize),
}

pub struct VisibleItem {
    pub entry: AppEntry,
    pub valid: bool,
}

/// 全局手势实时提示小浮窗（不抢焦点、鼠标穿透、仅约 25KB）。
struct GestureHud {
    hwnd: HWND,
    target: DibTarget,
    font: HFONT,
    text: String,
    scale: f32,
}

impl GestureHud {
    fn new() -> Self {
        Self {
            hwnd: HWND::default(),
            target: DibTarget::default(),
            font: HFONT::default(),
            text: String::new(),
            scale: 1.0,
        }
    }

    fn show_at(&mut self, hinst: HINSTANCE, owner: HWND, x: i32, y: i32, label: &str) {
        unsafe {
            if self.hwnd.is_invalid() {
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(hud_proc),
                    hInstance: hinst,
                    hbrBackground: HBRUSH::default(),
                    lpszClassName: w!("WinLaunchpadGestureHud"),
                    ..Default::default()
                };
                let _ = RegisterClassExW(&wc);
                self.hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                    w!("WinLaunchpadGestureHud"),
                    w!(""),
                    WS_POPUP,
                    0,
                    0,
                    160,
                    36,
                    owner,
                    None,
                    hinst,
                    Some(self as *mut GestureHud as *const _),
                )
                .unwrap_or_default();
                if !self.hwnd.is_invalid() {
                    enable_rounded_window(self.hwnd);
                }
            }
            if self.hwnd.is_invalid() {
                return;
            }
            let dpi = dpi_at_point(x, y);
            let scale = dpi as f32 / 96.0;
            if (self.scale - scale).abs() > 0.01 || self.font.is_invalid() {
                if !self.font.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.font.0));
                }
                self.scale = scale;
                self.font = make_font((12.5 * scale).round() as i32, 600);
            }
            self.text = label.to_string();
            let w = (168.0 * scale).round() as i32;
            let h = (34.0 * scale).round() as i32;
            let px = x - w / 2;
            let py = y - h - (22.0 * scale).round() as i32;
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                px,
                py.max(8),
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    fn hide(&mut self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.target.release();
        }
    }

    fn paint(&mut self) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        if !self.target.ensure(w, h) {
            return;
        }
        let Some(dc) = self.target.mem_dc else { return };
        let Some(mut sf) = self.target.surface() else { return };
        let s = self.scale;
        sf.clear(rgb(0x181C24, 1.0));
        sf.stroke_rrect(&RECT { left: 0, top: 0, right: w, bottom: h }, 8.0 * s, 1.0, rgb(0x3B82F6, 0.7));
        draw_text(dc, &self.text, self.font, &RECT { left: 8, top: 0, right: w - 8, bottom: h }, rgb(0xF8FAFC, 1.0), TextAlign::Center, true, false);
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let wdc = BeginPaint(self.hwnd, &mut ps);
            let _ = BitBlt(wdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
            let _ = EndPaint(self.hwnd, &mut ps);
        }
    }
}

unsafe extern "system" fn hud_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GestureHud;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let hud = &mut *ptr;
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            hud.paint();
            LRESULT(0)
        }
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
    pub view: Vec<VisibleItem>,
    pub filtered: Vec<usize>,
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
    pub menu_target: Option<usize>,
    pub taskbar_created_msg: u32,
    pub gesture: Option<gesture::GestureHandle>,
    gesture_hud: GestureHud,
    pub settings_hwnd: HWND,
    pub add_hwnd: HWND,
    pub exiting: bool,
    // 水平滑动翻页状态
    drag_active: bool,
    drag_moved: bool,
    drag_start_x: i32,
    drag_start_y: i32,
    drag_last_x: i32,
    drag_last_time: Instant,
    drag_vx: f32,
    slide_x: f32,
    slide_anim: Option<(f32, Instant)>,
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
            menu_target: None,
            taskbar_created_msg,
            gesture: None,
            gesture_hud: GestureHud::new(),
            settings_hwnd: HWND::default(),
            add_hwnd: HWND::default(),
            exiting: false,
            drag_active: false,
            drag_moved: false,
            drag_start_x: 0,
            drag_start_y: 0,
            drag_last_x: 0,
            drag_last_time: Instant::now(),
            drag_vx: 0.0,
            slide_x: 0.0,
            slide_anim: None,
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

    /// 窗口创建完成后调用：托盘、热键、图标与扫描调度、手势线程。
    pub fn post_init(&mut self) {
        self.hotkey = self.settings.hotkey;
        self.hotkey_active = hotkey::register(self.hwnd, self.hotkey);
        if !self.hotkey_active {
            log::log(&format!("全局热键 {} 注册失败（可能被占用）", self.hotkey.display()));
        }
        let mut tray = Tray::new(self.hwnd, self.hinst);
        tray.add();
        self.tray = Some(tray);
        self.icons = Some(IconWorker::start(self.hwnd, self.store.icons_dir()));
        self.gesture = gesture::start(self.hwnd);
        if self.settings.gestures_enabled {
            if let Some(g) = &self.gesture {
                g.set_enabled(true);
            }
        }
        self.rebuild_view();
        self.start_scan();
        self.ensure_wallpaper_async();
    }

    // ---------- 目录与视图 ----------

    pub fn rebuild_view(&mut self) {
        let mut view: Vec<VisibleItem> =
            Vec::with_capacity(self.scan.entries.len() + self.settings.manual.len());
        for e in self.scan.entries.iter().chain(self.settings.manual.iter()) {
            if !catalog::is_entry_visible(e, &self.settings) {
                continue;
            }
            let valid = catalog::is_entry_target_valid(e);
            view.push(VisibleItem {
                entry: e.clone(),
                valid,
            });
        }
        view.sort_by(|a, b| catalog::cmp_names(&a.entry.name, &b.entry.name));
        self.view = view;
        // 添加/删除条目或更新设置后保持当前页码，仅在超出总页数时向前收窄
        self.rebuild_filtered(false);
    }

    fn rebuild_filtered(&mut self, reset_page: bool) {
        let q = self.query().to_lowercase();
        let q = q.trim();
        self.filtered = if q.is_empty() {
            (0..self.view.len()).collect()
        } else {
            self.view
                .iter()
                .enumerate()
                .filter(|(_, v)| {
                    v.entry.name.to_lowercase().contains(q)
                        || v.entry.aliases.iter().any(|a| a.contains(q))
                })
                .map(|(i, _)| i)
                .collect()
        };
        if reset_page {
            self.page = 0;
        }
        self.selected = None;
        self.hover = None;
        self.slide_x = 0.0;
        self.slide_anim = None;
        self.slide_neighbor = None;
        self.clamp_page();
        self.position_edit();
        self.request_page_icons();
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
        self.filtered[start..self.filtered.len().min(start + per)]
            .iter()
            .map(|&i| ViewItem {
                key: self.view[i].entry.icon_key.clone(),
                name: self.view[i].entry.name.clone(),
                valid: self.view[i].valid,
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

    pub fn request_page_icons(&mut self) {
        let size = (self.layout.icon_size.max(64) as f32).clamp(48.0, 160.0) as u32;
        for item in self.page_items() {
            self.ensure_icon_loaded(&item.key, size);
        }
        if self.page + 1 < self.pages() {
            for item in self.items_for_page(self.page + 1) {
                self.ensure_icon_loaded(&item.key, size);
            }
        }
        if self.page > 0 {
            for item in self.items_for_page(self.page - 1) {
                self.ensure_icon_loaded(&item.key, size);
            }
        }
    }

    pub fn request_icon_async(&mut self, key: &str) {
        let size = (64.0 * self.scale.max(1.0)).round().clamp(48.0, 144.0) as u32;
        self.ensure_icon_loaded(key, size);
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

    pub fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
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
        if self.visible {
            self.invalidate();
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
        let prev_sel = self
            .selected
            .and_then(|i| self.filtered.get(i))
            .map(|&vi| self.view[vi].entry.id.clone());
        self.scanning = false;
        if !r.entries.is_empty() || self.scan.entries.is_empty() {
            self.scan = r;
            let cache = CatalogCache {
                version: 1,
                scanned_at: self.scan.scanned_at,
                entries: self.scan.entries.clone(),
            };
            self.store.save_catalog(&cache);
            self.rebuild_view();
            if let Some(id) = prev_sel {
                if let Some(pos) = self.filtered.iter().position(|&vi| self.view[vi].entry.id == id) {
                    self.selected = Some(pos);
                    self.page = pos / self.per_page();
                }
            }
        }
        if !self.scan.errors.is_empty() {
            log::log(&format!(
                "扫描完成，{} 个条目，{} 个来源问题",
                self.scan.entries.len(),
                self.scan.errors.len()
            ));
        }
        self.request_page_icons();
        if self.visible {
            self.invalidate();
        }
        unsafe {
            if !self.settings_hwnd.is_invalid() && IsWindow(self.settings_hwnd).as_bool() {
                let _ = PostMessageW(self.settings_hwnd, WM_APP_SCAN, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn on_icon(&mut self, r: IconResult) {
        let Some(px) = r.pixels else { return };
        self.renderer.add_icon(&r.key, &px, r.w, r.h);
        if self.visible && self.page_items().iter().any(|i| i.key == r.key) {
            self.invalidate_grid();
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

    pub fn show(&mut self) {
        if self.exiting {
            return;
        }
        self.ensure_wallpaper_async();
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
            let _ = SetLayeredWindowAttributes(self.hwnd, COLORREF(0), 0, LWA_ALPHA);
            self.renderer.ensure_target(self.hwnd, w as u32, h as u32, self.scale);
            self.visible = true;
            self.alpha = 0;
            self.fading = true;
            self.slide_x = 0.0;
            self.slide_anim = None;
            self.slide_neighbor = None;
            self.context_menu = None;
            self.tip_pending = None;
            self.tip_visible = None;
            let _ = SetTimer(self.hwnd, TIMER_FADE, 15, None);
            self.selected = None;
            self.hover = None;
            self.pressed = None;
            self.pressed_cell = None;
            self.clamp_page();
            self.request_page_icons();
            self.invalidate();
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
            if self.drag_active {
                self.drag_active = false;
                self.drag_moved = false;
                let _ = ReleaseCapture();
            }
            self.fading = false;
            self.visible = false;
            self.alpha = 255;
            self.context_menu = None;
            self.tip_pending = None;
            self.tip_visible = None;
            self.slide_x = 0.0;
            self.slide_anim = None;
            self.slide_neighbor = None;
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

    fn page_index_to_view(&self, i: usize) -> Option<usize> {
        self.filtered.get(self.page * self.per_page() + i).copied()
    }

    fn launch_view_index(&mut self, vi: usize) {
        let Some(v) = self.view.get(vi) else { return };
        let entry = v.entry.clone();
        let valid = v.valid;
        if !valid {
            let name = entry.name.clone();
            self.hide();
            self.show_error(&format!(
                "找不到「{name}」的目标文件，可能已被卸载。\n可以在设置「应用与文件夹 → 检测过期图标」中清理。"
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

    fn launch_selected(&mut self) {
        let idx = self.selected.or(if self.filtered.is_empty() { None } else { Some(0) });
        if let Some(i) = idx {
            if let Some(vi) = self.filtered.get(i).copied() {
                self.launch_view_index(vi);
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

    fn remove_view_index(&mut self, vi: usize) {
        let Some(v) = self.view.get(vi) else { return };
        let id = v.entry.id.clone();
        let source = v.entry.source;
        if source == Source::Manual {
            self.settings.manual.retain(|m| m.id != id);
        } else {
            self.settings.app_overrides.insert(id, false);
        }
        if !self.store.save_settings(&self.settings) {
            self.show_error("保存设置失败，本次移除在重启后可能失效。");
        }
        self.rebuild_view();
        self.invalidate();
    }

    fn open_location_view_index(&mut self, vi: usize) {
        let Some(v) = self.view.get(vi) else { return };
        let target = v.entry.target.clone();
        if target.starts_with("shell:") {
            return;
        }
        self.hide();
        open_shell_path("open", "explorer.exe", Some(&format!("/select,\"{target}\"")));
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
        self.request_page_icons();
        self.start_slide_anim();
        self.invalidate_grid();
    }

    fn start_slide_anim(&mut self) {
        self.slide_anim = Some((self.slide_x, Instant::now()));
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_SLIDE, 10, None);
        }
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

    fn update_tooltip_target(&mut self, hit: Hit) {
        let next: Option<(TipTarget, RECT, String)> = match hit {
            Hit::Add => Some((TipTarget::Add, self.layout.add_btn, "添加应用".into())),
            Hit::Settings => Some((TipTarget::Settings, self.layout.settings_btn, "打开设置".into())),
            Hit::Dot(i) => Some((
                TipTarget::Dot(i),
                dot_rect(&self.layout, i, self.pages(), self.scale),
                format!("第 {} 页", i + 1),
            )),
            Hit::Cell(i) => {
                if let Some(vi) = self.page_index_to_view(i) {
                    let name = &self.view[vi].entry.name;
                    if self.renderer.is_label_truncated(name, &self.layout) || !self.view[vi].valid {
                        let vr = cell_visual_rect(&self.layout.cells[i], self.layout.icon_size, self.scale);
                        let tip = if self.view[vi].valid {
                            name.clone()
                        } else {
                            format!("{name}（目标文件已失效）")
                        };
                        Some((TipTarget::Cell(i), vr, tip))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
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

    fn open_context_menu(&mut self, hit: Hit, pt: POINT) {
        self.cancel_tooltip();
        self.menu_target = match hit {
            Hit::Cell(i) => self.page_index_to_view(i),
            _ => None,
        };
        let mut items = Vec::new();
        if let Some(vi) = self.menu_target {
            items.push(ContextItem { cmd: ContextCmd::Open, label: "打开", danger: false });
            if !self.view[vi].entry.target.starts_with("shell:") {
                items.push(ContextItem { cmd: ContextCmd::OpenLocation, label: "打开文件位置", danger: false });
            }
            items.push(ContextItem { cmd: ContextCmd::Remove, label: "从启动台移除", danger: true });
        } else {
            items.push(ContextItem { cmd: ContextCmd::Add, label: "添加应用…", danger: false });
            items.push(ContextItem { cmd: ContextCmd::Rescan, label: "重新扫描应用", danger: false });
            items.push(ContextItem { cmd: ContextCmd::Settings, label: "设置…", danger: false });
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
                if let Some(v) = self.menu_target.take() {
                    self.launch_view_index(v);
                }
            }
            ContextCmd::OpenLocation => {
                if let Some(v) = self.menu_target.take() {
                    self.open_location_view_index(v);
                }
            }
            ContextCmd::Remove => {
                if let Some(v) = self.menu_target.take() {
                    self.remove_view_index(v);
                }
            }
            ContextCmd::Add => crate::add::open(self),
            ContextCmd::Rescan => self.start_scan(),
            ContextCmd::Settings => crate::settings::open(self),
        }
    }

    fn on_menu_command(&mut self, id: u32) {
        match id {
            tray::CMD_TOGGLE => self.toggle(),
            tray::CMD_RESCAN => self.start_scan(),
            tray::CMD_SETTINGS => crate::settings::open(self),
            tray::CMD_STARTUP => self.toggle_startup(),
            tray::CMD_EXIT => self.exit(),
            _ => {}
        }
    }

    fn toggle_startup(&mut self) {
        let enable = startup::state() != startup::StartupState::Enabled;
        match startup::set_enabled(enable) {
            Ok(_) => {
                self.settings.start_with_windows = enable;
                self.store.save_settings(&self.settings);
            }
            Err(e) => self.show_error(&e),
        }
    }

    fn on_tray(&mut self, msg: u32) {
        match msg {
            WM_LBUTTONUP => self.toggle(),
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                self.in_menu = true;
                if let Some(t) = &self.tray {
                    let enabled = startup::state() == startup::StartupState::Enabled;
                    t.show_menu(enabled, self.visible);
                }
                self.in_menu = false;
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
        if let Some(g) = &self.gesture {
            g.set_enabled(enabled);
        } else if enabled {
            log::log("手势线程不可用，无法启用手势");
        }
    }

    pub fn exit(&mut self) {
        if self.exiting {
            return;
        }
        self.exiting = true;
        self.gesture_hud.hide();
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
        let status = if self.scanning {
            "正在扫描已安装的应用…".to_string()
        } else if !self.scan.errors.is_empty() {
            format!("{} 个来源读取失败（详见日志）", self.scan.errors.len())
        } else {
            String::new()
        };
        let q = self.query();
        let empty_text = if self.scanning && self.view.is_empty() {
            "正在扫描应用…".to_string()
        } else if self.view.is_empty() {
            "暂无展示的应用，右键点击可添加或前往设置勾选文件夹".to_string()
        } else if !q.trim().is_empty() {
            format!("没有找到与「{}」匹配的应用", q.trim())
        } else {
            String::new()
        };
        let tip = self.tip_visible.as_ref().map(|(r, s)| (*r, s.as_str()));
        let frame = Frame {
            items: &items,
            neighbor_items: &neighbor_items,
            neighbor_offset,
            slide_x: self.slide_x,
            hover: self.hover,
            pressed_cell: self.pressed_cell,
            selected: self
                .selected
                .map(|s| s.saturating_sub(self.page * self.per_page()))
                .filter(|s| *s < self.per_page()),
            page: self.page,
            pages: self.pages(),
            query_empty: q.is_empty() && !self.edit_composing,
            status: &status,
            empty_text: &empty_text,
            hover_btn: self.hover_btn,
            pressed_btn: self.pressed,
            tooltip: tip,
            context_menu: self.context_menu.as_ref(),
            use_wallpaper: self.settings.use_wallpaper_blur,
            icon_roundness: self.settings.icon_roundness,
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
                        self.alpha = self.alpha.saturating_add(64);
                        unsafe {
                            let _ = SetLayeredWindowAttributes(
                                self.hwnd,
                                COLORREF(0),
                                self.alpha,
                                LWA_ALPHA,
                            );
                            if self.alpha >= 255 {
                                self.fading = false;
                                let _ = KillTimer(self.hwnd, TIMER_FADE);
                            }
                        }
                    }
                    TIMER_SLIDE => {
                        if let Some((start_x, t0)) = self.slide_anim {
                            let elapsed = t0.elapsed().as_secs_f32() * 1000.0;
                            let p = (elapsed / SLIDE_DURATION_MS).clamp(0.0, 1.0);
                            let inv = 1.0 - p;
                            let ease_rem = inv * inv * inv;
                            self.slide_x = start_x * ease_rem;
                            if p >= 1.0 || self.slide_x.abs() < 1.0 {
                                self.slide_x = 0.0;
                                self.slide_anim = None;
                                self.slide_neighbor = None;
                                unsafe {
                                    let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                                }
                            }
                        } else {
                            self.slide_x = 0.0;
                            self.slide_neighbor = None;
                            unsafe {
                                let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                            }
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
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_TRAY => {
                let m = loword(lparam.0 as usize) as u32;
                self.on_tray(m);
                LRESULT(0)
            }
            WM_APP_SCAN => {
                let r = unsafe { *Box::from_raw(lparam.0 as *mut ScanResult) };
                self.on_scan_done(r);
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
            WM_APP_CMD => {
                match wparam.0 {
                    2 => self.show(),
                    3 => crate::settings::open(self),
                    4 => crate::add::open(self),
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
                if let Some(g) = Gesture::from_id(wparam.0) {
                    let x = get_x_lparam(lparam.0);
                    let y = get_y_lparam(lparam.0);
                    let lbl = g.hud_label(self.settings.gesture_swap_lr);
                    self.gesture_hud.show_at(self.hinst, self.hwnd, x, y, lbl);
                } else {
                    self.gesture_hud.hide();
                }
                LRESULT(0)
            }
            gesture::WM_APP_GESTURE => {
                self.gesture_hud.hide();
                if let Some(g) = Gesture::from_id(wparam.0) {
                    if g == Gesture::UpDown {
                        self.toggle();
                    } else {
                        gesture::perform(g, self.settings.gesture_swap_lr);
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
                if self.drag_active {
                    let dx = pt.x - self.drag_start_x;
                    let dy = pt.y - self.drag_start_y;
                    let thresh = (8.0 * self.scale).round() as i32;
                    if !self.drag_moved && dx.abs() >= thresh && dx.abs() >= dy.abs() {
                        self.drag_moved = true;
                        self.pressed = None;
                        self.pressed_cell = None;
                        self.hover = None;
                        self.hover_btn = None;
                        self.slide_anim = None;
                        self.cancel_tooltip();
                        unsafe {
                            let _ = KillTimer(self.hwnd, TIMER_SLIDE);
                        }
                        self.request_page_icons();
                    }
                    if self.drag_moved {
                        let now = Instant::now();
                        let dt = (now - self.drag_last_time).as_secs_f32().max(0.001);
                        let inst_vx = (pt.x - self.drag_last_x) as f32 / dt;
                        self.drag_vx = self.drag_vx * 0.5 + inst_vx * 0.5;
                        self.drag_last_x = pt.x;
                        self.drag_last_time = now;

                        let pages = self.pages();
                        let span = self.layout.grid_span_w.max(1);
                        let mut off = dx as f32;
                        if (off > 0.0 && self.page == 0) || (off < 0.0 && self.page + 1 >= pages) {
                            off *= 0.25;
                        }
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
                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                let hover = match hit {
                    Hit::Cell(i) => Some(i),
                    _ => None,
                };
                let hover_btn = match hit {
                    Hit::Add | Hit::Settings | Hit::Dot(_) => Some(hit),
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
                if self.hover.is_some() || self.hover_btn.is_some() {
                    self.hover = None;
                    self.hover_btn = None;
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
                self.change_page(if delta > 0 { -1 } else { 1 });
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
                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                self.pressed = Some(hit);
                self.pressed_cell = match hit {
                    Hit::Cell(i) => Some(i),
                    _ => None,
                };
                if matches!(hit, Hit::Cell(_) | Hit::Blank) {
                    self.drag_active = true;
                    self.drag_moved = false;
                    self.drag_start_x = pt.x;
                    self.drag_start_y = pt.y;
                    self.drag_last_x = pt.x;
                    self.drag_last_time = Instant::now();
                    self.drag_vx = 0.0;
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
                if self.drag_active {
                    self.drag_active = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    if self.drag_moved {
                        self.drag_moved = false;
                        let pages = self.pages();
                        let span = self.layout.grid_span_w.max(1);
                        let dist_thresh = (self.layout.w as f32 * 0.10).max(70.0 * self.scale);
                        let flick_vx = 420.0 * self.scale;
                        let go_next = (self.slide_x < -dist_thresh
                            || (self.drag_vx < -flick_vx && self.slide_x < -16.0 * self.scale))
                            && self.page + 1 < pages;
                        let go_prev = (self.slide_x > dist_thresh
                            || (self.drag_vx > flick_vx && self.slide_x > 16.0 * self.scale))
                            && self.page > 0;
                        if go_next {
                            let old = self.page;
                            self.page += 1;
                            self.selected = None;
                            self.slide_neighbor = Some((old, -span));
                            self.slide_x += span as f32;
                            self.request_page_icons();
                        } else if go_prev {
                            let old = self.page;
                            self.page -= 1;
                            self.selected = None;
                            self.slide_neighbor = Some((old, span));
                            self.slide_x -= span as f32;
                            self.request_page_icons();
                        }
                        self.start_slide_anim();
                        self.invalidate();
                        return LRESULT(0);
                    }
                }

                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                let pressed = self.pressed.take();
                self.pressed_cell = None;
                if pressed.is_some() && pressed == Some(hit) {
                    match hit {
                        Hit::Cell(i) => {
                            self.selected = Some(self.page * self.per_page() + i);
                            if let Some(vi) = self.page_index_to_view(i) {
                                self.launch_view_index(vi);
                            }
                        }
                        Hit::Add => crate::add::open(self),
                        Hit::Settings => crate::settings::open(self),
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
                let pt = POINT {
                    x: get_x_lparam(lparam.0),
                    y: get_y_lparam(lparam.0),
                };
                let page_count = self.page_items().len();
                let pages = self.pages();
                let hit = hit_test(&self.layout, pt.x, pt.y, page_count, pages, self.scale);
                if let Hit::Cell(i) = hit {
                    self.selected = Some(self.page * self.per_page() + i);
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
                    let editing_text = self.selected.is_none() && !self.query().is_empty();
                    match vk {
                        x if x == VK_LEFT.0 as u32 => {
                            if editing_text {
                                return false;
                            }
                            self.move_selection(-1, 0);
                        }
                        x if x == VK_RIGHT.0 as u32 => {
                            if editing_text {
                                return false;
                            }
                            self.move_selection(1, 0);
                        }
                        x if x == VK_DOWN.0 as u32 => self.move_selection(0, 1),
                        x if x == VK_UP.0 as u32 => self.move_selection(0, -1),
                        x if x == VK_PRIOR.0 as u32 => self.change_page(-1),
                        x if x == VK_NEXT.0 as u32 => self.change_page(1),
                        x if x == VK_ESCAPE.0 as u32 => {
                            if self.context_menu.is_some() {
                                self.context_menu = None;
                                self.invalidate();
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
