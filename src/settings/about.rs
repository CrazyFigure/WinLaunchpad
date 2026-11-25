//! 关于页：版本信息、更新操作与可独立展开的离线发布日志。

use std::collections::HashSet;
use std::sync::{mpsc, OnceLock};

use serde::Deserialize;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{IntersectClipRect, RestoreDC, SaveDC, HDC};

use super::{inside, Dlg, Hit};
use crate::ui::{
    draw_button, draw_rocket_badge, draw_scrollbar, draw_text, rgb, BtnStyle, Surface, TextAlign,
};
use crate::update::{self, Channel, Release};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Check,
    Install,
    Version(usize),
    GitHub,
    Cnb,
    DataDir,
}

#[derive(Deserialize)]
struct Note {
    version: String,
    date: String,
    sections: Vec<Section>,
}

#[derive(Deserialize)]
struct Section {
    title: String,
    items: Vec<String>,
}

fn notes() -> &'static [Note] {
    static NOTES: OnceLock<Vec<Note>> = OnceLock::new();
    NOTES.get_or_init(|| {
        let mut notes: Vec<Note> =
            serde_json::from_str(include_str!("../../assets/release-notes.json"))
                .expect("内置更新日志 JSON 无效");
        // 日志文件按新版在前维护，关于页展示最近十项。
        notes.truncate(10);
        notes
    })
}

enum Outcome {
    Checked(Result<(Option<Release>, Channel), String>),
    Downloaded(Result<Vec<u8>, String>),
}

#[derive(Default)]
pub(super) struct State {
    release: Option<Release>,
    pending: Option<mpsc::Receiver<Outcome>>,
    status: String,
    failed: bool,
    restarting: bool,
    expanded: HashSet<usize>,
    scroll: i32,
}

impl State {
    fn action(&self) -> Option<Action> {
        if self.pending.is_some() || self.restarting {
            return None;
        }
        Some(if self.release.is_some() {
            Action::Install
        } else {
            Action::Check
        })
    }

