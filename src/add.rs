//! 「添加应用」窗口：参考 macOS 启动台卡片样式，支持拖放或浏览多选 .exe/.lnk，
//! 展示真实图标与路径并支持单条移除，确认后写入启动台并保留已移除自动条目的恢复语义。

use std::path::PathBuf;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, DeleteObject, EndPaint, InvalidateRect, HBRUSH, HFONT, HGDIOBJ,
    PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::App;
use crate::catalog::AppEntry;
use crate::ui::{
    draw_app_badge, draw_button, draw_scrollbar, draw_text, draw_tooltip, make_font, rgb,
    BtnStyle, DibTarget, TextAlign,
};
use crate::win32::*;

const TIMER_TOOLTIP: usize = 101;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    DragBar,
    Close,
    Browse,
    Done,
    DropEmpty,
    Row(usize),
    RowRemove(usize),
}

struct AddLayout {
    w: i32,
    h: i32,
    close_btn: RECT,
    rocket_cx: f32,
    rocket_cy: f32,
    rocket_sz: f32,
    title_rc: RECT,
    sub_rc: RECT,
    drop_rc: RECT,
    browse_btn: RECT,
    done_btn: RECT,
    scroll_track: RECT,
    row_h: i32,
    visible_rows: usize,
}

struct Dlg {
    app: *mut App,
    hwnd: HWND,
    scale: f32,
    target: DibTarget,
    font_title: HFONT,
    font_sub: HFONT,
    font_item: HFONT,
    font_path: HFONT,
    font_btn: HFONT,
    font_tip: HFONT,
    pending: Vec<AppEntry>,
    scroll: usize,
    hover: Hit,
    pressed: Hit,
    tip_pending: Option<(Hit, RECT, String)>,
    tip_visible: Option<(RECT, String)>,
}

impl Dlg {
    fn rebuild_fonts(&mut self) {
        self.destroy_fonts();
        let s = |px: f32| (px * self.scale).round() as i32;
        self.font_title = make_font(s(19.0), 700);
        self.font_sub = make_font(s(13.0), 400);
        self.font_item = make_font(s(13.5), 600);
        self.font_path = make_font(s(11.5), 400);
        self.font_btn = make_font(s(13.5), 600);
        self.font_tip = make_font(s(12.0), 400);
    }

