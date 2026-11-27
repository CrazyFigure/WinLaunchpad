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
    bake_icon_sprite, draw_text, draw_tooltip, make_font, measure_text, rgb, DibTarget,
    IconPixels, Rgba, Surface, TextAlign,
};
use crate::wallpaper::WallpaperTexture;

const ICON_BUDGET: usize = 24 * 1024 * 1024;
const SPRITE_BUDGET: usize = 12 * 1024 * 1024;

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
    pub new_folder_btn: RECT,
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
    NewFolder,
    Add,
    Settings,
    Dot(usize),
    Blank,
}

#[derive(Clone, Debug, Default)]
pub struct FolderLayout {
    pub card: RECT,
    pub title_rc: RECT,
    pub close_btn: RECT,
    pub cells: Vec<RECT>,
    pub icon_size: i32,
    pub cols: i32,
    pub dots_center: f32,
    pub dots_y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderHit {
    Cell(usize),
    Title,
    Close,
    Dot(usize),
    Inside,
    Outside,
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 任意水平居中分页圆点组第 `i` 个圆点的 x 坐标。
fn dot_x_at(center_x: f32, i: usize, pages: usize, scale: f32) -> f32 {
    let gap = 18.0 * scale;
    let total = (pages.saturating_sub(1) as f32) * gap;
    center_x - total * 0.5 + i as f32 * gap
}

/// 任意水平居中分页圆点组第 `i` 个圆点的命中矩形。
fn dot_rect_at(center_x: f32, y: f32, i: usize, pages: usize, scale: f32) -> RECT {
    let cx = dot_x_at(center_x, i, pages, scale);
    let hr = (9.0 * scale).round() as i32;
    RECT {
        left: cx.round() as i32 - hr,
        top: y.round() as i32 - hr,
        right: cx.round() as i32 + hr,
        bottom: y.round() as i32 + hr,
    }
}

/// 页码圆点中心 x 坐标。
pub fn dot_x(layout: &Layout, i: usize, pages: usize, scale: f32) -> f32 {
    dot_x_at(layout.dots_center, i, pages, scale)
}

/// 单页第 `i` 个圆点的命中矩形。
pub fn dot_rect(layout: &Layout, i: usize, pages: usize, scale: f32) -> RECT {
    dot_rect_at(layout.dots_center, layout.dots_y, i, pages, scale)
}

/// 展开文件夹卡片内第 `i` 个分页圆点的命中矩形。
pub fn folder_dot_rect(layout: &FolderLayout, i: usize, pages: usize, scale: f32) -> RECT {
    dot_rect_at(layout.dots_center, layout.dots_y, i, pages, scale)
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
    if inside(&layout.new_folder_btn, x, y) {
        return Hit::NewFolder;
    }
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

/// 计算展开文件夹浮层几何布局（居中玻璃卡片 + 标题栏 + 4×3 内部网格 + 分页圆点）。
pub fn compute_folder_layout(w: i32, h: i32, scale: f32, icon_size_dp: i32, pages: usize) -> FolderLayout {
    let s = |v: f32| (v * scale).round() as i32;
    let cols = if w < s(620.0) { 3 } else { 4 };
    let rows = 3;
    let header_h = s(58.0);
    let footer_h = if pages > 1 { s(38.0) } else { s(20.0) };
    let pad_x = s(24.0);

    let max_cw = ((w - s(80.0) - pad_x * 2) / cols).max(s(84.0));
    let max_ch = ((h - s(110.0) - header_h - footer_h) / rows).max(s(84.0));
    let cell_w = s(134.0).min(max_cw);
    let cell_h = s(122.0).min(max_ch);

    let card_w = cols * cell_w + pad_x * 2;
    let card_h = header_h + rows * cell_h + footer_h;
    let left = (w - card_w) / 2;
    let top = (h - card_h) / 2;
    let card = RECT {
        left,
        top,
        right: left + card_w,
        bottom: top + card_h,
    };
    let close_sz = s(28.0);
    let close_btn = RECT {
        left: card.right - s(16.0) - close_sz,
        top: card.top + s(15.0),
        right: card.right - s(16.0),
        bottom: card.top + s(15.0) + close_sz,
    };
    let title_rc = RECT {
        left: card.left + s(56.0),
        top: card.top + s(13.0),
        right: card.right - s(56.0),
        bottom: card.top + s(47.0),
    };
    let mut cells = Vec::with_capacity((cols * rows) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let x = card.left + pad_x + c * cell_w;
            let y = card.top + header_h + r * cell_h;
            cells.push(RECT {
                left: x,
                top: y,
                right: x + cell_w,
                bottom: y + cell_h,
            });
        }
    }
    let max_icon = (cell_h - s(38.0)).min(cell_w - s(18.0)).max(s(24.0));
    let icon_size = s(icon_size_dp.clamp(42, 64) as f32).clamp(24, max_icon.min(160));
    FolderLayout {
        card,
        title_rc,
        close_btn,
        cells,
        icon_size,
        cols,
        dots_center: (card.left + card.right) as f32 * 0.5,
        dots_y: (card.bottom - s(18.0)) as f32,
    }
}

/// 对展开文件夹浮层执行点击/悬停命中测试。
pub fn folder_hit_test(
    fl: &FolderLayout,
    x: i32,
    y: i32,
    page_item_count: usize,
    pages: usize,
    scale: f32,
) -> FolderHit {
    if !inside(&fl.card, x, y) {
        return FolderHit::Outside;
    }
    if inside(&fl.close_btn, x, y) {
        return FolderHit::Close;
    }
    if inside(&fl.title_rc, x, y) {
        return FolderHit::Title;
    }
    if pages > 1 {
        for i in 0..pages {
            let dr = folder_dot_rect(fl, i, pages, scale);
            if inside(&dr, x, y) {
                return FolderHit::Dot(i);
            }
        }
    }
    for (i, c) in fl.cells.iter().enumerate().take(page_item_count) {
        let vr = cell_visual_rect(c, fl.icon_size, scale);
        if inside(&vr, x, y) {
            return FolderHit::Cell(i);
        }
    }
    FolderHit::Inside
}

pub fn compute_layout(w: i32, h: i32, scale: f32, icon_size_dp: i32, grid_cols: i32, grid_rows: i32) -> Layout {
    let s = |v: f32| (v * scale).round() as i32;
    let mut l = Layout { w, h, ..Default::default() };

    // 顶部居中搜索胶囊
    let sw = s(272.0).min((w - s(320.0)).max(s(160.0)));
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

    // 右上角圆角玻璃图标按钮（📁+ 新建文件夹、+ 添加、⚙ 设置）
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
    l.new_folder_btn = RECT {
        left: l.add_btn.left - btn_sz - s(10.0),
        top: sy,
        right: l.add_btn.left - s(10.0),
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
        // 源图标更新时失效对应预烘焙精灵并同步扣减精灵缓存字节计数
        self.sprites.retain(|(k, _, _), sp| {
            if k == &key {
                self.sprite_bytes = self.sprite_bytes.saturating_sub((sp.w * sp.h * 4) as usize);
                false
            } else {
                true
            }
        });
        self.sprite_order.retain(|(k, _, _)| k != &key);
        while self.bytes > ICON_BUDGET {
            let Some(old) = self.order.pop_front() else { break };
            if let Some(b) = self.map.remove(&old) {
                self.bytes -= (b.w * b.h * 4) as usize;
            }
        }
    }

    /// 移除单个图标及其关联精灵缓存（用于应用卸载或图标源文件更新后失效缓存）。
    pub fn remove(&mut self, key: &str) {
        if let Some(old) = self.map.remove(key) {
            self.bytes = self.bytes.saturating_sub((old.w * old.h * 4) as usize);
            self.order.retain(|k| k != key);
        }
        self.sprites.retain(|(k, _, _), sp| {
            if k == key {
                self.sprite_bytes = self.sprite_bytes.saturating_sub((sp.w * sp.h * 4) as usize);
                false
            } else {
                true
            }
        });
        self.sprite_order.retain(|(k, _, _)| k != key);
    }

    /// 清空全部内存图标与预烘焙精灵缓存。
    pub fn clear_all(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
        self.clear_sprites();
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

    /// 隐藏后保留约一页常用图标缓存，兼顾空闲低内存占用与再次呼出瞬时响应。
    pub fn trim_icons_to(&mut self, budget: usize) {
        while self.bytes > budget {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(b) = self.map.remove(&old) {
                self.bytes = self.bytes.saturating_sub((b.w * b.h * 4) as usize);
            }
        }
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
    NewFolder = 23,
    OpenFolder = 30,
    RenameFolder = 31,
    DeleteFolder = 32,
    RemoveFromFolder = 33,
    CloseFolder = 34,
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

#[derive(Clone, Default)]
pub enum ViewItemKind {
    #[default]
    App,
    Folder {
        preview_keys: Vec<String>,
    },
}

#[derive(Default, Clone)]
pub struct ViewItem {
    pub id: String,
    pub key: String,
    pub name: String,
    pub valid: bool,
    pub kind: ViewItemKind,
}

pub struct DragGhost<'a> {
    pub icon_key: &'a str,
    pub name: &'a str,
    pub x: i32,
    pub y: i32,
    pub hint: Option<&'a str>,
}

pub struct OpenFolderFrame<'a> {
    pub layout: &'a FolderLayout,
    pub title: &'a str,
    pub items: &'a [ViewItem],
    pub page: usize,
    pub pages: usize,
    pub hover: Option<FolderHit>,
    pub pressed: Option<FolderHit>,
    pub selected: Option<usize>,
    pub hidden_app_id: Option<&'a str>,
    pub outside_drag: bool,
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
    pub hidden_app_id: Option<&'a str>,
    pub drop_folder_id: Option<&'a str>,
    pub open_folder: Option<OpenFolderFrame<'a>>,
    pub drag_ghost: Option<DragGhost<'a>>,
}

// ---------- 渲染器 ----------

struct Fonts {
    name: windows::Win32::Graphics::Gdi::HFONT,
    folder_title: windows::Win32::Graphics::Gdi::HFONT,
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
            folder_title: make_font(s(20.0), 600),
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
                self.folder_title,
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

    /// 隐藏窗口后释放屏幕尺寸 DIB 与半分辨率背景，仅保留轻量一页图标与精灵缓存以消除二次呼出卡顿。
    pub fn release_target(&mut self) {
        self.target.release();
        self.bg_half.release();
        self.bg_sig = (0, 0, 0, false);
        self.icons.trim_icons_to(6 * 1024 * 1024);
        self.icons.trim_sprites_to(3 * 1024 * 1024);
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
        let cw = layout.cells.first().map(|c| c.right - c.left).unwrap_or(120);
        self.is_label_truncated_in_cell(name, cw)
    }

    pub fn is_label_truncated_in_cell(&self, name: &str, cell_w: i32) -> bool {
        let Some(dc) = self.target.mem_dc else {
            return false;
        };
        let pad = (8.0 * self.scale).round() as i32;
        let max_w = (cell_w - pad * 2).max(20);
        let (tw, _) = measure_text(dc, name, self.fonts.name);
        tw > max_w
    }

    /// 绘制文件夹缩略图标（磨砂圆角卡片底板 + 最多 3×3 高清小图标预览，拖拽悬停时高亮描边）。
    #[allow(clippy::too_many_arguments)]
    fn paint_folder_icon(
        &mut self,
        surface: &mut Surface,
        preview_keys: &[String],
        icon_cx: f32,
        icon_cy: f32,
        draw_sz: f32,
        round_ratio: f32,
        is_drop_target: bool,
    ) {
        let s = self.scale;
        let half = draw_sz * 0.5;
        let r = RECT {
            left: (icon_cx - half).round() as i32,
            top: (icon_cy - half).round() as i32,
            right: (icon_cx + half).round() as i32,
            bottom: (icon_cy + half).round() as i32,
        };
        let rad = (draw_sz * round_ratio.max(0.18)).max(4.0);

        let bg = if is_drop_target {
            (0.26, 0.52, 0.96, 0.42)
        } else {
            (1.0, 1.0, 1.0, 0.20)
        };
        let border = if is_drop_target {
            rgb(0x60A5FA, 0.95)
        } else {
            (1.0, 1.0, 1.0, 0.36)
        };
        let bw = if is_drop_target {
            (2.2 * s).max(1.6)
        } else {
            (1.1 * s).max(1.0)
        };
        surface.fill_rrect(&r, rad, bg);
        surface.stroke_rrect(&r, rad, bw, border);

        if preview_keys.is_empty() {
            // 空文件夹显示居中的极简文件夹轮廓提示
            let fw = draw_sz * 0.46;
            let fh = draw_sz * 0.34;
            let fr = RECT {
                left: (icon_cx - fw * 0.5).round() as i32,
                top: (icon_cy - fh * 0.38).round() as i32,
                right: (icon_cx + fw * 0.5).round() as i32,
                bottom: (icon_cy + fh * 0.62).round() as i32,
            };
            let line_col = (1.0, 1.0, 1.0, 0.68);
            let sw = (1.5 * s).max(1.1);
            surface.stroke_rrect(&fr, 3.0 * s, sw, line_col);
            surface.thick_line(
                fr.left as f32 + 2.0 * s,
                fr.top as f32 - 2.5 * s,
                fr.left as f32 + fw * 0.44,
                fr.top as f32 - 2.5 * s,
                sw,
                line_col,
            );
            return;
        }

        // 3×3 小图标预览网格（复用预烘焙 Lanczos3 精灵以兼顾清晰度与滑动流畅性）
        let pad = draw_sz * 0.14;
        let gap = draw_sz * 0.06;
        let mini_sz = ((draw_sz - pad * 2.0 - gap * 2.0) / 3.0).max(4.0);
        let mini_sz_i = mini_sz.round().max(8.0) as i32;
        let origin_x = r.left as f32 + pad;
        let origin_y = r.top as f32 + pad;
        for (idx, key) in preview_keys.iter().take(9).enumerate() {
            let row = idx / 3;
            let col = idx % 3;
            let mcx = origin_x + col as f32 * (mini_sz + gap) + mini_sz * 0.5;
            let mcy = origin_y + row as f32 * (mini_sz + gap) + mini_sz * 0.5;
            if let Some(sprite) = self.icons.get_or_bake_sprite(key, mini_sz_i, 0, round_ratio) {
                surface.draw_icon_1to1(&sprite, mcx, mcy, mini_sz_i);
            } else {
                let mh = mini_sz * 0.5;
                let mr = RECT {
                    left: (mcx - mh).round() as i32,
                    top: (mcy - mh).round() as i32,
                    right: (mcx + mh).round() as i32,
                    bottom: (mcy + mh).round() as i32,
                };
                surface.fill_rrect(&mr, (mini_sz * 0.22).max(1.5), (1.0, 1.0, 1.0, 0.26));
            }
        }
    }

    /// 绘制任意一组图标单元格（复用在主网格与展开文件夹网格中）。
    #[allow(clippy::too_many_arguments)]
    fn paint_cells(
        &mut self,
        surface: &mut Surface,
        dc: windows::Win32::Graphics::Gdi::HDC,
        cells: &[RECT],
        bounds_w: i32,
        icon_base: i32,
        items: &[ViewItem],
        dx: i32,
        hover: Option<usize>,
        pressed_cell: Option<usize>,
        selected: Option<usize>,
        hidden_app_id: Option<&str>,
        drop_folder_id: Option<&str>,
        round_ratio: f32,
    ) {
        let s = self.scale;

        for (i, cell) in cells.iter().enumerate() {
            let Some(item) = items.get(i) else { break };
            let c = RECT {
                left: cell.left + dx,
                top: cell.top,
                right: cell.right + dx,
                bottom: cell.bottom,
            };
            if c.right <= 0 || c.left >= bounds_w {
                continue;
            }
            let is_hidden = hidden_app_id == Some(item.id.as_str())
                && matches!(item.kind, ViewItemKind::App);
            let is_drop = drop_folder_id == Some(item.id.as_str())
                && matches!(item.kind, ViewItemKind::Folder { .. });
            let is_sel = selected == Some(i) && !is_hidden;
            let is_hov = (hover == Some(i) && !is_hidden) || is_drop;
            let is_prs = pressed_cell == Some(i) && !is_hidden;

            let icon_cx = (c.left + c.right) as f32 * 0.5;
            let (icon_cy, label_top, label_bottom) = cell_content_bounds(&c, icon_base, s);

            if is_hidden {
                let half = icon_base as f32 * 0.45;
                let slot = RECT {
                    left: (icon_cx - half).round() as i32,
                    top: (icon_cy - half).round() as i32,
                    right: (icon_cx + half).round() as i32,
                    bottom: (icon_cy + half).round() as i32,
                };
                surface.stroke_rrect_dashed(
                    &slot,
                    12.0 * s,
                    (1.2 * s).max(1.0),
                    (4.0 * s).round() as i32,
                    (1.0, 1.0, 1.0, 0.22),
                );
                continue;
            }

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
                surface.fill_rrect(
                    &glow,
                    16.0 * s,
                    if is_drop {
                        (0.38, 0.65, 1.0, 0.24)
                    } else {
                        C_ICON_HOVER
                    },
                );
            }

            match &item.kind {
                ViewItemKind::Folder { preview_keys } => {
                    let draw_sz = if is_drop {
                        icon_base as f32 * 1.12
                    } else if is_prs {
                        icon_base as f32 * 0.94
                    } else if is_hov {
                        icon_base as f32 * 1.07
                    } else {
                        icon_base as f32
                    };
                    self.paint_folder_icon(
                        surface,
                        preview_keys,
                        icon_cx,
                        icon_cy,
                        draw_sz,
                        round_ratio,
                        is_drop,
                    );
                }
                ViewItemKind::App => {
                    // 正常、悬停与按下态统一走预烘焙 Lanczos3 无阴影精灵缓存，保证清晰度与高帧率
                    let draw_sz_i = if is_prs {
                        (icon_base as f32 * 0.94).round().max(8.0) as i32
                    } else if is_hov {
                        (icon_base as f32 * 1.07).round().max(8.0) as i32
                    } else {
                        icon_base
                    };
                    if let Some(sprite) =
                        self.icons.get_or_bake_sprite(&item.key, draw_sz_i, 0, round_ratio)
                    {
                        surface.draw_icon_1to1(&sprite, icon_cx, icon_cy, draw_sz_i);
                    } else {
                        self.paint_fallback_icon(surface, dc, &item.name, icon_cx, icon_cy, draw_sz_i as f32);
                    }
                }
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
        let top_bar_bottom = layout
            .search
            .bottom
            .max(layout.settings_btn.bottom)
            .max(layout.add_btn.bottom)
            .max(layout.new_folder_btn.bottom);
        let need_top_bar = paint_rc.top <= top_bar_bottom + 2 || f.open_folder.is_some() || f.drag_ghost.is_some();

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

            // 2. 右上角圆角玻璃图标按钮（📁+ 新建文件夹、+ 添加、⚙ 设置）
            for (r, hit) in [
                (&layout.new_folder_btn, Hit::NewFolder),
                (&layout.add_btn, Hit::Add),
                (&layout.settings_btn, Hit::Settings),
            ] {
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
                if hit == Hit::NewFolder {
                    let sw = (1.5 * s).max(1.1);
                    let fr = RECT {
                        left: (bx - 7.5 * s).round() as i32,
                        top: (by - 4.0 * s).round() as i32,
                        right: (bx + 7.5 * s).round() as i32,
                        bottom: (by + 6.0 * s).round() as i32,
                    };
                    surface.stroke_rrect(&fr, 2.5 * s, sw, c);
                    surface.thick_line(bx - 6.0 * s, by - 5.8 * s, bx - 0.5 * s, by - 5.8 * s, sw, c);
                    let ps = 2.8 * s;
                    surface.thick_line(bx - ps, by + 1.0 * s, bx + ps, by + 1.0 * s, sw, c);
                    surface.thick_line(bx, by + 1.0 * s - ps, bx, by + 1.0 * s + ps, sw, c);
                } else if hit == Hit::Add {
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
            self.paint_cells(
                &mut surface,
                dc,
                &layout.cells,
                layout.w,
                layout.icon_size,
                f.neighbor_items,
                dx + f.neighbor_offset,
                None,
                None,
                None,
                f.hidden_app_id,
                f.drop_folder_id,
                round_ratio,
            );
        }
        self.paint_cells(
            &mut surface,
            dc,
            &layout.cells,
            layout.w,
            layout.icon_size,
            f.items,
            dx,
            f.hover,
            f.pressed_cell,
            f.selected,
            f.hidden_app_id,
            f.drop_folder_id,
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

        // 7. 展开文件夹浮层（居中玻璃卡片）
        if let Some(of) = &f.open_folder {
            let full = RECT {
                left: 0,
                top: 0,
                right: layout.w,
                bottom: layout.h,
            };
            surface.fill_rect(&full, (0.0, 0.0, 0.0, 0.46));
            let card = &of.layout.card;
            let shadow = RECT {
                left: card.left,
                top: card.top + (6.0 * s).round() as i32,
                right: card.right,
                bottom: card.bottom + (10.0 * s).round() as i32,
            };
            surface.fill_rrect(&shadow, 22.0 * s, (0.0, 0.0, 0.0, 0.36));
            surface.fill_rrect(card, 22.0 * s, rgb(0x181D27, 0.92));
            let border_c = if of.outside_drag {
                rgb(0x60A5FA, 0.85)
            } else {
                (1.0, 1.0, 1.0, 0.24)
            };
            surface.stroke_rrect(card, 22.0 * s, (1.2 * s).max(1.0), border_c);

            // 文件夹标题（支持悬停胶囊与点击重命名）
            let title_hov = of.hover == Some(FolderHit::Title);
            let title_prs = of.pressed == Some(FolderHit::Title);
            if title_hov || title_prs {
                let (tw, _) = measure_text(dc, of.title, self.fonts.folder_title);
                let pill_w = (tw + (28.0 * s).round() as i32)
                    .clamp((80.0 * s).round() as i32, of.layout.title_rc.right - of.layout.title_rc.left);
                let tcx = (of.layout.title_rc.left + of.layout.title_rc.right) / 2;
                let pill = RECT {
                    left: tcx - pill_w / 2,
                    top: of.layout.title_rc.top,
                    right: tcx + (pill_w - pill_w / 2),
                    bottom: of.layout.title_rc.bottom,
                };
                surface.fill_rrect(
                    &pill,
                    9.0 * s,
                    if title_prs { C_PILL_PRESS } else { C_PILL_HOVER },
                );
            }
            draw_text(
                dc,
                of.title,
                self.fonts.folder_title,
                &of.layout.title_rc,
                C_TEXT,
                TextAlign::Center,
                true,
                true,
            );

            // 文件夹右上角关闭按钮
            let close_hov = of.hover == Some(FolderHit::Close);
            let close_prs = of.pressed == Some(FolderHit::Close);
            if close_hov || close_prs {
                surface.fill_rrect(
                    &of.layout.close_btn,
                    8.0 * s,
                    if close_prs { C_PILL_PRESS } else { C_PILL_HOVER },
                );
            }
            let ccx = (of.layout.close_btn.left + of.layout.close_btn.right) as f32 * 0.5;
            let ccy = (of.layout.close_btn.top + of.layout.close_btn.bottom) as f32 * 0.5;
            let ca = 4.5 * s;
            let cc = if close_hov || close_prs { C_TEXT } else { C_TEXT_DIM };
            surface.thick_line(ccx - ca, ccy - ca, ccx + ca, ccy + ca, (1.6 * s).max(1.2), cc);
            surface.thick_line(ccx - ca, ccy + ca, ccx + ca, ccy - ca, (1.6 * s).max(1.2), cc);

            let hov_cell = match of.hover {
                Some(FolderHit::Cell(i)) => Some(i),
                _ => None,
            };
            let prs_cell = match of.pressed {
                Some(FolderHit::Cell(i)) => Some(i),
                _ => None,
            };
            surface.set_clip(Some(*card));
            if of.items.is_empty() {
                let empty_rc = RECT {
                    left: card.left + (24.0 * s) as i32,
                    top: of.layout.title_rc.bottom,
                    right: card.right - (24.0 * s) as i32,
                    bottom: card.bottom - (20.0 * s) as i32,
                };
                draw_text(
                    dc,
                    "暂无应用",
                    self.fonts.empty,
                    &empty_rc,
                    C_TEXT_DIM,
                    TextAlign::Center,
                    false,
                    true,
                );
            } else {
                self.paint_cells(
                    &mut surface,
                    dc,
                    &of.layout.cells,
                    layout.w,
                    of.layout.icon_size,
                    of.items,
                    0,
                    hov_cell,
                    prs_cell,
                    of.selected,
                    of.hidden_app_id,
                    None,
                    round_ratio,
                );
            }
            surface.set_clip(None);

            if of.pages > 1 {
                for i in 0..of.pages {
                    let x = dot_x_at(of.layout.dots_center, i, of.pages, s);
                    let is_cur = i == of.page;
                    let is_hov = of.hover == Some(FolderHit::Dot(i));
                    let r = if is_cur || is_hov { 4.2 * s } else { 3.2 * s };
                    let col = if is_cur {
                        C_DOT_ON
                    } else if is_hov {
                        C_DOT_HOVER
                    } else {
                        C_DOT_OFF
                    };
                    surface.fill_circle(x, of.layout.dots_y, r, col);
                }
            }
        }

        // 8. 拖拽图标跟手浮层（无投影，采用 Lanczos3 预烘焙精灵保持跟手顺滑）
        if let Some(ghost) = &f.drag_ghost {
            let draw_sz = layout.icon_size as f32 * 1.12;
            let draw_sz_i = draw_sz.round().max(8.0) as i32;
            let gx = ghost.x as f32;
            let gy = ghost.y as f32;
            if let Some(sprite) =
                self.icons.get_or_bake_sprite(ghost.icon_key, draw_sz_i, 0, round_ratio)
            {
                surface.draw_icon_1to1(&sprite, gx, gy, draw_sz_i);
            } else {
                self.paint_fallback_icon(&mut surface, dc, ghost.name, gx, gy, draw_sz);
            }
            if let Some(hint) = ghost.hint {
                if !hint.is_empty() {
                    let (tw, th) = measure_text(dc, hint, self.fonts.tooltip);
                    let pad_x = (10.0 * s).round() as i32;
                    let pad_y = (4.0 * s).round() as i32;
                    let bw = (tw + pad_x * 2).min((layout.w - (24.0 * s) as i32).max(100));
                    let bh = th + pad_y * 2;
                    let left = (ghost.x - bw / 2).clamp((8.0 * s) as i32, (layout.w - bw - (8.0 * s) as i32).max(8));
                    let top = (ghost.y + (draw_sz * 0.5 + 10.0 * s).round() as i32)
                        .min((layout.h - bh - (8.0 * s) as i32).max(8));
                    let pill = RECT {
                        left,
                        top,
                        right: left + bw,
                        bottom: top + bh,
                    };
                    surface.fill_rrect(&pill, 8.0 * s, rgb(0x1677FF, 0.92));
                    surface.stroke_rrect(&pill, 8.0 * s, (1.0 * s).max(1.0), (1.0, 1.0, 1.0, 0.25));
                    draw_text(dc, hint, self.fonts.tooltip, &pill, C_TEXT, TextAlign::Center, true, false);
                }
            }
        }

        // 9. 自定义磨砂右键菜单（浮层，无下方投影）
        if let Some(menu) = f.context_menu {
            self.paint_context_menu(&mut surface, dc, menu);
        }

        // 10. 自定义 100ms 悬停 Tooltip
        if f.context_menu.is_none() && f.drag_ghost.is_none() {
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

    /// 绘制自定义磨砂右键菜单浮层（不绘制菜单下方阴影）。
    fn paint_context_menu(
        &self,
        surface: &mut Surface,
        dc: windows::Win32::Graphics::Gdi::HDC,
        menu: &ContextMenu,
    ) {
        let s = self.scale;
        surface.fill_rrect(&menu.rect, 10.0 * s, rgb(0x1E232E, 0.94));
        surface.stroke_rrect(&menu.rect, 10.0 * s, (1.0 * s).max(1.0), (1.0, 1.0, 1.0, 0.16));
        for (i, item) in menu.items.iter().enumerate() {
            let ir = &menu.item_rects[i];
            if menu.hover == Some(i) {
                let hbg = if item.danger {
                    rgb(0xDC2626, 0.85)
                } else {
                    rgb(0x1677FF, 0.90)
                };
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
            draw_text(dc, item.label, self.fonts.menu, &tr, tc, TextAlign::Left, false, false);
        }
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
    use super::{compute_folder_layout, compute_layout, folder_hit_test, hit_test, FolderHit, Hit};

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

    #[test]
    fn icon_cache_remove_and_clear_update_bytes() {
        let mut cache = super::IconCache::new();
        let px = crate::ui::IconPixels {
            w: 8,
            h: 8,
            px: std::sync::Arc::new(vec![255u8; 8 * 8 * 4]),
        };
        cache.insert("k1".into(), px.clone());
        cache.insert("k2".into(), px);
        let _ = cache.get_or_bake_sprite("k1", 16, 2, 0.225);
        assert!(cache.contains("k1"));
        cache.remove("k1");
        assert!(!cache.contains("k1"));
        assert!(cache.contains("k2"));
        cache.clear_all();
        assert!(!cache.contains("k2"));
    }

    #[test]
    fn top_buttons_and_folder_layout_hit_tests_work() {
        let l = compute_layout(1920, 1080, 1.25, 56, 0, 0);
        assert!(l.new_folder_btn.right < l.add_btn.left);
        assert!(l.search.right < l.new_folder_btn.left);
        let cx = (l.new_folder_btn.left + l.new_folder_btn.right) / 2;
        let cy = (l.new_folder_btn.top + l.new_folder_btn.bottom) / 2;
        assert_eq!(hit_test(&l, cx, cy, 0, 1, 1.25), Hit::NewFolder);

        let fl = compute_folder_layout(1920, 1080, 1.25, 56, 2);
        assert_eq!((fl.cols, fl.cells.len()), (4, 12));
        assert_eq!(folder_hit_test(&fl, 5, 5, 3, 2, 1.25), FolderHit::Outside);
        let tcx = (fl.title_rc.left + fl.title_rc.right) / 2;
        let tcy = (fl.title_rc.top + fl.title_rc.bottom) / 2;
        assert_eq!(folder_hit_test(&fl, tcx, tcy, 3, 2, 1.25), FolderHit::Title);
        let ccx = (fl.close_btn.left + fl.close_btn.right) / 2;
        let ccy = (fl.close_btn.top + fl.close_btn.bottom) / 2;
        assert_eq!(folder_hit_test(&fl, ccx, ccy, 3, 2, 1.25), FolderHit::Close);
    }

    #[test]
    fn launchpad_folder_icon_and_context_menu_have_no_bottom_shadow() {
        let mut renderer = super::Renderer::new().unwrap();
        renderer.scale = 1.0;
        let mut target = crate::ui::DibTarget::default();
        assert!(target.ensure(320, 320));
        let bg = crate::ui::rgb(0x405060, 1.0);
        let bg_px = 0xFF40_5060u32;
        let dc = target.mem_dc.unwrap();

        // 1. 文件夹图标下方不应存在黑影像素（仅保留 1px 描边 AA 余量外全部与背景一致）
        {
            let mut sf = target.surface().unwrap();
            sf.clear(bg);
            renderer.paint_folder_icon(&mut sf, &[], 80.0, 80.0, 64.0, 0.225, false);
            let slice = unsafe { std::slice::from_raw_parts(target.ptr as *const u32, 320 * 320) };
            for dy in 2..=6 {
                let py = 80 + 32 + dy;
                assert_eq!(slice[py * 320 + 80], bg_px, "unexpected shadow below folder icon at dy={dy}");
            }
        }

        // 2. 右键菜单下方不应存在黑影像素
        {
            let mut sf = target.surface().unwrap();
            sf.clear(bg);
            let menu = super::ContextMenu::build(
                40,
                40,
                320,
                320,
                vec![
                    super::ContextItem { cmd: super::ContextCmd::Open, label: "打开", danger: false },
                    super::ContextItem { cmd: super::ContextCmd::Remove, label: "从启动台移除", danger: true },
                ],
                1.0,
            );
            renderer.paint_context_menu(&mut sf, dc, &menu);
            let slice = unsafe { std::slice::from_raw_parts(target.ptr as *const u32, 320 * 320) };
            let mcx = ((menu.rect.left + menu.rect.right) / 2) as usize;
            for dy in 2..=8 {
                let py = menu.rect.bottom as usize + dy;
                assert_eq!(slice[py * 320 + mcx], bg_px, "unexpected shadow below context menu at dy={dy}");
            }
        }
        target.release();
    }

    #[test]
    fn icon_cache_trim_icons_respects_budget() {
        let mut cache = super::IconCache::new();
        let px = crate::ui::IconPixels {
            w: 192,
            h: 192,
            px: std::sync::Arc::new(vec![255u8; 192 * 192 * 4]),
        };
        for i in 0..8 {
            cache.insert(format!("k{i}"), px.clone());
        }
        cache.trim_icons_to(300 * 1024);
        let kept = (0..8).filter(|i| cache.contains(&format!("k{i}"))).count();
        assert_eq!(kept, 2);
    }
}
