//! 更新检测与自动更新弹窗：支持托盘主动检测（展示检测中、已是最新或失败结果）与新版本日志展示、一键下载验签安装。

use std::sync::{mpsc, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, DeleteObject, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint,
    MonitorFromWindow, ScreenToClient, HBRUSH, HDC, HFONT, HGDIOBJ, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
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
    Surface, TextAlign,
};
use crate::update::{self, Channel, UpdateBundle};
use crate::win32::*;

const TIMER_TOOLTIP: usize = 301;
const TIMER_UPDATE: usize = 302;
const WM_APP_REFRESH_BUNDLE: u32 = WM_APP + 30;
const WM_APP_START_CHECK: u32 = WM_APP + 31;

const COMPACT_W_DP: f32 = 440.0;
const COMPACT_H_DP: f32 = 198.0;
const EXPANDED_W_DP: f32 = 540.0;
const EXPANDED_H_DP: f32 = 460.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    None,
    DragBar,
    Close,
    Secondary,
    Primary,
    Version(usize),
}

struct ButtonSpec {
    secondary_label: &'static str,
    secondary_w_dp: i32,
    secondary_enabled: bool,
    primary_label: &'static str,
    primary_w_dp: i32,
    primary_enabled: bool,
}

#[allow(clippy::large_enum_variant)]
enum DialogState {
    /// 后台检测新版本中
    Checking {
        receiver: mpsc::Receiver<Result<(Option<UpdateBundle>, Channel), String>>,
    },
    /// 检测完成：当前已是最新版本
    UpToDate { channel: Channel },
    /// 检测失败
    CheckFailed { message: String },
    /// 发现可安装新版本，支持展开日志与一键下载验签安装
    Available {
        bundle: UpdateBundle,
        changelog: ChangelogView,
        pending: Option<mpsc::Receiver<Result<Vec<u8>, String>>>,
        status: String,
        failed: bool,
        restarting: bool,
    },
}

impl DialogState {
    fn is_compact(&self) -> bool {
        !matches!(self, Self::Available { .. })
    }

    fn is_restarting(&self) -> bool {
        matches!(
            self,
            Self::Available {
                restarting: true,
                ..
            }
        )
    }

    fn is_installing_or_restarting(&self) -> bool {
        matches!(
            self,
            Self::Available {
                pending: Some(_),
                ..
            } | Self::Available {
                restarting: true,
                ..
            }
        )
    }

    fn is_busy(&self) -> bool {
        matches!(self, Self::Checking { .. }) || self.is_installing_or_restarting()
    }

    fn button_spec(&self) -> ButtonSpec {
        match self {
            Self::Checking { .. } => ButtonSpec {
                secondary_label: "取消",
                secondary_w_dp: 84,
                secondary_enabled: true,
                primary_label: "检测中",
                primary_w_dp: 100,
                primary_enabled: false,
            },
            Self::UpToDate { .. } => ButtonSpec {
                secondary_label: "重新检测",
                secondary_w_dp: 96,
                secondary_enabled: true,
                primary_label: "关闭",
                primary_w_dp: 88,
                primary_enabled: true,
            },
            Self::CheckFailed { .. } => ButtonSpec {
                secondary_label: "关闭",
                secondary_w_dp: 84,
                secondary_enabled: true,
                primary_label: "重新检测",
                primary_w_dp: 96,
                primary_enabled: true,
            },
            Self::Available {
                pending,
                restarting,
                ..
            } => ButtonSpec {
                secondary_label: "稍后更新",
                secondary_w_dp: 96,
                secondary_enabled: !*restarting,
                primary_label: if *restarting {
                    "正在重启"
                } else if pending.is_some() {
                    "处理中"
                } else {
                    "一键更新"
                },
                primary_w_dp: 116,
                primary_enabled: pending.is_none() && !*restarting,
            },
        }
    }

    /// 生成顶部横幅标题、状态副文案及错误高亮标记。
    fn hero_display(&self) -> (String, String, bool) {
        match self {
            Self::Checking { .. } => (
                "正在检测更新".into(),
                format!("当前版本 v{}  ·  正在连接发布服务器", update::VERSION),
                false,
            ),
            Self::UpToDate { channel } => (
                "当前已是最新版本".into(),
                format!("当前版本 v{}  ·  {}", update::VERSION, channel.name()),
                false,
            ),
            Self::CheckFailed { message } => ("检测更新失败".into(), message.clone(), true),
            Self::Available {
                bundle,
                status,
                failed,
                ..
            } => {
                let sub = if status.is_empty() {
                    format!(
                        "当前版本 v{}  ·  {}",
                        update::VERSION,
                        bundle.latest.channel.name()
                    )
                } else {
                    status.clone()
                };
                (format!("发现新版本 v{}", bundle.latest.version), sub, *failed)
            }
        }
    }
}

