//! 可选内置右键手势（默认关闭）。独立线程安装 WH_MOUSE_LL 钩子：
//! 按住右键滑动 → 采集轨迹点 → 几何特征识别 → 通知主线程执行动作。
//! 未形成手势的普通右键会被吞掉后回放，保证右键菜单正常弹出。

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostMessageW, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PM_NOREMOVE, WH_MOUSE_LL, WM_APP, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

use crate::log;

pub const WM_APP_GESTURE: u32 = WM_APP + 5;
pub const WM_APP_GESTURE_HUD: u32 = WM_APP + 9;
const MSG_ENABLE: u32 = WM_APP + 20;
const MSG_DISABLE: u32 = WM_APP + 21;
const MAGIC_EXTRA: usize = 0x574C_5041;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Left,
    Right,
    Up,
    UpDown,
    DownUp,
}

impl Gesture {
    pub fn id(self) -> usize {
        match self {
            Gesture::Left => 1,
            Gesture::Right => 2,
            Gesture::Up => 3,
            Gesture::UpDown => 4,
            Gesture::DownUp => 5,
        }
    }

    pub fn from_id(id: usize) -> Option<Self> {
        match id {
            1 => Some(Gesture::Left),
            2 => Some(Gesture::Right),
            3 => Some(Gesture::Up),
            4 => Some(Gesture::UpDown),
            5 => Some(Gesture::DownUp),
            _ => None,
        }
    }

    pub fn hud_label(self, swap_lr: bool) -> &'static str {
        match self {
            Gesture::Left => {
                if swap_lr {
                    "← 切换桌面（向左）"
                } else {
                    "← 切换桌面（向右）"
                }
            }
            Gesture::Right => {
                if swap_lr {
                    "→ 切换桌面（向右）"
                } else {
                    "→ 切换桌面（向左）"
                }
            }
            Gesture::Up => "↑ 切换窗口",
            Gesture::UpDown => "∧ 启动台",
            Gesture::DownUp => "∨ 任务视图",
        }
    }
}

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

#[derive(Default)]
struct State {
    tracking: bool,
    moved: bool,
    start: POINT,
    last: POINT,
    pts: Vec<POINT>,
    last_hud: Option<Gesture>,
}

static STATE: Mutex<State> = Mutex::new(State {
    tracking: false,
    moved: false,
    start: POINT { x: 0, y: 0 },
    last: POINT { x: 0, y: 0 },
    pts: Vec::new(),
    last_hud: None,
});

pub struct GestureHandle {
    thread_id: u32,
}