    fn destroy_fonts(&mut self) {
        unsafe {
            for f in [
                self.font_title,
                self.font_sub,
                self.font_item,
                self.font_path,
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

    fn layout(&self) -> AddLayout {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let s = |v: f32| (v * self.scale).round() as i32;

        let close_sz = s(22.0);
        let close_btn = RECT {
            left: s(14.0),
            top: s(14.0),
            right: s(14.0) + close_sz,
            bottom: s(14.0) + close_sz,
        };
        let rocket_sz = 64.0 * self.scale;
        let rocket_cx = w as f32 * 0.5;
        let rocket_cy = 56.0 * self.scale;

        let title_rc = RECT {
            left: s(24.0),
            top: s(96.0),
            right: w - s(24.0),
            bottom: s(126.0),
        };
        let sub_rc = RECT {
            left: s(24.0),
            top: s(126.0),
            right: w - s(24.0),
            bottom: s(150.0),
        };
        let drop_rc = RECT {
            left: s(34.0),
            top: s(160.0),
            right: w - s(34.0),
            bottom: h - s(76.0),
        };
        let browse_btn = if self.pending.is_empty() {
            let bw = s(124.0);
            let bh = s(32.0);
            RECT {
                left: (w - bw) / 2,
                top: (drop_rc.top + drop_rc.bottom) / 2 + s(16.0),
                right: (w + bw) / 2,
                bottom: (drop_rc.top + drop_rc.bottom) / 2 + s(16.0) + bh,
            }
        } else {
            RECT {
                left: s(34.0),
                top: h - s(56.0),
                right: s(34.0) + s(110.0),
                bottom: h - s(22.0),
            }
        };
        let done_w = s(126.0);
        let done_h = s(36.0);
        let done_btn = RECT {
            left: (w - done_w) / 2,
            top: h - s(58.0),
            right: (w + done_w) / 2,
            bottom: h - s(58.0) + done_h,
        };
        let row_h = s(46.0);
        let inner_h = (drop_rc.bottom - drop_rc.top - s(12.0)).max(row_h);
        let visible_rows = (inner_h / row_h).max(1) as usize;
        let scroll_track = RECT {
            left: drop_rc.right - s(10.0),
            top: drop_rc.top + s(8.0),
            right: drop_rc.right - s(4.0),
            bottom: drop_rc.bottom - s(8.0),
        };

        AddLayout {
            w,
            h,
            close_btn,
            rocket_cx,
            rocket_cy,
            rocket_sz,
            title_rc,
            sub_rc,
            drop_rc,
            browse_btn,
            done_btn,
            scroll_track,
            row_h,
            visible_rows,
        }
    }

    fn row_rect(&self, l: &AddLayout, vis_idx: usize) -> RECT {
        let pad = (8.0 * self.scale).round() as i32;
        let top = l.drop_rc.top + (6.0 * self.scale).round() as i32 + vis_idx as i32 * l.row_h;
        RECT {
            left: l.drop_rc.left + pad,
            top,
            right: l.drop_rc.right - (16.0 * self.scale).round() as i32,
            bottom: (top + l.row_h - (4.0 * self.scale).round() as i32).min(l.drop_rc.bottom - (6.0 * self.scale).round() as i32),
        }
    }

    fn row_remove_rect(&self, row: &RECT) -> RECT {
        let sz = (22.0 * self.scale).round() as i32;
        let cy = (row.top + row.bottom) / 2;
        RECT {
            left: row.right - sz - (8.0 * self.scale).round() as i32,
            top: cy - sz / 2,
            right: row.right - (8.0 * self.scale).round() as i32,
            bottom: cy + sz / 2,
        }
    }

    fn hit_test(&self, l: &AddLayout, x: i32, y: i32) -> Hit {
        let inside = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if inside(&l.close_btn) {
            return Hit::Close;
        }
        if inside(&l.done_btn) {
            return Hit::Done;
        }
        if inside(&l.browse_btn) {
            return Hit::Browse;
        }
        if self.pending.is_empty() {
            if inside(&l.drop_rc) {
                return Hit::DropEmpty;
            }
        } else if inside(&l.drop_rc) {
            for vi in 0..l.visible_rows {
                let idx = self.scroll + vi;
                if idx >= self.pending.len() {
                    break;
                }
                let rr = self.row_rect(l, vi);
                if inside(&rr) {
                    let rm = self.row_remove_rect(&rr);
                    if inside(&rm) {
                        return Hit::RowRemove(idx);
                    }
                    return Hit::Row(idx);
                }
            }
        }
        if y < l.drop_rc.top {
            return Hit::DragBar;
        }
        Hit::None
    }
}

pub fn open(app: &mut App) {
    unsafe {
        if !app.add_hwnd.is_invalid() && IsWindow(app.add_hwnd).as_bool() {
            let _ = SetForegroundWindow(app.add_hwnd);
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
            font_sub: HFONT::default(),
            font_item: HFONT::default(),
            font_path: HFONT::default(),
            font_btn: HFONT::default(),
            font_tip: HFONT::default(),
            pending: Vec::new(),
            scroll: 0,
            hover: Hit::None,
            pressed: Hit::None,
            tip_pending: None,
            tip_visible: None,
        });
        dlg.rebuild_fonts();

        let cw = (500.0 * scale).round() as i32;
        let ch = (448.0 * scale).round() as i32;
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
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_ACCEPTFILES,
            w!("WinLaunchpadAdd"),
            w!("添加应用到启动台"),
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
            app.modal -= 1;
            return;
        }
        enable_rounded_window(hwnd);
        app.add_hwnd = hwnd;
        DragAcceptFiles(hwnd, true);
        // 以管理员身份运行时放行普通权限资源管理器的拖拽消息
        allow_uipi_messages(hwnd, &[WM_DROPFILES, WM_COPYDATA, 0x0049]);
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
            lpszClassName: w!("WinLaunchpadAdd"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

fn add_paths(dlg: &mut Dlg, app: &mut App, paths: Vec<PathBuf>) {
    for p in paths {
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if (ext != "exe" && ext != "lnk") || !p.is_file() {
            continue;
        }
        let e = AppEntry::manual_from_path(&p);
        if dlg.pending.iter().any(|x| x.id == e.id) {
            continue;
        }
        app.request_icon_async(&e.icon_key);
        dlg.pending.push(e);
    }
    let _ = unsafe { InvalidateRect(dlg.hwnd, None, false) };
}

fn commit_and_close(dlg: &mut Dlg, app: &mut App) {
    if !dlg.pending.is_empty() {
        let ids: Vec<String> = dlg.pending.iter().map(|e| e.id.clone()).collect();
        app.settings.manual.retain(|m| !ids.contains(&m.id));
        app.settings.manual.extend(dlg.pending.iter().cloned());
        // 若用户显式添加了一个曾被隐藏的自动发现目标，同步恢复它
        let targets: Vec<String> = dlg.pending.iter().map(|e| e.target.to_lowercase()).collect();
        for e in &app.scan.entries {
            if targets.contains(&e.target.to_lowercase()) {
                app.settings.app_overrides.insert(e.id.clone(), true);
            }
        }
        app.store.save_settings(&app.settings);
        app.rebuild_view();
    }
    unsafe {
        let _ = DestroyWindow(dlg.hwnd);
    }
}

fn update_tip(dlg: &mut Dlg, l: &AddLayout, hit: Hit) {
    let next: Option<(Hit, RECT, String)> = match hit {
        Hit::Close => Some((Hit::Close, l.close_btn, "关闭".into())),
        Hit::RowRemove(idx) => {
            if let Some(vi) = idx.checked_sub(dlg.scroll) {
                let rr = dlg.row_rect(l, vi);
                Some((Hit::RowRemove(idx), dlg.row_remove_rect(&rr), "从列表移除".into()))
            } else {
                None
            }
        }
        Hit::Row(idx) => {
            if let (Some(e), Some(vi)) = (dlg.pending.get(idx), idx.checked_sub(dlg.scroll)) {
                Some((Hit::Row(idx), dlg.row_rect(l, vi), e.target.clone()))
            } else {
                None
            }
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

fn paint_dlg(dlg: &mut Dlg, app: &App) {
    let l = dlg.layout();
    if !dlg.target.ensure(l.w, l.h) {
        return;
    }
    let Some(dc) = dlg.target.mem_dc else { return };
    let Some(mut surface) = dlg.target.surface() else { return };
    let s = dlg.scale;

    // 卡片背景与边框
    surface.clear(rgb(0xFFFFFF, 1.0));
    let full = RECT { left: 0, top: 0, right: l.w, bottom: l.h };
    surface.stroke_rrect(&full, 12.0 * s, (1.0 * s).max(1.0), rgb(0xE2E8F0, 1.0));

    // 左上角圆形关闭按钮（参照图 5）
    let close_cx = (l.close_btn.left + l.close_btn.right) as f32 * 0.5;
    let close_cy = (l.close_btn.top + l.close_btn.bottom) as f32 * 0.5;
    let close_r = (l.close_btn.right - l.close_btn.left) as f32 * 0.5;
    let close_bg = if dlg.pressed == Hit::Close {
        rgb(0x64748B, 1.0)
    } else if dlg.hover == Hit::Close {
        rgb(0x94A3B8, 1.0)
    } else {
        rgb(0xB8BFC9, 1.0)
    };
    surface.fill_circle(close_cx, close_cy, close_r, close_bg);
    let arm = 3.8 * s;
    let cross_c = rgb(0xFFFFFF, 1.0);
    surface.thick_line(close_cx - arm, close_cy - arm, close_cx + arm, close_cy + arm, (1.7 * s).max(1.2), cross_c);
    surface.thick_line(close_cx - arm, close_cy + arm, close_cx + arm, close_cy - arm, (1.7 * s).max(1.2), cross_c);

    // 顶部软件徽标与标题
    draw_app_badge(&mut surface, l.rocket_cx, l.rocket_cy, l.rocket_sz, s);
    draw_text(dc, "启动台", dlg.font_title, &l.title_rc, rgb(0x111827, 1.0), TextAlign::Center, true, false);
    draw_text(dc, "请将 .exe、.lnk 文件拖放到此处", dlg.font_sub, &l.sub_rc, rgb(0x64748B, 1.0), TextAlign::Center, true, false);

    // 中央拖放区
    let drop_hover = dlg.pending.is_empty() && matches!(dlg.hover, Hit::DropEmpty | Hit::Browse);
    let drop_bg = if drop_hover { rgb(0xEFF6FF, 1.0) } else { rgb(0xF1F5F9, 1.0) };
    let drop_border = if drop_hover { rgb(0x1677FF, 0.85) } else { rgb(0xCBD5E1, 1.0) };
    surface.fill_rrect(&l.drop_rc, 10.0 * s, drop_bg);
    surface.stroke_rrect_dashed(&l.drop_rc, 10.0 * s, (1.3 * s).max(1.0), (5.0 * s).round() as i32, drop_border);

    if dlg.pending.is_empty() {
        let cx = (l.drop_rc.left + l.drop_rc.right) as f32 * 0.5;
        let cy = (l.drop_rc.top + l.drop_rc.bottom) as f32 * 0.5 - 24.0 * s;
        let r = 18.0 * s;
        surface.fill_circle(cx, cy, r, rgb(0xE2E8F0, 0.9));
        let p = 6.5 * s;
        let pc = rgb(0x64748B, 1.0);
        surface.thick_line(cx - p, cy, cx + p, cy, (2.0 * s).max(1.4), pc);
        surface.thick_line(cx, cy - p, cx, cy + p, (2.0 * s).max(1.4), pc);
        draw_button(
            &mut surface,
            dc,
            &l.browse_btn,
            "选择文件",
            dlg.font_btn,
            BtnStyle::Secondary,
            dlg.hover == Hit::Browse,
            dlg.pressed == Hit::Browse,
            s,
        );
    } else {
        surface.set_clip(Some(l.drop_rc));
        for vi in 0..l.visible_rows {
            let idx = dlg.scroll + vi;
            let Some(e) = dlg.pending.get(idx) else { break };
            let rr = dlg.row_rect(&l, vi);
            let is_row_hov = matches!(dlg.hover, Hit::Row(i) | Hit::RowRemove(i) if i == idx);
            let row_bg = if is_row_hov { rgb(0xFFFFFF, 1.0) } else { rgb(0xFFFFFF, 0.72) };
            surface.fill_rrect(&rr, 8.0 * s, row_bg);
            if is_row_hov {
                surface.stroke_rrect(&rr, 8.0 * s, 1.0, rgb(0xDBEAFE, 1.0));
            }
            let icon_sz = 26.0 * s;
            let icx = rr.left as f32 + 22.0 * s;
            let icy = (rr.top + rr.bottom) as f32 * 0.5;
            if let Some(icon) = app.cached_icon(&e.icon_key) {
                surface.draw_icon_bilinear(&icon, icx, icy, icon_sz, false, 0.0);
            } else {
                let half = icon_sz * 0.5;
                let ir = RECT {
                    left: (icx - half) as i32,
                    top: (icy - half) as i32,
                    right: (icx + half) as i32,
                    bottom: (icy + half) as i32,
                };
                surface.fill_rrect(&ir, 6.0 * s, rgb(0xE2E8F0, 1.0));
            }
            let rm = dlg.row_remove_rect(&rr);
            let name_rc = RECT {
                left: rr.left + (44.0 * s) as i32,
                top: rr.top + (3.0 * s) as i32,
                right: rm.left - (8.0 * s) as i32,
                bottom: rr.top + (22.0 * s) as i32,
            };
            let path_rc = RECT {
                left: name_rc.left,
                top: rr.top + (22.0 * s) as i32,
                right: name_rc.right,
                bottom: rr.bottom - (2.0 * s) as i32,
            };
            crate::ui::draw_text(dc, &e.name, dlg.font_item, &name_rc, rgb(0x111827, 1.0), TextAlign::Left, true, false);
            crate::ui::draw_text(dc, &e.target, dlg.font_path, &path_rc, rgb(0x64748B, 1.0), TextAlign::Left, true, false);

            let rm_hov = dlg.hover == Hit::RowRemove(idx);
            let rm_prs = dlg.pressed == Hit::RowRemove(idx);
            if rm_hov || rm_prs {
                surface.fill_circle(
                    (rm.left + rm.right) as f32 * 0.5,
                    (rm.top + rm.bottom) as f32 * 0.5,
                    (rm.right - rm.left) as f32 * 0.5,
                    if rm_prs { rgb(0xFECACA, 1.0) } else { rgb(0xFEE2E2, 1.0) },
                );
            }
            let rcx = (rm.left + rm.right) as f32 * 0.5;
            let rcy = (rm.top + rm.bottom) as f32 * 0.5;
            let ra = 3.5 * s;
            let rc_col = if rm_hov || rm_prs { rgb(0xDC2626, 1.0) } else { rgb(0x94A3B8, 1.0) };
            surface.thick_line(rcx - ra, rcy - ra, rcx + ra, rcy + ra, (1.5 * s).max(1.0), rc_col);
            surface.thick_line(rcx - ra, rcy + ra, rcx + ra, rcy - ra, (1.5 * s).max(1.0), rc_col);
        }
        surface.set_clip(None);
        draw_scrollbar(
            &mut surface,
            &l.scroll_track,
            dlg.pending.len(),
            l.visible_rows,
            dlg.scroll,
            false,
            s,
        );
        draw_button(
            &mut surface,
            dc,
            &l.browse_btn,
            "继续添加",
            dlg.font_btn,
            BtnStyle::Ghost,
            dlg.hover == Hit::Browse,
            dlg.pressed == Hit::Browse,
            s,
        );
    }

    // 底部主按钮「完成」（参照图 5 蓝色胶囊按钮）
    draw_button(
        &mut surface,
        dc,
        &l.done_btn,
        "完成",
        dlg.font_btn,
        BtnStyle::Primary,
        dlg.hover == Hit::Done,
        dlg.pressed == Hit::Done,
        s,
    );

    if let Some((anchor, tip)) = &dlg.tip_visible {
        draw_tooltip(&mut surface, dc, dlg.font_tip, anchor, tip, l.w, l.h, s);
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
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(dlg, app, hwnd, msg, wparam, lparam)));
    let ret = match result {
        Ok(v) => v,
        Err(_) => {
            crate::log::log("添加窗口处理消息时 panic");
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    };
    if msg == WM_DESTROY {
        drop(Box::from_raw(ptr));
    }
    ret
}

fn handle(dlg: &mut Dlg, app: &mut App, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
                paint_dlg(dlg, app);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let new = lparam.0 as *const RECT;
                let r = *new;
                let dpi = hiword(wparam.0) as u32;
                dlg.scale = dpi as f32 / 96.0;
                dlg.rebuild_fonts();
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_DROPFILES => {
                let hdrop = HDROP(wparam.0 as *mut _);
                let n = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
                let mut paths = Vec::new();
                for i in 0..n {
                    let len = DragQueryFileW(hdrop, i, None);
                    let mut buf = vec![0u16; len as usize + 1];
                    let got = DragQueryFileW(hdrop, i, Some(&mut buf));
                    if got > 0 {
                        paths.push(PathBuf::from(from_wide(&buf)));
                    }
                }
                DragFinish(hdrop);
                add_paths(dlg, app, paths);
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
                if !dlg.app.is_null() {
                    (*dlg.app).ensure_gesture_alive();
                }
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
                if dlg.pending.len() > l.visible_rows {
                    let max_s = dlg.pending.len() - l.visible_rows;
                    let d = get_wheel_delta(wparam.0);
                    if d > 0 {
                        dlg.scroll = dlg.scroll.saturating_sub(1);
                    } else {
                        dlg.scroll = (dlg.scroll + 1).min(max_s);
                    }
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
                    let _ = SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
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
                        Hit::Close => {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        Hit::Browse | Hit::DropEmpty => {
                            let picked = pick_app_files(hwnd);
                            if !picked.is_empty() {
                                add_paths(dlg, app, picked);
                            }
                        }
                        Hit::RowRemove(idx) => {
                            if idx < dlg.pending.len() {
                                dlg.pending.remove(idx);
                                let max_s = dlg.pending.len().saturating_sub(l.visible_rows);
                                dlg.scroll = dlg.scroll.min(max_s);
                            }
                        }
                        Hit::Done => {
                            commit_and_close(dlg, app);
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
                    commit_and_close(dlg, app);
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
                app.add_hwnd = HWND::default();
                dlg.destroy_fonts();
                dlg.target.release();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
