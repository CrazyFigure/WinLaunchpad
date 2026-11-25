//! 可选内置右键手势（默认关闭）。独立高优先级线程安装 WH_MOUSE_LL 钩子：
//! 回调内仅做轻量采样、状态裁决与非阻塞派发，绝不在钩子内同步调用 SendInput；
//! 普通右键回放与快捷键动作由独立发送线程异步执行，避免触发系统低级钩子超时失效。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
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
const MSG_REINSTALL: u32 = WM_APP + 22;
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

    /// 返回实时轨迹浮窗展示的简洁动作名称。
    pub fn hud_label(self, swap_lr: bool, launchpad_visible: bool) -> &'static str {
        match self {
            Gesture::Left => {
                if swap_lr {
                    "← 向左切桌面"
                } else {
                    "← 向右切桌面"
                }
            }
            Gesture::Right => {
                if swap_lr {
                    "→ 向右切桌面"
                } else {
                    "→ 向左切桌面"
                }
            }
            Gesture::Up => "切换窗口",
            Gesture::UpDown => {
                if launchpad_visible {
                    "收起启动台"
                } else {
                    "打开启动台"
                }
            }
            Gesture::DownUp => "全部窗口",
        }
    }
}

/// 供主线程实时绘制手势轨迹浮窗的轻量快照。
#[derive(Clone, Default)]
pub struct TrailSnapshot {
    pub active: bool,
    pub pts: Vec<POINT>,
    pub cursor: POINT,
    pub gesture: Option<Gesture>,
}

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static HOOK_ENABLED: AtomicBool = AtomicBool::new(false);
static GESTURE_THRESHOLD_PCT: AtomicU8 = AtomicU8::new(crate::config::DEFAULT_GESTURE_THRESHOLD);
static LAST_HOOK_TICK: AtomicU64 = AtomicU64::new(0);
static LAST_REINSTALL_TICK: AtomicU64 = AtomicU64::new(0);
static TRAIL_DIRTY: AtomicBool = AtomicBool::new(false);

/// 实时更新全局手势识别的最低匹配度百分比（原子写入，无需重装鼠标钩子）。
pub fn set_threshold(pct: u8) {
    GESTURE_THRESHOLD_PCT.store(
        crate::config::clamp_gesture_threshold(pct),
        Ordering::Relaxed,
    );
}

static TRAIL_SNAPSHOT: Mutex<TrailSnapshot> = Mutex::new(TrailSnapshot {
    active: false,
    pts: Vec::new(),
    cursor: POINT { x: 0, y: 0 },
    gesture: None,
});

enum SenderCmd {
    ReplayRightClick,
    Perform(Gesture, bool),
}

static SENDER_TX: OnceLock<mpsc::Sender<SenderCmd>> = OnceLock::new();

fn sender_tx() -> &'static mpsc::Sender<SenderCmd> {
    SENDER_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<SenderCmd>();
        let _ = std::thread::Builder::new()
            .name("gesture-sender".into())
            .spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        SenderCmd::ReplayRightClick => replay_click(),
                        SenderCmd::Perform(g, swap_lr) => perform(g, swap_lr),
                    }
                }
            });
        tx
    })
}

