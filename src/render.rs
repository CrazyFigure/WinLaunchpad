//! 启动台主面板渲染层：毛玻璃桌面壁纸半分辨率缓存、预烘焙阴影图标高速 1:1 合成、
//! 自适应网格与可调图标大小、水平滑动翻页过渡、底部圆点分页、自定义右键菜单与 100ms Tooltip。

use std::collections::{HashMap, VecDeque};

use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, SetStretchBltMode, StretchBlt, COLORONCOLOR, PAINTSTRUCT,
    SRCCOPY,
};

use crate::config::roundness_ratio;
use crate::ui::{
    bake_icon_sprite, clip_icon_roundness, draw_text, draw_tooltip, make_font, measure_text, rgb,
    DibTarget, IconPixels, Rgba, Surface, TextAlign,
};
use crate::wallpaper::WallpaperTexture;

const ICON_BUDGET: usize = 8 * 1024 * 1024;
const SPRITE_BUDGET: usize = 8 * 1024 * 1024;

const C_SEARCH_BORDER: Rgba = (1.0, 1.0, 1.0, 0.24);
const C_TEXT: Rgba = (1.0, 1.0, 1.0, 0.96);
const C_TEXT_DIM: Rgba = (1.0, 1.0, 1.0, 0.58);
pub const C_TEXT_HINT: Rgba = (1.0, 1.0, 1.0, 0.48);
const C_PILL_BG: Rgba = (1.0, 1.0, 1.0, 0.08);
const C_PILL_HOVER: Rgba = (1.0, 1.0, 1.0, 0.18);
const C_PILL_PRESS: Rgba = (1.0, 1.0, 1.0, 0.26);
const C_ICON_HOVER: Rgba = (1.0, 1.0, 1.0, 0.13);
const C_SEL_FILL: Rgba = (1.0, 1.0, 1.0, 0.15);
const C_SEL_BORDER: Rgba = (1.0, 1.0, 1.0, 0.78);
const C_DOT_ON: Rgba = (1.0, 1.0, 1.0, 0.96);
const C_DOT_OFF: Rgba = (1.0, 1.0, 1.0, 0.30);
const C_DOT_HOVER: Rgba = (1.0, 1.0, 1.0, 0.65);

// ---------- 布局与命中 ----------

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub w: i32,
    pub h: i32,
    pub search: RECT,
    pub edit: RECT,
    pub add_btn: RECT,
    pub settings_btn: RECT,
    pub cells: Vec<RECT>,
    pub grid_area: RECT,
    pub icon_size: i32,
    pub cols: i32,
    pub rows: i32,
    pub grid_span_w: i32,
    pub dots_center: f32,
    pub dots_y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Cell(usize),
    Search,
    Add,
    Settings,
    Dot(usize),
    Blank,
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 页码圆点中心 x 坐标。
pub fn dot_x(layout: &Layout, i: usize, pages: usize, scale: f32) -> f32 {
    let gap = 18.0 * scale;
    let total = (pages.saturating_sub(1) as f32) * gap;
    layout.dots_center - total * 0.5 + i as f32 * gap
}

/// 单页第 `i` 个圆点的命中矩形。
pub fn dot_rect(layout: &Layout, i: usize, pages: usize, scale: f32) -> RECT {
    let cx = dot_x(layout, i, pages, scale);
    let cy = layout.dots_y;
    let hr = (9.0 * scale).round() as i32;
    RECT {
        left: cx.round() as i32 - hr,
        top: cy.round() as i32 - hr,
        right: cx.round() as i32 + hr,
        bottom: cy.round() as i32 + hr,
    }
}

/// 精确计算图标与标题的可点击区域（留白处视为 Blank 以便点击空白退出）。
/// 计算单元格内「图标 + 标题」的视觉中心包围盒，用于选框、点击命中与悬停定位。
fn cell_content_bounds(cell: &RECT, icon_size: i32, scale: f32) -> (f32, i32, i32) {
    let cell_h = (cell.bottom - cell.top) as f32;
    let vis_h = icon_size as f32 * 0.85 + 24.0 * scale;
    let vis_top = cell.top as f32 + ((cell_h - vis_h) * 0.45).max(6.0 * scale);
    let icon_cy = vis_top + icon_size as f32 * 0.425;
    let label_top = (vis_top + icon_size as f32 * 0.85 + 5.0 * scale).round() as i32;
    let label_bottom = (label_top + (20.0 * scale).round() as i32).min(cell.bottom - (2.0 * scale).round() as i32);
    (icon_cy, label_top, label_bottom)
}

