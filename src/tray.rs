//! 托盘图标与右键菜单。

use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, DeleteObject, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint,
    HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_END,
    VK_ESCAPE, VK_HOME, VK_RETURN, VK_SPACE, VK_UP,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow,
    GetSystemMetrics, GetWindowLongPtrW, IsWindow, LoadCursorW, LoadImageW, PostMessageW,
    RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HICON, IDC_ARROW, IMAGE_FLAGS, IMAGE_ICON, SM_CXSMICON,
    SW_SHOW, WA_INACTIVE, WM_ACTIVATE, WM_CANCELMODE, WM_COMMAND, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::ui::{draw_text, make_font, measure_text, rgb, DibTarget, Surface, TextAlign};
use crate::win32::{
    dpi_at_point, enable_rounded_window, get_x_lparam, get_y_lparam, loword, wide,
};

pub const CMD_TOGGLE: u32 = 1;
pub const CMD_RESCAN: u32 = 2;
pub const CMD_SETTINGS: u32 = 3;
pub const CMD_EXIT: u32 = 5;

// 托盘菜单布局与视觉常量（dp 基准，按光标所在显示器 DPI 等比缩放）
const MENU_PAD_X_DP: f32 = 5.0;
const MENU_PAD_Y_DP: f32 = 5.0;
const MENU_ITEM_H_DP: f32 = 28.0;
const MENU_ITEM_GAP_DP: f32 = 2.0;
const MENU_TEXT_PAD_X_DP: f32 = 12.0;
const MENU_EXTRA_RIGHT_DP: f32 = 16.0;
const MENU_MIN_W_DP: f32 = 136.0;
const MENU_FONT_DP: f32 = 13.0;
const MENU_ITEM_RADIUS_DP: f32 = 5.0;
const MENU_BORDER_RADIUS_DP: f32 = 8.0;

#[derive(Clone, Copy)]
struct TrayMenuItem {
    cmd: u32,
    label: &'static str,
}

fn build_menu_items(visible: bool) -> [TrayMenuItem; 4] {
    [
        TrayMenuItem {
            cmd: CMD_TOGGLE,
            label: if visible { "收起启动台" } else { "打开启动台" },
        },
        TrayMenuItem {
            cmd: CMD_RESCAN,
            label: "刷新应用图标",
        },
        // 托盘菜单仅保留常用操作入口，开机启动统一归入设置页管理
        TrayMenuItem {
            cmd: CMD_SETTINGS,
            label: "设置",
        },
        TrayMenuItem {
            cmd: CMD_EXIT,
            label: "退出",
        },
    ]
}

/// 托盘右键弹出菜单的纯几何布局，便于独立测试与 DPI 缩放计算。
#[derive(Clone, Debug)]
struct MenuLayout {
    w: i32,
    h: i32,
    item_rects: Vec<RECT>,
    hit_rects: Vec<RECT>,
    text_pad_x: i32,
    item_radius: f32,
    border_radius: f32,
}