    /// 网络请求交给后台线程，设置窗口用计时器取回结果。
    pub(super) fn start(&mut self, action: Action) -> bool {
        if self.action() != Some(action) {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        let release = self.release.clone();
        let spawn = std::thread::Builder::new()
            .name("winlaunchpad-update".into())
            .spawn(move || {
                let result = match release {
                    Some(release) => Outcome::Downloaded(update::download(&release)),
                    None => Outcome::Checked(update::check()),
                };
                let _ = sender.send(result);
            });
        if let Err(error) = spawn {
            self.fail("无法启动更新任务，请重试", &error.to_string());
            return false;
        }
        self.failed = false;
        self.status = if action == Action::Install {
            "正在下载并验证签名".into()
        } else {
            "正在检测新版本".into()
        };
        self.pending = Some(receiver);
        true
    }

    fn fail(&mut self, text: &str, error: &str) {
        crate::log::log(&format!("{text}：{error}"));
        self.status = text.into();
        self.failed = true;
    }

    /// 返回任务是否已结束、是否需要从主窗口执行正常退出。
    pub(super) fn poll(&mut self) -> (bool, bool) {
        let Some(receiver) = &self.pending else {
            return (true, false);
        };
        let result = receiver.try_recv();
        if matches!(result, Err(mpsc::TryRecvError::Empty)) {
            return (false, false);
        }
        self.pending = None;
        match result {
            Ok(Outcome::Checked(Ok((release, source)))) => {
                self.status = match &release {
                    Some(release) => {
                        format!("发现 v{} · {}", release.version, release.channel.name())
                    }
                    None => format!("当前已是最新版本 · {}", source.name()),
                };
                self.release = release;
            }
            Ok(Outcome::Checked(Err(error))) => self.fail("检测失败，请检查网络后重试", &error),
            Ok(Outcome::Downloaded(result)) => {
                match result.and_then(|setup| update::start_installation(&setup)) {
                    Ok(()) => {
                        self.status = "正在应用更新并重启".into();
                        self.restarting = true;
                        return (true, true);
                    }
                    Err(error) => self.fail("更新失败，可继续使用当前版本或重试", &error),
                }
            }
            Err(_) => self.fail("更新任务已中断，请重试", "工作线程断开"),
        }
        (true, false)
    }

    pub(super) fn toggle(&mut self, layout: &Layout, index: usize) {
        if !self.expanded.insert(index) {
            self.expanded.remove(&index);
        }
        self.scroll = layout.scroll_metrics(self).0;
    }

    pub(super) fn scroll(&mut self, layout: &Layout, x: i32, y: i32, delta: i16) -> bool {
        if !inside(&layout.viewport, x, y) || delta == 0 {
            return false;
        }
        let (offset, total, visible) = layout.scroll_metrics(self);
        self.scroll =
            (offset - i32::from(delta) * layout.px(44) / 120).clamp(0, (total - visible).max(0));
        true
    }
}

pub(super) struct Layout {
    hero: RECT,
    update: RECT,
    button: RECT,
    history: RECT,
    viewport: RECT,
    links: [(Action, RECT, &'static str); 3],
    scale: f32,
}

impl Layout {
    pub(super) fn new(content: RECT, footer: RECT, scale: f32) -> Self {
        let s = |dp: i32| (dp as f32 * scale).round() as i32;
        let band = |top, bottom| RECT {
            top: content.top + s(top),
            bottom: content.top + s(bottom),
            ..content
        };
        let link = |start, end| RECT {
            left: content.left + s(start),
            right: content.left + s(end),
            ..footer
        };
        let hero = band(0, 98);
        let update = band(110, 174);
        let history = RECT {
            top: content.top + s(186),
            ..content
        };
        Self {
            hero,
            update,
            button: RECT {
                left: update.right - s(122),
                right: update.right - s(16),
                top: update.top + s(14),
                bottom: update.top + s(50),
            },
            history,
            viewport: RECT {
                left: history.left + s(12),
                top: history.top + s(44),
                right: history.right - s(12),
                bottom: history.bottom - s(10),
            },
            links: [
                (Action::GitHub, link(0, 108), "GitHub 仓库"),
                (Action::Cnb, link(118, 236), "腾讯云 CNB"),
                (Action::DataDir, link(246, 354), "本地配置目录"),
            ],
            scale,
        }
    }

    fn px(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn section_height(&self, section: &Section) -> i32 {
        self.px(22) + self.px(20) * section.items.len() as i32
    }

    fn detail_height(&self, note: &Note) -> i32 {
        self.px(8)
            + note
                .sections
                .iter()
                .map(|section| self.section_height(section))
                .sum::<i32>()
    }

    fn scroll_metrics(&self, state: &State) -> (i32, i32, i32) {
        let total = notes()
            .iter()
            .enumerate()
            .map(|(i, note)| {
                self.px(38)
                    + if state.expanded.contains(&i) {
                        self.detail_height(note)
                    } else {
                        0
                    }
            })
            .sum::<i32>();
        let visible = (self.viewport.bottom - self.viewport.top).max(0);
        (
            state.scroll.clamp(0, (total - visible).max(0)),
            total,
            visible,
        )
    }

    /// 命中区与绘制共享位置，并限制在日志可视区内。
    pub(super) fn hit(&self, state: &State, x: i32, y: i32) -> Option<Action> {
        if inside(&self.button, x, y) {
            return state.action();
        }
        if let Some((action, _, _)) = self.links.iter().find(|(_, rect, _)| inside(rect, x, y)) {
            return Some(*action);
        }
        if !inside(&self.viewport, x, y) {
            return None;
        }
        let (offset, _, _) = self.scroll_metrics(state);
        let mut top = self.viewport.top - offset;
        for (index, note) in notes().iter().enumerate() {
            let bottom = top + self.px(38);
            if y >= top && y < bottom {
                return Some(Action::Version(index));
            }
            top = bottom
                + if state.expanded.contains(&index) {
                    self.detail_height(note)
                } else {
                    0
                };
        }
        None
    }

    pub(super) fn tip(&self, action: Action) -> Option<(RECT, &'static str)> {
        match action {
            Action::Check => Some((self.button, "优先检测 GitHub，连接失败时尝试腾讯云 CNB")),
            Action::Install => Some((self.button, "验签通过后更新 EXE 并重启 WinLaunchpad")),
            Action::DataDir => Some((self.links[2].1, "打开设置、缓存与运行日志所在目录")),
            _ => None,
        }
    }
}

pub(super) fn paint(dlg: &Dlg, l: &Layout, sf: &mut Surface, dc: HDC) {
    let s = l.scale;
    let p = |dp| l.px(dp);
    let primary = rgb(0x17253D, 1.0);
    let muted = rgb(0x52647D, 1.0);
    let border = rgb(0xE1E8F1, 1.0);
    let region = |parent: RECT, left, top, right, height| RECT {
        left: parent.left + p(left),
        top: parent.top + p(top),
        right: parent.right - p(right),
        bottom: parent.top + p(top + height),
    };

    // 头部保留原有图标，版本号来自 Cargo，和 EXE 版本使用同一发布校验。
    sf.fill_rrect(&l.hero, 14.0 * s, rgb(0xF1F6FF, 1.0));
    draw_rocket_badge(
        sf,
        (l.hero.left + p(55)) as f32,
        (l.hero.top + p(52)) as f32,
        64.0 * s,
        s,
    );
    draw_text(
        dc,
        "WinLaunchpad",
        dlg.font_about_title,
        &region(l.hero, 102, 17, 18, 36),
        primary,
        TextAlign::Left,
        true,
        false,
    );
    let version = format!("v{}  ·  CrazyFigure", update::VERSION);
    draw_text(
        dc,
        &version,
        dlg.font_section,
        &region(l.hero, 103, 53, 18, 20),
        rgb(0x195CB0, 1.0),
        TextAlign::Left,
        true,
        false,
    );
    draw_text(
        dc,
        "Windows 11 轻量应用启动台",
        dlg.font_body,
        &region(l.hero, 103, 74, 18, 19),
        muted,
        TextAlign::Left,
        true,
        false,
    );

    // 更新状态和主按钮保持在同一视线，后台任务期间按钮变为不可操作。
    sf.stroke_rrect(&l.update, 12.0 * s, s.max(1.0), border);
    draw_text(
        dc,
        "软件更新",
        dlg.font_section,
        &region(l.update, 16, 13, 130, 22),
        primary,
        TextAlign::Left,
        true,
        false,
    );
    let state = &dlg.about;
    let status = if state.status.is_empty() {
        "检测可用的正式版本"
    } else {
        &state.status
    };
    let status_color = if state.failed {
        rgb(0xB33B22, 1.0)
    } else {
        muted
    };
    draw_text(
        dc,
        status,
        dlg.font_body,
        &region(l.update, 16, 36, 130, 22),
        status_color,
        TextAlign::Left,
        true,
        false,
    );
    match state.action() {
        Some(action) => draw_button(
            sf,
            dc,
            &l.button,
            if action == Action::Check {
                "检测更新"
            } else {
                "一键更新"
            },
            dlg.font_btn,
            BtnStyle::Primary,
            dlg.hover == Hit::About(action),
            dlg.pressed == Hit::About(action),
            s,
        ),
        None => {
            sf.fill_rrect(&l.button, 8.0 * s, rgb(0xEDF2F8, 1.0));
            let label = if state.restarting {
                "正在重启"
            } else {
                "处理中"
            };
            draw_text(
                dc,
                label,
                dlg.font_btn,
                &l.button,
                muted,
                TextAlign::Center,
                false,
                false,
            );
        }
    }

    for (action, rc, text) in &l.links {
        draw_button(
            sf,
            dc,
            rc,
            text,
            dlg.font_btn,
            BtnStyle::Secondary,
            dlg.hover == Hit::About(*action),
            dlg.pressed == Hit::About(*action),
            s,
        );
    }

    sf.stroke_rrect(&l.history, 12.0 * s, s.max(1.0), border);
    draw_text(
        dc,
        "更新日志",
        dlg.font_section,
        &region(l.history, 16, 10, 16, 28),
        primary,
        TextAlign::Left,
        true,
        false,
    );
    let (offset, total, visible) = l.scroll_metrics(state);
    let rc = l.viewport;
    let save = unsafe { SaveDC(dc) };
    if save == 0 {
        return;
    }
    unsafe {
        IntersectClipRect(dc, rc.left, rc.top, rc.right, rc.bottom);
    }
    sf.set_clip(Some(rc));
    let mut top = rc.top - offset;
    for (index, note) in notes().iter().enumerate() {
        let row = RECT {
            top,
            bottom: top + p(38),
            right: rc.right - p(8),
            ..rc
        };
        let action = Hit::About(Action::Version(index));
        let open = state.expanded.contains(&index);
        if top < rc.bottom && row.bottom > rc.top {
            let fill = if dlg.pressed == action {
                0xE3ECFB
            } else if dlg.hover == action {
                0xEEF4FF
            } else {
                0xF7F9FC
            };
            sf.fill_rrect(
                &RECT {
                    bottom: row.bottom - p(3),
                    ..row
                },
                7.0 * s,
                rgb(fill, 1.0),
            );
            let (x, y) = ((row.left + p(15)) as f32, (top + p(18)) as f32);
            let a = 3.2 * s;
            if open {
                sf.thick_line(x - a, y - a * 0.5, x, y + a * 0.5, s.max(1.0), muted);
                sf.thick_line(x, y + a * 0.5, x + a, y - a * 0.5, s.max(1.0), muted);
            } else {
                sf.thick_line(x - a * 0.5, y - a, x + a * 0.5, y, s.max(1.0), muted);
                sf.thick_line(x + a * 0.5, y, x - a * 0.5, y + a, s.max(1.0), muted);
            }
            draw_text(
                dc,
                &format!("v{}", note.version),
                dlg.font_section,
                &region(row, 30, 0, 116, 35),
                primary,
                TextAlign::Left,
                true,
                false,
            );
            draw_text(
                dc,
                &note.date,
                dlg.font_sub,
                &region(row, 108, 0, 12, 35),
                muted,
                TextAlign::Right,
                true,
                false,
            );
        }
        top = row.bottom;
        if open {
            for section in &note.sections {
                let section_rc = RECT { top, ..row };
                draw_text(
                    dc,
                    &section.title,
                    dlg.font_section,
                    &region(section_rc, 30, 0, 12, 22),
                    primary,
                    TextAlign::Left,
                    true,
                    false,
                );
                top += p(22);
                for item in &section.items {
                    let item_rc = RECT { top, ..row };
                    sf.fill_circle(
                        (row.left + p(34)) as f32,
                        (top + p(10)) as f32,
                        1.7 * s,
                        muted,
                    );
                    draw_text(
                        dc,
                        item,
                        dlg.font_body,
                        &region(item_rc, 44, 0, 12, 20),
                        muted,
                        TextAlign::Left,
                        true,
                        false,
                    );
                    top += p(20);
                }
            }
            top += p(8);
        }
    }
    sf.set_clip(None);
    unsafe {
        let _ = RestoreDC(dc, save);
    }
    let track = RECT {
        left: rc.right - p(4),
        ..rc
    };
    draw_scrollbar(
        sf,
        &track,
        total as usize,
        visible as usize,
        offset as usize,
        false,
        s,
    );
}