pub fn cell_visual_rect(cell: &RECT, icon_size: i32, scale: f32) -> RECT {
    let cx = (cell.left + cell.right) / 2;
    let (icon_cy, _, label_bottom) = cell_content_bounds(cell, icon_size, scale);
    let half_w = ((icon_size as f32 * 0.78).max(52.0 * scale)).round() as i32;
    let top = (icon_cy - icon_size as f32 * 0.425 - 10.0 * scale).round() as i32;
    RECT {
        left: (cx - half_w).max(cell.left),
        top: top.max(cell.top),
        right: (cx + half_w).min(cell.right),
        bottom: (label_bottom + (6.0 * scale).round() as i32).min(cell.bottom),
    }
}

pub fn hit_test(
    layout: &Layout,
    x: i32,
    y: i32,
    page_item_count: usize,
    pages: usize,
    scale: f32,
) -> Hit {
    if inside(&layout.add_btn, x, y) {
        return Hit::Add;
    }
    if inside(&layout.settings_btn, x, y) {
        return Hit::Settings;
    }
    if inside(&layout.search, x, y) {
        return Hit::Search;
    }
    if pages > 1 {
        for i in 0..pages {
            let dr = dot_rect(layout, i, pages, scale);
            if inside(&dr, x, y) {
                return Hit::Dot(i);
            }
        }
    }
    for (i, c) in layout.cells.iter().enumerate().take(page_item_count) {
        let vr = cell_visual_rect(c, layout.icon_size, scale);
        if inside(&vr, x, y) {
            return Hit::Cell(i);
        }
    }
    Hit::Blank
}

/// 根据窗口像素与缩放比推算当前显示器推荐的默认行列数（0 代表采用自动计算）。
pub fn adaptive_grid(w: i32, h: i32, scale: f32) -> (i32, i32) {
    let s = |v: f32| (v * scale).round() as i32;
    let margin_x = ((w as f32 * 0.08).max(56.0 * scale)).round() as i32;
    let sy = s(26.0);
    let sh = s(32.0);
    let top = sy + sh + s(24.0);
    let bottom = h - s(62.0);
    let avail_w = (w - margin_x * 2).max(s(240.0));
    let avail_h = (bottom - top).max(s(160.0));

    let cols = (avail_w / s(172.0).max(1)).clamp(3, 8);
    // 全屏下包含任务栏高度（1536×960 逻辑像素），以 140dp 行步进保证 3072×1920 默认稳定为 7×5
    let rows = (avail_h / s(140.0).max(1)).clamp(2, 6);
    (cols, rows)
}

pub fn compute_layout(w: i32, h: i32, scale: f32, icon_size_dp: i32, grid_cols: i32, grid_rows: i32) -> Layout {
    let s = |v: f32| (v * scale).round() as i32;
    let mut l = Layout { w, h, ..Default::default() };

    // 顶部居中搜索胶囊
    let sw = s(272.0).min((w - s(220.0)).max(s(160.0)));
    let sh = s(32.0);
    let sx = (w - sw) / 2;
    let sy = s(26.0);
    l.search = RECT {
        left: sx,
        top: sy,
        right: sx + sw,
        bottom: sy + sh,
    };
    l.edit = RECT {
        left: sx + s(34.0),
        top: sy + s(5.0),
        right: sx + sw - s(12.0),
        bottom: sy + sh - s(5.0),
    };

    // 右上角圆角玻璃图标按钮（+ 与 ⚙）
    let btn_sz = s(32.0);
    let margin_r = s(28.0);
    l.settings_btn = RECT {
        left: w - margin_r - btn_sz,
        top: sy,
        right: w - margin_r,
        bottom: sy + btn_sz,
    };
    l.add_btn = RECT {
        left: l.settings_btn.left - btn_sz - s(10.0),
        top: sy,
        right: l.settings_btn.left - s(10.0),
        bottom: sy + btn_sz,
    };

    // 主网格区域：支持用户自定义行列（0=自动），并在网格紧凑时自动限制图标大小防溢出
    let margin_x = ((w as f32 * 0.08).max(56.0 * scale)).round() as i32;
    let top = sy + sh + s(24.0);
    let bottom = h - s(62.0);
    let avail_w = (w - margin_x * 2).max(s(240.0));
    let avail_h = (bottom - top).max(s(160.0));

    let (auto_cols, auto_rows) = adaptive_grid(w, h, scale);
    let cols = if grid_cols > 0 { grid_cols.clamp(4, 10) } else { auto_cols };
    let rows = if grid_rows > 0 { grid_rows.clamp(3, 8) } else { auto_rows };

    let cell_w = (avail_w / cols).min(s(200.0));
    let cell_h = (avail_h / rows).min(s(176.0));
    let grid_w = cols * cell_w;
    let grid_h = rows * cell_h;
    let left = (w - grid_w) / 2;
    let gtop = top + ((avail_h - grid_h) / 2).max(0);

    for r in 0..rows {
        for c in 0..cols {
            let x = left + c * cell_w;
            let y = gtop + r * cell_h;
            l.cells.push(RECT {
                left: x,
                top: y,
                right: x + cell_w,
                bottom: y + cell_h,
            });
        }
    }
    l.cols = cols;
    l.rows = rows;
    l.grid_area = RECT {
        left: 0,
        top: sy + sh + s(8.0),
        right: w,
        bottom: h - s(52.0),
    };
    l.grid_span_w = w.max(1);
    let max_icon = (cell_h - s(40.0)).min(cell_w - s(20.0)).max(s(24.0));
    l.icon_size = s(icon_size_dp.clamp(42, 78) as f32).clamp(24, max_icon.min(192));
    l.dots_y = (h - s(32.0)) as f32;
    l.dots_center = w as f32 * 0.5;
    l
}