impl MenuLayout {
    fn build(max_text_w: i32, count: usize, scale: f32) -> Self {
        let px = (MENU_PAD_X_DP * scale).round() as i32;
        let py = (MENU_PAD_Y_DP * scale).round() as i32;
        let item_h = (MENU_ITEM_H_DP * scale).round().max(1.0) as i32;
        let gap = (MENU_ITEM_GAP_DP * scale).round() as i32;
        let tpx = (MENU_TEXT_PAD_X_DP * scale).round() as i32;
        let extra_r = (MENU_EXTRA_RIGHT_DP * scale).round() as i32;
        let min_w = (MENU_MIN_W_DP * scale).round() as i32;

        let w = (max_text_w + (px + tpx) * 2 + extra_r).max(min_w);
        let h = py * 2 + item_h * count as i32 + gap * count.saturating_sub(1) as i32;

        let mut item_rects = Vec::with_capacity(count);
        let mut hit_rects = Vec::with_capacity(count);
        for i in 0..count {
            let top = py + i as i32 * (item_h + gap);
            let bottom = top + item_h;
            item_rects.push(RECT {
                left: px,
                top,
                right: w - px,
                bottom,
            });
            // 将相邻项间距均分纳入命中区，消除鼠标跨越 2dp 间隙时的悬停闪烁
            let hit_top = if i == 0 { 0 } else { top - gap / 2 };
            let hit_bottom = if i + 1 == count { h } else { bottom + (gap - gap / 2) };
            hit_rects.push(RECT {
                left: 0,
                top: hit_top,
                right: w,
                bottom: hit_bottom,
            });
        }

        MenuLayout {
            w,
            h,
            item_rects,
            hit_rects,
            text_pad_x: tpx,
            item_radius: MENU_ITEM_RADIUS_DP * scale,
            border_radius: MENU_BORDER_RADIUS_DP * scale,
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        self.hit_rects
            .iter()
            .position(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
    }
}

/// 依据光标位置与所在显示器边界计算弹窗坐标，空间不足时自动向左上翻转并限制在屏内。
fn place_menu_popup(cursor: POINT, mon: RECT, w: i32, h: i32) -> POINT {
    let x = if cursor.x + w <= mon.right {
        cursor.x
    } else if cursor.x - w >= mon.left {
        cursor.x - w
    } else {
        (mon.right - w).max(mon.left)
    };
    let y = if cursor.y + h <= mon.bottom {
        cursor.y
    } else if cursor.y - h >= mon.top {
        cursor.y - h
    } else {
        (mon.bottom - h).max(mon.top)
    };
    POINT {
        x: x.clamp(mon.left, (mon.right - w).max(mon.left)),
        y: y.clamp(mon.top, (mon.bottom - h).max(mon.top)),
    }
}

/// 将托盘右键菜单绘制到指定 Surface 与 DC（兼顾 Win11 亮色质感与 Hover/Active 反馈）。
fn render_menu_surface(
    sf: &mut Surface,
    dc: HDC,
    font: HFONT,
    items: &[TrayMenuItem],
    layout: &MenuLayout,
    active_idx: Option<usize>,
    pressed_idx: Option<usize>,
    scale: f32,
) {
    sf.clear(rgb(0xF9F9F9, 1.0));
    for (i, item) in items.iter().enumerate() {
        let ir = &layout.item_rects[i];
        if pressed_idx == Some(i) {
            sf.fill_rrect(ir, layout.item_radius, rgb(0xE4E4E4, 1.0));
        } else if active_idx == Some(i) {
            sf.fill_rrect(ir, layout.item_radius, rgb(0xEFEFEF, 1.0));
        }
        let tr = RECT {
            left: ir.left + layout.text_pad_x,
            top: ir.top,
            right: ir.right - layout.text_pad_x,
            bottom: ir.bottom,
        };
        draw_text(
            dc,
            item.label,
            font,
            &tr,
            rgb(0x1A1A1A, 1.0),
            TextAlign::Left,
            true,
            false,
        );
    }
    sf.stroke_rrect(
        &RECT {
            left: 0,
            top: 0,
            right: layout.w,
            bottom: layout.h,
        },
        layout.border_radius,
        (1.0 * scale).max(1.0),
        rgb(0xE5E5E5, 1.0),
    );
}

struct MenuState {
    hwnd: HWND,
    owner: HWND,
    items: [TrayMenuItem; 4],
    layout: MenuLayout,
    scale: f32,
    target: DibTarget,
    font: HFONT,
    hover: Option<usize>,
    pressed: Option<usize>,
    kbd_sel: Option<usize>,
    dispatched: bool,
    closing: bool,
}

impl Drop for MenuState {
    fn drop(&mut self) {
        self.target.release();
        if !self.font.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            self.font = HFONT::default();
        }
    }
}

impl MenuState {
    fn close_window(&mut self) {
        if !self.closing {
            self.closing = true;
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    fn dispatch_and_close(&mut self, idx: usize) {
        if !self.dispatched {
            if let Some(item) = self.items.get(idx) {
                self.dispatched = true;
                unsafe {
                    let _ = PostMessageW(self.owner, WM_COMMAND, WPARAM(item.cmd as usize), LPARAM(0));
                }
            }
        }
        self.close_window();
    }

    fn paint(&mut self) {
        let w = self.layout.w;
        let h = self.layout.h;
        if !self.target.ensure(w, h) {
            return;
        }
        let Some(dc) = self.target.mem_dc else { return };
        let Some(mut sf) = self.target.surface() else { return };
        let active_idx = self.hover.or(self.kbd_sel);
        render_menu_surface(
            &mut sf,
            dc,
            self.font,
            &self.items,
            &self.layout,
            active_idx,
            self.pressed,
            self.scale,
        );
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let wdc = BeginPaint(self.hwnd, &mut ps);
            let _ = BitBlt(wdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
            let _ = EndPaint(self.hwnd, &ps);
        }
    }
}

fn register_menu_class(hinst: HINSTANCE) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(menu_wnd_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: w!("WinLaunchpadTrayMenu"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

unsafe extern "system" fn menu_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
        if !(*cs).lpCreateParams.is_null() {
            (*((*cs).lpCreateParams as *mut MenuState)).hwnd = hwnd;
        }
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MenuState;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &mut *ptr;
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_menu(state, hwnd, msg, wparam, lparam)
    }));
    let ret = match res {
        Ok(v) => v,
        Err(_) => {
            crate::log::log("托盘菜单处理消息时 panic");
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    };
    // 释放菜单资源前先将句柄指针摘除并延迟到 WM_NCDESTROY 统一回收，避免销毁期重入访问
    if msg == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(ptr));
    }
    ret
}

