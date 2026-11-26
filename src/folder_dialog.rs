//! 新建 / 重命名文件夹弹窗：紧凑圆角卡片样式，支持快捷键确认/取消、DPI 自适应与 100ms Tooltip。

use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect, SetBkColor,
    SetTextColor, HBRUSH, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::Ime::{
    ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
    VK_RETURN,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::App;
use crate::ui::{draw_button, draw_text, draw_tooltip, make_font, rgb, BtnStyle, DibTarget, TextAlign};
use crate::win32::*;

const TIMER_TOOLTIP: usize = 401;
const ID_NAME_EDIT: i32 = 4001;
const EM_SETSEL: u32 = 0x00B1;

#[derive(Clone)]
pub enum Mode {
    Create,
    Rename { folder_id: String, current_name: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    DragBar,
    Close,
    InputBox,
    Cancel,
    Confirm,
}

struct DialogLayout {
    w: i32,
    h: i32,
    close_btn: RECT,
    title_rc: RECT,
    input_box: RECT,
    edit_rc: RECT,
    cancel_btn: RECT,
    confirm_btn: RECT,
}

struct Dlg {
    app: *mut App,
    hwnd: HWND,
    edit: HWND,
    edit_brush: HBRUSH,
    scale: f32,
    target: DibTarget,
    font_title: HFONT,
    font_edit: HFONT,
    font_btn: HFONT,
    font_tip: HFONT,
    mode: Mode,
    hover: Hit,
    pressed: Hit,
    tip_pending: Option<(Hit, RECT, String)>,
    tip_visible: Option<(RECT, String)>,
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

impl Dlg {
    fn rebuild_fonts(&mut self) {
        self.destroy_fonts();
        let s = |px: f32| (px * self.scale).round() as i32;
        self.font_title = make_font(s(15.5), 600);
        self.font_edit = make_font(s(13.5), 400);
        self.font_btn = make_font(s(13.0), 600);
        self.font_tip = make_font(s(12.0), 400);
        if !self.edit.is_invalid() {
            set_control_font(self.edit, self.font_edit);
        }
    }

    fn destroy_fonts(&mut self) {
        unsafe {
            for f in [self.font_title, self.font_edit, self.font_btn, self.font_tip] {
                if !f.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(f.0));
                }
            }
        }
        self.font_title = HFONT::default();
    }

    fn layout(&self) -> DialogLayout {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let s = |v: f32| (v * self.scale).round() as i32;

        let close_sz = s(26.0);
        let close_btn = RECT {
            left: w - s(14.0) - close_sz,
            top: s(14.0),
            right: w - s(14.0),
            bottom: s(14.0) + close_sz,
        };
        let title_rc = RECT {
            left: s(20.0),
            top: s(14.0),
            right: close_btn.left - s(8.0),
            bottom: s(42.0),
        };
        let input_box = RECT {
            left: s(20.0),
            top: s(56.0),
            right: w - s(20.0),
            bottom: s(96.0),
        };
        let edit_rc = RECT {
            left: input_box.left + s(12.0),
            top: input_box.top + s(8.0),
            right: input_box.right - s(12.0),
            bottom: input_box.bottom - s(7.0),
        };
        let btn_w = s(84.0);
        let btn_h = s(32.0);
        let btn_y = h - s(18.0) - btn_h;
        let confirm_btn = RECT {
            left: w - s(20.0) - btn_w,
            top: btn_y,
            right: w - s(20.0),
            bottom: btn_y + btn_h,
        };
        let cancel_btn = RECT {
            left: confirm_btn.left - s(10.0) - btn_w,
            top: btn_y,
            right: confirm_btn.left - s(10.0),
            bottom: btn_y + btn_h,
        };

        DialogLayout {
            w,
            h,
            close_btn,
            title_rc,
            input_box,
            edit_rc,
            cancel_btn,
            confirm_btn,
        }
    }

    fn sync_edit_pos(&self) {
        if self.edit.is_invalid() {
            return;
        }
        let l = self.layout();
        unsafe {
            let _ = SetWindowPos(
                self.edit,
                HWND_TOP,
                l.edit_rc.left,
                l.edit_rc.top,
                (l.edit_rc.right - l.edit_rc.left).max(1),
                (l.edit_rc.bottom - l.edit_rc.top).max(1),
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOZORDER,
            );
        }
    }

    fn text(&self) -> String {
        if self.edit.is_invalid() {
            return String::new();
        }
        unsafe {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(self.edit, &mut buf);
            from_wide(&buf[..n as usize])
        }
    }

    fn hit_test(&self, l: &DialogLayout, x: i32, y: i32) -> Hit {
        if inside(&l.close_btn, x, y) {
            return Hit::Close;
        }
        if inside(&l.confirm_btn, x, y) {
            return Hit::Confirm;
        }
        if inside(&l.cancel_btn, x, y) {
            return Hit::Cancel;
        }
        if inside(&l.input_box, x, y) {
            return Hit::InputBox;
        }
        if y < l.input_box.top {
            return Hit::DragBar;
        }
        Hit::None
    }
}

pub fn open_create(app: &mut App) {
    open_with_mode(app, Mode::Create);
}

pub fn open_rename(app: &mut App, folder_id: String, current_name: String) {
    open_with_mode(
        app,
        Mode::Rename {
            folder_id,
            current_name,
        },
    );
}

fn open_with_mode(app: &mut App, mode: Mode) {
    unsafe {
        if !app.folder_hwnd.is_invalid() && IsWindow(app.folder_hwnd).as_bool() {
            let _ = DestroyWindow(app.folder_hwnd);
        }
        register_class(app.hinst);
        app.modal += 1;
        let ptr = app as *mut App;
        let scale = dpi_scale_at_cursor();
        let mut dlg = Box::new(Dlg {
            app: ptr,
            hwnd: HWND::default(),
            edit: HWND::default(),
            edit_brush: CreateSolidBrush(COLORREF(0x00FF_FFFF)),
            scale,
            target: DibTarget::default(),
            font_title: HFONT::default(),
            font_edit: HFONT::default(),
            font_btn: HFONT::default(),
            font_tip: HFONT::default(),
            mode: mode.clone(),
            hover: Hit::None,
            pressed: Hit::None,
            tip_pending: None,
            tip_visible: None,
        });
        dlg.rebuild_fonts();

        let cw = (380.0 * scale).round() as i32;
        let ch = (168.0 * scale).round() as i32;
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = windows::Win32::Graphics::Gdi::MonitorFromPoint(
            pt,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );
        let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi);
        let x = mi.rcWork.left + ((mi.rcWork.right - mi.rcWork.left) - cw) / 2;
        let y = mi.rcWork.top + ((mi.rcWork.bottom - mi.rcWork.top) - ch) / 2;
        let title_w = match &mode {
            Mode::Create => w!("新建文件夹"),
            Mode::Rename { .. } => w!("重命名文件夹"),
        };
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("WinLaunchpadFolderDlg"),
            title_w,
            WS_POPUP | WS_CLIPCHILDREN,
            x,
            y,
            cw,
            ch,
            (*ptr).hwnd,
            None,
            (*ptr).hinst,
            Some(Box::into_raw(dlg) as *const _),
        )
        .unwrap_or_default();
        if hwnd.is_invalid() {
            app.modal = app.modal.saturating_sub(1);
            return;
        }
        enable_rounded_window(hwnd);
        app.folder_hwnd = hwnd;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn register_class(hinst: HINSTANCE) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(hinst, PCWSTR(1usize as *const u16)).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: w!("WinLaunchpadFolderDlg"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

fn commit(dlg: &mut Dlg, app: &mut App) {
    let raw = dlg.text();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    match &dlg.mode {
        Mode::Create => {
            app.create_folder_and_focus(trimmed);
        }
        Mode::Rename { folder_id, .. } => {
            app.rename_folder_and_focus(folder_id, trimmed);
        }
    }
    unsafe {
        let _ = DestroyWindow(dlg.hwnd);
    }
}

fn update_tip(dlg: &mut Dlg, l: &DialogLayout, hit: Hit) {
    let next: Option<(Hit, RECT, String)> = match hit {
        Hit::Close => Some((Hit::Close, l.close_btn, "关闭".into())),
        _ => None,
    };
    let same = match (&dlg.tip_pending, &next) {
        (Some((a, _, _)), Some((b, _, _))) => a == b,
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    dlg.tip_visible = None;
    dlg.tip_pending = next;
    unsafe {
        let _ = KillTimer(dlg.hwnd, TIMER_TOOLTIP);
        if dlg.tip_pending.is_some() {
            let _ = SetTimer(dlg.hwnd, TIMER_TOOLTIP, 100, None);
        }
    }
}

fn paint_dlg(dlg: &mut Dlg) {
    let l = dlg.layout();
    if !dlg.target.ensure(l.w, l.h) {
        return;
    }
    let Some(dc) = dlg.target.mem_dc else { return };
    let Some(mut sf) = dlg.target.surface() else { return };
    let s = dlg.scale;

    sf.clear(rgb(0xFFFFFF, 1.0));

    let title = match &dlg.mode {
        Mode::Create => "新建文件夹",
        Mode::Rename { .. } => "重命名文件夹",
    };
    draw_text(
        dc,
        title,
        dlg.font_title,
        &l.title_rc,
        rgb(0x111827, 1.0),
        TextAlign::Left,
        true,
        false,
    );

    if dlg.hover == Hit::Close || dlg.pressed == Hit::Close {
        let bg = if dlg.pressed == Hit::Close {
            rgb(0xFEE2E2, 1.0)
        } else {
            rgb(0xF1F5F9, 1.0)
        };
        sf.fill_rrect(&l.close_btn, 7.0 * s, bg);
    }
    let ccx = (l.close_btn.left + l.close_btn.right) as f32 * 0.5;
    let ccy = (l.close_btn.top + l.close_btn.bottom) as f32 * 0.5;
    let ca = 4.2 * s;
    let ccol = if dlg.hover == Hit::Close || dlg.pressed == Hit::Close {
        rgb(0xDC2626, 1.0)
    } else {
        rgb(0x64748B, 1.0)
    };
    sf.thick_line(ccx - ca, ccy - ca, ccx + ca, ccy + ca, (1.6 * s).max(1.2), ccol);
    sf.thick_line(ccx - ca, ccy + ca, ccx + ca, ccy - ca, (1.6 * s).max(1.2), ccol);

    // 输入框容器
    sf.fill_rrect(&l.input_box, 8.0 * s, rgb(0xFFFFFF, 1.0));
    let border_col = if dlg.hover == Hit::InputBox {
        rgb(0x60A5FA, 1.0)
    } else {
        rgb(0x1677FF, 0.85)
    };
    sf.stroke_rrect(&l.input_box, 8.0 * s, (1.4 * s).max(1.0), border_col);

    draw_button(
        &mut sf,
        dc,
        &l.cancel_btn,
        "取消",
        dlg.font_btn,
        BtnStyle::Secondary,
        dlg.hover == Hit::Cancel,
        dlg.pressed == Hit::Cancel,
        s,
    );

    let can_submit = !dlg.text().trim().is_empty();
    let confirm_style = if can_submit {
        BtnStyle::Primary
    } else {
        BtnStyle::Secondary
    };
    draw_button(
        &mut sf,
        dc,
        &l.confirm_btn,
        "确定",
        dlg.font_btn,
        confirm_style,
        can_submit && dlg.hover == Hit::Confirm,
        can_submit && dlg.pressed == Hit::Confirm,
        s,
    );

    sf.stroke_rrect(
        &RECT {
            left: 0,
            top: 0,
            right: l.w,
            bottom: l.h,
        },
        10.0 * s,
        (1.0 * s).max(1.0),
        rgb(0xD1D5DB, 1.0),
    );

    if let Some((anchor, tip)) = &dlg.tip_visible {
        draw_tooltip(&mut sf, dc, dlg.font_tip, anchor, tip, l.w, l.h, s);
    }

    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let wdc = BeginPaint(dlg.hwnd, &mut ps);
        let _ = BitBlt(wdc, 0, 0, l.w, l.h, dc, 0, 0, SRCCOPY);
        let _ = EndPaint(dlg.hwnd, &ps);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
        if !(*cs).lpCreateParams.is_null() {
            (*((*cs).lpCreateParams as *mut Dlg)).hwnd = hwnd;
        }
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Dlg;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let dlg = &mut *ptr;
    let app = &mut *dlg.app;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle(dlg, app, hwnd, msg, wparam, lparam)
    }));
    let ret = match result {
        Ok(v) => v,
        Err(_) => {
            crate::log::log("文件夹命名弹窗处理消息时 panic");
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    };
    if msg == WM_DESTROY {
        drop(Box::from_raw(ptr));
    }
    ret
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
        GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut Dlg
    } else {
        std::ptr::null_mut()
    };
    if !ptr.is_null() && matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN | WM_CHAR) {
        let dlg = &mut *ptr;
        let himc = ImmGetContext(hwnd);
        let composing = !himc.is_invalid() && ImmGetCompositionStringW(himc, GCS_COMPSTR, None, 0) > 0;
        if !himc.is_invalid() {
            let _ = ImmReleaseContext(hwnd, himc);
        }
        if !composing {
            let vk = wparam.0 as u32;
            if (msg == WM_KEYDOWN && vk == VK_RETURN.0 as u32) || (msg == WM_CHAR && wparam.0 == 0x0D)
            {
                let app = &mut *dlg.app;
                commit(dlg, app);
                return LRESULT(0);
            }
            if (msg == WM_KEYDOWN && vk == VK_ESCAPE.0 as u32) || (msg == WM_CHAR && wparam.0 == 0x1B)
            {
                let _ = DestroyWindow(parent);
                return LRESULT(0);
            }
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn handle(
    dlg: &mut Dlg,
    app: &mut App,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let dpi = window_dpi(hwnd);
                dlg.scale = dpi as f32 / 96.0;
                dlg.edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_LEFT as u32 | ES_AUTOHSCROLL as u32),
                    0,
                    0,
                    10,
                    10,
                    hwnd,
                    HMENU(ID_NAME_EDIT as *mut _),
                    app.hinst,
                    None,
                )
                .unwrap_or_default();
                dlg.rebuild_fonts();
                // 必须在写入初始文本并全选之前先把 EDIT 控件尺寸同步到位，
                // 否则 Win32 单行 ES_AUTOHSCROLL 在默认 10px 宽度下执行 EM_SETSEL(-1) 会把文本横向滚出左侧视野。
                dlg.sync_edit_pos();
                if !dlg.edit.is_invalid() {
                    let _ = SetWindowSubclass(dlg.edit, Some(edit_proc), 1, 0);
                    let cue = wide("请输入文件夹名称");
                    let _ = SendMessageW(dlg.edit, 0x1501, WPARAM(1), LPARAM(cue.as_ptr() as isize));
                    let init_text = match &dlg.mode {
                        Mode::Create => "新建文件夹",
                        Mode::Rename { current_name, .. } => current_name.as_str(),
                    };
                    let wi = wide(init_text);
                    let _ = SetWindowTextW(dlg.edit, PCWSTR(wi.as_ptr()));
                    // 默认全选已有名称，方便直接输入替换或按回车确认
                    let _ = SendMessageW(dlg.edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                if loword(wparam.0) as u32 != WA_INACTIVE && !dlg.edit.is_invalid() {
                    let _ = SetFocus(dlg.edit);
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint_dlg(dlg);
                LRESULT(0)
            }
            WM_CTLCOLOREDIT => {
                if lparam.0 as isize == dlg.edit.0 as isize {
                    let hdc = HDC(wparam.0 as *mut _);
                    SetTextColor(hdc, COLORREF(0x0027_1811));
                    SetBkColor(hdc, COLORREF(0x00FF_FFFF));
                    return LRESULT(dlg.edit_brush.0 as isize);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                if loword(wparam.0) as i32 == ID_NAME_EDIT && hiword(wparam.0) as u32 == EN_CHANGE {
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let new = lparam.0 as *const RECT;
                let r = *new;
                let dpi = hiword(wparam.0) as u32;
                dlg.scale = dpi as f32 / 96.0;
                dlg.rebuild_fonts();
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                dlg.sync_edit_pos();
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_TOOLTIP {
                    let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                    if let Some((_, rc, text)) = &dlg.tip_pending {
                        dlg.tip_visible = Some((*rc, text.clone()));
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                app.ensure_gesture_alive();
                let l = dlg.layout();
                let hit = dlg.hit_test(&l, get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                update_tip(dlg, &l, hit);
                if hit != dlg.hover {
                    dlg.hover = hit;
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
                dlg.hover = Hit::None;
                dlg.tip_pending = None;
                dlg.tip_visible = None;
                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let l = dlg.layout();
                let hit = dlg.hit_test(&l, get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                dlg.tip_visible = None;
                if hit == Hit::DragBar {
                    let _ = ReleaseCapture();
                    let _ = SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
                    return LRESULT(0);
                }
                if hit == Hit::InputBox && !dlg.edit.is_invalid() {
                    let _ = SetFocus(dlg.edit);
                    return LRESULT(0);
                }
                dlg.pressed = hit;
                let _ = SetCapture(hwnd);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                let l = dlg.layout();
                let hit = dlg.hit_test(&l, get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                let pressed = dlg.pressed;
                dlg.pressed = Hit::None;
                if pressed == hit {
                    match hit {
                        Hit::Close | Hit::Cancel => {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        Hit::Confirm => {
                            commit(dlg, app);
                            return LRESULT(0);
                        }
                        _ => {}
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u32;
                if vk == VK_ESCAPE.0 as u32 {
                    let _ = DestroyWindow(hwnd);
                } else if vk == VK_RETURN.0 as u32 {
                    commit(dlg, app);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                app.modal = app.modal.saturating_sub(1);
                app.folder_hwnd = HWND::default();
                if !dlg.edit_brush.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(dlg.edit_brush.0));
                }
                dlg.destroy_fonts();
                dlg.target.release();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