// ---------- 图标与预烘焙精灵缓存 ----------

pub struct IconCache {
    map: HashMap<String, IconPixels>,
    order: VecDeque<String>,
    bytes: usize,
    sprites: HashMap<(String, i32, u16), IconPixels>,
    sprite_order: VecDeque<(String, i32, u16)>,
    sprite_bytes: usize,
}

impl IconCache {
    pub fn new() -> Self {
        IconCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            sprites: HashMap::new(),
            sprite_order: VecDeque::new(),
            sprite_bytes: 0,
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<IconPixels> {
        self.map.get(key).cloned()
    }

    pub fn insert(&mut self, key: String, icon: IconPixels) {
        let cost = (icon.w * icon.h * 4) as usize;
        if let Some(old) = self.map.insert(key.clone(), icon) {
            self.bytes -= (old.w * old.h * 4) as usize;
        } else {
            self.order.push_back(key.clone());
        }
        self.bytes += cost;
        // 源图标更新时失效对应预烘焙精灵
        self.sprites.retain(|(k, _, _), _| k != &key);
        while self.bytes > ICON_BUDGET {
            let Some(old) = self.order.pop_front() else { break };
            if let Some(b) = self.map.remove(&old) {
                self.bytes -= (b.w * b.h * 4) as usize;
            }
        }
    }

    /// 获取或一次性烘焙指定尺寸及圆角弧度的含阴影 1:1 图标精灵，保障滑动翻页高帧率。
    pub fn get_or_bake_sprite(&mut self, key: &str, size: i32, shadow_offset: i32, round_ratio: f32) -> Option<IconPixels> {
        let rm = (round_ratio * 1000.0).round() as u16;
        let sk = (key.to_string(), size, rm);
        if let Some(sp) = self.sprites.get(&sk) {
            return Some(sp.clone());
        }
        let src = self.map.get(key)?;
        let sp = bake_icon_sprite(src, size, shadow_offset, round_ratio);
        let cost = (sp.w * sp.h * 4) as usize;
        if let Some(old) = self.sprites.insert(sk.clone(), sp.clone()) {
            self.sprite_bytes -= (old.w * old.h * 4) as usize;
        } else {
            self.sprite_order.push_back(sk);
        }
        self.sprite_bytes += cost;
        while self.sprite_bytes > SPRITE_BUDGET {
            let Some(old_k) = self.sprite_order.pop_front() else { break };
            if let Some(b) = self.sprites.remove(&old_k) {
                self.sprite_bytes -= (b.w * b.h * 4) as usize;
            }
        }
        Some(sp)
    }

    pub fn clear_sprites(&mut self) {
        self.sprites.clear();
        self.sprite_order.clear();
        self.sprite_bytes = 0;
    }

    /// 隐藏后保留约一页常用图标精灵缓存，兼顾空闲低内存占用与再次呼出瞬时响应。
    pub fn trim_sprites_to(&mut self, budget: usize) {
        while self.sprite_bytes > budget {
            let Some(old_k) = self.sprite_order.pop_front() else {
                break;
            };
            if let Some(b) = self.sprites.remove(&old_k) {
                self.sprite_bytes -= (b.w * b.h * 4) as usize;
            }
        }
    }
}

// ---------- 右键浮层菜单与帧描述 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextCmd {
    Open = 10,
    OpenLocation = 11,
    Remove = 12,
    Add = 20,
    Rescan = 21,
    Settings = 22,
}

#[derive(Clone, Debug)]
pub struct ContextItem {
    pub cmd: ContextCmd,
    pub label: &'static str,
    pub danger: bool,
}

#[derive(Clone, Debug)]
pub struct ContextMenu {
    pub rect: RECT,
    pub items: Vec<ContextItem>,
    pub item_rects: Vec<RECT>,
    pub hover: Option<usize>,
}

