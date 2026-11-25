//! 可折叠版本更新日志视图：供「设置 → 关于」与「自动更新提示弹窗」复用，
//! 支持默认折叠、单版本展开/收起、自动换行与 Direct2D 彩色 Emoji 渲染。

use std::cell::RefCell;
use std::collections::HashSet;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{IntersectClipRect, RestoreDC, SaveDC, HDC, HFONT};

use crate::ui::{
    draw_scrollbar, draw_text, draw_wrapped_text, measure_wrapped_text_height, rgb,
    ColorTextRenderer, Rgba, Surface, TextAlign,
};
use crate::update::ReleaseNote;

#[derive(Default)]
enum ColorTextCache {
    #[default]
    Uninit,
    Ready(ColorTextRenderer),
    Unavailable,
}

/// 共享的版本日志列表状态与滚动/绘制控制器（默认全部折叠）。
#[derive(Default)]
pub struct ChangelogView {
    pub expanded: HashSet<usize>,
    pub scroll: i32,
    color_text: RefCell<ColorTextCache>,
}

impl ChangelogView {
    fn px(scale: f32, value: i32) -> i32 {
        (value as f32 * scale).round() as i32
    }

    fn text_width(viewport: &RECT, scale: f32) -> i32 {
        let row_w = (viewport.right - viewport.left - Self::px(scale, 8)).max(1);
        (row_w - Self::px(scale, 56)).max(40)
    }