impl GestureHandle {
    pub fn set_enabled(&self, on: bool) {
        let msg_id = if on { MSG_ENABLE } else { MSG_DISABLE };
        unsafe {
            // 若消息队列偶发繁忙，短重试保证启用/禁用手势命令不静默丢失
            for attempt in 0..3 {
                if PostThreadMessageW(self.thread_id, msg_id, WPARAM(0), LPARAM(0)).is_ok() {
                    return;
                }
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            log::log(&format!("发送手势线程消息失败 (on={on})"));
        }
    }
}

/// 启动手势线程（初始不装钩子）。
pub fn start(main_hwnd: HWND) -> Option<GestureHandle> {
    MAIN_HWND.store(main_hwnd.0 as isize, Ordering::SeqCst);
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    std::thread::Builder::new()
        .name("gesture-hook".into())
        .spawn(move || {
            let mut hook: Option<HHOOK> = None;
            unsafe {
                let mut msg = MSG::default();
                // 必须先通过 PeekMessageW 创建本线程消息队列，再通知主线程 thread_id，防止 PostThreadMessageW 竞态丢失
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                let tid = windows::Win32::System::Threading::GetCurrentThreadId();
                let _ = tx.send(tid);
                loop {
                    let r = GetMessageW(&mut msg, None, 0, 0);
                    if r.0 <= 0 {
                        if let Some(h) = hook.take() {
                            let _ = UnhookWindowsHookEx(h);
                        }
                        break;
                    }
                    match msg.message {
                        MSG_ENABLE => {
                            if hook.is_none() {
                                let hmod = crate::win32::hinstance();
                                match SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), hmod, 0) {
                                    Ok(h) => {
                                        hook = Some(h);
                                        log::log("手势钩子已安装");
                                    }
                                    Err(e) => log::log(&format!(
                                        "手势钩子安装失败: {}",
                                        crate::win32::error_string(e)
                                    )),
                                }
                            }
                        }
                        MSG_DISABLE => {
                            if let Some(h) = hook.take() {
                                let _ = UnhookWindowsHookEx(h);
                                reset_state();
                                log::log("手势钩子已卸载");
                            }
                        }
                        _ => {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }
            }
        })
        .ok()?;
    let thread_id = rx.recv().unwrap_or(0);
    if thread_id == 0 {
        return None;
    }
    Some(GestureHandle { thread_id })
}

fn reset_state() {
    if let Ok(mut st) = STATE.lock() {
        st.tracking = false;
        st.moved = false;
        st.pts.clear();
        st.last_hud = None;
    }
}

/// 基于几何拐点与弦长比识别五种预设手势，容忍人手划动的轻微弧度与斜角。
pub fn recognize_pts(pts: &[POINT]) -> Option<Gesture> {
    if pts.len() < 2 {
        return None;
    }
    let start = pts[0];
    let end = *pts.last().unwrap();

    let mut path_len = 0.0f32;
    let (mut min_y_idx, mut min_y) = (0usize, start.y);
    let (mut max_y_idx, mut max_y) = (0usize, start.y);
    for (i, p) in pts.iter().enumerate() {
        if i > 0 {
            let prev = pts[i - 1];
            path_len += (((p.x - prev.x) as f32).powi(2) + ((p.y - prev.y) as f32).powi(2)).sqrt();
        }
        if p.y < min_y {
            min_y = p.y;
            min_y_idx = i;
        }
        if p.y > max_y {
            max_y = p.y;
            max_y_idx = i;
        }
    }

    if path_len < 36.0 {
        return None;
    }

    let dx = (end.x - start.x) as f32;
    let dy = (end.y - start.y) as f32;
    let chord = (dx * dx + dy * dy).sqrt();

    // 1. 折线手势 ∧（先明显向上、再明显向下）
    let up_rise = (start.y - min_y) as f32;
    let down_fall = (end.y - min_y) as f32;
    if up_rise >= 26.0
        && down_fall >= 26.0
        && min_y_idx > 0
        && min_y_idx + 1 < pts.len()
        && path_len <= (up_rise + down_fall + dx.abs()) * 1.75
    {
        return Some(Gesture::UpDown);
    }

    // 2. 折线手势 ∨（先明显向下、再明显向上）
    let down_drop = (max_y - start.y) as f32;
    let up_climb = (max_y - end.y) as f32;
    if down_drop >= 26.0
        && up_climb >= 26.0
        && max_y_idx > 0
        && max_y_idx + 1 < pts.len()
        && path_len <= (down_drop + up_climb + dx.abs()) * 1.75
    {
        return Some(Gesture::DownUp);
    }

    // 3. 直线单向手势（弦长占总路径 >= 72%，避免把圈/折线误判为直线）
    if chord >= 36.0 && (chord / path_len) >= 0.72 {
        if dx < 0.0 && dx.abs() > dy.abs() * 1.3 {
            return Some(Gesture::Left);
        }
        if dx > 0.0 && dx.abs() > dy.abs() * 1.3 {
            return Some(Gesture::Right);
        }
        if dy < 0.0 && dy.abs() > dx.abs() * 1.3 {
            return Some(Gesture::Up);
        }
    }

    None
}

fn is_extended_vk(vk: u16) -> bool {
    matches!(vk, 0x5B | 0x5C | 0x25 | 0x26 | 0x27 | 0x28)
}

unsafe fn key(vk: u16, up: bool) -> INPUT {
    let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    // LWIN 与方向键必须带 KEYEVENTF_EXTENDEDKEY，否则 Win11 虚拟桌面与任务视图热键不响应
    if is_extended_vk(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let mut input = INPUT::default();
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk),
        wScan: MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: MAGIC_EXTRA,
    };
    input
}

unsafe fn send(inputs: &[INPUT]) {
    SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
}

/// 回放一次普通右键（被吞掉的按下 + 释放）。
fn replay_click() {
    unsafe {
        let mut down = INPUT::default();
        down.r#type = INPUT_MOUSE;
        down.Anonymous.mi.mouseData = 0;
        down.Anonymous.mi.dwFlags = MOUSEEVENTF_RIGHTDOWN;
        down.Anonymous.mi.dwExtraInfo = MAGIC_EXTRA;
        let mut up = down;
        up.Anonymous.mi.dwFlags = MOUSEEVENTF_RIGHTUP;
        send(&[down, up]);
    }
}