impl ContextMenu {
    pub fn build(x: i32, y: i32, bounds_w: i32, bounds_h: i32, items: Vec<ContextItem>, scale: f32) -> Self {
        let w = (164.0 * scale).round() as i32;
        let pad_y = (6.0 * scale).round() as i32;
        let row_h = (32.0 * scale).round() as i32;
        let h = pad_y * 2 + row_h * items.len() as i32;
        let margin = (8.0 * scale).round() as i32;
        let left = x.clamp(margin, (bounds_w - w - margin).max(margin));
        let top = y.clamp(margin, (bounds_h - h - margin).max(margin));
        let rect = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
        let pad_x = (6.0 * scale).round() as i32;
        let item_rects = (0..items.len())
            .map(|i| RECT {
                left: left + pad_x,
                top: top + pad_y + i as i32 * row_h,
                right: left + w - pad_x,
                bottom: top + pad_y + (i as i32 + 1) * row_h,
            })
            .collect();
        ContextMenu {
            rect,
            items,
            item_rects,
            hover: None,
        }
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<usize> {
        self.item_rects.iter().position(|r| inside(r, x, y))
    }
}

#[derive(Default, Clone)]
pub struct ViewItem {
    pub key: String,
    pub name: String,
    pub valid: bool,
}

pub struct Frame<'a> {
    pub items: &'a [ViewItem],
    pub neighbor_items: &'a [ViewItem],
    pub neighbor_offset: i32,
    pub slide_x: f32,
    pub hover: Option<usize>,
    pub pressed_cell: Option<usize>,
    pub selected: Option<usize>,
    pub page: usize,
    pub pages: usize,
    pub query_empty: bool,
    pub status: &'a str,
    pub empty_text: &'a str,
    pub hover_btn: Option<Hit>,
    pub pressed_btn: Option<Hit>,
    pub tooltip: Option<(RECT, &'a str)>,
    pub context_menu: Option<&'a ContextMenu>,
    pub use_wallpaper: bool,
    pub icon_roundness: u8,
}

// ---------- 渲染器 ----------

struct Fonts {
    name: windows::Win32::Graphics::Gdi::HFONT,
    status: windows::Win32::Graphics::Gdi::HFONT,
    empty: windows::Win32::Graphics::Gdi::HFONT,
    generic: windows::Win32::Graphics::Gdi::HFONT,
    menu: windows::Win32::Graphics::Gdi::HFONT,
    tooltip: windows::Win32::Graphics::Gdi::HFONT,
}

impl Fonts {
    fn new(scale: f32) -> Self {
        let s = |px: f32| (px * scale).round() as i32;
        Fonts {
            name: make_font(s(12.5), 400),
            status: make_font(s(12.0), 400),
            empty: make_font(s(18.0), 400),
            generic: make_font(s(22.0), 500),
            menu: make_font(s(13.0), 400),
            tooltip: make_font(s(12.0), 400),
        }
    }
    fn destroy(&self) {
        unsafe {
            for f in [
                self.name,
                self.status,
                self.empty,
                self.generic,
                self.menu,
                self.tooltip,
            ] {
                let _ = windows::Win32::Graphics::Gdi::DeleteObject(
                    windows::Win32::Graphics::Gdi::HGDIOBJ(f.0),
                );
            }
        }
    }
}

pub struct Renderer {
    target: DibTarget,
    /// 半分辨率预放大背景缓存（仅在可见时分配，隐藏即释放，消除每帧 HALFTONE 耗时）
    bg_half: DibTarget,
    bg_sig: (u64, i32, i32, bool),
    fonts: Fonts,
    pub scale: f32,
    pub icons: IconCache,
    pub wallpaper: WallpaperTexture,
    pub search_bg_rgb: (f32, f32, f32),
}

impl Renderer {
    pub fn new() -> windows::core::Result<Self> {
        Ok(Renderer {
            target: DibTarget::default(),
            bg_half: DibTarget::default(),
            bg_sig: (0, 0, 0, false),
            fonts: Fonts::new(1.0),
            scale: 1.0,
            icons: IconCache::new(),
            wallpaper: WallpaperTexture::default(),
            search_bg_rgb: (0.14, 0.17, 0.24),
        })
    }

    pub fn ensure_target(&mut self, _hwnd: HWND, w: u32, h: u32, scale: f32) -> bool {
        let size_changed = self.target.w != w as i32 || self.target.h != h as i32;
        let scale_changed = (self.scale - scale).abs() >= 0.01;
        if self.target.mem_dc.is_some() && !size_changed && !scale_changed {
            return true;
        }
        if !self.target.ensure(w as i32, h as i32) {
            return false;
        }
        if scale_changed {
            self.fonts.destroy();
            self.fonts = Fonts::new(scale);
            self.scale = scale;
            self.icons.clear_sprites();
        }
        true
    }