/// 取出最新的实时轨迹快照并清除脏标记。
pub fn take_trail_snapshot() -> TrailSnapshot {
    TRAIL_DIRTY.store(false, Ordering::Release);
    TRAIL_SNAPSHOT
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

#[derive(Default)]
struct State {
    tracking: bool,
    moved: bool,
    dpi_scale: f32,
    start: POINT,
    last: POINT,
    last_trail: POINT,
    pts: Vec<POINT>,
    trail: Vec<POINT>,
}

static STATE: Mutex<State> = Mutex::new(State {
    tracking: false,
    moved: false,
    dpi_scale: 1.0,
    start: POINT { x: 0, y: 0 },
    last: POINT { x: 0, y: 0 },
    last_trail: POINT { x: 0, y: 0 },
    pts: Vec::new(),
    trail: Vec::new(),
});

pub struct GestureHandle {
    thread_id: u32,
}

impl GestureHandle {
    fn post_cmd(&self, msg_id: u32) -> bool {
        unsafe {
            // 若消息队列偶发繁忙，短重试保证启用/禁用/重装命令不静默丢失
            for attempt in 0..3 {
                if PostThreadMessageW(self.thread_id, msg_id, WPARAM(0), LPARAM(0)).is_ok() {
                    return true;
                }
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            false
        }
    }

    pub fn set_enabled(&self, on: bool) {
        HOOK_ENABLED.store(on, Ordering::SeqCst);
        let msg_id = if on { MSG_ENABLE } else { MSG_DISABLE };
        if !self.post_cmd(msg_id) {
            log::log(&format!("发送手势线程消息失败 (on={on})"));
        }
    }

    /// 当非提权自有窗口收到鼠标事件却长时间未触发钩子时，判定系统已静默脱钩并自动重装。
    pub fn ensure_alive(&self) {
        if !HOOK_ENABLED.load(Ordering::Relaxed) {
            return;
        }
        let now = unsafe { GetTickCount64() };
        let last = LAST_HOOK_TICK.load(Ordering::Relaxed);
        if now.saturating_sub(last) > 650 {
            let prev = LAST_REINSTALL_TICK.load(Ordering::Relaxed);
            if now.saturating_sub(prev) > 1500
                && LAST_REINSTALL_TICK
                    .compare_exchange(prev, now, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                let _ = self.post_cmd(MSG_REINSTALL);
            }
        }
    }

    /// 系统睡眠唤醒后强制重新挂载低级鼠标钩子，避免休眠后钩子链失效。
    pub fn force_reinstall(&self) {
        if !HOOK_ENABLED.load(Ordering::Relaxed) {
            return;
        }
        let now = unsafe { GetTickCount64() };
        LAST_REINSTALL_TICK.store(now, Ordering::Relaxed);
        let _ = self.post_cmd(MSG_REINSTALL);
    }
}

/// 启动手势线程与异步发送线程（初始不装钩子）。
pub fn start(main_hwnd: HWND) -> Option<GestureHandle> {
    MAIN_HWND.store(main_hwnd.0 as isize, Ordering::SeqCst);
    let _ = sender_tx();
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    std::thread::Builder::new()
        .name("gesture-hook".into())
        .spawn(move || {
            let mut hook: Option<HHOOK> = None;
            unsafe {
                // 提升钩子消息泵优先级，防止主界面或扫描子进程 CPU 尖峰导致钩子响应超时被系统摘除
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                let mut msg = MSG::default();
                // 必须先通过 PeekMessageW 创建本线程消息队列，再通知主线程 thread_id，防止 PostThreadMessageW 竞态丢失
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                let tid = GetCurrentThreadId();
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
                                        LAST_HOOK_TICK.store(GetTickCount64(), Ordering::Relaxed);
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
                        MSG_REINSTALL => {
                            if HOOK_ENABLED.load(Ordering::Relaxed) {
                                if let Some(h) = hook.take() {
                                    let _ = UnhookWindowsHookEx(h);
                                }
                                reset_state();
                                let hmod = crate::win32::hinstance();
                                match SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), hmod, 0) {
                                    Ok(h) => {
                                        hook = Some(h);
                                        LAST_HOOK_TICK.store(GetTickCount64(), Ordering::Relaxed);
                                        log::log("手势钩子已重新安装");
                                    }
                                    Err(e) => log::log(&format!(
                                        "手势钩子重装失败: {}",
                                        crate::win32::error_string(e)
                                    )),
                                }
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
        st.dpi_scale = 1.0;
        st.pts.clear();
        st.trail.clear();
    }
    if let Ok(mut snap) = TRAIL_SNAPSHOT.lock() {
        snap.active = false;
        snap.pts.clear();
        snap.gesture = None;
    }
    TRAIL_DIRTY.store(false, Ordering::Release);
}

/// 手势识别重采样分段数（32 段切线 + 33 个等弧长采样点）。
const N_STEPS: usize = 32;
const N_PTS: usize = N_STEPS + 1;
/// 切线 DTW 的 Sakoe-Chiba 带状窗口半径，允许折线双段长度适度不对称（±12.5%）。
const DTW_BAND: usize = 4;
/// 非对角规整步附加惩罚，避免过度拉伸短折返段蒙混过关。
const DTW_WARP_PENALTY: f32 = 0.04;

/// 预计算的手势模板（包含 Protractor 单位空间点云与单位切线序列）。
struct GestureTemplate {
    gesture: Gesture,
    is_chevron: bool,
    norm_pts: [(f32, f32); N_PTS],
    tangents: [(f32, f32); N_STEPS],
}

static TEMPLATES: OnceLock<Vec<GestureTemplate>> = OnceLock::new();

/// 对任意点序列执行等弧长重采样，返回 33 个采样点、总弧长与包络跨度。
fn resample_with<F>(len: usize, get: F) -> Option<([(f32, f32); N_PTS], f32, f32, f32)>
where
    F: Fn(usize) -> (f32, f32),
{
    if len < 2 {
        return None;
    }
    let first = get(0);
    let last = get(len - 1);
    let mut total = 0.0f32;
    let (mut min_x, mut max_x) = (first.0, first.0);
    let (mut min_y, mut max_y) = (first.1, first.1);
    let mut prev = first;
    for i in 1..len {
        let p = get(i);
        total += (p.0 - prev.0).hypot(p.1 - prev.1);
        min_x = min_x.min(p.0);
        max_x = max_x.max(p.0);
        min_y = min_y.min(p.1);
        max_y = max_y.max(p.1);
        prev = p;
    }
    if total <= 1e-3 {
        return None;
    }

    let mut out = [(0.0f32, 0.0f32); N_PTS];
    out[0] = first;
    out[N_STEPS] = last;

    let step = total / N_STEPS as f32;
    let mut acc = 0.0f32;
    let mut seg_idx = 0usize;
    for k in 1..N_STEPS {
        let target = k as f32 * step;
        while seg_idx + 1 < len {
            let p0 = get(seg_idx);
            let p1 = get(seg_idx + 1);
            let seg_len = (p1.0 - p0.0).hypot(p1.1 - p0.1);
            if seg_len > 1e-6 && acc + seg_len >= target {
                let t = ((target - acc) / seg_len).clamp(0.0, 1.0);
                out[k] = (p0.0 + (p1.0 - p0.0) * t, p0.1 + (p1.1 - p0.1) * t);
                break;
            }
            acc += seg_len;
            seg_idx += 1;
        }
        if seg_idx + 1 >= len {
            out[k] = out[N_STEPS];
        }
    }
    Some((out, total, max_x - min_x, max_y - min_y))
}

/// 将采样点中心化并等比例缩放到单位均方根半径（保留原始长宽比，避免直线噪声被非均匀拉伸）。
fn normalize_rms(pts: &[(f32, f32); N_PTS]) -> [(f32, f32); N_PTS] {
    let n = N_PTS as f32;
    let mut cx = 0.0f32;
    let mut cy = 0.0f32;
    for &(x, y) in pts {
        cx += x;
        cy += y;
    }
    cx /= n;
    cy /= n;

    let mut centered = [(0.0f32, 0.0f32); N_PTS];
    let mut sum_sq = 0.0f32;
    for i in 0..N_PTS {
        let dx = pts[i].0 - cx;
        let dy = pts[i].1 - cy;
        centered[i] = (dx, dy);
        sum_sq += dx * dx + dy * dy;
    }
    let rms = (sum_sq / n).sqrt();
    if rms > 1e-6 {
        for p in &mut centered {
            p.0 /= rms;
            p.1 /= rms;
        }
    }
    centered
}

/// 由等弧长采样点提取 32 段单位方向切线向量。
fn compute_tangents(pts: &[(f32, f32); N_PTS]) -> [(f32, f32); N_STEPS] {
    let mut ts = [(0.0f32, 0.0f32); N_STEPS];
    for i in 0..N_STEPS {
        let dx = pts[i + 1].0 - pts[i].0;
        let dy = pts[i + 1].1 - pts[i].1;
        let d = dx.hypot(dy);
        if d > 1e-6 {
            ts[i] = (dx / d, dy / d);
        }
    }
    ts
}

/// 构建多纵横比预置手势模板库（单向直线各 1 个，∧ / ∨ 折线各含窄、标准、宽 3 种常见书写比例）。
fn build_template(gesture: Gesture, is_chevron: bool, raw: &[(f32, f32)]) -> GestureTemplate {
    let (resampled, _, _, _) = resample_with(raw.len(), |i| raw[i]).expect("手势模板点数有效");
    GestureTemplate {
        gesture,
        is_chevron,
        norm_pts: normalize_rms(&resampled),
        tangents: compute_tangents(&resampled),
    }
}

fn templates() -> &'static [GestureTemplate] {
    TEMPLATES.get_or_init(|| {
        vec![
            build_template(Gesture::Left, false, &[(100.0, 0.0), (0.0, 0.0)]),
            build_template(Gesture::Right, false, &[(0.0, 0.0), (100.0, 0.0)]),
            build_template(Gesture::Up, false, &[(0.0, 100.0), (0.0, 0.0)]),
            build_template(Gesture::UpDown, true, &[(0.0, 100.0), (25.0, 0.0), (50.0, 100.0)]),
            build_template(Gesture::UpDown, true, &[(0.0, 80.0), (50.0, 0.0), (100.0, 80.0)]),
            build_template(Gesture::UpDown, true, &[(0.0, 55.0), (80.0, 0.0), (160.0, 55.0)]),
            build_template(Gesture::DownUp, true, &[(0.0, 0.0), (25.0, 100.0), (50.0, 0.0)]),
            build_template(Gesture::DownUp, true, &[(0.0, 0.0), (50.0, 80.0), (100.0, 0.0)]),
            build_template(Gesture::DownUp, true, &[(0.0, 0.0), (80.0, 55.0), (160.0, 0.0)]),
        ]
    })
}

/// Protractor 方向敏感空间余弦相似度（90° 正交归零，保留几何对称性与整体落点分布）。
fn protractor_score(a: &[(f32, f32); N_PTS], b: &[(f32, f32); N_PTS]) -> f32 {
    let mut dot = 0.0f32;
    for i in 0..N_PTS {
        dot += a[i].0 * b[i].0 + a[i].1 * b[i].1;
    }
    let rho = (dot / N_PTS as f32).clamp(-1.0, 1.0);
    let ang = rho.acos();
    (1.0 - ang / std::f32::consts::FRAC_PI_2).max(0.0)
}

/// Sakoe-Chiba 带状约束的切线方向动态时间规整相似度（栈上 1D DP，无堆分配）。
fn dtw_tangent_score(ta: &[(f32, f32); N_STEPS], tb: &[(f32, f32); N_STEPS]) -> f32 {
    const INF: f32 = 1.0e9;
    let mut prev = [INF; N_PTS];
    prev[0] = 0.0;
    for i in 1..=N_STEPS {
        let mut curr = [INF; N_PTS];
        let j0 = i.saturating_sub(DTW_BAND).max(1);
        let j1 = (i + DTW_BAND).min(N_STEPS);
        let (tx, ty) = ta[i - 1];
        for j in j0..=j1 {
            let (ux, uy) = tb[j - 1];
            let dot = (tx * ux + ty * uy).clamp(-1.0, 1.0);
            let ang_cost = dot.acos() / std::f32::consts::FRAC_PI_2;
            let best_prev = prev[j - 1]
                .min(prev[j] + DTW_WARP_PENALTY)
                .min(curr[j - 1] + DTW_WARP_PENALTY);
            curr[j] = ang_cost + best_prev;
        }
        prev = curr;
    }
    (1.0 - prev[N_STEPS] / N_STEPS as f32).max(0.0)
}

/// DPI 归一化轨迹尺度门控：屏蔽微小误触抖动，并在达到标准有效行程后平滑过渡到满分权重 1.0。
fn scale_completeness(total_len: f32, y_span: f32, is_chevron: bool, dpi_scale: f32) -> f32 {
    let scale = dpi_scale.max(1.0);
    let len_dp = total_len / scale;
    let yspan_dp = y_span / scale;
    if is_chevron {
        if len_dp < 48.0 || yspan_dp < 22.0 {
            return 0.0;
        }
        let f_len = (len_dp / 96.0).min(1.0);
        let f_amp = (yspan_dp / 40.0).min(1.0);
        (f_len * f_amp).sqrt()
    } else {
        if len_dp < 28.0 {
            return 0.0;
        }
        (len_dp / 56.0).min(1.0)
    }
}

/// 基于 Protractor 空间余弦 + 受限切线 DTW + DPI 尺度门控计算最佳手势及其百分比置信度（0..=100）。
pub fn score_pts(pts: &[POINT], dpi_scale: f32) -> Option<(Gesture, f32)> {
    let (resampled, total_len, _x_span, y_span) =
        resample_with(pts.len(), |i| (pts[i].x as f32, pts[i].y as f32))?;
    let norm = normalize_rms(&resampled);
    let tangs = compute_tangents(&resampled);

    let sf_line = scale_completeness(total_len, y_span, false, dpi_scale);
    let sf_chev = scale_completeness(total_len, y_span, true, dpi_scale);
    if sf_line <= 0.0 && sf_chev <= 0.0 {
        return None;
    }

    let mut best_by_gesture = [0.0f32; 5];
    for tmpl in templates() {
        let sf = if tmpl.is_chevron { sf_chev } else { sf_line };
        if sf <= 0.0 {
            continue;
        }
        let sd = dtw_tangent_score(&tangs, &tmpl.tangents);
        let sp = protractor_score(&norm, &tmpl.norm_pts);
        let score = (0.55 * sd + 0.45 * sp) * sf * 100.0;
        let slot = &mut best_by_gesture[tmpl.gesture.id() - 1];
        if score > *slot {
            *slot = score;
        }
    }

    let mut best_idx = 0usize;
    let mut best_score = -1.0f32;
    let mut second_score = -1.0f32;
    for (idx, &s) in best_by_gesture.iter().enumerate() {
        if s > best_score {
            second_score = best_score;
            best_score = s;
            best_idx = idx;
        } else if s > second_score {
            second_score = s;
        }
    }

    // 防止 45° 对角线等模糊轨迹在低阈值下因微小浮点差误判
    if best_score <= 0.0 || best_score - second_score.max(0.0) < 1.5 {
        return None;
    }
    Gesture::from_id(best_idx + 1).map(|g| (g, best_score))
}

/// 按指定匹配度阈值（百分比）与显示器 DPI 缩放比例识别五种预设手势。
pub fn recognize_pts_with(pts: &[POINT], min_pct: u8, dpi_scale: f32) -> Option<Gesture> {
    let threshold = crate::config::clamp_gesture_threshold(min_pct) as f32;
    score_pts(pts, dpi_scale).and_then(|(g, score)| if score >= threshold { Some(g) } else { None })
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

/// 异步执行手势对应的系统快捷键，避免阻塞主界面消息循环与手势钩子。
pub fn perform_async(g: Gesture, swap_lr: bool) {
    let _ = sender_tx().send(SenderCmd::Perform(g, swap_lr));
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

/// 点序列达到上限时抽稀中间采样点，同时完整保留起点、终点与整体曲线走向。
fn compact_points(pts: &mut Vec<POINT>) {
    if pts.len() < 4 {
        return;
    }
    let last = *pts.last().unwrap();
    let mut kept = Vec::with_capacity(pts.len() / 2 + 2);
    kept.push(pts[0]);
    for i in (1..pts.len() - 1).step_by(2) {
        kept.push(pts[i]);
    }
    kept.push(last);
    *pts = kept;
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    // 每次进入回调即刷新心跳时间戳，供主线程检测系统是否静默摘除了低级钩子
    LAST_HOOK_TICK.store(GetTickCount64(), Ordering::Relaxed);
    let mi = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    if mi.flags & LLMHF_INJECTED != 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let msg = wparam.0 as u32;
    let main = HWND(MAIN_HWND.load(Ordering::SeqCst) as *mut _);
    match msg {
        WM_RBUTTONDOWN => {
            if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_MBUTTON.0 as i32) < 0 {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            // 按下时采样当前显示器 DPI 缩放，保证不同缩放倍率屏幕下的有效行程一致
            let dpi_scale = (crate::win32::dpi_at_point(mi.pt.x, mi.pt.y) as f32 / 96.0).max(1.0);
            let mut had_stale_trail = false;
            if let Ok(mut st) = STATE.lock() {
                had_stale_trail = st.moved;
                st.tracking = true;
                st.moved = false;
                st.dpi_scale = dpi_scale;
                st.pts.clear();
                st.pts.push(mi.pt);
                st.trail.clear();
                st.trail.push(mi.pt);
                st.start = mi.pt;
                st.last = mi.pt;
                st.last_trail = mi.pt;
            }
            if had_stale_trail {
                if let Ok(mut snap) = TRAIL_SNAPSHOT.lock() {
                    snap.active = false;
                    snap.pts.clear();
                    snap.gesture = None;
                }
                TRAIL_DIRTY.store(true, Ordering::Release);
                let _ = PostMessageW(main, WM_APP_GESTURE_HUD, WPARAM(0), LPARAM(0));
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            let mut next_snap: Option<TrailSnapshot> = None;
            if let Ok(mut st) = STATE.lock() {
                if st.tracking {
                    let tdx = (mi.pt.x - st.last_trail.x) as f32;
                    let tdy = (mi.pt.y - st.last_trail.y) as f32;
                    let mut trail_updated = false;
                    // 每移动约 3px 采集一个高密度渲染点，确保实时手势曲线平滑无棱角
                    if tdx * tdx + tdy * tdy >= 9.0 {
                        if st.trail.len() >= 512 {
                            compact_points(&mut st.trail);
                        }
                        st.trail.push(mi.pt);
                        st.last_trail = mi.pt;
                        trail_updated = true;
                    }
                    let dx = (mi.pt.x - st.last.x) as f32;
                    let dy = (mi.pt.y - st.last.y) as f32;
                    if dx * dx + dy * dy >= 49.0 {
                        if st.pts.len() >= 256 {
                            compact_points(&mut st.pts);
                        }
                        st.pts.push(mi.pt);
                        st.last = mi.pt;
                    }
                    if !st.moved {
                        let sdx = (mi.pt.x - st.start.x) as f32;
                        let sdy = (mi.pt.y - st.start.y) as f32;
                        let deadzone = 16.0 * st.dpi_scale.max(1.0);
                        if sdx * sdx + sdy * sdy >= deadzone * deadzone {
                            st.moved = true;
                            trail_updated = true;
                        }
                    }
                    if st.moved && trail_updated {
                        let threshold = GESTURE_THRESHOLD_PCT.load(Ordering::Relaxed);
                        let cur = recognize_pts_with(&st.pts, threshold, st.dpi_scale);
                        next_snap = Some(TrailSnapshot {
                            active: true,
                            pts: st.trail.clone(),
                            cursor: mi.pt,
                            gesture: cur,
                        });
                    }
                }
            }
            // 先释放 STATE 锁，再更新渲染快照与投递合并通知，绝不在锁内调用系统消息 API
            if let Some(s) = next_snap {
                if let Ok(mut snap) = TRAIL_SNAPSHOT.lock() {
                    *snap = s;
                }
                if TRAIL_DIRTY
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    let _ = PostMessageW(main, WM_APP_GESTURE_HUD, WPARAM(1), LPARAM(0));
                }
            }
            CallNextHookEx(None, code, wparam, lparam)
        }
        WM_RBUTTONUP => {
            let (was_tracking, had_trail, g) = match STATE.lock() {
                Ok(mut st) => {
                    if !st.tracking {
                        (false, false, None)
                    } else {
                        st.tracking = false;
                        let had = st.moved;
                        if had {
                            st.pts.push(mi.pt);
                            st.trail.push(mi.pt);
                        }
                        let rec = if had {
                            let threshold = GESTURE_THRESHOLD_PCT.load(Ordering::Relaxed);
                            recognize_pts_with(&st.pts, threshold, st.dpi_scale)
                        } else {
                            None
                        };
                        st.pts.clear();
                        st.trail.clear();
                        (true, had, rec)
                    }
                }
                Err(_) => (false, false, None),
            };
            if !was_tracking {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            if had_trail {
                if let Ok(mut snap) = TRAIL_SNAPSHOT.lock() {
                    snap.active = false;
                    snap.pts.clear();
                    snap.gesture = None;
                }
                TRAIL_DIRTY.store(true, Ordering::Release);
                let _ = PostMessageW(main, WM_APP_GESTURE_HUD, WPARAM(0), LPARAM(0));
            }
            match g {
                Some(g) => {
                    let _ = PostMessageW(main, WM_APP_GESTURE, WPARAM(g.id()), LPARAM(0));
                }
                None => {
                    // 通过独立发送线程回放右键，避免钩子回调内阻塞于 SendInput
                    let _ = sender_tx().send(SenderCmd::ReplayRightClick);
                }
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
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(-140, 8)], 70, 1.0), Some(Gesture::Left));
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(160, -10)], 70, 1.0), Some(Gesture::Right));
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(8, -120)], 70, 1.0), Some(Gesture::Up));
    }

    #[test]
    fn recognizes_diagonal_chevrons_and_rejects_noise() {
        // 45° 斜向 ∧（先右上再右下，复刻真实人手轨迹）
        assert_eq!(
            recognize_pts_with(&[pt(0, 0), pt(25, -45), pt(48, -90), pt(72, -40), pt(96, 5)], 70, 1.0),
            Some(Gesture::UpDown)
        );
        // 45° 斜向 ∨（先右下再右上）
        assert_eq!(
            recognize_pts_with(&[pt(0, 0), pt(25, 45), pt(50, 90), pt(75, 40), pt(100, -5)], 70, 1.0),
            Some(Gesture::DownUp)
        );
        // 自然不对称折线（第二段为第一段约 70%）在默认 70% 阈值下正常识别
        assert_eq!(
            recognize_pts_with(&[pt(0, 0), pt(35, -100), pt(70, -30)], 70, 1.0),
            Some(Gesture::UpDown)
        );
        // 过短位移与微小试探折返不识别（防止满足一点点就触发）
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(15, 0)], 70, 1.0), None);
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(36, 0)], 70, 1.0), None);
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(20, -120), pt(40, -80)], 70, 1.0), None);
        // 对勾、圆形、L 形、向下划等非目标轨迹不误判
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(20, 30), pt(80, -90)], 70, 1.0), None);
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(100, 0), pt(100, -100)], 70, 1.0), None);
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(0, 140)], 70, 1.0), None);
        let circle: Vec<POINT> = (0..=24)
            .map(|i| {
                let a = i as f32 * std::f32::consts::TAU / 24.0;
                pt((50.0 * a.cos()).round() as i32, (50.0 * a.sin()).round() as i32)
            })
            .collect();
        assert_eq!(recognize_pts_with(&circle, 70, 1.0), None);
    }

    #[test]
    fn threshold_percentage_and_dpi_scale_gate_work() {
        // 约 30° 斜向左划：默认 70% 拒绝，降至 65% 允许通过
        let tilted_30 = [pt(0, 0), pt(-120, 69)];
        assert_eq!(recognize_pts_with(&tilted_30, 70, 1.0), None);
        assert_eq!(recognize_pts_with(&tilted_30, 65, 1.0), Some(Gesture::Left));

        // 25° 斜向左划：70% 通过，90% 严格模式拒绝
        let tilted_25 = [pt(0, 0), pt(-120, 56)];
        assert_eq!(recognize_pts_with(&tilted_25, 70, 1.0), Some(Gesture::Left));
        assert_eq!(recognize_pts_with(&tilted_25, 90, 1.0), None);

        // 45° 对角模糊轨迹即使降到最低 50% 也不应随机匹配相邻方向
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(-100, -100)], 50, 1.0), None);

        // 高 DPI（200%）下 48px 物理位移仅相当于 24dp，应被尺度门控拦截；120px（60dp）正常通过
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(-48, 0)], 70, 2.0), None);
        assert_eq!(recognize_pts_with(&[pt(0, 0), pt(-120, 0)], 70, 2.0), Some(Gesture::Left));
    }

    #[test]
    fn hud_labels_and_compaction_work() {
        assert_eq!(Gesture::UpDown.hud_label(false, false), "打开启动台");
        assert_eq!(Gesture::UpDown.hud_label(false, true), "收起启动台");
        assert_eq!(Gesture::DownUp.hud_label(false, false), "全部窗口");
        assert_ne!(Gesture::Left.hud_label(false, false), Gesture::Left.hud_label(true, false));

        let mut pts: Vec<POINT> = (0..20).map(|i| pt(i * 4, i * 2)).collect();
        let first = pts[0];
        let last = *pts.last().unwrap();
        compact_points(&mut pts);
        assert_eq!(pts[0], first);
        assert_eq!(*pts.last().unwrap(), last);
        assert!(pts.len() < 20);
    }
}
