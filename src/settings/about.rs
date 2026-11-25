//! 关于页：版本信息、更新操作、自动检测更新开关与可独立展开的离线发布日志。

use std::sync::{mpsc, OnceLock};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{HDC, HFONT};

use super::{inside, Dlg, Hit};
use crate::app::App;
use crate::changelog::ChangelogView;
use crate::ui::{draw_app_badge, draw_button, draw_text, draw_toggle, rgb, BtnStyle, Surface, TextAlign};
use crate::update::{self, Channel, ReleaseNote, UpdateBundle};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Check,
    Install,
    ShowPopup,
    ToggleAutoUpdate,
    Version(usize),
    GitHub,
    Cnb,
    DataDir,
}

fn notes() -> &'static [ReleaseNote] {
    static NOTES: OnceLock<Vec<ReleaseNote>> = OnceLock::new();
    NOTES.get_or_init(|| {
        let mut notes: Vec<ReleaseNote> =
            serde_json::from_str(include_str!("../../assets/release-notes.json"))
                .expect("内置更新日志 JSON 无效");
        // 日志文件按新版在前维护，关于页展示最近十项。
        notes.truncate(10);
        notes
    })
}

enum Outcome {
    Checked(Result<(Option<UpdateBundle>, Channel), String>),
    Downloaded(Result<Vec<u8>, String>),
}

#[derive(Default)]
pub(super) struct State {
    bundle: Option<UpdateBundle>,
    pending: Option<mpsc::Receiver<Outcome>>,
    status: String,
    failed: bool,
    restarting: bool,
    changelog: ChangelogView,
}

impl State {
    /// 同步后台自动检测或主状态已发现的可安装更新。
    pub(super) fn sync_available(&mut self, available: Option<&UpdateBundle>) {
        if self.pending.is_some() || self.restarting {
            return;
        }
        if let Some(bundle) = available {
            self.bundle = Some(bundle.clone());
            self.status = format!("发现 v{} · {}", bundle.latest.version, bundle.latest.channel.name());
            self.failed = false;
        }
    }

    pub(super) fn bundle(&self) -> Option<&UpdateBundle> {
        self.bundle.as_ref()
    }

    fn action(&self) -> Option<Action> {
        if self.pending.is_some() || self.restarting {
            return None;
        }
        Some(if self.bundle.is_some() {
            Action::Install
        } else {
            Action::Check
        })
    }

    fn can_show_popup(&self) -> bool {
        self.bundle.is_some() && self.pending.is_none() && !self.restarting
    }