fn handle_menu(
    state: &mut MenuState,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                state.paint();
                LRESULT(0)
            }
            // 点击菜单外部或切走前台窗口时自动收起托盘菜单
            WM_ACTIVATE => {
                if loword(wparam.0) as u32 == WA_INACTIVE {
                    state.close_window();
                }
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                state.close_window();
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let hit = state.layout.hit(get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                if hit != state.hover || (hit.is_some() && state.kbd_sel.is_some()) {
                    state.hover = hit;
                    if hit.is_some() {
                        state.kbd_sel = None;
                    }
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                if state.hover.is_some() || state.pressed.is_some() {
                    state.hover = None;
                    state.pressed = None;
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let hit = state.layout.hit(get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                state.pressed = hit;
                if hit.is_some() {
                    let _ = SetCapture(hwnd);
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let pressed = state.pressed.take();
                if pressed.is_some() {
                    let _ = ReleaseCapture();
                }
                let hit = state.layout.hit(get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                if pressed.is_some() && pressed == hit {
                    if let Some(idx) = hit {
                        state.dispatch_and_close(idx);
                        return LRESULT(0);
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_CANCELMODE => {
                if state.pressed.take().is_some() {
                    let _ = ReleaseCapture();
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u32;
                let n = state.items.len();
                match vk {
                    x if x == VK_ESCAPE.0 as u32 => {
                        state.close_window();
                    }
                    x if x == VK_DOWN.0 as u32 => {
                        let cur = state.kbd_sel.or(state.hover);
                        state.hover = None;
                        state.kbd_sel = Some(cur.map_or(0, |i| (i + 1) % n));
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    x if x == VK_UP.0 as u32 => {
                        let cur = state.kbd_sel.or(state.hover);
                        state.hover = None;
                        state.kbd_sel = Some(cur.map_or(n - 1, |i| (i + n - 1) % n));
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    x if x == VK_HOME.0 as u32 => {
                        state.hover = None;
                        state.kbd_sel = Some(0);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    x if x == VK_END.0 as u32 => {
                        state.hover = None;
                        state.kbd_sel = Some(n - 1);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    x if x == VK_RETURN.0 as u32 || x == VK_SPACE.0 as u32 => {
                        if let Some(idx) = state.kbd_sel.or(state.hover) {
                            state.dispatch_and_close(idx);
                            return LRESULT(0);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                state.closing = true;
                state.target.release();
                if !state.font.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(state.font.0));
                    state.font = HFONT::default();
                }
                let _ = PostMessageW(
                    state.owner,
                    crate::app::WM_APP_TRAY_MENU_CLOSED,
                    WPARAM(hwnd.0 as usize),
                    LPARAM(0),
                );
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

pub struct Tray {
    pub hwnd: HWND,
    pub hinst: HINSTANCE,
    pub hicon: HICON,
    pub added: bool,
    pub menu_hwnd: HWND,
}

impl Tray {
    pub fn new(hwnd: HWND, hinst: HINSTANCE) -> Tray {
        let sz = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let hicon = unsafe {
            LoadImageW(
                hinst,
                PCWSTR(1usize as *const u16),
                IMAGE_ICON,
                sz,
                sz,
                IMAGE_FLAGS(0x8000), // LR_DEFAULTCOLOR
            )
            .map(|h| HICON(h.0))
            .unwrap_or_default()
        };
        Tray {
            hwnd,
            hinst,
            hicon,
            added: false,
            menu_hwnd: HWND::default(),
        }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut tip = [0u16; 128];
        for (i, c) in wide("WinLaunchpad 启动台").into_iter().take(127).enumerate() {
            tip[i] = c;
        }
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: crate::app::WM_APP_TRAY,
            hIcon: self.hicon,
            szTip: tip,
            ..Default::default()
        }
    }

    pub fn add(&mut self) {
        unsafe {
            if self.added {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data());
            } else {
                self.added = Shell_NotifyIconW(NIM_ADD, &self.data()).as_bool();
            }
        }
    }

    pub fn remove(&mut self) {
        self.close_menu();
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
            self.added = false;
        }
    }

    /// 托盘右键菜单当前是否处于打开状态。
    pub fn is_menu_open(&self) -> bool {
        !self.menu_hwnd.is_invalid() && unsafe { IsWindow(self.menu_hwnd).as_bool() }
    }

    /// 若托盘右键菜单仍打开则立即销毁。
    pub fn close_menu(&mut self) {
        if self.is_menu_open() {
            unsafe {
                let _ = DestroyWindow(self.menu_hwnd);
            }
        }
        self.menu_hwnd = HWND::default();
    }

    /// 菜单窗口销毁回调到来时，仅清理匹配句柄避免误伤新打开的菜单。
    pub fn on_menu_closed(&mut self, hwnd: HWND) {
        if self.menu_hwnd == hwnd {
            self.menu_hwnd = HWND::default();
        }
    }

    /// 弹出自绘圆角右键菜单。选择结果通过 WM_COMMAND（lparam == 0）送到主窗口。
    pub fn show_menu(&mut self, visible: bool) -> bool {
        self.close_menu();
        register_menu_class(self.hinst);

        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let dpi = dpi_at_point(pt.x, pt.y);
            let scale = dpi as f32 / 96.0;

            let items = build_menu_items(visible);
            let font = make_font((MENU_FONT_DP * scale).round() as i32, 400);

            // 预创建测算与绘制用 DIB 表面，保证弹出时直接精确贴合文本与尺寸
            let mut target = DibTarget::default();
            let max_text_w = if target.ensure(16, 16) {
                if let Some(dc) = target.mem_dc {
                    items
                        .iter()
                        .map(|it| measure_text(dc, it.label, font).0)
                        .max()
                        .unwrap_or(0)
                } else {
                    0
                }
            } else {
                0
            };

            let layout = MenuLayout::build(max_text_w, items.len(), scale);
            let _ = target.ensure(layout.w, layout.h);

            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let pos = place_menu_popup(pt, mi.rcMonitor, layout.w, layout.h);

            let state = Box::new(MenuState {
                hwnd: HWND::default(),
                owner: self.hwnd,
                items,
                layout: layout.clone(),
                scale,
                target,
                font,
                hover: None,
                pressed: None,
                kbd_sel: None,
                dispatched: false,
                closing: false,
            });
            let state_ptr = Box::into_raw(state);

            let menu_hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("WinLaunchpadTrayMenu"),
                w!(""),
                WS_POPUP,
                pos.x,
                pos.y,
                layout.w,
                layout.h,
                self.hwnd,
                None,
                self.hinst,
                Some(state_ptr as *const _),
            )
            .unwrap_or_default();

            if menu_hwnd.is_invalid() {
                drop(Box::from_raw(state_ptr));
                return false;
            }

            self.menu_hwnd = menu_hwnd;
            enable_rounded_window(menu_hwnd);
            let _ = ShowWindow(menu_hwnd, SW_SHOW);
            let _ = SetForegroundWindow(menu_hwnd);
            if GetForegroundWindow() != menu_hwnd {
                let _ = SetForegroundWindow(self.hwnd);
                let _ = SetForegroundWindow(menu_hwnd);
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_layout_has_comfortable_row_height_and_continuous_hit_areas() {
        let layout = MenuLayout::build(96, 4, 2.0);
        assert_eq!(layout.item_rects.len(), 4);
        assert_eq!(layout.hit_rects.len(), 4);
        // 200% DPI 下每项高亮块高度 56px（28dp）、步进 60px（30dp），相比系统原生 ~42px 更舒展且保持紧致
        let item0_h = layout.item_rects[0].bottom - layout.item_rects[0].top;
        assert_eq!(item0_h, 56);
        let step = layout.item_rects[1].top - layout.item_rects[0].top;
        assert_eq!(step, 60);
        assert_eq!(layout.hit_rects[0].bottom, layout.hit_rects[1].top);
        assert_eq!(layout.hit_rects[1].bottom, layout.hit_rects[2].top);
        assert_eq!(layout.hit_rects[2].bottom, layout.hit_rects[3].top);
        assert_eq!(layout.hit(layout.w / 2, layout.hit_rects[0].bottom - 1), Some(0));
        assert_eq!(layout.hit(layout.w / 2, layout.hit_rects[1].top), Some(1));
    }

    #[test]
    fn tray_menu_popup_flips_when_near_screen_edges() {
        let mon = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert_eq!(place_menu_popup(POINT { x: 100, y: 200 }, mon, 300, 280), POINT { x: 100, y: 200 });
        // 右下角托盘附近空间不足时向左上翻转
        assert_eq!(
            place_menu_popup(POINT { x: 1850, y: 1050 }, mon, 300, 280),
            POINT { x: 1550, y: 770 }
        );
    }

    #[test]
    fn tray_menu_renders_light_surface_with_hover_and_text() {
        let scale = 2.0f32;
        let items = build_menu_items(false);
        let font = make_font((MENU_FONT_DP * scale).round() as i32, 400);
        let mut target = DibTarget::default();
        assert!(target.ensure(16, 16));
        let max_text_w = items
            .iter()
            .map(|it| measure_text(target.mem_dc.unwrap(), it.label, font).0)
            .max()
            .unwrap_or(0);
        let layout = MenuLayout::build(max_text_w, items.len(), scale);
        assert!(target.ensure(layout.w, layout.h));

        {
            let dc = target.mem_dc.unwrap();
            let mut sf = target.surface().unwrap();
            render_menu_surface(&mut sf, dc, font, &items, &layout, Some(0), None, scale);
        }

        let px = unsafe {
            std::slice::from_raw_parts(target.ptr as *const u32, (layout.w * layout.h) as usize)
        };
        // 底部空白项内部像素应为悬停外背景 #F9F9F9，第一项内部非文字区域应呈悬停灰 #EFEFEF
        let bg_sample = px[(layout.h as usize - 6) * layout.w as usize + layout.w as usize - 16];
        assert_eq!(bg_sample & 0x00FF_FFFF, 0x00F9_F9F9);
        let hov_y = (layout.item_rects[0].top + 6) as usize;
        let hov_x = (layout.item_rects[0].right - 8) as usize;
        let hov_sample = px[hov_y * layout.w as usize + hov_x];
        assert_eq!(hov_sample & 0x00FF_FFFF, 0x00EF_EFEF);

        // 第一项文本区域内应出现深色文字像素
        let text_area_dark = (layout.item_rects[0].top..layout.item_rects[0].bottom).any(|y| {
            (layout.item_rects[0].left..layout.item_rects[0].right).any(|x| {
                let c = px[y as usize * layout.w as usize + x as usize];
                let r = (c >> 16) & 0xFF;
                r < 0x80
            })
        });
        assert!(text_area_dark, "第一项文字区域应存在渲染出的深色字形像素");

        unsafe {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        target.release();
    }
}


