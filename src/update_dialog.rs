//! 自动检测更新弹窗：展示全部未安装版本的更新日志（默认折叠），支持一键下载验签安装。

use std::sync::{mpsc, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, DeleteObject, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint,
    ScreenToClient, HBRUSH, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
    SRCCOPY,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{App, WM_APP_UPDATE_EXIT};
use crate::changelog::ChangelogView;
use crate::ui::{
    draw_app_badge, draw_button, draw_text, draw_tooltip, make_font, rgb, BtnStyle, DibTarget,
    TextAlign,
};
use crate::update::{self, UpdateBundle};
use crate::win32::*;

const TIMER_TOOLTIP: usize = 301;
const TIMER_UPDATE: usize = 302;
const WM_APP_REFRESH_BUNDLE: u32 = WM_APP + 30;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    DragBar,
    Close,
    Later,
    Install,
    Version(usize),
}

struct UpdateLayout {
    w: i32,
    h: i32,
    close_btn: RECT,
    hero: RECT,
    history: RECT,
    viewport: RECT,
    footer_top: i32,
    later_btn: RECT,
    install_btn: RECT,
}

struct Dlg {
    app: *mut App,
    hwnd: HWND,
    scale: f32,
    target: DibTarget,
    font_title: HFONT,
    font_section: HFONT,
    font_body: HFONT,
    font_sub: HFONT,
    font_btn: HFONT,
    font_tip: HFONT,
    bundle: UpdateBundle,
    changelog: ChangelogView,
    pending: Option<mpsc::Receiver<Result<Vec<u8>, String>>>,
    status: String,
    failed: bool,
    restarting: bool,
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
        self.font_title = make_font(s(18.0), 650);
        self.font_section = make_font(s(13.5), 600);
        self.font_body = make_font(s(13.0), 400);
        self.font_sub = make_font(s(11.5), 400);
        self.font_btn = make_font(s(13.0), 600);
        self.font_tip = make_font(s(12.0), 400);
    }

    fn destroy_fonts(&mut self) {
        unsafe {
            for f in [
                self.font_title,
                self.font_section,
                self.font_body,
                self.font_sub,
                self.font_btn,
                self.font_tip,
            ] {
                if !f.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(f.0));
                }
            }
        }
        self.font_title = HFONT::default();
    }

    fn px(&self, dp: i32) -> i32 {
        (dp as f32 * self.scale).round() as i32
    }

    fn layout(&self) -> UpdateLayout {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let p = |dp: i32| self.px(dp);

        let close_sz = p(28);
        let close_btn = RECT {
            left: w - p(14) - close_sz,
            top: p(12),
            right: w - p(14),
            bottom: p(12) + close_sz,
        };
        let hero = RECT {
            left: p(20),
            top: p(18),
            right: w - p(20),
            bottom: p(104),
        };
        let footer_h = p(58);
        let footer_top = h - footer_h;
        let history = RECT {
            left: p(20),
            top: hero.bottom + p(12),
            right: w - p(20),
            bottom: footer_top - p(14),
        };
        let viewport = RECT {
            left: history.left + p(12),
            top: history.top + p(42),
            right: history.right - p(12),
            bottom: history.bottom - p(10),
        };
        let btn_h = p(34);
        let btn_y = footer_top + (footer_h - btn_h) / 2;
        let install_btn = RECT {
            left: w - p(20) - p(116),
            top: btn_y,
            right: w - p(20),
            bottom: btn_y + btn_h,
        };
        let later_btn = RECT {
            left: install_btn.left - p(10) - p(96),
            top: btn_y,
            right: install_btn.left - p(10),
            bottom: btn_y + btn_h,
        };

        UpdateLayout {
            w,
            h,
            close_btn,
            hero,
            history,
            viewport,
            footer_top,
            later_btn,
            install_btn,
        }
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.restarting
    }

    fn start_install(&mut self) -> bool {
        if self.busy() {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        let release = self.bundle.latest.clone();
        let spawn = std::thread::Builder::new()
            .name("winlaunchpad-popup-update".into())
            .spawn(move || {
                let _ = sender.send(update::download(&release));
            });
        if let Err(err) = spawn {
            crate::log::log(&format!("启动更新下载失败：{err}"));
            self.status = "无法启动更新任务，请重试".into();
            self.failed = true;
            return false;
        }
        self.failed = false;
        self.status = "正在下载并验证签名".into();
        self.pending = Some(receiver);
        true
    }

    /// 轮询后台下载与安装结果；返回 `(任务已结束, 是否触发退出重启)`。
    fn poll(&mut self) -> (bool, bool) {
        let Some(receiver) = &self.pending else {
            return (true, false);
        };
        let result = receiver.try_recv();
        if matches!(result, Err(mpsc::TryRecvError::Empty)) {
            return (false, false);
        }
        self.pending = None;
        match result {
            Ok(download_res) => match download_res.and_then(|setup| update::start_installation(&setup))
            {
                Ok(()) => {
                    self.status = "正在应用更新并重启".into();
                    self.restarting = true;
                    return (true, true);
                }
                Err(err) => {
                    crate::log::log(&format!("一键更新失败：{err}"));
                    self.status = "更新失败，可继续使用当前版本或重试".into();
                    self.failed = true;
                }
            },
            Err(_) => {
                crate::log::log("更新下载线程异常断开");
                self.status = "更新任务已中断，请重试".into();
                self.failed = true;
            }
        }
        (true, false)
    }

    fn hit_test(&self, l: &UpdateLayout, x: i32, y: i32) -> Hit {
        if inside(&l.close_btn, x, y) {
            return Hit::Close;
        }
        if !self.busy() && inside(&l.install_btn, x, y) {
            return Hit::Install;
        }
        if !self.restarting && inside(&l.later_btn, x, y) {
            return Hit::Later;
        }
        if let Some(idx) = self.changelog.hit_version(
            x,
            y,
            &self.bundle.uninstalled,
            &l.viewport,
            self.target.mem_dc,
            self.font_body,
            self.scale,
        ) {
            return Hit::Version(idx);
        }
        if y < l.hero.bottom && !inside(&l.close_btn, x, y) {
            return Hit::DragBar;
        }
        Hit::None
    }
}