    fn ensure_color_text<'a>(cache: &'a mut ColorTextCache) -> Option<&'a mut ColorTextRenderer> {
        if matches!(*cache, ColorTextCache::Uninit) {
            *cache = match ColorTextRenderer::new() {
                Ok(renderer) => ColorTextCache::Ready(renderer),
                Err(_) => ColorTextCache::Unavailable,
            };
        }
        match cache {
            ColorTextCache::Ready(renderer) => Some(renderer),
            _ => None,
        }
    }

    /// 计算单条更新日志在指定宽度下的行高（支持自动换行，避免长文案被截断）。
    fn item_height(
        &self,
        dc: Option<HDC>,
        font_body: HFONT,
        viewport: &RECT,
        item: &str,
        scale: f32,
    ) -> i32 {
        let max_w = Self::text_width(viewport, scale);
        let font_px = Self::px(scale, 13);
        let min_h = Self::px(scale, 20);
        let measured = {
            let mut cache = self.color_text.borrow_mut();
            Self::ensure_color_text(&mut cache)
                .and_then(|r| r.measure_wrapped_height(font_px, max_w, item))
        }
        .or_else(|| dc.map(|hdc| measure_wrapped_text_height(hdc, item, font_body, max_w)))
        .unwrap_or(min_h);
        (measured + Self::px(scale, 4)).max(min_h)
    }

    fn detail_height(
        &self,
        note: &ReleaseNote,
        viewport: &RECT,
        dc: Option<HDC>,
        font_body: HFONT,
        scale: f32,
    ) -> i32 {
        let sec_h = Self::px(scale, 22);
        let items_h: i32 = note
            .sections
            .iter()
            .map(|section| {
                sec_h
                    + section
                        .items
                        .iter()
                        .map(|item| self.item_height(dc, font_body, viewport, item, scale))
                        .sum::<i32>()
            })
            .sum();
        Self::px(scale, 8) + items_h
    }

    /// 返回 `(当前有效滚动偏移, 内容总高度, 可视区高度)`。
    pub fn scroll_metrics(
        &self,
        notes: &[ReleaseNote],
        viewport: &RECT,
        dc: Option<HDC>,
        font_body: HFONT,
        scale: f32,
    ) -> (i32, i32, i32) {
        let row_h = Self::px(scale, 38);
        let total: i32 = notes
            .iter()
            .enumerate()
            .map(|(i, note)| {
                row_h
                    + if self.expanded.contains(&i) {
                        self.detail_height(note, viewport, dc, font_body, scale)
                    } else {
                        0
                    }
            })
            .sum();
        let visible = (viewport.bottom - viewport.top).max(0);
        (
            self.scroll.clamp(0, (total - visible).max(0)),
            total,
            visible,
        )
    }

    /// 切换指定版本的展开/折叠状态，并收窄滚动偏移。
    pub fn toggle(
        &mut self,
        index: usize,
        notes: &[ReleaseNote],
        viewport: &RECT,
        dc: Option<HDC>,
        font_body: HFONT,
        scale: f32,
    ) {
        if !self.expanded.insert(index) {
            self.expanded.remove(&index);
        }
        self.scroll = self.scroll_metrics(notes, viewport, dc, font_body, scale).0;
    }

    /// 响应滚轮滚动；若滚动位置发生变化返回 true。
    pub fn scroll_by(
        &mut self,
        x: i32,
        y: i32,
        delta: i16,
        notes: &[ReleaseNote],
        viewport: &RECT,
        dc: Option<HDC>,
        font_body: HFONT,
        scale: f32,
    ) -> bool {
        if x < viewport.left
            || x >= viewport.right
            || y < viewport.top
            || y >= viewport.bottom
            || delta == 0
        {
            return false;
        }
        let (offset, total, visible) =
            self.scroll_metrics(notes, viewport, dc, font_body, scale);
        self.scroll = (offset - i32::from(delta) * Self::px(scale, 44) / 120)
            .clamp(0, (total - visible).max(0));
        true
    }

    /// 命中测试：返回鼠标命中的版本行索引。
    pub fn hit_version(
        &self,
        x: i32,
        y: i32,
        notes: &[ReleaseNote],
        viewport: &RECT,
        dc: Option<HDC>,
        font_body: HFONT,
        scale: f32,
    ) -> Option<usize> {
        if x < viewport.left || x >= viewport.right || y < viewport.top || y >= viewport.bottom {
            return None;
        }
        let (offset, _, _) = self.scroll_metrics(notes, viewport, dc, font_body, scale);
        let row_h = Self::px(scale, 38);
        let mut top = viewport.top - offset;
        for (index, note) in notes.iter().enumerate() {
            let bottom = top + row_h;
            if y >= top && y < bottom {
                return Some(index);
            }
            top = bottom
                + if self.expanded.contains(&index) {
                    self.detail_height(note, viewport, dc, font_body, scale)
                } else {
                    0
                };
        }
        None
    }

    fn draw_color_wrapped(
        &self,
        dc: HDC,
        clip: &RECT,
        px: i32,
        color: Rgba,
        items: &[(RECT, &str)],
    ) -> bool {
        if items.is_empty() {
            return true;
        }
        let mut cache = self.color_text.borrow_mut();
        match Self::ensure_color_text(&mut cache) {
            Some(renderer) => renderer.draw_wrapped_lines(dc, clip, px, color, items),
            None => false,
        }
    }

    /// 在裁剪区内绘制全部版本标题行、已展开版本的分段条目（含彩色 Emoji）及滚动条。
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        sf: &mut Surface,
        dc: HDC,
        notes: &[ReleaseNote],
        viewport: &RECT,
        font_section: HFONT,
        font_body: HFONT,
        font_sub: HFONT,
        hover_idx: Option<usize>,
        pressed_idx: Option<usize>,
        scale: f32,
    ) {
        let p = |dp: i32| Self::px(scale, dp);
        let primary = rgb(0x17253D, 1.0);
        let muted = rgb(0x52647D, 1.0);
        let region = |parent: RECT, left, top, right, height| RECT {
            left: parent.left + p(left),
            top: parent.top + p(top),
            right: parent.right - p(right),
            bottom: parent.top + p(top + height),
        };

        let (offset, total, visible) =
            self.scroll_metrics(notes, viewport, Some(dc), font_body, scale);
        let rc = *viewport;
        let save = unsafe { SaveDC(dc) };
        if save == 0 {
            return;
        }
        unsafe {
            IntersectClipRect(dc, rc.left, rc.top, rc.right, rc.bottom);
        }
        sf.set_clip(Some(rc));

        let mut top = rc.top - offset;
        let mut color_items: Vec<(RECT, &str)> = Vec::new();
        for (index, note) in notes.iter().enumerate() {
            let row = RECT {
                top,
                bottom: top + p(38),
                right: rc.right - p(8),
                ..rc
            };
            let open = self.expanded.contains(&index);
            if top < rc.bottom && row.bottom > rc.top {
                let fill = if pressed_idx == Some(index) {
                    0xE3ECFB
                } else if hover_idx == Some(index) {
                    0xEEF4FF
                } else {
                    0xF7F9FC
                };
                sf.fill_rrect(
                    &RECT {
                        bottom: row.bottom - p(3),
                        ..row
                    },
                    7.0 * scale,
                    rgb(fill, 1.0),
                );
                let (x, y) = ((row.left + p(15)) as f32, (top + p(18)) as f32);
                let a = 3.2 * scale;
                let sw = scale.max(1.0);
                if open {
                    sf.thick_line(x - a, y - a * 0.5, x, y + a * 0.5, sw, muted);
                    sf.thick_line(x, y + a * 0.5, x + a, y - a * 0.5, sw, muted);
                } else {
                    sf.thick_line(x - a * 0.5, y - a, x + a * 0.5, y, sw, muted);
                    sf.thick_line(x + a * 0.5, y, x - a * 0.5, y + a, sw, muted);
                }
                draw_text(
                    dc,
                    &format!("v{}", note.version),
                    font_section,
                    &region(row, 30, 0, 116, 35),
                    primary,
                    TextAlign::Left,
                    true,
                    false,
                );
                if !note.date.is_empty() {
                    draw_text(
                        dc,
                        &note.date,
                        font_sub,
                        &region(row, 108, 0, 12, 35),
                        muted,
                        TextAlign::Right,
                        true,
                        false,
                    );
                }
            }
            top = row.bottom;
            if open {
                for section in &note.sections {
                    let section_rc = RECT { top, ..row };
                    if top < rc.bottom && top + p(22) > rc.top {
                        draw_text(
                            dc,
                            &section.title,
                            font_section,
                            &region(section_rc, 30, 0, 12, 22),
                            primary,
                            TextAlign::Left,
                            true,
                            false,
                        );
                    }
                    top += p(22);
                    for item in &section.items {
                        let ih = self.item_height(Some(dc), font_body, &rc, item, scale);
                        let text_rc = RECT {
                            left: row.left + p(44),
                            top: top + p(2),
                            right: row.right - p(12),
                            bottom: top + ih,
                        };
                        if top < rc.bottom && text_rc.bottom > rc.top {
                            sf.fill_circle(
                                (row.left + p(34)) as f32,
                                (top + p(10)) as f32,
                                1.7 * scale,
                                muted,
                            );
                            color_items.push((text_rc, item.as_str()));
                        }
                        top += ih;
                    }
                }
                top += p(8);
            }
        }

        // 批量绘制日志彩色 Emoji 多行条目，保持滚动区裁剪并保留 GDI 回退路径。
        if !self.draw_color_wrapped(dc, &rc, p(13), muted, &color_items) {
            for (text_rc, item) in color_items {
                draw_wrapped_text(dc, item, font_body, &text_rc, muted);
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
            scale,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::ReleaseSection;

    #[test]
    fn changelog_starts_collapsed_and_toggles_cleanly() {
        let mut view = ChangelogView::default();
        assert!(view.expanded.is_empty());

        let notes = vec![
            ReleaseNote {
                version: "0.0.3".into(),
                date: "2026-11-25".into(),
                sections: vec![ReleaseSection {
                    title: "更新".into(),
                    items: vec!["🔔 很长的一条测试更新说明，用来验证自动换行高度计算不会返回零。".into()],
                }],
            },
            ReleaseNote {
                version: "0.0.2".into(),
                date: "2026-11-25".into(),
                sections: vec![ReleaseSection {
                    title: "修复".into(),
                    items: vec!["📦 另一条说明".into()],
                }],
            },
        ];
        let viewport = RECT {
            left: 0,
            top: 0,
            right: 220,
            bottom: 180,
        };
        let font = crate::ui::make_font(13, 400);

        let (_, collapsed_h, _) = view.scroll_metrics(&notes, &viewport, None, font, 1.0);
        assert_eq!(collapsed_h, 76);
        assert_eq!(
            view.hit_version(20, 10, &notes, &viewport, None, font, 1.0),
            Some(0)
        );
        assert_eq!(
            view.hit_version(20, 48, &notes, &viewport, None, font, 1.0),
            Some(1)
        );

        view.toggle(0, &notes, &viewport, None, font, 1.0);
        assert!(view.expanded.contains(&0));
        let (_, expanded_h, _) = view.scroll_metrics(&notes, &viewport, None, font, 1.0);
        assert!(expanded_h > collapsed_h + 40);

        view.toggle(0, &notes, &viewport, None, font, 1.0);
        assert!(!view.expanded.contains(&0));
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(
                windows::Win32::Graphics::Gdi::HGDIOBJ(font.0),
            );
        }
    }
}