#[derive(Default)]
struct DialogFonts {
    title: HFONT,
    section: HFONT,
    body: HFONT,
    sub: HFONT,
    btn: HFONT,
    tip: HFONT,
}

impl DialogFonts {
    fn rebuild(&mut self, scale: f32) {
        self.destroy();
        let s = |px: f32| (px * scale).round() as i32;
        self.title = make_font(s(18.0), 650);
        self.section = make_font(s(13.5), 600);
        self.body = make_font(s(13.0), 400);
        self.sub = make_font(s(11.5), 400);
        self.btn = make_font(s(13.0), 600);
        self.tip = make_font(s(12.0), 400);
    }

    fn destroy(&mut self) {
        unsafe {
            for f in [
                self.title,
                self.section,
                self.body,
                self.sub,
                self.btn,
                self.tip,
            ] {
                if !f.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(f.0));
                }
            }
        }
        self.title = HFONT::default();
        self.section = HFONT::default();
        self.body = HFONT::default();
        self.sub = HFONT::default();
        self.btn = HFONT::default();
        self.tip = HFONT::default();
    }
}

impl Drop for DialogFonts {
    fn drop(&mut self) {
        self.destroy();
    }
}

struct UpdateLayout {
    w: i32,
    h: i32,
    compact: bool,
    close_btn: RECT,
    hero: RECT,
    history: RECT,
    viewport: RECT,
    footer_top: i32,
    secondary_btn: RECT,
    primary_btn: RECT,
}

impl UpdateLayout {
    /// 纯几何布局构建，供窗口绘制与无窗口单元测试共用。
    fn build(w: i32, h: i32, compact: bool, spec: &ButtonSpec, scale: f32) -> Self {
        let w = w.max(1);
        let h = h.max(1);
        let p = |dp: i32| (dp as f32 * scale).round() as i32;

        let close_sz = p(28);
        let close_btn = RECT {
            left: w - p(14) - close_sz,
            top: p(12),
            right: w - p(14),
            bottom: p(12) + close_sz,
        };
        let hero_bottom = if compact { p(118) } else { p(104) };
        let hero = RECT {
            left: p(20),
            top: p(18),
            right: w - p(20),
            bottom: hero_bottom,
        };
        let footer_h = p(58);
        let footer_top = h - footer_h;
        let history = if compact {
            RECT::default()
        } else {
            RECT {
                left: p(20),
                top: hero.bottom + p(12),
                right: w - p(20),
                bottom: footer_top - p(14),
            }
        };
        let viewport = if compact {
            RECT::default()
        } else {
            RECT {
                left: history.left + p(12),
                top: history.top + p(42),
                right: history.right - p(12),
                bottom: history.bottom - p(10),
            }
        };
        let btn_h = p(34);
        let btn_y = footer_top + (footer_h - btn_h) / 2;
        let primary_btn = RECT {
            left: w - p(20) - p(spec.primary_w_dp),
            top: btn_y,
            right: w - p(20),
            bottom: btn_y + btn_h,
        };
        let secondary_btn = RECT {
            left: primary_btn.left - p(10) - p(spec.secondary_w_dp),
            top: btn_y,
            right: primary_btn.left - p(10),
            bottom: btn_y + btn_h,
        };

        UpdateLayout {
            w,
            h,
            compact,
            close_btn,
            hero,
            history,
            viewport,
            footer_top,
            secondary_btn,
            primary_btn,
        }
    }
}