/// 打开或激活更新提示弹窗，展示所有未安装版本的折叠更新日志与一键更新按钮。
pub fn open(app: &mut App, bundle: UpdateBundle) {
    unsafe {
        if !app.update_hwnd.is_invalid() && IsWindow(app.update_hwnd).as_bool() {
            let boxed = Box::into_raw(Box::new(bundle)) as isize;
            let _ = SendMessageW(app.update_hwnd, WM_APP_REFRESH_BUNDLE, WPARAM(0), LPARAM(boxed));
            let _ = SetForegroundWindow(app.update_hwnd);
            return;
        }
        register_class(app.hinst);
        app.modal += 1;
        let ptr = app as *mut App;
        let scale = dpi_scale_at_cursor();
        let mut dlg = Box::new(Dlg {
            app: ptr,
            hwnd: HWND::default(),
            scale,
            target: DibTarget::default(),
            font_title: HFONT::default(),
            font_section: HFONT::default(),
            font_body: HFONT::default(),
            font_sub: HFONT::default(),
            font_btn: HFONT::default(),
            font_tip: HFONT::default(),
            bundle,
            changelog: ChangelogView::default(),
            pending: None,
            status: String::new(),
            failed: false,
            restarting: false,
            hover: Hit::None,
            pressed: Hit::None,
            tip_pending: None,
            tip_visible: None,
        });
        dlg.rebuild_fonts();

        let cw = (540.0 * scale).round() as i32;
        let ch = (460.0 * scale).round() as i32;
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let x = mi.rcWork.left + ((mi.rcWork.right - mi.rcWork.left) - cw) / 2;
        let y = mi.rcWork.top + ((mi.rcWork.bottom - mi.rcWork.top) - ch) / 2;

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("WinLaunchpadUpdate"),
            w!("WinLaunchpad 更新"),
            WS_POPUP,
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
        app.update_hwnd = hwnd;
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
            lpszClassName: w!("WinLaunchpadUpdate"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

fn update_tip(dlg: &mut Dlg, l: &UpdateLayout, hit: Hit) {
    let next: Option<(Hit, RECT, String)> = match hit {
        Hit::Close => Some((Hit::Close, l.close_btn, "关闭".into())),
        Hit::Later => Some((
            Hit::Later,
            l.later_btn,
            "关闭弹窗，稍后可从「设置 → 关于」更新".into(),
        )),
        Hit::Install => Some((
            Hit::Install,
            l.install_btn,
            "验签通过后安装并重启 WinLaunchpad".into(),
        )),
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
    let p = |dp: i32| dlg.px(dp);
    let primary = rgb(0x17253D, 1.0);
    let muted = rgb(0x52647D, 1.0);
    let border = rgb(0xE1E8F1, 1.0);

    sf.clear(rgb(0xFFFFFF, 1.0));

    // 顶部更新横幅卡片
    sf.fill_rrect(&l.hero, 12.0 * s, rgb(0xF1F6FF, 1.0));
    draw_app_badge(
        &mut sf,
        (l.hero.left + p(42)) as f32,
        ((l.hero.top + l.hero.bottom) / 2) as f32,
        52.0 * s,
        s,
    );
    let title_rc = RECT {
        left: l.hero.left + p(82),
        top: l.hero.top + p(17),
        right: l.close_btn.left - p(10),
        bottom: l.hero.top + p(46),
    };
    draw_text(
        dc,
        &format!("发现新版本 v{}", dlg.bundle.latest.version),
        dlg.font_title,
        &title_rc,
        primary,
        TextAlign::Left,
        true,
        false,
    );

    let default_status = format!(
        "当前版本 v{}  ·  {}",
        update::VERSION,
        dlg.bundle.latest.channel.name()
    );
    let status = if dlg.status.is_empty() {
        &default_status
    } else {
        &dlg.status
    };
    let status_col = if dlg.failed {
        rgb(0xB33B22, 1.0)
    } else {
        muted
    };
    let status_rc = RECT {
        left: l.hero.left + p(82),
        top: l.hero.top + p(47),
        right: l.hero.right - p(16),
        bottom: l.hero.top + p(70),
    };
    draw_text(
        dc,
        status,
        dlg.font_body,
        &status_rc,
        status_col,
        TextAlign::Left,
        true,
        false,
    );

    // 右上角关闭按钮
    if dlg.hover == Hit::Close || dlg.pressed == Hit::Close {
        let cbg = if dlg.pressed == Hit::Close {
            rgb(0xFEE2E2, 1.0)
        } else {
            rgb(0xE2E8F0, 0.8)
        };
        sf.fill_rrect(&l.close_btn, 7.0 * s, cbg);
    }
    let ccx = (l.close_btn.left + l.close_btn.right) as f32 * 0.5;
    let ccy = (l.close_btn.top + l.close_btn.bottom) as f32 * 0.5;
    let ca = 4.4 * s;
    let ccol = if dlg.hover == Hit::Close || dlg.pressed == Hit::Close {
        rgb(0xDC2626, 1.0)
    } else {
        rgb(0x64748B, 1.0)
    };
    sf.thick_line(ccx - ca, ccy - ca, ccx + ca, ccy + ca, (1.5 * s).max(1.1), ccol);
    sf.thick_line(ccx - ca, ccy + ca, ccx + ca, ccy - ca, (1.5 * s).max(1.1), ccol);

    // 未安装更新日志区（默认全部折叠，点击版本行可独立展开）
    sf.stroke_rrect(&l.history, 12.0 * s, s.max(1.0), border);
    draw_text(
        dc,
        "更新日志",
        dlg.font_section,
        &RECT {
            left: l.history.left + p(16),
            top: l.history.top + p(10),
            right: l.history.right - p(16),
            bottom: l.history.top + p(38),
        },
        primary,
        TextAlign::Left,
        true,
        false,
    );
    let hover_idx = match dlg.hover {
        Hit::Version(i) => Some(i),
        _ => None,
    };
    let pressed_idx = match dlg.pressed {
        Hit::Version(i) => Some(i),
        _ => None,
    };
    dlg.changelog.paint(
        &mut sf,
        dc,
        &dlg.bundle.uninstalled,
        &l.viewport,
        dlg.font_section,
        dlg.font_body,
        dlg.font_sub,
        hover_idx,
        pressed_idx,
        s,
    );

    // 底部操作栏
    sf.fill_rect(
        &RECT {
            left: 0,
            top: l.footer_top,
            right: l.w,
            bottom: l.h,
        },
        rgb(0xFAFAFC, 1.0),
    );
    sf.fill_rect(
        &RECT {
            left: 0,
            top: l.footer_top,
            right: l.w,
            bottom: l.footer_top + 1,
        },
        rgb(0xE5E7EB, 1.0),
    );
    if !dlg.restarting {
        draw_button(
            &mut sf,
            dc,
            &l.later_btn,
            "稍后更新",
            dlg.font_btn,
            BtnStyle::Secondary,
            dlg.hover == Hit::Later,
            dlg.pressed == Hit::Later,
            s,
        );
    }
    if dlg.busy() {
        sf.fill_rrect(&l.install_btn, 8.0 * s, rgb(0xEDF2F8, 1.0));
        let label = if dlg.restarting {
            "正在重启"
        } else {
            "处理中"
        };
        draw_text(
            dc,
            label,
            dlg.font_btn,
            &l.install_btn,
            muted,
            TextAlign::Center,
            false,
            false,
        );
    } else {
        draw_button(
            &mut sf,
            dc,
            &l.install_btn,
            "一键更新",
            dlg.font_btn,
            BtnStyle::Primary,
            dlg.hover == Hit::Install,
            dlg.pressed == Hit::Install,
            s,
        );
    }

    // 外边框与自定义 100ms Tooltip
    sf.stroke_rrect(
        &RECT {
            left: 0,
            top: 0,
            right: l.w,
            bottom: l.h,
        },
        10.0 * s,
        s.max(1.0),
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
            crate::log::log("更新弹窗处理消息时 panic");
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    };
    if msg == WM_DESTROY {
        drop(Box::from_raw(ptr));
    }
    ret
}

fn handle(
    dlg: &mut Dlg,
    app: &mut App,
    hwnd: HWND,
    msg: u32,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let dpi = window_dpi(hwnd);
                dlg.scale = dpi as f32 / 96.0;
                dlg.rebuild_fonts();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint_dlg(dlg);
                LRESULT(0)
            }
            WM_APP_REFRESH_BUNDLE => {
                let next = *Box::from_raw(lparam.0 as *mut UpdateBundle);
                if !dlg.busy() && next.latest.version != dlg.bundle.latest.version {
                    dlg.bundle = next;
                    dlg.changelog = ChangelogView::default();
                    dlg.status.clear();
                    dlg.failed = false;
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let new = lparam.0 as *const RECT;
                let r = *new;
                let dpi = hiword(_wparam.0) as u32;
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
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_TIMER => {
                if _wparam.0 == TIMER_TOOLTIP {
                    let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                    if let Some((_, rc, text)) = &dlg.tip_pending {
                        dlg.tip_visible = Some((*rc, text.clone()));
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if _wparam.0 == TIMER_UPDATE {
                    let (done, restart) = dlg.poll();
                    if done {
                        let _ = KillTimer(hwnd, TIMER_UPDATE);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    if restart {
                        let _ = PostMessageW(app.hwnd, WM_APP_UPDATE_EXIT, WPARAM(0), LPARAM(0));
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
            WM_MOUSEWHEEL => {
                let l = dlg.layout();
                let mut pt = POINT {
                    x: get_x_lparam(lparam.0),
                    y: get_y_lparam(lparam.0),
                };
                let _ = ScreenToClient(hwnd, &mut pt);
                if dlg.changelog.scroll_by(
                    pt.x,
                    pt.y,
                    get_wheel_delta(_wparam.0),
                    &dlg.bundle.uninstalled,
                    &l.viewport,
                    dlg.target.mem_dc,
                    dlg.font_body,
                    dlg.scale,
                ) {
                    dlg.hover = dlg.hit_test(&l, pt.x, pt.y);
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let l = dlg.layout();
                let hit = dlg.hit_test(&l, get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                dlg.tip_visible = None;
                if hit == Hit::DragBar {
                    let _ = ReleaseCapture();
                    let _ = SendMessageW(
                        hwnd,
                        WM_NCLBUTTONDOWN,
                        WPARAM(HTCAPTION as usize),
                        LPARAM(0),
                    );
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
                        Hit::Close | Hit::Later if !dlg.restarting => {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        Hit::Version(index) => {
                            dlg.changelog.toggle(
                                index,
                                &dlg.bundle.uninstalled,
                                &l.viewport,
                                dlg.target.mem_dc,
                                dlg.font_body,
                                dlg.scale,
                            );
                        }
                        Hit::Install => {
                            dlg.tip_pending = None;
                            let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                            if dlg.start_install() {
                                let _ = SetTimer(hwnd, TIMER_UPDATE, 100, None);
                            }
                        }
                        _ => {}
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if _wparam.0 as u32 == VK_ESCAPE.0 as u32 && !dlg.restarting {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if !dlg.restarting {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                let _ = KillTimer(hwnd, TIMER_UPDATE);
                app.modal = app.modal.saturating_sub(1);
                app.update_hwnd = HWND::default();
                dlg.destroy_fonts();
                dlg.target.release();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, _wparam, lparam),
        }
    }
}