    fn ensure_bg_half(&mut self, w: i32, h: i32, use_wallpaper: bool) {
        let hw = (w / 2).max(1);
        let hh = (h / 2).max(1);
        let sig = (
            if use_wallpaper && self.wallpaper.is_ready() { self.wallpaper.sig } else { 0 },
            hw,
            hh,
            use_wallpaper,
        );
        if self.bg_half.mem_dc.is_some() && self.bg_sig == sig {
            return;
        }
        if !self.bg_half.ensure(hw, hh) {
            return;
        }
        let Some(bdc) = self.bg_half.mem_dc else { return };
        let drew = use_wallpaper && self.wallpaper.blit_cover(bdc, hw, hh);
        if !drew {
            if let Some(mut sf) = self.bg_half.surface() {
                sf.clear_vertical_gradient((0.14, 0.17, 0.28), (0.08, 0.10, 0.18), (0.04, 0.05, 0.10));
            }
        }
        self.bg_sig = sig;
    }

    /// 隐藏窗口后释放屏幕尺寸 DIB 与半分辨率背景，仅保留轻量一页图标精灵以消除二次呼出卡顿。
    pub fn release_target(&mut self) {
        self.target.release();
        self.bg_half.release();
        self.bg_sig = (0, 0, 0, false);
        self.icons.trim_sprites_to(2 * 1024 * 1024);
    }

    pub fn add_icon(&mut self, key: &str, pixels: &[u8], w: u32, h: u32) {
        let icon = IconPixels {
            w,
            h,
            px: std::sync::Arc::new(pixels.to_vec()),
        };
        self.icons.insert(key.to_string(), icon);
    }

    pub fn compute_search_colorref(&mut self, layout: &Layout, use_wallpaper: bool) -> COLORREF {
        let base = if use_wallpaper && self.wallpaper.is_ready() {
            self.wallpaper.sample_region_rgb(&layout.search, layout.w, layout.h)
        } else {
            (0.11, 0.14, 0.22)
        };
        let a = 0.14;
        self.search_bg_rgb = (
            base.0 * (1.0 - a) + a,
            base.1 * (1.0 - a) + a,
            base.2 * (1.0 - a) + a,
        );
        crate::ui::to_colorref((
            self.search_bg_rgb.0,
            self.search_bg_rgb.1,
            self.search_bg_rgb.2,
            1.0,
        ))
    }

    pub fn is_label_truncated(&self, name: &str, layout: &Layout) -> bool {
        let Some(dc) = self.target.mem_dc else {
            return false;
        };
        let pad = (8.0 * self.scale).round() as i32;
        let max_w = (layout.cells.first().map(|c| c.right - c.left).unwrap_or(120) - pad * 2).max(20);
        let (tw, _) = measure_text(dc, name, self.fonts.name);
        tw > max_w
    }

    /// 绘制单页应用网格（非悬停图标走 1:1 预烘焙贴图，保证滑页流畅）。
    fn paint_page(
        &mut self,
        surface: &mut Surface,
        dc: windows::Win32::Graphics::Gdi::HDC,
        layout: &Layout,
        items: &[ViewItem],
        dx: i32,
        hover: Option<usize>,
        pressed_cell: Option<usize>,
        selected: Option<usize>,
        round_ratio: f32,
    ) {
        let s = self.scale;
        let icon_base = layout.icon_size;
        let shadow_off = (3.0 * s).round() as i32;

        for (i, cell) in layout.cells.iter().enumerate() {
            let Some(item) = items.get(i) else { break };
            let c = RECT {
                left: cell.left + dx,
                top: cell.top,
                right: cell.right + dx,
                bottom: cell.bottom,
            };
            if c.right <= 0 || c.left >= layout.w {
                continue;
            }
            let is_sel = selected == Some(i);
            let is_hov = hover == Some(i);
            let is_prs = pressed_cell == Some(i);

            let icon_cx = (c.left + c.right) as f32 * 0.5;
            let (icon_cy, label_top, label_bottom) = cell_content_bounds(&c, icon_base, s);

            if is_sel {
                let vr = cell_visual_rect(&c, icon_base, s);
                surface.fill_rrect(&vr, 14.0 * s, C_SEL_FILL);
                surface.stroke_rrect(&vr, 14.0 * s, (1.5 * s).max(1.0), C_SEL_BORDER);
            } else if is_hov {
                let half = icon_base as f32 * 0.5 + 10.0 * s;
                let glow = RECT {
                    left: (icon_cx - half).round() as i32,
                    top: (icon_cy - half).round() as i32,
                    right: (icon_cx + half).round() as i32,
                    bottom: (icon_cy + half).round() as i32,
                };
                surface.fill_rrect(&glow, 16.0 * s, C_ICON_HOVER);
            }

            if is_hov || is_prs {
                let draw_sz = if is_prs { icon_base as f32 * 0.94 } else { icon_base as f32 * 1.07 };
                if let Some(icon) = self.icons.get(&item.key) {
                    let rounded = clip_icon_roundness(&icon, round_ratio);
                    surface.draw_icon_bilinear(&rounded, icon_cx, icon_cy, draw_sz, true, 3.0 * s);
                } else {
                    self.paint_fallback_icon(surface, dc, &item.name, icon_cx, icon_cy, draw_sz);
                }
            } else if let Some(sprite) = self.icons.get_or_bake_sprite(&item.key, icon_base, shadow_off, round_ratio) {
                surface.draw_icon_1to1(&sprite, icon_cx, icon_cy, icon_base);
            } else {
                self.paint_fallback_icon(surface, dc, &item.name, icon_cx, icon_cy, icon_base as f32);
            }

            let pad = (8.0 * s).round() as i32;
            let label = RECT {
                left: c.left + pad,
                top: label_top,
                right: c.right - pad,
                bottom: label_bottom,
            };
            draw_text(
                dc,
                &item.name,
                self.fonts.name,
                &label,
                if item.valid { C_TEXT } else { C_TEXT_DIM },
                TextAlign::Center,
                true,
                true,
            );
        }
    }