struct Dlg {
    app: *mut App,
    hwnd: HWND,
    scale: f32,
    target: DibTarget,
    fonts: DialogFonts,
    state: DialogState,
    hover: Hit,
    pressed: Hit,
    tip_pending: Option<(Hit, RECT, String)>,
    tip_visible: Option<(RECT, String)>,
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 启动后台版本检测线程并返回对应初始状态。
fn spawn_check_state() -> DialogState {
    let (sender, receiver) = mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("winlaunchpad-tray-update-check".into())
        .spawn(move || {
            let _ = sender.send(update::check());
        });
    match spawn {
        Ok(_) => DialogState::Checking { receiver },
        Err(err) => {
            crate::log::log(&format!("启动检测更新线程失败：{err}"));
            DialogState::CheckFailed {
                message: "无法启动检测任务，请重试".into(),
            }
        }
    }
}

/// 计算目标尺寸窗口在指定显示器工作区内的居中位置。
unsafe fn centered_bounds_on_monitor(mon: HMONITOR, cw: i32, ch: i32) -> (i32, i32) {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(mon, &mut mi).as_bool() {
        let x = mi.rcWork.left + ((mi.rcWork.right - mi.rcWork.left) - cw) / 2;
        let y = mi.rcWork.top + ((mi.rcWork.bottom - mi.rcWork.top) - ch) / 2;
        return (x, y);
    }
    let sw = GetSystemMetrics(SM_CXSCREEN).max(cw);
    let sh = GetSystemMetrics(SM_CYSCREEN).max(ch);
    ((sw - cw) / 2, (sh - ch) / 2)
}

impl Dlg {
    fn layout(&self) -> UpdateLayout {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let spec = self.state.button_spec();
        UpdateLayout::build(
            rc.right - rc.left,
            rc.bottom - rc.top,
            self.state.is_compact(),
            &spec,
            self.scale,
        )
    }

    fn reset_interaction(&mut self) {
        self.hover = Hit::None;
        self.pressed = Hit::None;
        self.tip_pending = None;
        self.tip_visible = None;
    }