/// 执行手势对应的系统快捷键（支持左右切桌面方向反转）。
pub fn perform(g: Gesture, swap_lr: bool) {
    let (lctrl, lalt, lwin, tab, left, right) = (0xA2u16, 0xA4u16, 0x5Bu16, 0x09u16, 0x25u16, 0x27u16);
    unsafe {
        match g {
            Gesture::Left => {
                let arrow = if swap_lr { left } else { right };
                send(&[
                    key(lctrl, false),
                    key(lwin, false),
                    key(arrow, false),
                    key(arrow, true),
                    key(lwin, true),
                    key(lctrl, true),
                ]);
            }
            Gesture::Right => {
                let arrow = if swap_lr { right } else { left };
                send(&[
                    key(lctrl, false),
                    key(lwin, false),
                    key(arrow, false),
                    key(arrow, true),
                    key(lwin, true),
                    key(lctrl, true),
                ]);
            }
            Gesture::Up => send(&[
                key(lctrl, false),
                key(lalt, false),
                key(tab, false),
                key(tab, true),
                key(lalt, true),
                key(lctrl, true),
            ]),
            Gesture::DownUp => send(&[
                key(lwin, false),
                key(tab, false),
                key(tab, true),
                key(lwin, true),
            ]),
            Gesture::UpDown => {}
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let mi = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    if mi.flags & LLMHF_INJECTED != 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let msg = wparam.0 as u32;
    let main = HWND(MAIN_HWND.load(Ordering::SeqCst) as *mut _);
    let Ok(mut st) = STATE.lock() else {
        return CallNextHookEx(None, code, wparam, lparam);
    };
    match msg {
        WM_RBUTTONDOWN => {
            if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_MBUTTON.0 as i32) < 0 {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            st.tracking = true;
            st.moved = false;
            st.pts.clear();
            st.pts.push(mi.pt);
            st.start = mi.pt;
            st.last = mi.pt;
            st.last_hud = None;
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            if st.tracking {
                let dx = (mi.pt.x - st.last.x) as f32;
                let dy = (mi.pt.y - st.last.y) as f32;
                if dx * dx + dy * dy >= 64.0 {
                    st.pts.push(mi.pt);
                    st.last = mi.pt;
                    if st.pts.len() > 256 {
                        st.pts.remove(1);
                    }
                    if !st.moved {
                        let sdx = (mi.pt.x - st.start.x) as f32;
                        let sdy = (mi.pt.y - st.start.y) as f32;
                        if sdx * sdx + sdy * sdy >= 24.0 * 24.0 {
                            st.moved = true;
                        }
                    }
                    if st.moved {
                        let cur = recognize_pts(&st.pts);
                        if cur != st.last_hud {
                            st.last_hud = cur;
                            let gid = cur.map(|g| g.id()).unwrap_or(0);
                            let lp = (((mi.pt.y & 0xFFFF) as isize) << 16)
                                | ((mi.pt.x & 0xFFFF) as isize);
                            let _ = PostMessageW(main, WM_APP_GESTURE_HUD, WPARAM(gid), LPARAM(lp));
                        }
                    }
                }
            }
            CallNextHookEx(None, code, wparam, lparam)
        }
        WM_RBUTTONUP => {
            if !st.tracking {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            st.tracking = false;
            if st.moved {
                st.pts.push(mi.pt);
            }
            let g = if st.moved { recognize_pts(&st.pts) } else { None };
            let had_hud = st.last_hud.take().is_some();
            st.pts.clear();
            drop(st);
            if had_hud {
                let _ = PostMessageW(main, WM_APP_GESTURE_HUD, WPARAM(0), LPARAM(0));
            }
            match g {
                Some(g) => {
                    let _ = PostMessageW(main, WM_APP_GESTURE, WPARAM(g.id()), LPARAM(0));
                }
                None => replay_click(),
            }
            LRESULT(1)
        }
        _ => CallNextHookEx(None, code, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    #[test]
    fn recognizes_straight_swipes() {
        assert_eq!(recognize_pts(&[pt(0, 0), pt(-140, 8)]), Some(Gesture::Left));
        assert_eq!(recognize_pts(&[pt(0, 0), pt(160, -10)]), Some(Gesture::Right));
        assert_eq!(recognize_pts(&[pt(0, 0), pt(8, -120)]), Some(Gesture::Up));
    }

    #[test]
    fn recognizes_diagonal_chevrons_and_rejects_noise() {
        // 45° 斜向 ∧（先右上再右下，复刻真实人手轨迹）
        assert_eq!(
            recognize_pts(&[pt(0, 0), pt(25, -45), pt(48, -90), pt(72, -40), pt(96, 5)]),
            Some(Gesture::UpDown)
        );
        // 45° 斜向 ∨（先右下再右上）
        assert_eq!(
            recognize_pts(&[pt(0, 0), pt(25, 45), pt(50, 90), pt(75, 40), pt(100, -5)]),
            Some(Gesture::DownUp)
        );
        // 过短位移不识别
        assert_eq!(recognize_pts(&[pt(0, 0), pt(15, 0)]), None);
    }
}