    fn paint_fallback_icon(
        &self,
        surface: &mut Surface,
        dc: windows::Win32::Graphics::Gdi::HDC,
        name: &str,
        icon_cx: f32,
        icon_cy: f32,
        draw_sz: f32,
    ) {
        let s = self.scale;
        let half = draw_sz * 0.5;
        let r = RECT {
            left: (icon_cx - half) as i32,
            top: (icon_cy - half) as i32,
            right: (icon_cx + half) as i32,
            bottom: (icon_cy + half) as i32,
        };
        surface.fill_rrect(&r, 14.0 * s, (1.0, 1.0, 1.0, 0.14));
        surface.stroke_rrect(&r, 14.0 * s, 1.0, (1.0, 1.0, 1.0, 0.22));
        let ch: String = name.chars().next().map(|c| c.to_string()).unwrap_or_default();
        if !ch.is_empty() {
            draw_text(dc, &ch, self.fonts.generic, &r, C_TEXT_DIM, TextAlign::Center, false, false);
        }
    }

    pub fn paint(&mut self, hwnd: HWND, f: &Frame<'_>, layout: &Layout) -> bool {
        if !self.ensure_target(hwnd, layout.w.max(1) as u32, layout.h.max(1) as u32, self.scale) {
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            return false;
        }
        self.ensure_bg_half(layout.w, layout.h, f.use_wallpaper);
        let dc = self.target.mem_dc.unwrap();

        let mut ps = PAINTSTRUCT::default();
        let wdc = unsafe { BeginPaint(hwnd, &mut ps) };
        if wdc.is_invalid() {
            return true;
        }
        let prc = ps.rcPaint;
        let paint_rc = if prc.right > prc.left && prc.bottom > prc.top {
            RECT {
                left: prc.left.max(0),
                top: prc.top.max(0),
                right: prc.right.min(layout.w),
                bottom: prc.bottom.min(layout.h),
            }
        } else {
            RECT {
                left: 0,
                top: 0,
                right: layout.w,
                bottom: layout.h,
            }
        };
        let pw = (paint_rc.right - paint_rc.left).max(0);
        let ph = (paint_rc.bottom - paint_rc.top).max(0);
        if pw == 0 || ph == 0 {
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            return true;
        }

        // 高速 2× 拉伸拷贝半分辨率背景（局部脏区更新时仅拉伸对齐后的脏矩形）
        if let Some(bdc) = self.bg_half.mem_dc {
            unsafe {
                let _ = SetStretchBltMode(dc, COLORONCOLOR);
                if layout.w % 2 == 0
                    && layout.h % 2 == 0
                    && self.bg_half.w == layout.w / 2
                    && self.bg_half.h == layout.h / 2
                    && (pw < layout.w || ph < layout.h)
                {
                    let ax0 = paint_rc.left & !1;
                    let ay0 = paint_rc.top & !1;
                    let ax1 = ((paint_rc.right + 1) & !1).min(layout.w);
                    let ay1 = ((paint_rc.bottom + 1) & !1).min(layout.h);
                    let aw = (ax1 - ax0).max(0);
                    let ah = (ay1 - ay0).max(0);
                    if aw > 0 && ah > 0 {
                        let _ = StretchBlt(
                            dc,
                            ax0,
                            ay0,
                            aw,
                            ah,
                            bdc,
                            ax0 / 2,
                            ay0 / 2,
                            aw / 2,
                            ah / 2,
                            SRCCOPY,
                        );
                    }
                } else {
                    let _ = StretchBlt(
                        dc,
                        0,
                        0,
                        layout.w,
                        layout.h,
                        bdc,
                        0,
                        0,
                        self.bg_half.w,
                        self.bg_half.h,
                        SRCCOPY,
                    );
                }
            }
        }

        let Some(mut surface) = self.target.surface() else {
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            return false;
        };
        let s = self.scale;
        let top_bar_bottom = layout.search.bottom.max(layout.settings_btn.bottom).max(layout.add_btn.bottom);
        let need_top_bar = paint_rc.top <= top_bar_bottom + 2;

        if need_top_bar {
            // 1. 顶部磨砂搜索胶囊（空框时由编辑框统一居中画放大镜与「搜索」，有输入时放大镜固定左侧）
            let sb_col = (self.search_bg_rgb.0, self.search_bg_rgb.1, self.search_bg_rgb.2, 1.0);
            surface.fill_rrect(&layout.search, 9.0 * s, sb_col);
            surface.stroke_rrect(&layout.search, 9.0 * s, (1.0 * s).max(1.0), C_SEARCH_BORDER);

            if !f.query_empty {
                let cx = layout.search.left as f32 + 18.0 * s;
                let cy = (layout.search.top + layout.search.bottom) as f32 * 0.5;
                let icon_col = C_TEXT_DIM;
                surface.stroke_circle(cx, cy - 1.0 * s, 5.2 * s, (1.5 * s).max(1.0), icon_col);
                surface.thick_line(
                    cx + 3.6 * s,
                    cy + 2.6 * s,
                    cx + 7.4 * s,
                    cy + 6.4 * s,
                    (1.6 * s).max(1.0),
                    icon_col,
                );
            }

            // 2. 右上角圆角玻璃图标按钮（+ 添加、⚙ 设置）
            for (r, hit) in [(&layout.add_btn, Hit::Add), (&layout.settings_btn, Hit::Settings)] {
                let is_hov = f.hover_btn == Some(hit);
                let is_prs = f.pressed_btn == Some(hit);
                let bg = if is_prs {
                    C_PILL_PRESS
                } else if is_hov {
                    C_PILL_HOVER
                } else {
                    C_PILL_BG
                };
                surface.fill_rrect(r, 10.0 * s, bg);
                surface.stroke_rrect(
                    r,
                    10.0 * s,
                    (1.0 * s).max(1.0),
                    (1.0, 1.0, 1.0, if is_hov { 0.30 } else { 0.14 }),
                );
                let bx = (r.left + r.right) as f32 * 0.5;
                let by = (r.top + r.bottom) as f32 * 0.5;
                let c = if is_hov || is_prs { C_TEXT } else { C_TEXT_DIM };
                if hit == Hit::Add {
                    let arm = 6.0 * s;
                    let sw = (1.8 * s).max(1.2);
                    surface.thick_line(bx - arm, by, bx + arm, by, sw, c);
                    surface.thick_line(bx, by - arm, bx, by + arm, sw, c);
                } else {
                    let rad = 5.4 * s;
                    surface.stroke_circle(bx, by, rad, (1.7 * s).max(1.2), c);
                    surface.fill_circle(bx, by, 1.6 * s, c);
                    for k in 0..6 {
                        let ang = k as f32 * std::f32::consts::PI / 3.0;
                        let px = bx + ang.cos() * (rad + 2.2 * s);
                        let py = by + ang.sin() * (rad + 2.2 * s);
                        surface.fill_circle(px, py, 1.25 * s, c);
                    }
                }
            }
        }

        // 3. 图标网格（带裁剪区，支持鼠标左右滑动翻页过渡）
        surface.set_clip(Some(layout.grid_area));
        let round_ratio = roundness_ratio(f.icon_roundness);
        let dx = f.slide_x.round() as i32;
        if !f.neighbor_items.is_empty() && dx != 0 {
            self.paint_page(
                &mut surface,
                dc,
                layout,
                f.neighbor_items,
                dx + f.neighbor_offset,
                None,
                None,
                None,
                round_ratio,
            );
        }
        self.paint_page(
            &mut surface,
            dc,
            layout,
            f.items,
            dx,
            f.hover,
            f.pressed_cell,
            f.selected,
            round_ratio,
        );
        surface.set_clip(None);

        // 4. 空状态文案
        if f.items.is_empty() && !f.empty_text.is_empty() {
            let area = if layout.cells.is_empty() {
                RECT { left: 0, top: 0, right: layout.w, bottom: layout.h }
            } else {
                let first = layout.cells[0];
                let last = layout.cells[layout.cells.len() - 1];
                RECT {
                    left: first.left,
                    top: first.top,
                    right: last.right,
                    bottom: last.bottom,
                }
            };
            draw_text(dc, f.empty_text, self.fonts.empty, &area, C_TEXT_DIM, TextAlign::Center, false, true);
        }

        // 5. 底部居中分页圆点
        if f.pages > 1 {
            for i in 0..f.pages {
                let x = dot_x(layout, i, f.pages, s);
                let is_cur = i == f.page;
                let is_hov = f.hover_btn == Some(Hit::Dot(i));
                let r = if is_cur || is_hov { 4.3 * s } else { 3.3 * s };
                let col = if is_cur {
                    C_DOT_ON
                } else if is_hov {
                    C_DOT_HOVER
                } else {
                    C_DOT_OFF
                };
                surface.fill_circle(x, layout.dots_y, r, col);
            }
        }

        // 6. 左下角轻量扫描状态提示
        if !f.status.is_empty() {
            let r = RECT {
                left: (28.0 * s) as i32,
                top: layout.h - (44.0 * s) as i32,
                right: layout.w / 2 - (80.0 * s) as i32,
                bottom: layout.h - (16.0 * s) as i32,
            };
            draw_text(dc, f.status, self.fonts.status, &r, C_TEXT_DIM, TextAlign::Left, true, true);
        }

        // 7. 自定义磨砂右键菜单（浮层）
        if let Some(menu) = f.context_menu {
            let shadow = RECT {
                left: menu.rect.left,
                top: menu.rect.top + (4.0 * s) as i32,
                right: menu.rect.right,
                bottom: menu.rect.bottom + (6.0 * s) as i32,
            };
            surface.fill_rrect(&shadow, 10.0 * s, (0.0, 0.0, 0.0, 0.35));
            surface.fill_rrect(&menu.rect, 10.0 * s, rgb(0x1E232E, 0.94));
            surface.stroke_rrect(&menu.rect, 10.0 * s, (1.0 * s).max(1.0), (1.0, 1.0, 1.0, 0.16));
            for (i, item) in menu.items.iter().enumerate() {
                let ir = &menu.item_rects[i];
                if menu.hover == Some(i) {
                    let hbg = if item.danger { rgb(0xDC2626, 0.85) } else { rgb(0x1677FF, 0.90) };
                    surface.fill_rrect(ir, 6.0 * s, hbg);
                }
                let tr = RECT {
                    left: ir.left + (12.0 * s) as i32,
                    top: ir.top,
                    right: ir.right - (12.0 * s) as i32,
                    bottom: ir.bottom,
                };
                let tc = if item.danger && menu.hover != Some(i) {
                    rgb(0xFCA5A5, 0.96)
                } else {
                    C_TEXT
                };
                draw_text(dc, item.label, self.fonts.menu, &tr, tc, TextAlign::Left, true, false);
            }
        }

        // 8. 自定义 100ms 悬停 Tooltip
        if f.context_menu.is_none() {
            if let Some((anchor, tip)) = f.tooltip {
                draw_tooltip(&mut surface, dc, self.fonts.tooltip, &anchor, tip, layout.w, layout.h, s);
            }
        }

        unsafe {
            let _ = BitBlt(wdc, paint_rc.left, paint_rc.top, pw, ph, dc, paint_rc.left, paint_rc.top, SRCCOPY);
            let _ = EndPaint(hwnd, &ps);
        }
        true
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.target.release();
        self.bg_half.release();
        self.fonts.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::compute_layout;

    #[test]
    fn adaptive_grid_matches_screen_sizes() {
        let l_3k = compute_layout(3072, 1920, 2.0, 56, 0, 0);
        assert_eq!((l_3k.cols, l_3k.rows), (7, 5));

        let l_1080_125 = compute_layout(1920, 1080, 1.25, 56, 0, 0);
        assert_eq!((l_1080_125.cols, l_1080_125.rows), (7, 5));

        let l_1080_150 = compute_layout(1920, 1080, 1.5, 56, 0, 0);
        assert_eq!((l_1080_150.cols, l_1080_150.rows), (6, 4));

        let l_768 = compute_layout(1366, 768, 1.0, 56, 0, 0);
        assert_eq!((l_768.cols, l_768.rows), (6, 4));

        let l_2k = compute_layout(2560, 1440, 1.0, 56, 0, 0);
        assert_eq!((l_2k.cols, l_2k.rows), (8, 6));

        let l_custom = compute_layout(3072, 1920, 2.0, 56, 9, 6);
        assert_eq!((l_custom.cols, l_custom.rows), (9, 6));
    }
}