    /// 根据当前状态（紧凑结果页 / 完整日志页）调整弹窗尺寸并保持在所在显示器居中。
    fn sync_window_size(&self) {
        if self.hwnd.is_invalid() {
            return;
        }
        let (w_dp, h_dp) = if self.state.is_compact() {
            (COMPACT_W_DP, COMPACT_H_DP)
        } else {
            (EXPANDED_W_DP, EXPANDED_H_DP)
        };
        let cw = (w_dp * self.scale).round() as i32;
        let ch = (h_dp * self.scale).round() as i32;
        unsafe {
            let mut cur = RECT::default();
            if GetWindowRect(self.hwnd, &mut cur).is_ok()
                && cur.right - cur.left == cw
                && cur.bottom - cur.top == ch
            {
                return;
            }
            let mon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let (x, y) = centered_bounds_on_monitor(mon, cw, ch);
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                cw,
                ch,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn start_check(&mut self) -> bool {
        if self.state.is_busy() {
            return false;
        }
        self.reset_interaction();
        self.state = spawn_check_state();
        self.sync_window_size();
        matches!(self.state, DialogState::Checking { .. })
    }

    fn start_install(&mut self) -> bool {
        let DialogState::Available {
            bundle,
            pending,
            status,
            failed,
            restarting,
            ..
        } = &mut self.state
        else {
            return false;
        };
        if pending.is_some() || *restarting {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        let release = bundle.latest.clone();
        let spawn = std::thread::Builder::new()
            .name("winlaunchpad-popup-update".into())
            .spawn(move || {
                let _ = sender.send(update::download(&release));
            });
        if let Err(err) = spawn {
            crate::log::log(&format!("启动更新下载失败：{err}"));
            *status = "无法启动更新任务，请重试".into();
            *failed = true;
            return false;
        }
        *failed = false;
        *status = "正在下载并验证签名".into();
        *pending = Some(receiver);
        true
    }

    /// 轮询后台检测或下载安装结果；返回 `(任务已结束, 是否触发退出重启, 窗口模式是否变化)`。
    fn poll(&mut self, app: &mut App) -> (bool, bool, bool) {
        match &mut self.state {
            DialogState::Checking { receiver } => {
                let result = receiver.try_recv();
                if matches!(result, Err(mpsc::TryRecvError::Empty)) {
                    return (false, false, false);
                }
                match result {
                    Ok(Ok((Some(bundle), _))) => {
                        app.sync_manual_update_result(Some(bundle.clone()));
                        self.state = DialogState::Available {
                            bundle,
                            changelog: ChangelogView::default(),
                            pending: None,
                            status: String::new(),
                            failed: false,
                            restarting: false,
                        };
                        self.reset_interaction();
                        (true, false, true)
                    }
                    Ok(Ok((None, channel))) => {
                        app.sync_manual_update_result(None);
                        self.state = DialogState::UpToDate { channel };
                        self.reset_interaction();
                        (true, false, false)
                    }
                    Ok(Err(err)) => {
                        crate::log::log(&format!("检测更新失败：{err}"));
                        let message = if err == "当前不是正式版本" {
                            "当前版本不支持在线检测".into()
                        } else {
                            "检测失败，请检查网络后重试".into()
                        };
                        self.state = DialogState::CheckFailed { message };
                        self.reset_interaction();
                        (true, false, false)
                    }
                    Err(_) => {
                        crate::log::log("更新检测线程异常断开");
                        self.state = DialogState::CheckFailed {
                            message: "检测任务已中断，请重试".into(),
                        };
                        self.reset_interaction();
                        (true, false, false)
                    }
                }
            }
            DialogState::Available {
                pending,
                status,
                failed,
                restarting,
                ..
            } => {
                let Some(rx) = pending else {
                    return (true, false, false);
                };
                let result = rx.try_recv();
                if matches!(result, Err(mpsc::TryRecvError::Empty)) {
                    return (false, false, false);
                }
                *pending = None;
                match result {
                    Ok(download_res) => {
                        match download_res.and_then(|setup| update::start_installation(&setup)) {
                            Ok(()) => {
                                *status = "正在应用更新并重启".into();
                                *restarting = true;
                                return (true, true, false);
                            }
                            Err(err) => {
                                crate::log::log(&format!("一键更新失败：{err}"));
                                *status = "更新失败，可继续使用当前版本或重试".into();
                                *failed = true;
                            }
                        }
                    }
                    Err(_) => {
                        crate::log::log("更新下载线程异常断开");
                        *status = "更新任务已中断，请重试".into();
                        *failed = true;
                    }
                }
                (true, false, false)
            }
            _ => (true, false, false),
        }
    }

    fn hit_test(&self, l: &UpdateLayout, x: i32, y: i32) -> Hit {
        hit_test_state(&self.state, l, self.target.mem_dc, self.fonts.body, self.scale, x, y)
    }
}

fn hit_test_state(
    state: &DialogState,
    l: &UpdateLayout,
    dc: Option<HDC>,
    font_body: HFONT,
    scale: f32,
    x: i32,
    y: i32,
) -> Hit {
    if inside(&l.close_btn, x, y) {
        return Hit::Close;
    }
    let spec = state.button_spec();
    if spec.primary_enabled && inside(&l.primary_btn, x, y) {
        return Hit::Primary;
    }
    if spec.secondary_enabled && inside(&l.secondary_btn, x, y) {
        return Hit::Secondary;
    }
    if let DialogState::Available {
        bundle, changelog, ..
    } = state
    {
        if let Some(idx) =
            changelog.hit_version(x, y, &bundle.uninstalled, &l.viewport, dc, font_body, scale)
        {
            return Hit::Version(idx);
        }
    }
    let drag_bottom = if l.compact { l.footer_top } else { l.hero.bottom };
    if y < drag_bottom && !inside(&l.close_btn, x, y) {
        return Hit::DragBar;
    }
    Hit::None
}

/// 以指定初始状态创建并居中显示更新弹窗。
fn create_dialog_window(app: &mut App, state: DialogState) {
    unsafe {
        register_class(app.hinst);
        app.modal += 1;
        let ptr = app as *mut App;
        let scale = dpi_scale_at_cursor();
        let (w_dp, h_dp) = if state.is_compact() {
            (COMPACT_W_DP, COMPACT_H_DP)
        } else {
            (EXPANDED_W_DP, EXPANDED_H_DP)
        };
        let mut fonts = DialogFonts::default();
        fonts.rebuild(scale);

        let dlg = Box::new(Dlg {
            app: ptr,
            hwnd: HWND::default(),
            scale,
            target: DibTarget::default(),
            fonts,
            state,
            hover: Hit::None,
            pressed: Hit::None,
            tip_pending: None,
            tip_visible: None,
        });

        let cw = (w_dp * scale).round() as i32;
        let ch = (h_dp * scale).round() as i32;
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let (x, y) = centered_bounds_on_monitor(mon, cw, ch);

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

/// 打开或激活更新提示弹窗，展示所有未安装版本的折叠更新日志与一键更新按钮。
pub fn open(app: &mut App, bundle: UpdateBundle) {
    app.clear_pending_update_popup();
    unsafe {
        if !app.update_hwnd.is_invalid() && IsWindow(app.update_hwnd).as_bool() {
            let boxed = Box::into_raw(Box::new(bundle)) as isize;
            let _ = SendMessageW(app.update_hwnd, WM_APP_REFRESH_BUNDLE, WPARAM(0), LPARAM(boxed));
            let _ = SetForegroundWindow(app.update_hwnd);
            return;
        }
    }
    create_dialog_window(
        app,
        DialogState::Available {
            bundle,
            changelog: ChangelogView::default(),
            pending: None,
            status: String::new(),
            failed: false,
            restarting: false,
        },
    );
}

/// 从托盘右键菜单打开居中更新弹窗，立即开始检测并展示结果或触发更新。
pub fn open_check(app: &mut App) {
    app.clear_pending_update_popup();
    unsafe {
        if !app.update_hwnd.is_invalid() && IsWindow(app.update_hwnd).as_bool() {
            let _ = SendMessageW(app.update_hwnd, WM_APP_START_CHECK, WPARAM(0), LPARAM(0));
            let _ = SetForegroundWindow(app.update_hwnd);
            return;
        }
    }
    let state = spawn_check_state();
    create_dialog_window(app, state);
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
        Hit::Secondary => {
            let text = match &dlg.state {
                DialogState::Checking { .. } => "取消本次检测并关闭弹窗",
                DialogState::UpToDate { .. } => "重新连接发布服务器检测是否有新版本",
                DialogState::CheckFailed { .. } => "关闭弹窗",
                DialogState::Available { .. } => "关闭弹窗，稍后可从「设置 → 关于」更新",
            };
            Some((Hit::Secondary, l.secondary_btn, text.into()))
        }
        Hit::Primary => {
            let text = match &dlg.state {
                DialogState::Checking { .. } => None,
                DialogState::UpToDate { .. } => Some("关闭弹窗"),
                DialogState::CheckFailed { .. } => {
                    Some("优先检测 GitHub，连接失败时自动尝试腾讯云 CNB")
                }
                DialogState::Available { .. } => Some("验签通过后安装并重启 WinLaunchpad"),
            };
            text.map(|t| (Hit::Primary, l.primary_btn, t.into()))
        }
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

/// 将弹窗内容渲染到给定 Surface（同时服务窗口绘制与离屏像素单元测试）。
#[allow(clippy::too_many_arguments)]
fn render_dialog(
    sf: &mut Surface,
    dc: HDC,
    fonts: &DialogFonts,
    state: &DialogState,
    l: &UpdateLayout,
    hover: Hit,
    pressed: Hit,
    tip_visible: Option<&(RECT, String)>,
    s: f32,
) {
    let p = |dp: i32| (dp as f32 * s).round() as i32;
    let primary = rgb(0x17253D, 1.0);
    let muted = rgb(0x52647D, 1.0);
    let border = rgb(0xE1E8F1, 1.0);

    sf.clear(rgb(0xFFFFFF, 1.0));

    // 顶部状态横幅卡片
    sf.fill_rrect(&l.hero, 12.0 * s, rgb(0xF1F6FF, 1.0));
    draw_app_badge(
        sf,
        (l.hero.left + p(44)) as f32,
        ((l.hero.top + l.hero.bottom) / 2) as f32,
        52.0 * s,
        s,
    );

    let (title, status, is_error) = state.hero_display();
    let title_top = if l.compact { p(22) } else { p(17) };
    let title_rc = RECT {
        left: l.hero.left + p(84),
        top: l.hero.top + title_top,
        right: l.close_btn.left - p(10),
        bottom: l.hero.top + title_top + p(28),
    };
    draw_text(
        dc,
        &title,
        fonts.title,
        &title_rc,
        primary,
        TextAlign::Left,
        true,
        false,
    );

    let status_col = if is_error {
        rgb(0xB33B22, 1.0)
    } else {
        muted
    };
    let status_top = if l.compact { p(53) } else { p(47) };
    let status_rc = RECT {
        left: l.hero.left + p(84),
        top: l.hero.top + status_top,
        right: l.hero.right - p(16),
        bottom: l.hero.top + status_top + p(24),
    };
    draw_text(
        dc,
        &status,
        fonts.body,
        &status_rc,
        status_col,
        TextAlign::Left,
        true,
        false,
    );

    // 右上角关闭按钮
    if hover == Hit::Close || pressed == Hit::Close {
        let cbg = if pressed == Hit::Close {
            rgb(0xFEE2E2, 1.0)
        } else {
            rgb(0xE2E8F0, 0.8)
        };
        sf.fill_rrect(&l.close_btn, 7.0 * s, cbg);
    }
    let ccx = (l.close_btn.left + l.close_btn.right) as f32 * 0.5;
    let ccy = (l.close_btn.top + l.close_btn.bottom) as f32 * 0.5;
    let ca = 4.4 * s;
    let ccol = if hover == Hit::Close || pressed == Hit::Close {
        rgb(0xDC2626, 1.0)
    } else {
        rgb(0x64748B, 1.0)
    };
    sf.thick_line(ccx - ca, ccy - ca, ccx + ca, ccy + ca, (1.5 * s).max(1.1), ccol);
    sf.thick_line(ccx - ca, ccy + ca, ccx + ca, ccy - ca, (1.5 * s).max(1.1), ccol);

    // 发现新版本时展示全部未安装版本的折叠更新日志
    if let DialogState::Available {
        bundle, changelog, ..
    } = state
    {
        sf.stroke_rrect(&l.history, 12.0 * s, s.max(1.0), border);
        draw_text(
            dc,
            "更新日志",
            fonts.section,
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
        let hover_idx = match hover {
            Hit::Version(i) => Some(i),
            _ => None,
        };
        let pressed_idx = match pressed {
            Hit::Version(i) => Some(i),
            _ => None,
        };
        changelog.paint(
            sf,
            dc,
            &bundle.uninstalled,
            &l.viewport,
            fonts.section,
            fonts.body,
            fonts.sub,
            hover_idx,
            pressed_idx,
            s,
        );
    }

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

    let spec = state.button_spec();
    if spec.secondary_enabled {
        draw_button(
            sf,
            dc,
            &l.secondary_btn,
            spec.secondary_label,
            fonts.btn,
            BtnStyle::Secondary,
            hover == Hit::Secondary,
            pressed == Hit::Secondary,
            s,
        );
    }
    if spec.primary_enabled {
        draw_button(
            sf,
            dc,
            &l.primary_btn,
            spec.primary_label,
            fonts.btn,
            BtnStyle::Primary,
            hover == Hit::Primary,
            pressed == Hit::Primary,
            s,
        );
    } else {
        sf.fill_rrect(&l.primary_btn, 8.0 * s, rgb(0xEDF2F8, 1.0));
        draw_text(
            dc,
            spec.primary_label,
            fonts.btn,
            &l.primary_btn,
            muted,
            TextAlign::Center,
            false,
            false,
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
    if let Some((anchor, tip)) = tip_visible {
        draw_tooltip(sf, dc, fonts.tip, anchor, tip, l.w, l.h, s);
    }
}

fn paint_dlg(dlg: &mut Dlg) {
    let l = dlg.layout();
    if !dlg.target.ensure(l.w, l.h) {
        return;
    }
    let Some(dc) = dlg.target.mem_dc else { return };
    let Some(mut sf) = dlg.target.surface() else { return };
    render_dialog(
        &mut sf,
        dc,
        &dlg.fonts,
        &dlg.state,
        &l,
        dlg.hover,
        dlg.pressed,
        dlg.tip_visible.as_ref(),
        dlg.scale,
    );

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
    // 延迟至 WM_NCDESTROY 摘除指针并释放状态，防止销毁过程重入访问悬垂指针
    if msg == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(ptr));
    }
    ret
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
                dlg.fonts.rebuild(dlg.scale);
                if matches!(dlg.state, DialogState::Checking { .. }) {
                    let _ = SetTimer(hwnd, TIMER_UPDATE, 100, None);
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint_dlg(dlg);
                LRESULT(0)
            }
            WM_APP_REFRESH_BUNDLE => {
                let next = *Box::from_raw(lparam.0 as *mut UpdateBundle);
                if !dlg.state.is_installing_or_restarting() {
                    let same_version = matches!(
                        &dlg.state,
                        DialogState::Available { bundle, .. } if bundle.latest.version == next.latest.version
                    );
                    if !same_version {
                        let _ = KillTimer(hwnd, TIMER_UPDATE);
                        dlg.state = DialogState::Available {
                            bundle: next,
                            changelog: ChangelogView::default(),
                            pending: None,
                            status: String::new(),
                            failed: false,
                            restarting: false,
                        };
                        dlg.reset_interaction();
                        dlg.sync_window_size();
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_APP_START_CHECK => {
                if dlg.start_check() {
                    let _ = SetTimer(hwnd, TIMER_UPDATE, 100, None);
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let new = lparam.0 as *const RECT;
                let r = *new;
                let dpi = hiword(wparam.0) as u32;
                dlg.scale = dpi as f32 / 96.0;
                dlg.fonts.rebuild(dlg.scale);
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
                if wparam.0 == TIMER_TOOLTIP {
                    let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                    if let Some((_, rc, text)) = &dlg.tip_pending {
                        dlg.tip_visible = Some((*rc, text.clone()));
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if wparam.0 == TIMER_UPDATE {
                    let (done, restart, size_changed) = dlg.poll(app);
                    if done {
                        let _ = KillTimer(hwnd, TIMER_UPDATE);
                        if size_changed {
                            dlg.sync_window_size();
                        }
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
                if let DialogState::Available {
                    bundle, changelog, ..
                } = &mut dlg.state
                {
                    if changelog.scroll_by(
                        pt.x,
                        pt.y,
                        get_wheel_delta(wparam.0),
                        &bundle.uninstalled,
                        &l.viewport,
                        dlg.target.mem_dc,
                        dlg.fonts.body,
                        dlg.scale,
                    ) {
                        dlg.hover = dlg.hit_test(&l, pt.x, pt.y);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
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
                        Hit::Close if !dlg.state.is_restarting() => {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        Hit::Secondary => match &dlg.state {
                            DialogState::UpToDate { .. } => {
                                dlg.tip_pending = None;
                                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                                if dlg.start_check() {
                                    let _ = SetTimer(hwnd, TIMER_UPDATE, 100, None);
                                }
                            }
                            DialogState::Checking { .. }
                            | DialogState::CheckFailed { .. }
                            | DialogState::Available { .. }
                                if !dlg.state.is_restarting() =>
                            {
                                let _ = DestroyWindow(hwnd);
                                return LRESULT(0);
                            }
                            _ => {}
                        },
                        Hit::Primary => match &dlg.state {
                            DialogState::UpToDate { .. } => {
                                let _ = DestroyWindow(hwnd);
                                return LRESULT(0);
                            }
                            DialogState::CheckFailed { .. } => {
                                dlg.tip_pending = None;
                                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                                if dlg.start_check() {
                                    let _ = SetTimer(hwnd, TIMER_UPDATE, 100, None);
                                }
                            }
                            DialogState::Available { .. } => {
                                dlg.tip_pending = None;
                                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                                if dlg.start_install() {
                                    let _ = SetTimer(hwnd, TIMER_UPDATE, 100, None);
                                }
                            }
                            DialogState::Checking { .. } => {}
                        },
                        Hit::Version(index) => {
                            if let DialogState::Available {
                                bundle, changelog, ..
                            } = &mut dlg.state
                            {
                                changelog.toggle(
                                    index,
                                    &bundle.uninstalled,
                                    &l.viewport,
                                    dlg.target.mem_dc,
                                    dlg.fonts.body,
                                    dlg.scale,
                                );
                            }
                        }
                        _ => {}
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 as u32 == VK_ESCAPE.0 as u32 && !dlg.state.is_restarting() {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if !dlg.state.is_restarting() {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                let _ = KillTimer(hwnd, TIMER_UPDATE);
                app.modal = app.modal.saturating_sub(1);
                app.update_hwnd = HWND::default();
                dlg.fonts.destroy();
                dlg.target.release();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{Release, ReleaseNote, ReleaseSection};

    fn sample_bundle() -> UpdateBundle {
        UpdateBundle {
            latest: Release {
                version: "9.9.9".into(),
                channel: Channel::GitHub,
            },
            uninstalled: vec![ReleaseNote {
                version: "9.9.9".into(),
                date: "2026-11-26".into(),
                sections: vec![ReleaseSection {
                    title: "更新".into(),
                    items: vec!["✨ 支持托盘检测更新弹窗".into()],
                }],
            }],
        }
    }

    #[test]
    fn compact_and_expanded_states_expose_expected_hits_and_specs() {
        let (_tx, rx) = mpsc::channel();
        let checking = DialogState::Checking { receiver: rx };
        let spec_c = checking.button_spec();
        assert_eq!(spec_c.secondary_label, "取消");
        assert!(spec_c.secondary_enabled);
        assert_eq!(spec_c.primary_label, "检测中");
        assert!(!spec_c.primary_enabled);

        let lc = UpdateLayout::build(440, 198, true, &spec_c, 1.0);
        // 检测中主按钮禁点，点击应返回 None 而非 Hit::Primary
        assert_eq!(
            hit_test_state(
                &checking,
                &lc,
                None,
                HFONT::default(),
                1.0,
                (lc.primary_btn.left + lc.primary_btn.right) / 2,
                (lc.primary_btn.top + lc.primary_btn.bottom) / 2,
            ),
            Hit::None
        );
        assert_eq!(
            hit_test_state(
                &checking,
                &lc,
                None,
                HFONT::default(),
                1.0,
                (lc.secondary_btn.left + lc.secondary_btn.right) / 2,
                (lc.secondary_btn.top + lc.secondary_btn.bottom) / 2,
            ),
            Hit::Secondary
        );

        let up_to_date = DialogState::UpToDate {
            channel: Channel::Cnb,
        };
        let spec_u = up_to_date.button_spec();
        assert_eq!(spec_u.secondary_label, "重新检测");
        assert_eq!(spec_u.primary_label, "关闭");
        let lu = UpdateLayout::build(440, 198, true, &spec_u, 1.0);
        assert_eq!(
            hit_test_state(
                &up_to_date,
                &lu,
                None,
                HFONT::default(),
                1.0,
                (lu.primary_btn.left + lu.primary_btn.right) / 2,
                (lu.primary_btn.top + lu.primary_btn.bottom) / 2,
            ),
            Hit::Primary
        );

        let avail = DialogState::Available {
            bundle: sample_bundle(),
            changelog: ChangelogView::default(),
            pending: None,
            status: String::new(),
            failed: false,
            restarting: false,
        };
        let spec_a = avail.button_spec();
        assert_eq!(spec_a.secondary_label, "稍后更新");
        assert_eq!(spec_a.primary_label, "一键更新");
        let la = UpdateLayout::build(540, 460, false, &spec_a, 1.0);
        assert_eq!(
            hit_test_state(
                &avail,
                &la,
                None,
                HFONT::default(),
                1.0,
                (la.primary_btn.left + la.primary_btn.right) / 2,
                (la.primary_btn.top + la.primary_btn.bottom) / 2,
            ),
            Hit::Primary
        );
    }

    #[test]
    fn compact_result_states_render_cleanly_with_text_and_buttons() {
        let scale = 1.5f32;
        let mut fonts = DialogFonts::default();
        fonts.rebuild(scale);
        let mut target = DibTarget::default();

        for state in [
            DialogState::UpToDate {
                channel: Channel::GitHub,
            },
            DialogState::CheckFailed {
                message: "检测失败，请检查网络后重试".into(),
            },
        ] {
            let spec = state.button_spec();
            let cw = (COMPACT_W_DP * scale).round() as i32;
            let ch = (COMPACT_H_DP * scale).round() as i32;
            let l = UpdateLayout::build(cw, ch, true, &spec, scale);
            assert!(target.ensure(l.w, l.h));

            {
                let dc = target.mem_dc.unwrap();
                let mut sf = target.surface().unwrap();
                render_dialog(
                    &mut sf,
                    dc,
                    &fonts,
                    &state,
                    &l,
                    Hit::Primary,
                    Hit::None,
                    None,
                    scale,
                );
            }

            let px = unsafe {
                std::slice::from_raw_parts(target.ptr as *const u32, (l.w * l.h) as usize)
            };
            // 主按钮在悬停状态下应呈亮蓝 #3B82F6（R < B）
            let btn_cx = ((l.primary_btn.left + l.primary_btn.right) / 2) as usize;
            let btn_top_sample = px[(l.primary_btn.top as usize + 4) * l.w as usize + btn_cx];
            let r = (btn_top_sample >> 16) & 0xFF;
            let b = btn_top_sample & 0xFF;
            assert!(b > r + 80, "主按钮悬停背景应呈蓝色，实际像素: {btn_top_sample:#010X}");
        }

        target.release();
        fonts.destroy();
    }
}