    /// 网络请求交给后台线程，设置窗口用计时器取回结果。
    pub(super) fn start(&mut self, action: Action) -> bool {
        if self.action() != Some(action) {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        let release = self.bundle.as_ref().map(|b| b.latest.clone());
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

    /// 返回任务是否已结束、是否需要从主窗口执行正常退出，并同步最新的远端可用更新缓存。
    pub(super) fn poll(&mut self, available: &mut Option<UpdateBundle>) -> (bool, bool) {
        let Some(receiver) = &self.pending else {
            return (true, false);
        };
        let result = receiver.try_recv();
        if matches!(result, Err(mpsc::TryRecvError::Empty)) {
            return (false, false);
        }
        self.pending = None;
        match result {
            Ok(Outcome::Checked(Ok((bundle, source)))) => {
                self.status = match &bundle {
                    Some(bundle) => {
                        format!(
                            "发现 v{} · {}",
                            bundle.latest.version,
                            bundle.latest.channel.name()
                        )
                    }
                    None => format!("当前已是最新版本 · {}", source.name()),
                };
                self.bundle = bundle.clone();
                *available = bundle;
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

    pub(super) fn toggle(&mut self, layout: &Layout, dc: Option<HDC>, font_body: HFONT, index: usize) {
        self.changelog
            .toggle(index, notes(), &layout.viewport, dc, font_body, layout.scale);
    }

    pub(super) fn scroll(
        &mut self,
        layout: &Layout,
        dc: Option<HDC>,
        font_body: HFONT,
        x: i32,
        y: i32,
        delta: i16,
    ) -> bool {
        self.changelog
            .scroll_by(x, y, delta, notes(), &layout.viewport, dc, font_body, layout.scale)
    }
}

pub(super) struct Layout {
    hero: RECT,
    update: RECT,
    button: RECT,
    details_button: RECT,
    auto_row: RECT,
    auto_toggle: RECT,
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
        let update = band(110, 218);
        let button = RECT {
            left: update.right - s(122),
            right: update.right - s(16),
            top: update.top + s(14),
            bottom: update.top + s(50),
        };
        let details_button = RECT {
            left: button.left - s(8) - s(88),
            right: button.left - s(8),
            top: button.top,
            bottom: button.bottom,
        };
        let auto_row = RECT {
            left: update.left,
            top: update.top + s(64),
            right: update.right,
            bottom: update.bottom,
        };
        let tw = s(44);
        let th = s(24);
        let auto_toggle = RECT {
            left: update.right - s(16) - tw,
            top: auto_row.top + (s(44) - th) / 2,
            right: update.right - s(16),
            bottom: auto_row.top + (s(44) + th) / 2,
        };
        let history = RECT {
            top: content.top + s(230),
            ..content
        };
        Self {
            hero,
            update,
            button,
            details_button,
            auto_row,
            auto_toggle,
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

    /// 命中区与绘制共享位置，并限制在日志可视区内。
    pub(super) fn hit(
        &self,
        state: &State,
        dc: Option<HDC>,
        font_body: HFONT,
        x: i32,
        y: i32,
    ) -> Option<Action> {
        if inside(&self.button, x, y) {
            return state.action();
        }
        if state.can_show_popup() && inside(&self.details_button, x, y) {
            return Some(Action::ShowPopup);
        }
        if inside(&self.auto_toggle, x, y) || inside(&self.auto_row, x, y) {
            return Some(Action::ToggleAutoUpdate);
        }
        if let Some((action, _, _)) = self.links.iter().find(|(_, rect, _)| inside(rect, x, y)) {
            return Some(*action);
        }
        state
            .changelog
            .hit_version(x, y, notes(), &self.viewport, dc, font_body, self.scale)
            .map(Action::Version)
    }

    pub(super) fn tip(&self, action: Action) -> Option<(RECT, &'static str)> {
        match action {
            Action::Check => Some((self.button, "优先检测 GitHub，连接失败时尝试腾讯云 CNB")),
            Action::Install => Some((self.button, "验签通过后安装更新并重启 WinLaunchpad")),
            Action::ShowPopup => Some((self.details_button, "打开弹窗查看所有未安装版本的更新日志")),
            Action::ToggleAutoUpdate => Some((
                self.auto_toggle,
                "开启后启动时及每 4 小时自动后台检测新版本，发现更新时弹窗提示",
            )),
            Action::DataDir => Some((self.links[2].1, "打开设置、缓存与运行日志所在目录")),
            _ => None,
        }
    }
}

pub(super) fn paint(dlg: &Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: HDC) {
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

    // 头部使用软件自身图标，版本号来自 Cargo，和 EXE 版本使用同一发布校验。
    sf.fill_rrect(&l.hero, 14.0 * s, rgb(0xF1F6FF, 1.0));
    draw_app_badge(
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

    // 更新状态、主按钮与自动检测更新开关集中在同一卡片内
    sf.fill_rrect(&l.update, 12.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.update, 12.0 * s, s.max(1.0), border);
    if dlg.hover == Hit::About(Action::ToggleAutoUpdate) {
        sf.fill_rrect(&l.auto_row, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    sf.fill_rect(
        &RECT {
            left: l.update.left + p(16),
            top: l.auto_row.top,
            right: l.update.right - p(16),
            bottom: l.auto_row.top + 1,
        },
        border,
    );

    let state = &dlg.about;
    let text_right_pad = if state.can_show_popup() { 224 } else { 130 };
    draw_text(
        dc,
        "软件更新",
        dlg.font_section,
        &region(l.update, 16, 13, text_right_pad, 22),
        primary,
        TextAlign::Left,
        true,
        false,
    );
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
        &region(l.update, 16, 36, text_right_pad, 22),
        status_color,
        TextAlign::Left,
        true,
        false,
    );
    if state.can_show_popup() {
        draw_button(
            sf,
            dc,
            &l.details_button,
            "更新详情",
            dlg.font_sub,
            BtnStyle::Secondary,
            dlg.hover == Hit::About(Action::ShowPopup),
            dlg.pressed == Hit::About(Action::ShowPopup),
            s,
        );
    }
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

    let auto_lbl = RECT {
        left: l.auto_row.left + p(16),
        top: l.auto_row.top,
        right: l.auto_toggle.left - p(12),
        bottom: l.auto_row.bottom,
    };
    draw_text(
        dc,
        "自动检测更新",
        dlg.font_body,
        &auto_lbl,
        primary,
        TextAlign::Left,
        true,
        false,
    );
    draw_toggle(
        sf,
        &l.auto_toggle,
        app.settings.auto_check_update,
        dlg.hover == Hit::About(Action::ToggleAutoUpdate),
        dlg.pressed == Hit::About(Action::ToggleAutoUpdate),
        s,
    );

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
    let hover_idx = match dlg.hover {
        Hit::About(Action::Version(i)) => Some(i),
        _ => None,
    };
    let pressed_idx = match dlg.pressed {
        Hit::About(Action::Version(i)) => Some(i),
        _ => None,
    };
    state.changelog.paint(
        sf,
        dc,
        notes(),
        &l.viewport,
        dlg.font_section,
        dlg.font_body,
        dlg.font_sub,
        hover_idx,
        pressed_idx,
        s,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_release_notes_parse_and_have_current_version_first() {
        let list = notes();
        assert!(!list.is_empty());
        assert_eq!(list[0].version, update::VERSION);
    }
}
