//! 设置窗口：左侧多标签导航（常规设置 / 图标管理 / 扫描刷新 / 鼠标手势 / 关于），
//! 所有修改实时生效并持久化，支持大类分组 + 右侧树形展开管理应用、网格行列与图标档位调节。

mod about;

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect, SetBkColor,
    SetTextColor, HBRUSH, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, MOD_NOREPEAT, MOD_SHIFT, TME_LEAVE,
    TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{App, WM_APP_SCAN, WM_APP_UPDATE_EXIT};
use crate::catalog::{self, AppEntry, ExpiredItem};
use crate::config::{
    clamp_gesture_threshold, snap_icon_size_dp, snap_scan_interval_secs, DEFAULT_GESTURE_THRESHOLD,
    GESTURE_THRESHOLD_STEP, ICON_ROUNDNESS_PRESETS, ICON_SIZE_PRESETS, MAX_GESTURE_THRESHOLD,
    MIN_GESTURE_THRESHOLD, SCAN_INTERVAL_PRESETS,
};
use crate::hotkey::{self, HotkeySpec};
use crate::icons::IconWorker;
use crate::startup;
use crate::ui::{
    draw_button, draw_checkbox, draw_gesture_glyph, draw_scrollbar, draw_text,
    draw_toggle, draw_tooltip, make_font, measure_text, rgb, BtnStyle, CheckState, DibTarget,
    GestureGlyph, Surface, TextAlign,
};
use crate::win32::*;

const TIMER_TOOLTIP: usize = 201;
const TIMER_UPDATE: usize = 202;
const ID_FOLDER_SEARCH: i32 = 3001;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Folders,
    Refresh,
    Gestures,
    About,
}

#[derive(Clone)]
struct FolderGroup {
    id: String,
    name: String,
    default_on: bool,
    is_manual: bool,
    entries: Vec<AppEntry>,
}

/// 左侧聚合大类：将数十个零散子目录收拢为固定清晰的顶层分类，子文件夹移至右侧树形列表。
#[derive(Clone)]
struct CategoryGroup {
    id: &'static str,
    name: &'static str,
    is_tree: bool,
    is_manual: bool,
    subfolders: Vec<FolderGroup>,
}

/// 右侧树形清单展平后的可视行。
#[derive(Clone)]
enum TreeRow {
    Folder { sub_idx: usize },
    App { sub_idx: usize, entry_idx: usize, indented: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    DragBar,
    Close,
    Tab(Tab),
    Rescan,
    CheckExpired,
    // 常规设置
    HkModify,
    HkDefault,
    ToggleStartup,
    ToggleWallpaper,
    IconPreset(usize),
    RoundnessPreset(usize),
    ColsAuto,
    ColsDec,
    ColsInc,
    RowsAuto,
    RowsDec,
    RowsInc,
    LayoutReset,
    // 应用与文件夹
    FolderResetDefault,
    FolderSelectAll,
    FolderSearchBox,
    CategoryRow(usize),
    CategoryCheck(usize),
    AppSelectAllInCat,
    TreeExpandAll,
    TreeRowItem(usize),
    TreeRowCheck(usize),
    TreeRowToggle(usize),
    AppRemoveManual(usize),
    AddManualBtn,
    ExpiredBack,
    ExpiredDeleteAll,
    ExpiredDeleteOne(usize),
    ExpiredRow(usize),
    // 扫描刷新
    RefreshIntervalPreset(usize),
    ToggleScanOnShow,
    ToggleEventWatch,
    ToggleRefreshChangedIcons,
    RebuildIconsCache,
    RefreshReset,
    // 鼠标手势
    ToggleGesture,
    ToggleGestureSwap,
    GestureThreshDef,
    GestureThreshDec,
    GestureThreshInc,
    GestureThreshVal,
    GestureCard(usize),
    // 关于
    About(about::Action),
}

struct Dlg {
    app: *mut App,
    hwnd: HWND,
    scale: f32,
    target: DibTarget,
    search_edit: HWND,
    search_brush: HBRUSH,
    font_title: HFONT,
    font_about_title: HFONT,
    font_tab: HFONT,
    font_section: HFONT,
    font_body: HFONT,
    font_sub: HFONT,
    font_btn: HFONT,
    font_edit: HFONT,
    font_tip: HFONT,
    tab: Tab,
    start_with_windows: bool,
    capture_hk: bool,
    hk_msg: Option<String>,
    categories: Vec<CategoryGroup>,
    selected_cat: usize,
    expanded_folders: HashSet<String>,
    tree_rows: Vec<TreeRow>,
    cat_scroll: usize,
    app_scroll: usize,
    show_expired: bool,
    expired_list: Vec<ExpiredItem>,
    expired_scroll: usize,
    expired_status: Option<String>,
    about: about::State,
    hover: Hit,
    pressed: Hit,
    tip_pending: Option<(Hit, RECT, String)>,
    tip_visible: Option<(RECT, String)>,
}

struct Layout {
    w: i32,
    h: i32,
    sidebar_w: i32,
    header_h: i32,
    footer_h: i32,
    close_btn: RECT,
    tabs: [(Tab, RECT, &'static str); 5],
    rescan_btn: RECT,
    expired_btn: RECT,
    // Tab General
    hk_card: RECT,
    hk_badge: RECT,
    hk_mod_btn: RECT,
    hk_def_btn: RECT,
    sys_card: RECT,
    startup_row: RECT,
    startup_toggle: RECT,
    wallpaper_row: RECT,
    wallpaper_toggle: RECT,
    grid_card: RECT,
    icon_row: RECT,
    icon_presets: [RECT; 5],
    round_row: RECT,
    round_presets: [RECT; 3],
    cols_row: RECT,
    cols_auto_btn: RECT,
    cols_dec_btn: RECT,
    cols_val_rc: RECT,
    cols_inc_btn: RECT,
    rows_row: RECT,
    rows_auto_btn: RECT,
    rows_dec_btn: RECT,
    rows_val_rc: RECT,
    rows_inc_btn: RECT,
    layout_reset_btn: RECT,
    // Tab Folders
    left_pane: RECT,
    right_pane: RECT,
    search_box: RECT,
    search_edit_rc: RECT,
    btn_reset_def: RECT,
    btn_all_folders: RECT,
    folder_list_rc: RECT,
    folder_scroll_rc: RECT,
    folder_row_h: i32,
    folder_vis: usize,
    right_header: RECT,
    btn_folder_all: RECT,
    btn_tree_expand: RECT,
    btn_add_manual: RECT,
    btn_exp_back: RECT,
    btn_exp_del_all: RECT,
    app_list_rc: RECT,
    app_scroll_rc: RECT,
    app_row_h: i32,
    app_vis: usize,
    exp_row_h: i32,
    exp_vis: usize,
    // Tab Gestures
    gesture_card: RECT,
    gesture_row1: RECT,
    gesture_toggle: RECT,
    gesture_row2: RECT,
    gesture_swap_toggle: RECT,
    gesture_row3: RECT,
    gesture_thresh_def_btn: RECT,
    gesture_thresh_dec_btn: RECT,
    gesture_thresh_val_rc: RECT,
    gesture_thresh_inc_btn: RECT,
    gesture_cards: [RECT; 5],
    // Tab Refresh
    refresh_timer_card: RECT,
    refresh_interval_row: RECT,
    refresh_interval_presets: [RECT; 6],
    refresh_on_show_row: RECT,
    refresh_on_show_toggle: RECT,
    refresh_event_card: RECT,
    refresh_event_row: RECT,
    refresh_event_toggle: RECT,
    refresh_icons_row: RECT,
    refresh_icons_toggle: RECT,
    refresh_status_card: RECT,
    refresh_rebuild_btn: RECT,
    refresh_reset_btn: RECT,
    // Tab About
    about: about::Layout,
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

impl Dlg {
    fn rebuild_fonts(&mut self) {
        self.destroy_fonts();
        let s = |px: f32| (px * self.scale).round() as i32;
        self.font_title = make_font(s(14.5), 600);
        self.font_about_title = make_font(s(25.0), 650);
        self.font_tab = make_font(s(13.5), 500);
        self.font_section = make_font(s(13.5), 600);
        self.font_body = make_font(s(13.0), 400);
        self.font_sub = make_font(s(11.5), 400);
        self.font_btn = make_font(s(13.0), 600);
        self.font_edit = make_font(s(12.5), 400);
        self.font_tip = make_font(s(12.0), 400);
        if !self.search_edit.is_invalid() {
            set_control_font(self.search_edit, self.font_edit);
        }
    }

    fn destroy_fonts(&mut self) {
        unsafe {
            for f in [
                self.font_title,
                self.font_about_title,
                self.font_tab,
                self.font_section,
                self.font_body,
                self.font_sub,
                self.font_btn,
                self.font_edit,
                self.font_tip,
            ] {
                if !f.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(f.0));
                }
            }
        }
        self.font_title = HFONT::default();
        self.font_about_title = HFONT::default();
    }

    fn layout(&self) -> Layout {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let s = |v: f32| (v * self.scale).round() as i32;

        let sidebar_w = s(172.0);
        let header_h = s(48.0);
        let footer_h = s(54.0);

        let close_sz = s(28.0);
        let close_btn = RECT {
            left: w - s(14.0) - close_sz,
            top: (header_h - close_sz) / 2,
            right: w - s(14.0),
            bottom: (header_h + close_sz) / 2,
        };

        let tab_h = s(38.0);
        let tab_gap = s(6.0);
        let tab_top0 = header_h + s(10.0);
        let tabs = [
            (Tab::General, RECT { left: s(12.0), top: tab_top0, right: sidebar_w - s(12.0), bottom: tab_top0 + tab_h }, "常规设置"),
            (Tab::Folders, RECT { left: s(12.0), top: tab_top0 + (tab_h + tab_gap), right: sidebar_w - s(12.0), bottom: tab_top0 + tab_h * 2 + tab_gap }, "图标管理"),
            (Tab::Refresh, RECT { left: s(12.0), top: tab_top0 + (tab_h + tab_gap) * 2, right: sidebar_w - s(12.0), bottom: tab_top0 + tab_h * 3 + tab_gap * 2 }, "扫描刷新"),
            (Tab::Gestures, RECT { left: s(12.0), top: tab_top0 + (tab_h + tab_gap) * 3, right: sidebar_w - s(12.0), bottom: tab_top0 + tab_h * 4 + tab_gap * 3 }, "鼠标手势"),
            (Tab::About, RECT { left: s(12.0), top: tab_top0 + (tab_h + tab_gap) * 4, right: sidebar_w - s(12.0), bottom: tab_top0 + tab_h * 5 + tab_gap * 4 }, "关于"),
        ];

        let fy = h - footer_h + (footer_h - s(32.0)) / 2;
        let rescan_btn = RECT { left: sidebar_w + s(22.0), top: fy, right: sidebar_w + s(134.0), bottom: fy + s(32.0) };
        let expired_btn = RECT { left: rescan_btn.right + s(10.0), top: fy, right: rescan_btn.right + s(10.0) + s(116.0), bottom: fy + s(32.0) };

        let content = RECT {
            left: sidebar_w + s(22.0),
            top: header_h + s(14.0),
            right: w - s(22.0),
            bottom: h - footer_h - s(14.0),
        };
        let cw = content.right - content.left;

        // Tab General（单行快捷键无 presets、两行系统开关、四行图标/圆角档位与网格行列设置）
        let hk_card = RECT { left: content.left, top: content.top, right: content.right, bottom: content.top + s(64.0) };
        let hk_def_btn = RECT { left: hk_card.right - s(16.0) - s(80.0), top: hk_card.top + s(16.0), right: hk_card.right - s(16.0), bottom: hk_card.bottom - s(16.0) };
        let hk_mod_btn = RECT { left: hk_def_btn.left - s(8.0) - s(76.0), top: hk_card.top + s(16.0), right: hk_def_btn.left - s(8.0), bottom: hk_card.bottom - s(16.0) };
        let hk_badge = RECT { left: hk_card.left + s(116.0), top: hk_card.top + s(16.0), right: hk_mod_btn.left - s(10.0), bottom: hk_card.bottom - s(16.0) };

        let sys_card = RECT { left: content.left, top: hk_card.bottom + s(10.0), right: content.right, bottom: hk_card.bottom + s(10.0) + s(96.0) };
        let startup_row = RECT { left: sys_card.left, top: sys_card.top, right: sys_card.right, bottom: sys_card.top + s(48.0) };
        let wallpaper_row = RECT { left: sys_card.left, top: sys_card.top + s(48.0), right: sys_card.right, bottom: sys_card.bottom };
        let tw = s(44.0);
        let th = s(24.0);
        let startup_toggle = RECT { left: sys_card.right - s(16.0) - tw, top: startup_row.top + (s(48.0) - th) / 2, right: sys_card.right - s(16.0), bottom: startup_row.top + (s(48.0) + th) / 2 };
        let wallpaper_toggle = RECT { left: sys_card.right - s(16.0) - tw, top: wallpaper_row.top + (s(48.0) - th) / 2, right: sys_card.right - s(16.0), bottom: wallpaper_row.top + (s(48.0) + th) / 2 };

        let grh = s(48.0);
        let grid_card = RECT { left: content.left, top: sys_card.bottom + s(10.0), right: content.right, bottom: sys_card.bottom + s(10.0) + grh * 4 };
        let icon_row = RECT { left: grid_card.left, top: grid_card.top, right: grid_card.right, bottom: grid_card.top + grh };
        let round_row = RECT { left: grid_card.left, top: grid_card.top + grh, right: grid_card.right, bottom: grid_card.top + grh * 2 };
        let cols_row = RECT { left: grid_card.left, top: grid_card.top + grh * 2, right: grid_card.right, bottom: grid_card.top + grh * 3 };
        let rows_row = RECT { left: grid_card.left, top: grid_card.top + grh * 3, right: grid_card.right, bottom: grid_card.bottom };

        // 5 档图标大小按钮（极小 / 偏小 / 标准 / 偏大 / 超大）
        let pw = s(56.0);
        let pgap = s(6.0);
        let p_start = grid_card.right - s(16.0) - (pw * 5 + pgap * 4);
        let mut icon_presets = [RECT::default(); 5];
        for (i, slot) in icon_presets.iter_mut().enumerate() {
            let x = p_start + i as i32 * (pw + pgap);
            *slot = RECT {
                left: x,
                top: icon_row.top + s(9.0),
                right: x + pw,
                bottom: icon_row.bottom - s(9.0),
            };
        }

        // 3 档图标圆角程度按钮（微圆角 / 标准 / 圆润）
        let rw = s(68.0);
        let r_start = grid_card.right - s(16.0) - (rw * 3 + pgap * 2);
        let mut round_presets = [RECT::default(); 3];
        for (i, slot) in round_presets.iter_mut().enumerate() {
            let x = r_start + i as i32 * (rw + pgap);
            *slot = RECT {
                left: x,
                top: round_row.top + s(9.0),
                right: x + rw,
                bottom: round_row.bottom - s(9.0),
            };
        }

        // 列数 / 行数步进控件及右侧统一「恢复默认」按钮
        let layout_reset_btn = RECT {
            left: grid_card.right - s(16.0) - s(76.0),
            top: rows_row.top + s(9.0),
            right: grid_card.right - s(16.0),
            bottom: rows_row.bottom - s(9.0),
        };
        let step_right = layout_reset_btn.left - s(12.0);
        let inc_w = s(28.0);
        let val_w = s(92.0);
        let dec_w = s(28.0);
        let auto_w = s(48.0);

        let cols_inc_btn = RECT { left: step_right - inc_w, top: cols_row.top + s(9.0), right: step_right, bottom: cols_row.bottom - s(9.0) };
        let cols_val_rc = RECT { left: cols_inc_btn.left - s(4.0) - val_w, top: cols_row.top + s(9.0), right: cols_inc_btn.left - s(4.0), bottom: cols_row.bottom - s(9.0) };
        let cols_dec_btn = RECT { left: cols_val_rc.left - s(4.0) - dec_w, top: cols_row.top + s(9.0), right: cols_val_rc.left - s(4.0), bottom: cols_row.bottom - s(9.0) };
        let cols_auto_btn = RECT { left: cols_dec_btn.left - s(10.0) - auto_w, top: cols_row.top + s(9.0), right: cols_dec_btn.left - s(10.0), bottom: cols_row.bottom - s(9.0) };

        let rows_inc_btn = RECT { left: step_right - inc_w, top: rows_row.top + s(9.0), right: step_right, bottom: rows_row.bottom - s(9.0) };
        let rows_val_rc = RECT { left: rows_inc_btn.left - s(4.0) - val_w, top: rows_row.top + s(9.0), right: rows_inc_btn.left - s(4.0), bottom: rows_row.bottom - s(9.0) };
        let rows_dec_btn = RECT { left: rows_val_rc.left - s(4.0) - dec_w, top: rows_row.top + s(9.0), right: rows_val_rc.left - s(4.0), bottom: rows_row.bottom - s(9.0) };
        let rows_auto_btn = RECT { left: rows_dec_btn.left - s(10.0) - auto_w, top: rows_row.top + s(9.0), right: rows_dec_btn.left - s(10.0), bottom: rows_row.bottom - s(9.0) };

        // Tab Folders
        let lw = s(226.0).min(cw / 2 - s(12.0));
        let left_pane = RECT { left: content.left, top: content.top, right: content.left + lw, bottom: content.bottom };
        let right_pane = RECT { left: left_pane.right + s(14.0), top: content.top, right: content.right, bottom: content.bottom };
        let search_box = RECT { left: left_pane.left + s(10.0), top: left_pane.top + s(10.0), right: left_pane.right - s(10.0), bottom: left_pane.top + s(40.0) };
        let search_edit_rc = RECT { left: search_box.left + s(28.0), top: search_box.top + s(5.0), right: search_box.right - s(8.0), bottom: search_box.bottom - s(4.0) };
        let qbw = (lw - s(28.0)) / 2;
        let btn_all_folders = RECT { left: left_pane.left + s(10.0), top: search_box.bottom + s(8.0), right: left_pane.left + s(10.0) + qbw, bottom: search_box.bottom + s(34.0) };
        let btn_reset_def = RECT { left: btn_all_folders.right + s(8.0), top: btn_all_folders.top, right: left_pane.right - s(10.0), bottom: btn_all_folders.bottom };
        let folder_list_rc = RECT { left: left_pane.left + s(6.0), top: btn_all_folders.bottom + s(8.0), right: left_pane.right - s(12.0), bottom: left_pane.bottom - s(8.0) };
        let folder_scroll_rc = RECT { left: left_pane.right - s(9.0), top: folder_list_rc.top, right: left_pane.right - s(3.0), bottom: folder_list_rc.bottom };
        let folder_row_h = s(38.0);
        let folder_vis = ((folder_list_rc.bottom - folder_list_rc.top) / folder_row_h).max(1) as usize;

        let right_header = RECT { left: right_pane.left, top: right_pane.top, right: right_pane.right, bottom: right_pane.top + s(44.0) };
        let btn_folder_all = RECT { left: right_header.right - s(14.0) - s(76.0), top: right_header.top + s(8.0), right: right_header.right - s(14.0), bottom: right_header.bottom - s(8.0) };
        let btn_tree_expand = RECT { left: btn_folder_all.left - s(8.0) - s(76.0), top: right_header.top + s(8.0), right: btn_folder_all.left - s(8.0), bottom: right_header.bottom - s(8.0) };
        let btn_add_manual = RECT { left: right_header.right - s(14.0) - s(98.0), top: right_header.top + s(8.0), right: right_header.right - s(14.0), bottom: right_header.bottom - s(8.0) };
        let btn_exp_del_all = RECT { left: right_header.right - s(14.0) - s(104.0), top: right_header.top + s(8.0), right: right_header.right - s(14.0), bottom: right_header.bottom - s(8.0) };
        let btn_exp_back = RECT { left: btn_exp_del_all.left - s(8.0) - s(64.0), top: right_header.top + s(8.0), right: btn_exp_del_all.left - s(8.0), bottom: right_header.bottom - s(8.0) };
        let app_list_rc = RECT { left: right_pane.left + s(8.0), top: right_header.bottom + s(6.0), right: right_pane.right - s(14.0), bottom: right_pane.bottom - s(8.0) };
        let app_scroll_rc = RECT { left: right_pane.right - s(9.0), top: app_list_rc.top, right: right_pane.right - s(3.0), bottom: app_list_rc.bottom };
        let app_row_h = s(38.0);
        let app_vis = ((app_list_rc.bottom - app_list_rc.top) / app_row_h).max(1) as usize;
        let exp_row_h = s(46.0);
        let exp_vis = ((app_list_rc.bottom - app_list_rc.top) / exp_row_h).max(1) as usize;

        // Tab Gestures（手势开关、方向反转、识别匹配度步进调节与下方五张只读手势卡）
        let g_rh = s(44.0);
        let gesture_card = RECT { left: content.left, top: content.top, right: content.right, bottom: content.top + g_rh * 3 };
        let gesture_row1 = RECT { left: gesture_card.left, top: gesture_card.top, right: gesture_card.right, bottom: gesture_card.top + g_rh };
        let gesture_row2 = RECT { left: gesture_card.left, top: gesture_card.top + g_rh, right: gesture_card.right, bottom: gesture_card.top + g_rh * 2 };
        let gesture_row3 = RECT { left: gesture_card.left, top: gesture_card.top + g_rh * 2, right: gesture_card.right, bottom: gesture_card.bottom };
        let gesture_toggle = RECT { left: gesture_card.right - s(18.0) - tw, top: gesture_row1.top + (g_rh - th) / 2, right: gesture_card.right - s(18.0), bottom: gesture_row1.top + (g_rh + th) / 2 };
        let gesture_swap_toggle = RECT { left: gesture_card.right - s(18.0) - tw, top: gesture_row2.top + (g_rh - th) / 2, right: gesture_card.right - s(18.0), bottom: gesture_row2.top + (g_rh + th) / 2 };

        let g_step_right = gesture_card.right - s(18.0);
        let g_val_w = s(76.0);
        let gesture_thresh_inc_btn = RECT { left: g_step_right - inc_w, top: gesture_row3.top + s(8.0), right: g_step_right, bottom: gesture_row3.bottom - s(8.0) };
        let gesture_thresh_val_rc = RECT { left: gesture_thresh_inc_btn.left - s(4.0) - g_val_w, top: gesture_row3.top + s(8.0), right: gesture_thresh_inc_btn.left - s(4.0), bottom: gesture_row3.bottom - s(8.0) };
        let gesture_thresh_dec_btn = RECT { left: gesture_thresh_val_rc.left - s(4.0) - dec_w, top: gesture_row3.top + s(8.0), right: gesture_thresh_val_rc.left - s(4.0), bottom: gesture_row3.bottom - s(8.0) };
        let gesture_thresh_def_btn = RECT { left: gesture_thresh_dec_btn.left - s(10.0) - auto_w, top: gesture_row3.top + s(8.0), right: gesture_thresh_dec_btn.left - s(10.0), bottom: gesture_row3.bottom - s(8.0) };

        // 预留卡片底部只读快捷键徽标高度，并保持上下卡片间距均衡
        let g_top = gesture_card.bottom + s(12.0);
        let g_gap = s(10.0);
        let card_w3 = (cw - g_gap * 2) / 3;
        let card_h = s(136.0);
        let g0 = RECT { left: content.left, top: g_top, right: content.left + card_w3, bottom: g_top + card_h };
        let g1 = RECT { left: g0.right + g_gap, top: g_top, right: g0.right + g_gap + card_w3, bottom: g_top + card_h };
        let g2 = RECT { left: g1.right + g_gap, top: g_top, right: content.right, bottom: g_top + card_h };
        let g_top2 = g_top + card_h + g_gap;
        let g3 = RECT { left: content.left, top: g_top2, right: content.left + card_w3, bottom: g_top2 + card_h };
        let g4 = RECT { left: g3.right + g_gap, top: g_top2, right: g3.right + g_gap + card_w3, bottom: g_top2 + card_h };
        let gesture_cards = [g0, g1, g2, g3, g4];

        // Tab Refresh（定时扫描间隔 6 档 + 呼出刷新 + 事件驱动监听 + 图标缓存同步 + 运行状态）
        let r_rh = s(48.0);
        let refresh_timer_card = RECT { left: content.left, top: content.top, right: content.right, bottom: content.top + r_rh * 2 };
        let refresh_interval_row = RECT { left: refresh_timer_card.left, top: refresh_timer_card.top, right: refresh_timer_card.right, bottom: refresh_timer_card.top + r_rh };
        let refresh_on_show_row = RECT { left: refresh_timer_card.left, top: refresh_timer_card.top + r_rh, right: refresh_timer_card.right, bottom: refresh_timer_card.bottom };
        let refresh_on_show_toggle = RECT { left: refresh_timer_card.right - s(16.0) - tw, top: refresh_on_show_row.top + (r_rh - th) / 2, right: refresh_timer_card.right - s(16.0), bottom: refresh_on_show_row.top + (r_rh + th) / 2 };

        let spw = s(56.0);
        let sp_start = refresh_timer_card.right - s(16.0) - (spw * 6 + pgap * 5);
        let mut refresh_interval_presets = [RECT::default(); 6];
        for (i, slot) in refresh_interval_presets.iter_mut().enumerate() {
            let x = sp_start + i as i32 * (spw + pgap);
            *slot = RECT {
                left: x,
                top: refresh_interval_row.top + s(9.0),
                right: x + spw,
                bottom: refresh_interval_row.bottom - s(9.0),
            };
        }

        let refresh_event_card = RECT { left: content.left, top: refresh_timer_card.bottom + s(10.0), right: content.right, bottom: refresh_timer_card.bottom + s(10.0) + r_rh * 2 };
        let refresh_event_row = RECT { left: refresh_event_card.left, top: refresh_event_card.top, right: refresh_event_card.right, bottom: refresh_event_card.top + r_rh };
        let refresh_icons_row = RECT { left: refresh_event_card.left, top: refresh_event_card.top + r_rh, right: refresh_event_card.right, bottom: refresh_event_card.bottom };
        let refresh_event_toggle = RECT { left: refresh_event_card.right - s(16.0) - tw, top: refresh_event_row.top + (r_rh - th) / 2, right: refresh_event_card.right - s(16.0), bottom: refresh_event_row.top + (r_rh + th) / 2 };
        let refresh_icons_toggle = RECT { left: refresh_event_card.right - s(16.0) - tw, top: refresh_icons_row.top + (r_rh - th) / 2, right: refresh_event_card.right - s(16.0), bottom: refresh_icons_row.top + (r_rh + th) / 2 };

        let refresh_status_card = RECT { left: content.left, top: refresh_event_card.bottom + s(10.0), right: content.right, bottom: refresh_event_card.bottom + s(10.0) + s(56.0) };
        let refresh_reset_btn = RECT { left: refresh_status_card.right - s(16.0) - s(76.0), top: refresh_status_card.top + s(13.0), right: refresh_status_card.right - s(16.0), bottom: refresh_status_card.bottom - s(13.0) };
        let refresh_rebuild_btn = RECT { left: refresh_reset_btn.left - s(8.0) - s(104.0), top: refresh_status_card.top + s(13.0), right: refresh_reset_btn.left - s(8.0), bottom: refresh_status_card.bottom - s(13.0) };

        let about = about::Layout::new(content, rescan_btn, self.scale);

        Layout {
            w, h, sidebar_w, header_h, footer_h, close_btn, tabs, rescan_btn, expired_btn,
            hk_card, hk_badge, hk_mod_btn, hk_def_btn, sys_card,
            startup_row, startup_toggle, wallpaper_row, wallpaper_toggle,
            grid_card, icon_row, icon_presets, round_row, round_presets, cols_row, cols_auto_btn, cols_dec_btn, cols_val_rc, cols_inc_btn,
            rows_row, rows_auto_btn, rows_dec_btn, rows_val_rc, rows_inc_btn, layout_reset_btn,
            left_pane, right_pane, search_box, search_edit_rc, btn_reset_def, btn_all_folders,
            folder_list_rc, folder_scroll_rc, folder_row_h, folder_vis, right_header,
            btn_folder_all, btn_tree_expand, btn_add_manual, btn_exp_back, btn_exp_del_all,
            app_list_rc, app_scroll_rc, app_row_h, app_vis, exp_row_h, exp_vis,
            gesture_card, gesture_row1, gesture_toggle, gesture_row2, gesture_swap_toggle,
            gesture_row3, gesture_thresh_def_btn, gesture_thresh_dec_btn, gesture_thresh_val_rc, gesture_thresh_inc_btn,
            gesture_cards,
            refresh_timer_card, refresh_interval_row, refresh_interval_presets, refresh_on_show_row, refresh_on_show_toggle,
            refresh_event_card, refresh_event_row, refresh_event_toggle, refresh_icons_row, refresh_icons_toggle,
            refresh_status_card, refresh_rebuild_btn, refresh_reset_btn,
            about,
        }
    }

    /// 聚合生成固定的 5 个顶层大类，零散软件子文件夹收进右侧可折叠树形面板。
    fn rebuild_groups(&mut self, app: &App) {
        let mut by_id: HashMap<String, FolderGroup> = HashMap::new();
        for e in &app.scan.entries {
            let (fid, fname) = catalog::resolve_folder(e);
            let default_on = catalog::is_folder_default_enabled(&fid, &fname);
            let g = by_id.entry(fid.clone()).or_insert_with(|| FolderGroup {
                id: fid,
                name: fname,
                default_on,
                is_manual: false,
                entries: Vec::new(),
            });
            g.entries.push(e.clone());
        }

        let mut start_subs = Vec::new();
        let mut store_subs = Vec::new();
        let mut third_party_subs = Vec::new();
        let mut sys_dev_subs = Vec::new();

        for mut g in by_id.into_values() {
            g.entries.sort_by(|a, b| catalog::cmp_names(&a.name, &b.name));
            if g.id == "start_root" {
                start_subs.push(g);
            } else if g.id == "apps_store" {
                store_subs.push(g);
            } else if g.id == "apps_system" || g.id == "app_paths" || !g.default_on {
                sys_dev_subs.push(g);
            } else {
                third_party_subs.push(g);
            }
        }
        third_party_subs.sort_by(|a, b| catalog::cmp_names(&a.name, &b.name));
        sys_dev_subs.sort_by(|a, b| {
            let rank = |g: &FolderGroup| -> u8 {
                if g.id == "apps_system" { 0 } else if g.id == "app_paths" { 1 } else { 2 }
            };
            rank(a).cmp(&rank(b)).then_with(|| catalog::cmp_names(&a.name, &b.name))
        });

        let mut manual_entries = app.settings.manual.clone();
        manual_entries.sort_by(|a, b| catalog::cmp_names(&a.name, &b.name));
        let manual_sub = FolderGroup {
            id: "manual".into(),
            name: "手动添加的应用".into(),
            default_on: true,
            is_manual: true,
            entries: manual_entries,
        };

        let prev_id = self.categories.get(self.selected_cat).map(|c| c.id);
        self.categories = vec![
            CategoryGroup { id: "start_root", name: "开始菜单常用程序", is_tree: false, is_manual: false, subfolders: start_subs },
            CategoryGroup { id: "apps_store", name: "微软商店与预装", is_tree: false, is_manual: false, subfolders: store_subs },
            CategoryGroup { id: "cat_third_party", name: "软件独立目录", is_tree: true, is_manual: false, subfolders: third_party_subs },
            CategoryGroup { id: "cat_sys_dev", name: "系统与开发工具", is_tree: true, is_manual: false, subfolders: sys_dev_subs },
            CategoryGroup { id: "manual", name: "手动添加的应用", is_tree: false, is_manual: true, subfolders: vec![manual_sub] },
        ];
        if let Some(id) = prev_id {
            if let Some(idx) = self.categories.iter().position(|c| c.id == id) {
                self.selected_cat = idx;
            } else {
                self.selected_cat = 0;
            }
        } else {
            self.selected_cat = 0;
        }
        self.rebuild_tree();
    }

    fn search_query(&self) -> String {
        if self.search_edit.is_invalid() {
            return String::new();
        }
        unsafe {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(self.search_edit, &mut buf);
            from_wide(&buf[..n as usize])
        }
    }

    fn category_matches_query(&self, cat: &CategoryGroup, q: &str) -> bool {
        if q.is_empty() {
            return true;
        }
        cat.name.to_lowercase().contains(q)
            || cat.subfolders.iter().any(|g| {
                g.name.to_lowercase().contains(q)
                    || g.entries.iter().any(|e| e.name.to_lowercase().contains(q))
            })
    }

    /// 按当前选中的大类与筛选词重建右侧树形可视行列表。
    fn rebuild_tree(&mut self) {
        self.tree_rows.clear();
        let q = self.search_query().trim().to_lowercase();
        let Some(cat) = self.categories.get(self.selected_cat) else { return };

        if !cat.is_tree {
            for (si, sub) in cat.subfolders.iter().enumerate() {
                for (ei, e) in sub.entries.iter().enumerate() {
                    if q.is_empty()
                        || e.name.to_lowercase().contains(&q)
                        || sub.name.to_lowercase().contains(&q)
                    {
                        self.tree_rows.push(TreeRow::App {
                            sub_idx: si,
                            entry_idx: ei,
                            indented: false,
                        });
                    }
                }
            }
        } else {
            for (si, sub) in cat.subfolders.iter().enumerate() {
                if sub.entries.is_empty() {
                    continue;
                }
                let folder_hit = !q.is_empty() && sub.name.to_lowercase().contains(&q);
                let matched_entries: Vec<usize> = sub
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| q.is_empty() || folder_hit || e.name.to_lowercase().contains(&q))
                    .map(|(i, _)| i)
                    .collect();
                if !q.is_empty() && matched_entries.is_empty() {
                    continue;
                }
                self.tree_rows.push(TreeRow::Folder { sub_idx: si });
                let is_open = !q.is_empty() || self.expanded_folders.contains(&sub.id);
                if is_open {
                    for ei in matched_entries {
                        self.tree_rows.push(TreeRow::App {
                            sub_idx: si,
                            entry_idx: ei,
                            indented: true,
                        });
                    }
                }
            }
        }
        if self.app_scroll >= self.tree_rows.len() {
            self.app_scroll = self.tree_rows.len().saturating_sub(1);
        }
    }

    fn on_search_changed(&mut self) {
        let q = self.search_query().trim().to_lowercase();
        if !q.is_empty() {
            if let Some(cur) = self.categories.get(self.selected_cat) {
                if !self.category_matches_query(cur, &q) {
                    if let Some(idx) = self.categories.iter().position(|c| self.category_matches_query(c, &q)) {
                        self.selected_cat = idx;
                    }
                }
            }
        }
        self.app_scroll = 0;
        self.rebuild_tree();
    }

    fn entry_checked(&self, app: &App, g: &FolderGroup, e: &AppEntry) -> bool {
        if g.is_manual {
            return true;
        }
        if let Some(&v) = app.settings.app_overrides.get(&e.id) {
            return v;
        }
        if let Some(&v) = app.settings.folder_overrides.get(&g.id) {
            return v;
        }
        g.default_on
    }

    fn folder_check_state(&self, app: &App, g: &FolderGroup) -> CheckState {
        if g.is_manual || g.entries.is_empty() {
            return if g.default_on { CheckState::Checked } else { CheckState::Unchecked };
        }
        let mut on = 0usize;
        for e in &g.entries {
            if self.entry_checked(app, g, e) {
                on += 1;
            }
        }
        if on == 0 {
            CheckState::Unchecked
        } else if on == g.entries.len() {
            CheckState::Checked
        } else {
            CheckState::Indeterminate
        }
    }

    fn category_check_state(&self, app: &App, cat: &CategoryGroup) -> CheckState {
        if cat.is_manual {
            return CheckState::Checked;
        }
        let mut total = 0usize;
        let mut on = 0usize;
        for g in &cat.subfolders {
            for e in &g.entries {
                total += 1;
                if self.entry_checked(app, g, e) {
                    on += 1;
                }
            }
        }
        if total == 0 {
            return CheckState::Unchecked;
        }
        if on == 0 {
            CheckState::Unchecked
        } else if on == total {
            CheckState::Checked
        } else {
            CheckState::Indeterminate
        }
    }

    fn commit_filter_settings(&mut self, app: &mut App) {
        app.store.save_settings(&app.settings);
        app.rebuild_view();
        if app.visible {
            unsafe {
                let _ = InvalidateRect(app.hwnd, None, false);
            }
        }
    }

    fn commit_layout_settings(&mut self, app: &mut App) {
        app.store.save_settings(&app.settings);
        app.apply_icon_size();
    }

    fn toggle_category(&mut self, app: &mut App, ci: usize) {
        let Some(cat) = self.categories.get(ci).cloned() else { return };
        if cat.is_manual {
            return;
        }
        let next = self.category_check_state(app, &cat) != CheckState::Checked;
        for g in &cat.subfolders {
            app.settings.folder_overrides.insert(g.id.clone(), next);
            for e in &g.entries {
                app.settings.app_overrides.remove(&e.id);
            }
        }
        self.commit_filter_settings(app);
    }

    fn toggle_subfolder(&mut self, app: &mut App, sub_idx: usize) {
        let Some(cat) = self.categories.get(self.selected_cat) else { return };
        let Some(g) = cat.subfolders.get(sub_idx).cloned() else { return };
        if g.is_manual {
            return;
        }
        let next = self.folder_check_state(app, &g) != CheckState::Checked;
        app.settings.folder_overrides.insert(g.id.clone(), next);
        for e in &g.entries {
            app.settings.app_overrides.remove(&e.id);
        }
        self.commit_filter_settings(app);
    }

    fn toggle_entry(&mut self, app: &mut App, sub_idx: usize, ei: usize) {
        let Some(cat) = self.categories.get(self.selected_cat) else { return };
        let Some(g) = cat.subfolders.get(sub_idx) else { return };
        let Some(e) = g.entries.get(ei) else { return };
        if g.is_manual {
            return;
        }
        let cur = self.entry_checked(app, g, e);
        app.settings.app_overrides.insert(e.id.clone(), !cur);
        self.commit_filter_settings(app);
    }

    fn all_tree_expanded(&self) -> bool {
        let Some(cat) = self.categories.get(self.selected_cat) else { return false };
        if !cat.is_tree {
            return false;
        }
        cat.subfolders
            .iter()
            .filter(|g| !g.entries.is_empty())
            .all(|g| self.expanded_folders.contains(&g.id))
    }

    fn sync_search_edit_pos(&self) {
        if self.search_edit.is_invalid() {
            return;
        }
        let l = self.layout();
        unsafe {
            if self.tab == Tab::Folders {
                let r = l.search_edit_rc;
                let _ = SetWindowPos(
                    self.search_edit,
                    HWND_TOP,
                    r.left,
                    r.top,
                    (r.right - r.left).max(1),
                    (r.bottom - r.top).max(1),
                    SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOZORDER,
                );
            } else {
                let _ = ShowWindow(self.search_edit, SW_HIDE);
            }
        }
    }

    fn request_visible_app_icons(&self, app: &mut App, l: &Layout) {
        if self.tab != Tab::Folders {
            return;
        }
        let Some(cat) = self.categories.get(self.selected_cat) else { return };
        for vi in 0..l.app_vis {
            let idx = self.app_scroll + vi;
            if let Some(TreeRow::App { sub_idx, entry_idx, .. }) = self.tree_rows.get(idx) {
                if let Some(e) = cat.subfolders.get(*sub_idx).and_then(|g| g.entries.get(*entry_idx)) {
                    app.request_icon_async(&e.icon_key);
                }
            }
        }
    }

    fn cat_row_rects(&self, l: &Layout, vi: usize) -> (RECT, RECT) {
        let top = l.folder_list_rc.top + vi as i32 * l.folder_row_h;
        let row = RECT {
            left: l.folder_list_rc.left,
            top,
            right: l.folder_list_rc.right,
            bottom: (top + l.folder_row_h - (2.0 * self.scale).round() as i32).min(l.folder_list_rc.bottom),
        };
        let cb_sz = (16.0 * self.scale).round() as i32;
        let cy = (row.top + row.bottom) / 2;
        let cb = RECT {
            left: row.left + (8.0 * self.scale).round() as i32,
            top: cy - cb_sz / 2,
            right: row.left + (8.0 * self.scale).round() as i32 + cb_sz,
            bottom: cy + cb_sz / 2,
        };
        (row, cb)
    }

    /// 返回 `(整行区域, 折叠箭头/操作区域, 复选框区域)`。
    fn tree_row_rects(&self, l: &Layout, vi: usize, indented: bool) -> (RECT, RECT, RECT) {
        let top = l.app_list_rc.top + vi as i32 * l.app_row_h;
        let row = RECT {
            left: l.app_list_rc.left,
            top,
            right: l.app_list_rc.right,
            bottom: (top + l.app_row_h - (3.0 * self.scale).round() as i32).min(l.app_list_rc.bottom),
        };
        let cy = (row.top + row.bottom) / 2;
        let indent = if indented { (22.0 * self.scale).round() as i32 } else { 0 };
        let arrow = RECT {
            left: row.left + (6.0 * self.scale).round() as i32 + indent,
            top: cy - (11.0 * self.scale).round() as i32,
            right: row.left + (24.0 * self.scale).round() as i32 + indent,
            bottom: cy + (11.0 * self.scale).round() as i32,
        };
        let cb_sz = (16.0 * self.scale).round() as i32;
        let cb_x = row.left + (10.0 * self.scale).round() as i32 + indent + if indented { 0 } else { (16.0 * self.scale).round() as i32 };
        let cb = RECT { left: cb_x, top: cy - cb_sz / 2, right: cb_x + cb_sz, bottom: cy + cb_sz / 2 };
        (row, arrow, cb)
    }

    fn manual_remove_rect(&self, row: &RECT) -> RECT {
        let sz = (20.0 * self.scale).round() as i32;
        let cy = (row.top + row.bottom) / 2;
        RECT {
            left: row.right - sz - (10.0 * self.scale).round() as i32,
            top: cy - sz / 2,
            right: row.right - (10.0 * self.scale).round() as i32,
            bottom: cy + sz / 2,
        }
    }

    fn exp_row_rects(&self, l: &Layout, vi: usize) -> (RECT, RECT) {
        let top = l.app_list_rc.top + vi as i32 * l.exp_row_h;
        let row = RECT {
            left: l.app_list_rc.left,
            top,
            right: l.app_list_rc.right,
            bottom: (top + l.exp_row_h - (4.0 * self.scale).round() as i32).min(l.app_list_rc.bottom),
        };
        let bw = (54.0 * self.scale).round() as i32;
        let bh = (26.0 * self.scale).round() as i32;
        let cy = (row.top + row.bottom) / 2;
        let del = RECT {
            left: row.right - bw - (8.0 * self.scale).round() as i32,
            top: cy - bh / 2,
            right: row.right - (8.0 * self.scale).round() as i32,
            bottom: cy + bh / 2,
        };
        (row, del)
    }

    fn hit_test(&self, l: &Layout, x: i32, y: i32) -> Hit {
        if inside(&l.close_btn, x, y) {
            return Hit::Close;
        }
        for (t, r, _) in &l.tabs {
            if inside(r, x, y) {
                return Hit::Tab(*t);
            }
        }
        // 关于页底部用于项目入口，其余页面继续使用原有应用管理操作。
        if self.tab != Tab::About {
            if inside(&l.rescan_btn, x, y) {
                return Hit::Rescan;
            }
            if inside(&l.expired_btn, x, y) {
                return Hit::CheckExpired;
            }
        }
        match self.tab {
            Tab::General => {
                if inside(&l.hk_mod_btn, x, y) {
                    return Hit::HkModify;
                }
                if inside(&l.hk_def_btn, x, y) {
                    return Hit::HkDefault;
                }
                if inside(&l.startup_toggle, x, y) || inside(&l.startup_row, x, y) {
                    return Hit::ToggleStartup;
                }
                if inside(&l.wallpaper_toggle, x, y) || inside(&l.wallpaper_row, x, y) {
                    return Hit::ToggleWallpaper;
                }
                for (i, r) in l.icon_presets.iter().enumerate() {
                    if inside(r, x, y) {
                        return Hit::IconPreset(i);
                    }
                }
                for (i, r) in l.round_presets.iter().enumerate() {
                    if inside(r, x, y) {
                        return Hit::RoundnessPreset(i);
                    }
                }
                if inside(&l.cols_auto_btn, x, y) {
                    return Hit::ColsAuto;
                }
                if inside(&l.cols_dec_btn, x, y) {
                    return Hit::ColsDec;
                }
                if inside(&l.cols_inc_btn, x, y) {
                    return Hit::ColsInc;
                }
                if inside(&l.layout_reset_btn, x, y) {
                    return Hit::LayoutReset;
                }
                if inside(&l.rows_auto_btn, x, y) {
                    return Hit::RowsAuto;
                }
                if inside(&l.rows_dec_btn, x, y) {
                    return Hit::RowsDec;
                }
                if inside(&l.rows_inc_btn, x, y) {
                    return Hit::RowsInc;
                }
            }
            Tab::Folders => {
                if inside(&l.search_box, x, y) {
                    return Hit::FolderSearchBox;
                }
                if inside(&l.btn_all_folders, x, y) {
                    return Hit::FolderSelectAll;
                }
                if inside(&l.btn_reset_def, x, y) {
                    return Hit::FolderResetDefault;
                }
                if inside(&l.folder_list_rc, x, y) {
                    for vi in 0..l.folder_vis {
                        let ci = self.cat_scroll + vi;
                        if ci >= self.categories.len() {
                            break;
                        }
                        let (row, cb) = self.cat_row_rects(l, vi);
                        if inside(&row, x, y) {
                            let cb_hit = RECT {
                                left: cb.left - (4.0 * self.scale) as i32,
                                top: row.top,
                                right: cb.right + (6.0 * self.scale) as i32,
                                bottom: row.bottom,
                            };
                            if !self.categories[ci].is_manual && inside(&cb_hit, x, y) {
                                return Hit::CategoryCheck(ci);
                            }
                            return Hit::CategoryRow(ci);
                        }
                    }
                }
                if self.show_expired {
                    if inside(&l.btn_exp_back, x, y) {
                        return Hit::ExpiredBack;
                    }
                    if !self.expired_list.is_empty() && inside(&l.btn_exp_del_all, x, y) {
                        return Hit::ExpiredDeleteAll;
                    }
                    if inside(&l.app_list_rc, x, y) {
                        for vi in 0..l.exp_vis {
                            let idx = self.expired_scroll + vi;
                            if idx >= self.expired_list.len() {
                                break;
                            }
                            let (row, del) = self.exp_row_rects(l, vi);
                            if inside(&row, x, y) {
                                if inside(&del, x, y) {
                                    return Hit::ExpiredDeleteOne(idx);
                                }
                                return Hit::ExpiredRow(idx);
                            }
                        }
                    }
                } else if let Some(cat) = self.categories.get(self.selected_cat) {
                    if cat.is_manual {
                        if inside(&l.btn_add_manual, x, y) {
                            return Hit::AddManualBtn;
                        }
                    } else {
                        if cat.is_tree && inside(&l.btn_tree_expand, x, y) {
                            return Hit::TreeExpandAll;
                        }
                        if inside(&l.btn_folder_all, x, y) {
                            return Hit::AppSelectAllInCat;
                        }
                    }
                    if inside(&l.app_list_rc, x, y) {
                        for vi in 0..l.app_vis {
                            let idx = self.app_scroll + vi;
                            let Some(tr) = self.tree_rows.get(idx) else { break };
                            match tr {
                                TreeRow::Folder { .. } => {
                                    let (row, _arrow, cb) = self.tree_row_rects(l, vi, false);
                                    if inside(&row, x, y) {
                                        let cb_hit = RECT {
                                            left: cb.left - (4.0 * self.scale) as i32,
                                            top: row.top,
                                            right: cb.right + (6.0 * self.scale) as i32,
                                            bottom: row.bottom,
                                        };
                                        if inside(&cb_hit, x, y) {
                                            return Hit::TreeRowCheck(idx);
                                        }
                                        return Hit::TreeRowToggle(idx);
                                    }
                                }
                                TreeRow::App { indented, .. } => {
                                    let (row, _, _) = self.tree_row_rects(l, vi, *indented);
                                    if inside(&row, x, y) {
                                        if cat.is_manual {
                                            let action = self.manual_remove_rect(&row);
                                            if inside(&action, x, y) {
                                                return Hit::AppRemoveManual(idx);
                                            }
                                        }
                                        return Hit::TreeRowItem(idx);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Tab::Refresh => {
                for (i, r) in l.refresh_interval_presets.iter().enumerate() {
                    if inside(r, x, y) {
                        return Hit::RefreshIntervalPreset(i);
                    }
                }
                if inside(&l.refresh_on_show_toggle, x, y) || inside(&l.refresh_on_show_row, x, y) {
                    return Hit::ToggleScanOnShow;
                }
                if inside(&l.refresh_event_toggle, x, y) || inside(&l.refresh_event_row, x, y) {
                    return Hit::ToggleEventWatch;
                }
                if inside(&l.refresh_icons_toggle, x, y) || inside(&l.refresh_icons_row, x, y) {
                    return Hit::ToggleRefreshChangedIcons;
                }
                if inside(&l.refresh_rebuild_btn, x, y) {
                    return Hit::RebuildIconsCache;
                }
                if inside(&l.refresh_reset_btn, x, y) {
                    return Hit::RefreshReset;
                }
            }
            Tab::Gestures => {
                if inside(&l.gesture_toggle, x, y) || inside(&l.gesture_row1, x, y) {
                    return Hit::ToggleGesture;
                }
                if inside(&l.gesture_swap_toggle, x, y) || inside(&l.gesture_row2, x, y) {
                    return Hit::ToggleGestureSwap;
                }
                if inside(&l.gesture_thresh_def_btn, x, y) {
                    return Hit::GestureThreshDef;
                }
                if inside(&l.gesture_thresh_dec_btn, x, y) {
                    return Hit::GestureThreshDec;
                }
                if inside(&l.gesture_thresh_inc_btn, x, y) {
                    return Hit::GestureThreshInc;
                }
                if inside(&l.gesture_thresh_val_rc, x, y) || inside(&l.gesture_row3, x, y) {
                    return Hit::GestureThreshVal;
                }
                for (i, r) in l.gesture_cards.iter().enumerate() {
                    if inside(r, x, y) {
                        return Hit::GestureCard(i);
                    }
                }
            }
            Tab::About => {
                if let Some(action) = l.about.hit(&self.about, x, y) {
                    return Hit::About(action);
                }
            }
        }
        if y < l.header_h {
            return Hit::DragBar;
        }
        Hit::None
    }
}

pub fn open(app: &mut App) {
    unsafe {
        if !app.settings_hwnd.is_invalid() && IsWindow(app.settings_hwnd).as_bool() {
            let _ = SetForegroundWindow(app.settings_hwnd);
            return;
        }
        register_class(app.hinst);
        app.modal += 1;
        app.settings.icon_size_dp = snap_icon_size_dp(app.settings.icon_size_dp);
        let ptr = app as *mut App;
        let scale = dpi_scale_at_cursor();
        let st = startup::state();
        let mut dlg = Box::new(Dlg {
            app: ptr,
            hwnd: HWND::default(),
            scale,
            target: DibTarget::default(),
            search_edit: HWND::default(),
            search_brush: CreateSolidBrush(COLORREF(0x00FF_FFFF)),
            font_title: HFONT::default(),
            font_about_title: HFONT::default(),
            font_tab: HFONT::default(),
            font_section: HFONT::default(),
            font_body: HFONT::default(),
            font_sub: HFONT::default(),
            font_btn: HFONT::default(),
            font_edit: HFONT::default(),
            font_tip: HFONT::default(),
            tab: Tab::General,
            start_with_windows: st == startup::StartupState::Enabled,
            capture_hk: false,
            hk_msg: None,
            categories: Vec::new(),
            selected_cat: 0,
            expanded_folders: HashSet::new(),
            tree_rows: Vec::new(),
            cat_scroll: 0,
            app_scroll: 0,
            show_expired: false,
            expired_list: Vec::new(),
            expired_scroll: 0,
            expired_status: None,
            about: about::State::default(),
            hover: Hit::None,
            pressed: Hit::None,
            tip_pending: None,
            tip_visible: None,
        });
        dlg.rebuild_fonts();
        dlg.rebuild_groups(&*ptr);

        let cw = (820.0 * scale).round() as i32;
        let ch = (560.0 * scale).round() as i32;
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
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("WinLaunchpadSettings"),
            w!("WinLaunchpad 设置"),
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
        app.settings_hwnd = hwnd;
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
            lpszClassName: w!("WinLaunchpadSettings"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

fn update_tip(dlg: &mut Dlg, app: &App, l: &Layout, hit: Hit) {
    let next: Option<(Hit, RECT, String)> = match hit {
        Hit::Close => Some((Hit::Close, l.close_btn, "关闭设置（所有更改已自动生效）".into())),
        Hit::About(action) => l.about.tip(action).map(|(anchor, text)| (hit, anchor, text.into())),
        Hit::IconPreset(i) => ICON_SIZE_PRESETS.get(i).map(|&(dp, name)| {
            let suffix = if dp == 56 { " · 默认" } else { "" };
            (Hit::IconPreset(i), l.icon_presets[i], format!("{name}图标 ({dp} px{suffix})"))
        }),
        Hit::RoundnessPreset(i) => ICON_ROUNDNESS_PRESETS.get(i).map(|&(mode, name, _)| {
            let desc = match mode {
                0 => "微圆角 · 线条更利落",
                1 => "标准圆角（默认）· macOS 经典比例",
                _ => "圆润圆角 · 更加柔和饱满",
            };
            (Hit::RoundnessPreset(i), l.round_presets[i], format!("{name}：{desc}"))
        }),
        Hit::ColsAuto | Hit::RowsAuto => {
            let (ac, ar) = app.recommended_grid();
            let rc = if hit == Hit::ColsAuto { l.cols_auto_btn } else { l.rows_auto_btn };
            Some((hit, rc, format!("恢复按当前屏幕分辨率自适应（推荐 {ac}×{ar}）")))
        }
        Hit::LayoutReset => Some((Hit::LayoutReset, l.layout_reset_btn, "将图标大小、圆角圆润度与网格行列全部恢复为默认值".into())),
        Hit::CategoryRow(ci) | Hit::CategoryCheck(ci) => {
            if let (Some(cat), Some(vi)) = (dlg.categories.get(ci), ci.checked_sub(dlg.cat_scroll)) {
                let (row, _) = dlg.cat_row_rects(l, vi);
                let tip = match cat.id {
                    "start_root" => "直接位于开始菜单程序根目录的常用桌面软件".into(),
                    "apps_store" => "Microsoft Store 及系统内置常用应用".into(),
                    "cat_third_party" => "桌面软件独立建出的子目录（右侧可展开树形细选）".into(),
                    "cat_sys_dev" => "管理工具、运行库、SDK 与命令行套件（默认隐藏）".into(),
                    _ => "用户手动拖放或浏览添加的应用".into(),
                };
                Some((Hit::CategoryRow(ci), row, tip))
            } else {
                None
            }
        }
        Hit::TreeRowItem(idx) | Hit::TreeRowToggle(idx) | Hit::TreeRowCheck(idx) => {
            if let (Some(cat), Some(vi), Some(tr)) = (
                dlg.categories.get(dlg.selected_cat),
                idx.checked_sub(dlg.app_scroll),
                dlg.tree_rows.get(idx),
            ) {
                match tr {
                    TreeRow::Folder { sub_idx } => cat.subfolders.get(*sub_idx).map(|g| {
                        let (row, _, _) = dlg.tree_row_rects(l, vi, false);
                        let tip = if g.default_on {
                            format!("{} · 点击展开/收起，勾选复选框整目录切换", g.name)
                        } else {
                            format!("{} · 工具/系统子目录，按需勾选或展开细选", g.name)
                        };
                        (Hit::TreeRowToggle(idx), row, tip)
                    }),
                    TreeRow::App { sub_idx, entry_idx, indented } => cat
                        .subfolders
                        .get(*sub_idx)
                        .and_then(|g| g.entries.get(*entry_idx))
                        .map(|e| {
                            let (row, _, _) = dlg.tree_row_rects(l, vi, *indented);
                            (Hit::TreeRowItem(idx), row, format!("{} · {}", e.name, e.target))
                        }),
                }
            } else {
                None
            }
        }
        Hit::AppRemoveManual(idx) => {
            if let Some(vi) = idx.checked_sub(dlg.app_scroll) {
                let (row, _, _) = dlg.tree_row_rects(l, vi, false);
                let action = dlg.manual_remove_rect(&row);
                Some((Hit::AppRemoveManual(idx), action, "从启动台移除".into()))
            } else {
                None
            }
        }
        Hit::ToggleStartup => Some((
            Hit::ToggleStartup,
            l.startup_toggle,
            "开机后静默驻留系统托盘，随时通过快捷键呼出".into(),
        )),
        Hit::ToggleWallpaper => Some((
            Hit::ToggleWallpaper,
            l.wallpaper_toggle,
            "实时将当前桌面壁纸模糊化作为启动台背景".into(),
        )),
        Hit::CheckExpired => Some((
            Hit::CheckExpired,
            l.expired_btn,
            "扫描已卸载或损坏的快捷方式与缓存图标，支持一键清理本地文件".into(),
        )),
        Hit::ExpiredRow(idx) => {
            if let (Some(item), Some(vi)) = (dlg.expired_list.get(idx), idx.checked_sub(dlg.expired_scroll)) {
                let (row, _) = dlg.exp_row_rects(l, vi);
                Some((Hit::ExpiredRow(idx), row, format!("{} · {}", item.reason, item.entry.target)))
            } else {
                None
            }
        }
        Hit::ExpiredDeleteOne(idx) => {
            if let Some(vi) = idx.checked_sub(dlg.expired_scroll) {
                let (_, del) = dlg.exp_row_rects(l, vi);
                Some((Hit::ExpiredDeleteOne(idx), del, "删除记录并尝试清理本地失效快捷方式与图标缓存".into()))
            } else {
                None
            }
        }
        Hit::ToggleGesture => Some((
            Hit::ToggleGesture,
            l.gesture_toggle,
            "按住鼠标右键滑动触发手势，普通右键单击会自动回放".into(),
        )),
        Hit::ToggleGestureSwap => Some((
            Hit::ToggleGestureSwap,
            l.gesture_swap_toggle,
            "反转向左/向右划对应的虚拟桌面切换方向".into(),
        )),
        Hit::GestureThreshDef => Some((
            Hit::GestureThreshDef,
            l.gesture_thresh_def_btn,
            format!("恢复默认手势识别匹配度阈值（{DEFAULT_GESTURE_THRESHOLD}%）"),
        )),
        Hit::GestureThreshDec | Hit::GestureThreshInc | Hit::GestureThreshVal => {
            let anchor = match hit {
                Hit::GestureThreshDec => l.gesture_thresh_dec_btn,
                Hit::GestureThreshInc => l.gesture_thresh_inc_btn,
                _ => l.gesture_thresh_val_rc,
            };
            Some((
                hit,
                anchor,
                format!(
                    "手势形状与有效轨迹最低匹配度（{MIN_GESTURE_THRESHOLD}%–{MAX_GESTURE_THRESHOLD}%，默认 {DEFAULT_GESTURE_THRESHOLD}%）：越高越防误触，越低越容易触发"
                ),
            ))
        }
        Hit::GestureCard(idx) => {
            let specs = gesture_card_specs(app.settings.gesture_swap_lr, &app.hotkey.display());
            specs.get(idx).map(|spec| (Hit::GestureCard(idx), l.gesture_cards[idx], spec.tip.clone()))
        }
        Hit::RefreshIntervalPreset(i) => SCAN_INTERVAL_PRESETS.get(i).map(|&(secs, name)| {
            let tip = match secs {
                0 => "关闭定时自动扫描，仅通过事件驱动、呼出或手动触发".to_string(),
                30 => format!("每 {name} 自动扫描一次（默认 · 兼顾实时性与资源占用）"),
                _ => format!("每 {name} 自动扫描一次"),
            };
            (Hit::RefreshIntervalPreset(i), l.refresh_interval_presets[i], tip)
        }),
        Hit::ToggleScanOnShow => Some((
            Hit::ToggleScanOnShow,
            l.refresh_on_show_toggle,
            "每次呼出启动台后在后台静默检查一次应用变化（5 秒节流）".into(),
        )),
        Hit::ToggleEventWatch => Some((
            Hit::ToggleEventWatch,
            l.refresh_event_toggle,
            "监听开始菜单、App Paths、卸载列表与商店应用包变更，安装或卸载后自动扫描并更新图标".into(),
        )),
        Hit::ToggleRefreshChangedIcons => Some((
            Hit::ToggleRefreshChangedIcons,
            l.refresh_icons_toggle,
            "检测到快捷方式或程序文件更新时，自动失效旧缓存并重新提取图标".into(),
        )),
        Hit::RebuildIconsCache => Some((
            Hit::RebuildIconsCache,
            l.refresh_rebuild_btn,
            "清空内存与磁盘中的全部图标缓存并立即重新提取".into(),
        )),
        Hit::RefreshReset => Some((
            Hit::RefreshReset,
            l.refresh_reset_btn,
            "将定时扫描间隔、事件驱动与图标更新选项恢复为默认值".into(),
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

fn draw_sidebar_icon(sf: &mut Surface, tab: Tab, cx: f32, cy: f32, active: bool, s: f32) {
    let c = if active { rgb(0x1677FF, 1.0) } else { rgb(0x64748B, 1.0) };
    let sw = (1.6 * s).max(1.1);
    match tab {
        Tab::General => {
            sf.stroke_circle(cx, cy, 4.8 * s, sw, c);
            sf.fill_circle(cx, cy, 1.5 * s, c);
            for k in 0..6 {
                let a = k as f32 * std::f32::consts::PI / 3.0;
                sf.fill_circle(cx + a.cos() * 6.8 * s, cy + a.sin() * 6.8 * s, 1.1 * s, c);
            }
        }
        Tab::Folders => {
            let r = RECT {
                left: (cx - 7.0 * s) as i32,
                top: (cy - 5.0 * s) as i32,
                right: (cx + 7.0 * s) as i32,
                bottom: (cy + 5.5 * s) as i32,
            };
            sf.stroke_rrect(&r, 2.5 * s, sw, c);
            sf.thick_line(cx - 5.5 * s, cy - 6.5 * s, cx - 0.5 * s, cy - 6.5 * s, sw, c);
        }
        Tab::Refresh => {
            let r = 5.6 * s;
            let steps = 20usize;
            let a_start = 0.45f32;
            let a_end = 5.42f32;
            let mut prev = (cx + a_start.cos() * r, cy + a_start.sin() * r);
            for k in 1..=steps {
                let t = k as f32 / steps as f32;
                let a = a_start + (a_end - a_start) * t;
                let cur = (cx + a.cos() * r, cy + a.sin() * r);
                sf.thick_line(prev.0, prev.1, cur.0, cur.1, sw, c);
                prev = cur;
            }
            let tx = -a_end.sin();
            let ty = a_end.cos();
            let nx = a_end.cos();
            let ny = a_end.sin();
            let head = 3.2 * s;
            sf.thick_line(prev.0, prev.1, prev.0 - tx * head + nx * head * 0.55, prev.1 - ty * head + ny * head * 0.55, sw, c);
            sf.thick_line(prev.0, prev.1, prev.0 - tx * head - nx * head * 0.75, prev.1 - ty * head - ny * head * 0.75, sw, c);
        }
        Tab::Gestures => {
            sf.thick_line(cx - 6.5 * s, cy + 3.0 * s, cx - 1.5 * s, cy - 4.0 * s, sw, c);
            sf.thick_line(cx - 1.5 * s, cy - 4.0 * s, cx + 2.5 * s, cy + 2.5 * s, sw, c);
            sf.thick_line(cx + 2.5 * s, cy + 2.5 * s, cx + 6.5 * s, cy - 3.5 * s, sw, c);
        }
        Tab::About => {
            sf.stroke_circle(cx, cy, 6.5 * s, sw, c);
            sf.fill_circle(cx, cy - 2.6 * s, 1.1 * s, c);
            sf.thick_line(cx, cy - 0.2 * s, cx, cy + 3.6 * s, sw, c);
        }
    }
}

fn paint_dlg(dlg: &mut Dlg, app: &App) {
    let l = dlg.layout();
    if !dlg.target.ensure(l.w, l.h) {
        return;
    }
    let Some(dc) = dlg.target.mem_dc else { return };
    let Some(mut sf) = dlg.target.surface() else { return };
    let s = dlg.scale;

    sf.clear(rgb(0xFFFFFF, 1.0));
    // 左侧导航栏背景
    let sb = RECT { left: 0, top: 0, right: l.sidebar_w, bottom: l.h };
    sf.fill_rect(&sb, rgb(0xF5F6F8, 1.0));
    sf.fill_rect(&RECT { left: l.sidebar_w, top: 0, right: l.sidebar_w + 1, bottom: l.h }, rgb(0xE5E7EB, 1.0));

    // 顶部栏标题与关闭按钮
    let title_rc = RECT { left: (18.0 * s) as i32, top: 0, right: l.sidebar_w - (10.0 * s) as i32, bottom: l.header_h };
    draw_text(dc, "WinLaunchpad", dlg.font_title, &title_rc, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    let page_title = match dlg.tab {
        Tab::General => "常规设置",
        Tab::Folders => "图标管理",
        Tab::Refresh => "扫描刷新",
        Tab::Gestures => "鼠标手势",
        Tab::About => "关于 WinLaunchpad",
    };
    let pt_rc = RECT { left: l.sidebar_w + (22.0 * s) as i32, top: 0, right: l.close_btn.left - (12.0 * s) as i32, bottom: l.header_h };
    draw_text(dc, page_title, dlg.font_title, &pt_rc, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    sf.fill_rect(&RECT { left: l.sidebar_w + 1, top: l.header_h, right: l.w, bottom: l.header_h + 1 }, rgb(0xE5E7EB, 1.0));

    // 右上角关闭按钮
    if dlg.hover == Hit::Close || dlg.pressed == Hit::Close {
        let cbg = if dlg.pressed == Hit::Close { rgb(0xFEE2E2, 1.0) } else { rgb(0xF1F5F9, 1.0) };
        sf.fill_rrect(&l.close_btn, 7.0 * s, cbg);
    }
    let ccx = (l.close_btn.left + l.close_btn.right) as f32 * 0.5;
    let ccy = (l.close_btn.top + l.close_btn.bottom) as f32 * 0.5;
    let ca = 4.6 * s;
    let ccol = if dlg.hover == Hit::Close || dlg.pressed == Hit::Close { rgb(0xDC2626, 1.0) } else { rgb(0x64748B, 1.0) };
    sf.thick_line(ccx - ca, ccy - ca, ccx + ca, ccy + ca, (1.6 * s).max(1.2), ccol);
    sf.thick_line(ccx - ca, ccy + ca, ccx + ca, ccy - ca, (1.6 * s).max(1.2), ccol);

    // 左侧 Tabs
    for (t, r, label) in &l.tabs {
        let active = dlg.tab == *t;
        let hov = dlg.hover == Hit::Tab(*t);
        let prs = dlg.pressed == Hit::Tab(*t);
        if active {
            sf.fill_rrect(r, 8.0 * s, rgb(0xE8EFFD, 1.0));
        } else if prs {
            sf.fill_rrect(r, 8.0 * s, rgb(0xE2E8F0, 1.0));
        } else if hov {
            sf.fill_rrect(r, 8.0 * s, rgb(0xECEFF3, 1.0));
        }
        draw_sidebar_icon(&mut sf, *t, r.left as f32 + 18.0 * s, (r.top + r.bottom) as f32 * 0.5, active, s);
        let lr = RECT { left: r.left + (34.0 * s) as i32, top: r.top, right: r.right - (8.0 * s) as i32, bottom: r.bottom };
        let tc = if active { rgb(0x1677FF, 1.0) } else { rgb(0x334155, 1.0) };
        draw_text(dc, label, dlg.font_tab, &lr, tc, TextAlign::Left, true, false);
    }

    // 底部操作栏（无保存/取消按钮，所有设置更改均实时生效）
    let footer_bg = RECT { left: l.sidebar_w + 1, top: l.h - l.footer_h, right: l.w, bottom: l.h };
    sf.fill_rect(&footer_bg, rgb(0xFAFAFC, 1.0));
    sf.fill_rect(&RECT { left: l.sidebar_w + 1, top: l.h - l.footer_h, right: l.w, bottom: l.h - l.footer_h + 1 }, rgb(0xE5E7EB, 1.0));
    if dlg.tab != Tab::About {
        let rescan_label = if app.scanning { "正在扫描…" } else { "重新扫描应用" };
        draw_button(&mut sf, dc, &l.rescan_btn, rescan_label, dlg.font_btn, BtnStyle::Secondary, dlg.hover == Hit::Rescan, dlg.pressed == Hit::Rescan, s);
        draw_button(&mut sf, dc, &l.expired_btn, "检测过期图标", dlg.font_btn, BtnStyle::Secondary, dlg.hover == Hit::CheckExpired, dlg.pressed == Hit::CheckExpired, s);
    }

    // 右侧内容区
    match dlg.tab {
        Tab::General => paint_tab_general(dlg, app, &l, &mut sf, dc, s),
        Tab::Folders => paint_tab_folders(dlg, app, &l, &mut sf, dc, s),
        Tab::Refresh => paint_tab_refresh(dlg, app, &l, &mut sf, dc, s),
        Tab::Gestures => paint_tab_gestures(dlg, app, &l, &mut sf, dc, s),
        Tab::About => about::paint(dlg, &l.about, &mut sf, dc),
    }

    // 窗口外框线
    sf.stroke_rrect(&RECT { left: 0, top: 0, right: l.w, bottom: l.h }, 10.0 * s, (1.0 * s).max(1.0), rgb(0xD1D5DB, 1.0));

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

fn draw_stepper_row(
    sf: &mut Surface,
    dc: windows::Win32::Graphics::Gdi::HDC,
    dlg: &Dlg,
    label: &str,
    row_rc: &RECT,
    auto_rc: &RECT,
    dec_rc: &RECT,
    val_rc: &RECT,
    inc_rc: &RECT,
    reset_label: &str,
    auto_hit: Hit,
    dec_hit: Hit,
    inc_hit: Hit,
    is_auto: bool,
    val_text: &str,
    s: f32,
) {
    let lbl_rc = RECT { left: row_rc.left + (18.0 * s) as i32, top: row_rc.top, right: auto_rc.left - (10.0 * s) as i32, bottom: row_rc.bottom };
    draw_text(dc, label, dlg.font_body, &lbl_rc, rgb(0x111827, 1.0), TextAlign::Left, true, false);

    // 快捷重置胶囊（如网格行列「自动」或手势匹配度「默认」）
    let auto_hov = dlg.hover == auto_hit;
    let auto_prs = dlg.pressed == auto_hit;
    let (abg, abd, atc) = if is_auto {
        (rgb(0xEFF6FF, 1.0), rgb(0x1677FF, 1.0), rgb(0x1677FF, 1.0))
    } else if auto_prs {
        (rgb(0xE2E8F0, 1.0), rgb(0xCBD5E1, 1.0), rgb(0x334155, 1.0))
    } else if auto_hov {
        (rgb(0xF1F5F9, 1.0), rgb(0x94A3B8, 1.0), rgb(0x334155, 1.0))
    } else {
        (rgb(0xFFFFFF, 1.0), rgb(0xCBD5E1, 1.0), rgb(0x64748B, 1.0))
    };
    sf.fill_rrect(auto_rc, 7.0 * s, abg);
    sf.stroke_rrect(auto_rc, 7.0 * s, (1.1 * s).max(1.0), abd);
    draw_text(dc, reset_label, dlg.font_sub, auto_rc, atc, TextAlign::Center, true, false);

    draw_button(sf, dc, dec_rc, "-", dlg.font_btn, BtnStyle::Secondary, dlg.hover == dec_hit, dlg.pressed == dec_hit, s);
    sf.fill_rrect(val_rc, 7.0 * s, rgb(0xFFFFFF, 1.0));
    sf.stroke_rrect(val_rc, 7.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    draw_text(dc, val_text, dlg.font_sub, val_rc, rgb(0x1E293B, 1.0), TextAlign::Center, true, false);
    draw_button(sf, dc, inc_rc, "+", dlg.font_btn, BtnStyle::Secondary, dlg.hover == inc_hit, dlg.pressed == inc_hit, s);
}

/// 绘制行内多档分段选择器（用于图标大小 5 档与图标圆角 3 档）。
fn draw_preset_group(
    sf: &mut Surface,
    dc: windows::Win32::Graphics::Gdi::HDC,
    dlg: &Dlg,
    row_rc: &RECT,
    label: &str,
    rects: &[RECT],
    labels: &[&str],
    active_idx: usize,
    make_hit: impl Fn(usize) -> Hit,
    s: f32,
) {
    let lbl_rc = RECT { left: row_rc.left + (18.0 * s) as i32, top: row_rc.top, right: rects[0].left - (10.0 * s) as i32, bottom: row_rc.bottom };
    draw_text(dc, label, dlg.font_body, &lbl_rc, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    for (i, r) in rects.iter().enumerate() {
        let hit = make_hit(i);
        let active = i == active_idx;
        let hov = dlg.hover == hit;
        let prs = dlg.pressed == hit;
        let (bg, bd, tc) = if active {
            (rgb(0xEFF6FF, 1.0), rgb(0x1677FF, 1.0), rgb(0x1677FF, 1.0))
        } else if prs {
            (rgb(0xE2E8F0, 1.0), rgb(0xCBD5E1, 1.0), rgb(0x334155, 1.0))
        } else if hov {
            (rgb(0xF1F5F9, 1.0), rgb(0x94A3B8, 1.0), rgb(0x1E293B, 1.0))
        } else {
            (rgb(0xFFFFFF, 1.0), rgb(0xCBD5E1, 1.0), rgb(0x475569, 1.0))
        };
        sf.fill_rrect(r, 7.0 * s, bg);
        sf.stroke_rrect(r, 7.0 * s, (1.1 * s).max(1.0), bd);
        draw_text(dc, labels[i], dlg.font_sub, r, tc, TextAlign::Center, true, false);
    }
}

fn paint_tab_general(dlg: &Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: windows::Win32::Graphics::Gdi::HDC, s: f32) {
    // 快捷键卡片（精简单行，不含半预设按键）
    sf.fill_rrect(&l.hk_card, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.hk_card, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    let lbl = RECT { left: l.hk_card.left + (18.0 * s) as i32, top: l.hk_card.top, right: l.hk_badge.left - (8.0 * s) as i32, bottom: l.hk_card.bottom };
    draw_text(dc, "全局快捷键", dlg.font_section, &lbl, rgb(0x111827, 1.0), TextAlign::Left, true, false);

    let badge_bg = if dlg.capture_hk { rgb(0xEFF6FF, 1.0) } else { rgb(0xFFFFFF, 1.0) };
    let badge_bd = if dlg.capture_hk { rgb(0x1677FF, 1.0) } else { rgb(0xCBD5E1, 1.0) };
    sf.fill_rrect(&l.hk_badge, 8.0 * s, badge_bg);
    sf.stroke_rrect(&l.hk_badge, 8.0 * s, (1.2 * s).max(1.0), badge_bd);
    let hk_str = if dlg.capture_hk {
        dlg.hk_msg.clone().unwrap_or_else(|| "请按下组合键…（Esc 取消）".into())
    } else {
        app.hotkey.display()
    };
    let hk_col = if dlg.capture_hk { rgb(0x1677FF, 1.0) } else { rgb(0x1E293B, 1.0) };
    draw_text(dc, &hk_str, dlg.font_body, &l.hk_badge, hk_col, TextAlign::Center, true, false);

    let mod_lbl = if dlg.capture_hk { "取消" } else { "修改" };
    draw_button(sf, dc, &l.hk_mod_btn, mod_lbl, dlg.font_btn, BtnStyle::Secondary, dlg.hover == Hit::HkModify, dlg.pressed == Hit::HkModify, s);
    draw_button(sf, dc, &l.hk_def_btn, "恢复默认", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::HkDefault, dlg.pressed == Hit::HkDefault, s);

    // 系统开关卡片
    sf.fill_rrect(&l.sys_card, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.sys_card, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    if dlg.hover == Hit::ToggleStartup {
        sf.fill_rrect(&l.startup_row, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    if dlg.hover == Hit::ToggleWallpaper {
        sf.fill_rrect(&l.wallpaper_row, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    sf.fill_rect(&RECT { left: l.sys_card.left + (16.0 * s) as i32, top: l.wallpaper_row.top, right: l.sys_card.right - (16.0 * s) as i32, bottom: l.wallpaper_row.top + 1 }, rgb(0xE2E8F0, 1.0));

    let st_sys_disabled = startup::state() == startup::StartupState::DisabledBySystem;
    let st_label = if st_sys_disabled {
        "开机启动（已被系统任务管理器禁用）"
    } else {
        "开机启动"
    };
    let r1_lbl = RECT { left: l.startup_row.left + (18.0 * s) as i32, top: l.startup_row.top, right: l.startup_toggle.left - (12.0 * s) as i32, bottom: l.startup_row.bottom };
    let r2_lbl = RECT { left: l.wallpaper_row.left + (18.0 * s) as i32, top: l.wallpaper_row.top, right: l.wallpaper_toggle.left - (12.0 * s) as i32, bottom: l.wallpaper_row.bottom };
    draw_text(dc, st_label, dlg.font_body, &r1_lbl, if st_sys_disabled { rgb(0xD97706, 1.0) } else { rgb(0x111827, 1.0) }, TextAlign::Left, true, false);
    draw_text(dc, "桌面壁纸毛玻璃背景", dlg.font_body, &r2_lbl, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    draw_toggle(sf, &l.startup_toggle, dlg.start_with_windows, dlg.hover == Hit::ToggleStartup, dlg.pressed == Hit::ToggleStartup, s);
    draw_toggle(sf, &l.wallpaper_toggle, app.settings.use_wallpaper_blur, dlg.hover == Hit::ToggleWallpaper, dlg.pressed == Hit::ToggleWallpaper, s);

    // 启动台图标大小、圆角程度与网格行列设置卡片
    sf.fill_rrect(&l.grid_card, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.grid_card, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    for sep_y in [l.round_row.top, l.cols_row.top, l.rows_row.top] {
        sf.fill_rect(&RECT { left: l.grid_card.left + (16.0 * s) as i32, top: sep_y, right: l.grid_card.right - (16.0 * s) as i32, bottom: sep_y + 1 }, rgb(0xE2E8F0, 1.0));
    }

    let active_dp = snap_icon_size_dp(app.settings.icon_size_dp);
    let size_idx = ICON_SIZE_PRESETS.iter().position(|&(dp, _)| dp == active_dp).unwrap_or(2);
    let size_labels: Vec<&str> = ICON_SIZE_PRESETS.iter().map(|&(_, n)| n).collect();
    draw_preset_group(sf, dc, dlg, &l.icon_row, "启动台图标大小", &l.icon_presets, &size_labels, size_idx, Hit::IconPreset, s);

    let round_idx = (app.settings.icon_roundness as usize).min(ICON_ROUNDNESS_PRESETS.len() - 1);
    let round_labels: Vec<&str> = ICON_ROUNDNESS_PRESETS.iter().map(|&(_, n, _)| n).collect();
    draw_preset_group(sf, dc, dlg, &l.round_row, "启动台图标圆角", &l.round_presets, &round_labels, round_idx, Hit::RoundnessPreset, s);

    let (rec_cols, rec_rows) = app.recommended_grid();
    let cols_auto = app.settings.grid_cols <= 0;
    let cols_txt = if cols_auto {
        format!("推荐 {rec_cols} 列")
    } else {
        format!("{} 列", app.settings.grid_cols)
    };
    draw_stepper_row(
        sf, dc, dlg, "启动台网格列数", &l.cols_row,
        &l.cols_auto_btn, &l.cols_dec_btn, &l.cols_val_rc, &l.cols_inc_btn,
        "自动", Hit::ColsAuto, Hit::ColsDec, Hit::ColsInc, cols_auto, &cols_txt, s,
    );

    let rows_auto = app.settings.grid_rows <= 0;
    let rows_txt = if rows_auto {
        format!("推荐 {rec_rows} 行")
    } else {
        format!("{} 行", app.settings.grid_rows)
    };
    draw_stepper_row(
        sf, dc, dlg, "启动台网格行数", &l.rows_row,
        &l.rows_auto_btn, &l.rows_dec_btn, &l.rows_val_rc, &l.rows_inc_btn,
        "自动", Hit::RowsAuto, Hit::RowsDec, Hit::RowsInc, rows_auto, &rows_txt, s,
    );
    draw_button(sf, dc, &l.layout_reset_btn, "恢复默认", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::LayoutReset, dlg.pressed == Hit::LayoutReset, s);
}

fn draw_tree_folder_icon(sf: &mut Surface, cx: f32, cy: f32, active: bool, s: f32) {
    let col = if active { rgb(0x1677FF, 0.9) } else { rgb(0x94A3B8, 0.95) };
    let r = RECT { left: (cx - 6.5 * s) as i32, top: (cy - 4.0 * s) as i32, right: (cx + 6.5 * s) as i32, bottom: (cy + 5.0 * s) as i32 };
    sf.stroke_rrect(&r, 2.0 * s, (1.3 * s).max(1.0), col);
    sf.thick_line(cx - 5.2 * s, cy - 5.5 * s, cx - 0.5 * s, cy - 5.5 * s, (1.3 * s).max(1.0), col);
}

fn paint_tab_folders(dlg: &Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: windows::Win32::Graphics::Gdi::HDC, s: f32) {
    // 左栏卡片：聚合后的大类导航（5 类全览）
    sf.fill_rrect(&l.left_pane, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.left_pane, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));

    // 搜索边框
    sf.fill_rrect(&l.search_box, 7.0 * s, rgb(0xFFFFFF, 1.0));
    sf.stroke_rrect(&l.search_box, 7.0 * s, 1.0, rgb(0xCBD5E1, 1.0));
    let scx = l.search_box.left as f32 + 15.0 * s;
    let scy = (l.search_box.top + l.search_box.bottom) as f32 * 0.5;
    sf.stroke_circle(scx, scy - 1.0 * s, 4.2 * s, (1.3 * s).max(1.0), rgb(0x94A3B8, 1.0));
    sf.thick_line(scx + 3.0 * s, scy + 2.0 * s, scx + 6.0 * s, scy + 5.0 * s, (1.4 * s).max(1.0), rgb(0x94A3B8, 1.0));

    draw_button(sf, dc, &l.btn_all_folders, "全部勾选", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::FolderSelectAll, dlg.pressed == Hit::FolderSelectAll, s);
    draw_button(sf, dc, &l.btn_reset_def, "恢复默认", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::FolderResetDefault, dlg.pressed == Hit::FolderResetDefault, s);

    let q = dlg.search_query().trim().to_lowercase();
    sf.set_clip(Some(l.folder_list_rc));
    for vi in 0..l.folder_vis {
        let ci = dlg.cat_scroll + vi;
        let Some(cat) = dlg.categories.get(ci) else { break };
        let (row, cb) = dlg.cat_row_rects(l, vi);
        let is_sel = ci == dlg.selected_cat;
        let is_hov = matches!(dlg.hover, Hit::CategoryRow(i) | Hit::CategoryCheck(i) if i == ci);
        if is_sel {
            sf.fill_rrect(&row, 7.0 * s, rgb(0xEBF4FF, 1.0));
        } else if is_hov {
            sf.fill_rrect(&row, 7.0 * s, rgb(0xEEF2F6, 1.0));
        }

        if !cat.is_manual {
            let st = dlg.category_check_state(app, cat);
            draw_checkbox(sf, &cb, st, dlg.hover == Hit::CategoryCheck(ci), dlg.pressed == Hit::CategoryCheck(ci), s);
        } else {
            let icx = (cb.left + cb.right) as f32 * 0.5;
            let icy = (cb.top + cb.bottom) as f32 * 0.5;
            sf.fill_circle(icx, icy, 4.0 * s, rgb(0x1677FF, 0.85));
        }

        let tr = RECT { left: cb.right + (8.0 * s) as i32, top: row.top, right: row.right - (16.0 * s) as i32, bottom: row.bottom };
        let tc = if is_sel {
            rgb(0x1677FF, 1.0)
        } else if cat.id == "cat_sys_dev" {
            rgb(0x64748B, 1.0)
        } else {
            rgb(0x111827, 1.0)
        };
        draw_text(dc, cat.name, dlg.font_body, &tr, tc, TextAlign::Left, true, false);
        if !q.is_empty() && dlg.category_matches_query(cat, &q) {
            sf.fill_circle(row.right as f32 - 8.0 * s, (row.top + row.bottom) as f32 * 0.5, 2.8 * s, rgb(0x1677FF, 0.75));
        }
    }
    sf.set_clip(None);
    draw_scrollbar(sf, &l.folder_scroll_rc, dlg.categories.len(), l.folder_vis, dlg.cat_scroll, false, s);

    // 右栏卡片：过期图标检测 或 当前大类下的应用/树形列表
    sf.fill_rrect(&l.right_pane, 10.0 * s, rgb(0xFFFFFF, 1.0));
    sf.stroke_rrect(&l.right_pane, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    sf.fill_rect(&RECT { left: l.right_header.left + 1, top: l.right_header.bottom, right: l.right_header.right - 1, bottom: l.right_header.bottom + 1 }, rgb(0xE2E8F0, 1.0));

    if dlg.show_expired {
        let hr = RECT { left: l.right_header.left + (14.0 * s) as i32, top: l.right_header.top, right: l.btn_exp_back.left - (10.0 * s) as i32, bottom: l.right_header.bottom };
        draw_text(dc, "过期图标检测", dlg.font_section, &hr, rgb(0x111827, 1.0), TextAlign::Left, true, false);
        draw_button(sf, dc, &l.btn_exp_back, "返回", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::ExpiredBack, dlg.pressed == Hit::ExpiredBack, s);
        if !dlg.expired_list.is_empty() {
            draw_button(sf, dc, &l.btn_exp_del_all, "一键删除全部", dlg.font_sub, BtnStyle::Primary, dlg.hover == Hit::ExpiredDeleteAll, dlg.pressed == Hit::ExpiredDeleteAll, s);
        }

        if dlg.expired_list.is_empty() {
            let msg = dlg.expired_status.as_deref().unwrap_or("未检测到过期或失效的应用图标");
            let empty_rc = RECT { left: l.app_list_rc.left + (16.0 * s) as i32, top: l.app_list_rc.top + (64.0 * s) as i32, right: l.app_list_rc.right - (16.0 * s) as i32, bottom: l.app_list_rc.top + (108.0 * s) as i32 };
            let cx = (empty_rc.left + empty_rc.right) as f32 * 0.5;
            let cy = l.app_list_rc.top as f32 + 40.0 * s;
            sf.fill_circle(cx, cy, 14.0 * s, rgb(0xDCFCE7, 1.0));
            let ok_c = rgb(0x16A34A, 1.0);
            sf.thick_line(cx - 5.0 * s, cy + 0.5 * s, cx - 1.2 * s, cy + 4.2 * s, (2.0 * s).max(1.2), ok_c);
            sf.thick_line(cx - 1.2 * s, cy + 4.2 * s, cx + 6.0 * s, cy - 4.0 * s, (2.0 * s).max(1.2), ok_c);
            draw_text(dc, msg, dlg.font_body, &empty_rc, rgb(0x475569, 1.0), TextAlign::Center, false, false);
        } else {
            sf.set_clip(Some(l.app_list_rc));
            for vi in 0..l.exp_vis {
                let idx = dlg.expired_scroll + vi;
                let Some(item) = dlg.expired_list.get(idx) else { break };
                let (row, del) = dlg.exp_row_rects(l, vi);
                let hov = matches!(dlg.hover, Hit::ExpiredRow(i) | Hit::ExpiredDeleteOne(i) if i == idx);
                if hov {
                    sf.fill_rrect(&row, 8.0 * s, rgb(0xFEF2F2, 0.65));
                }
                let isz = 24.0 * s;
                let icx = row.left as f32 + 12.0 * s + isz * 0.5;
                let icy = (row.top + row.bottom) as f32 * 0.5;
                if let Some(icon) = app.cached_icon(&item.entry.icon_key) {
                    sf.draw_icon_bilinear(&icon, icx, icy, isz, false, 0.0);
                } else {
                    let half = isz * 0.5;
                    sf.fill_rrect(&RECT { left: (icx - half) as i32, top: (icy - half) as i32, right: (icx + half) as i32, bottom: (icy + half) as i32 }, 6.0 * s, rgb(0xFEE2E2, 1.0));
                }
                let tx_left = row.left + (18.0 * s) as i32 + isz.round() as i32;
                let name_rc = RECT { left: tx_left, top: row.top + (4.0 * s) as i32, right: del.left - (8.0 * s) as i32, bottom: row.top + (24.0 * s) as i32 };
                let sub_rc = RECT { left: tx_left, top: row.top + (23.0 * s) as i32, right: del.left - (8.0 * s) as i32, bottom: row.bottom - (3.0 * s) as i32 };
                draw_text(dc, &item.entry.name, dlg.font_body, &name_rc, rgb(0x111827, 1.0), TextAlign::Left, true, false);
                draw_text(dc, &format!("{} · {}", item.reason, item.entry.target), dlg.font_sub, &sub_rc, rgb(0xDC2626, 0.85), TextAlign::Left, true, false);
                draw_button(sf, dc, &del, "删除", dlg.font_sub, BtnStyle::DangerGhost, dlg.hover == Hit::ExpiredDeleteOne(idx), dlg.pressed == Hit::ExpiredDeleteOne(idx), s);
            }
            sf.set_clip(None);
            draw_scrollbar(sf, &l.app_scroll_rc, dlg.expired_list.len(), l.exp_vis, dlg.expired_scroll, false, s);
        }
        return;
    }

    if let Some(cat) = dlg.categories.get(dlg.selected_cat) {
        let header_right = if cat.is_tree { l.btn_tree_expand.left - (8.0 * s) as i32 } else { l.btn_add_manual.left - (10.0 * s) as i32 };
        let hr = RECT { left: l.right_header.left + (14.0 * s) as i32, top: l.right_header.top, right: header_right, bottom: l.right_header.bottom };
        draw_text(dc, cat.name, dlg.font_section, &hr, rgb(0x111827, 1.0), TextAlign::Left, true, false);
        if cat.is_manual {
            draw_button(sf, dc, &l.btn_add_manual, "+ 添加应用", dlg.font_sub, BtnStyle::Primary, dlg.hover == Hit::AddManualBtn, dlg.pressed == Hit::AddManualBtn, s);
        } else {
            if cat.is_tree {
                let exp_lbl = if dlg.all_tree_expanded() { "全部折叠" } else { "全部展开" };
                draw_button(sf, dc, &l.btn_tree_expand, exp_lbl, dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::TreeExpandAll, dlg.pressed == Hit::TreeExpandAll, s);
            }
            let all_on = dlg.category_check_state(app, cat) == CheckState::Checked;
            let lbl = if all_on { "取消全选" } else { "全选" };
            draw_button(sf, dc, &l.btn_folder_all, lbl, dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::AppSelectAllInCat, dlg.pressed == Hit::AppSelectAllInCat, s);
        }

        if dlg.tree_rows.is_empty() {
            let empty_rc = RECT { left: l.app_list_rc.left, top: l.app_list_rc.top + (40.0 * s) as i32, right: l.app_list_rc.right, bottom: l.app_list_rc.bottom };
            let msg = if cat.is_manual { "暂无手动添加的应用" } else { "暂无匹配条目" };
            draw_text(dc, msg, dlg.font_body, &empty_rc, rgb(0x94A3B8, 1.0), TextAlign::Center, false, false);
        } else {
            sf.set_clip(Some(l.app_list_rc));
            for vi in 0..l.app_vis {
                let idx = dlg.app_scroll + vi;
                let Some(tr) = dlg.tree_rows.get(idx) else { break };
                match tr {
                    TreeRow::Folder { sub_idx } => {
                        let Some(g) = cat.subfolders.get(*sub_idx) else { continue };
                        let (row, arrow, cb) = dlg.tree_row_rects(l, vi, false);
                        let is_hov = matches!(dlg.hover, Hit::TreeRowToggle(i) | Hit::TreeRowCheck(i) if i == idx);
                        let is_prs = dlg.pressed == Hit::TreeRowToggle(idx);
                        if is_prs {
                            sf.fill_rrect(&row, 8.0 * s, rgb(0xE2E8F0, 0.75));
                        } else if is_hov {
                            sf.fill_rrect(&row, 8.0 * s, rgb(0xF1F5F9, 1.0));
                        }
                        let is_open = !q.is_empty() || dlg.expanded_folders.contains(&g.id);
                        let acx = (arrow.left + arrow.right) as f32 * 0.5;
                        let acy = (arrow.top + arrow.bottom) as f32 * 0.5;
                        let acol = rgb(0x64748B, 1.0);
                        let sw = (1.5 * s).max(1.1);
                        if is_open {
                            sf.thick_line(acx - 3.2 * s, acy - 1.5 * s, acx, acy + 2.0 * s, sw, acol);
                            sf.thick_line(acx, acy + 2.0 * s, acx + 3.2 * s, acy - 1.5 * s, sw, acol);
                        } else {
                            sf.thick_line(acx - 1.5 * s, acy - 3.2 * s, acx + 2.0 * s, acy, sw, acol);
                            sf.thick_line(acx + 2.0 * s, acy, acx - 1.5 * s, acy + 3.2 * s, sw, acol);
                        }
                        let st = dlg.folder_check_state(app, g);
                        draw_checkbox(sf, &cb, st, dlg.hover == Hit::TreeRowCheck(idx), dlg.pressed == Hit::TreeRowCheck(idx), s);

                        let fcx = cb.right as f32 + 17.0 * s;
                        let fcy = (row.top + row.bottom) as f32 * 0.5;
                        draw_tree_folder_icon(sf, fcx, fcy, st != CheckState::Unchecked, s);

                        let name_rc = RECT { left: (fcx + 12.0 * s) as i32, top: row.top, right: row.right - (10.0 * s) as i32, bottom: row.bottom };
                        draw_text(dc, &g.name, dlg.font_section, &name_rc, if st == CheckState::Unchecked { rgb(0x64748B, 1.0) } else { rgb(0x111827, 1.0) }, TextAlign::Left, true, false);
                    }
                    TreeRow::App { sub_idx, entry_idx, indented } => {
                        let Some(g) = cat.subfolders.get(*sub_idx) else { continue };
                        let Some(e) = g.entries.get(*entry_idx) else { continue };
                        let (row, _, cb) = dlg.tree_row_rects(l, vi, *indented);
                        let is_hov = matches!(dlg.hover, Hit::TreeRowItem(i) | Hit::AppRemoveManual(i) if i == idx);
                        let is_prs = dlg.pressed == Hit::TreeRowItem(idx);
                        if is_prs {
                            sf.fill_rrect(&row, 8.0 * s, rgb(0xE2E8F0, 0.8));
                        } else if is_hov {
                            sf.fill_rrect(&row, 8.0 * s, rgb(0xF1F5F9, 1.0));
                        }

                        let mut icon_left = if *indented { cb.left } else { row.left + (10.0 * s) as i32 };
                        if !g.is_manual {
                            let cbox = if *indented {
                                cb
                            } else {
                                let cb_sz = (16.0 * s).round() as i32;
                                let cy = (row.top + row.bottom) / 2;
                                RECT { left: icon_left, top: cy - cb_sz / 2, right: icon_left + cb_sz, bottom: cy + cb_sz / 2 }
                            };
                            let on = dlg.entry_checked(app, g, e);
                            draw_checkbox(sf, &cbox, if on { CheckState::Checked } else { CheckState::Unchecked }, is_hov, is_prs, s);
                            icon_left = cbox.right + (10.0 * s) as i32;
                        }

                        let isz = 22.0 * s;
                        let icx = icon_left as f32 + isz * 0.5;
                        let icy = (row.top + row.bottom) as f32 * 0.5;
                        if let Some(icon) = app.cached_icon(&e.icon_key) {
                            sf.draw_icon_bilinear(&icon, icx, icy, isz, false, 0.0);
                        } else {
                            let half = isz * 0.5;
                            sf.fill_rrect(&RECT { left: (icx - half) as i32, top: (icy - half) as i32, right: (icx + half) as i32, bottom: (icy + half) as i32 }, 5.0 * s, rgb(0xE2E8F0, 1.0));
                        }

                        let action = dlg.manual_remove_rect(&row);
                        let right_lim = if g.is_manual { action.left - (6.0 * s) as i32 } else { row.right - (10.0 * s) as i32 };
                        let name_rc = RECT { left: icon_left + isz.round() as i32 + (10.0 * s) as i32, top: row.top, right: right_lim, bottom: row.bottom };
                        let active_txt = dlg.entry_checked(app, g, e);
                        draw_text(dc, &e.name, dlg.font_body, &name_rc, if active_txt { rgb(0x111827, 1.0) } else { rgb(0x64748B, 1.0) }, TextAlign::Left, true, false);

                        if g.is_manual {
                            let rm_hov = dlg.hover == Hit::AppRemoveManual(idx);
                            if rm_hov {
                                sf.fill_circle((action.left + action.right) as f32 * 0.5, (action.top + action.bottom) as f32 * 0.5, 10.0 * s, rgb(0xFEE2E2, 1.0));
                            }
                            let ax = (action.left + action.right) as f32 * 0.5;
                            let ay = (action.top + action.bottom) as f32 * 0.5;
                            let aa = 3.6 * s;
                            let ac = if rm_hov { rgb(0xDC2626, 1.0) } else { rgb(0x94A3B8, 1.0) };
                            sf.thick_line(ax - aa, ay - aa, ax + aa, ay + aa, (1.5 * s).max(1.0), ac);
                            sf.thick_line(ax - aa, ay + aa, ax + aa, ay - aa, (1.5 * s).max(1.0), ac);
                        }
                    }
                }
            }
            sf.set_clip(None);
            draw_scrollbar(sf, &l.app_scroll_rc, dlg.tree_rows.len(), l.app_vis, dlg.app_scroll, false, s);
        }
    }
}

struct GestureCardSpec {
    glyph: GestureGlyph,
    name: &'static str,
    action: &'static str,
    shortcut: String,
    tip: String,
}

/// 生成五张手势说明卡的展示数据（与左右方向反转设置及全局快捷键保持同步）。
fn gesture_card_specs(swap: bool, launchpad_hotkey: &str) -> [GestureCardSpec; 5] {
    let left_sc = if swap { "Ctrl + Win + ←" } else { "Ctrl + Win + →" };
    let right_sc = if swap { "Ctrl + Win + →" } else { "Ctrl + Win + ←" };
    [
        GestureCardSpec {
            glyph: GestureGlyph::Left,
            name: "向左划",
            action: if swap { "切换桌面 · 向左" } else { "切换桌面 · 向右" },
            shortcut: left_sc.into(),
            tip: format!("按住右键向左划：{left_sc}"),
        },
        GestureCardSpec {
            glyph: GestureGlyph::Right,
            name: "向右划",
            action: if swap { "切换桌面 · 向右" } else { "切换桌面 · 向左" },
            shortcut: right_sc.into(),
            tip: format!("按住右键向右划：{right_sc}"),
        },
        GestureCardSpec {
            glyph: GestureGlyph::Up,
            name: "向上划",
            action: "切换窗口",
            shortcut: "Ctrl + Alt + Tab".into(),
            tip: "按住右键向上划：Ctrl + Alt + Tab（切换窗口）".into(),
        },
        GestureCardSpec {
            glyph: GestureGlyph::UpDown,
            name: "∧ 轨迹",
            action: "打开 / 收起启动台",
            shortcut: launchpad_hotkey.into(),
            tip: format!(
                "按住右键先右上再右下：打开或收起启动台（快捷键 {launchpad_hotkey}，支持 Quicker --toggle）"
            ),
        },
        GestureCardSpec {
            glyph: GestureGlyph::DownUp,
            name: "∨ 轨迹",
            action: "全部窗口（任务视图）",
            shortcut: "Win + Tab".into(),
            tip: "按住右键先右下再右上：Win + Tab（任务视图）".into(),
        },
    ]
}

/// 绘制「扫描刷新」标签页：定时扫描间隔档位、展开静默刷新、事件驱动监听与状态卡片。
fn paint_tab_refresh(dlg: &Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: windows::Win32::Graphics::Gdi::HDC, s: f32) {
    // 定时扫描与展开刷新卡片
    sf.fill_rrect(&l.refresh_timer_card, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.refresh_timer_card, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    if dlg.hover == Hit::ToggleScanOnShow {
        sf.fill_rrect(&l.refresh_on_show_row, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    sf.fill_rect(
        &RECT {
            left: l.refresh_timer_card.left + (16.0 * s) as i32,
            top: l.refresh_on_show_row.top,
            right: l.refresh_timer_card.right - (16.0 * s) as i32,
            bottom: l.refresh_on_show_row.top + 1,
        },
        rgb(0xE2E8F0, 1.0),
    );

    let active_secs = snap_scan_interval_secs(app.settings.scan_interval_secs);
    let interval_idx = SCAN_INTERVAL_PRESETS
        .iter()
        .position(|&(secs, _)| secs == active_secs)
        .unwrap_or(2);
    let interval_labels: Vec<&str> = SCAN_INTERVAL_PRESETS.iter().map(|&(_, name)| name).collect();
    draw_preset_group(
        sf,
        dc,
        dlg,
        &l.refresh_interval_row,
        "定时扫描间隔",
        &l.refresh_interval_presets,
        &interval_labels,
        interval_idx,
        Hit::RefreshIntervalPreset,
        s,
    );

    let show_lbl = RECT {
        left: l.refresh_on_show_row.left + (18.0 * s) as i32,
        top: l.refresh_on_show_row.top,
        right: l.refresh_on_show_toggle.left - (12.0 * s) as i32,
        bottom: l.refresh_on_show_row.bottom,
    };
    draw_text(dc, "展开启动台时后台刷新", dlg.font_body, &show_lbl, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    draw_toggle(
        sf,
        &l.refresh_on_show_toggle,
        app.settings.scan_on_show,
        dlg.hover == Hit::ToggleScanOnShow,
        dlg.pressed == Hit::ToggleScanOnShow,
        s,
    );

    // 安装/卸载事件驱动更新与图标缓存同步卡片
    sf.fill_rrect(&l.refresh_event_card, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.refresh_event_card, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    if dlg.hover == Hit::ToggleEventWatch {
        sf.fill_rrect(&l.refresh_event_row, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    if dlg.hover == Hit::ToggleRefreshChangedIcons {
        sf.fill_rrect(&l.refresh_icons_row, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    sf.fill_rect(
        &RECT {
            left: l.refresh_event_card.left + (16.0 * s) as i32,
            top: l.refresh_icons_row.top,
            right: l.refresh_event_card.right - (16.0 * s) as i32,
            bottom: l.refresh_icons_row.top + 1,
        },
        rgb(0xE2E8F0, 1.0),
    );

    let ev_lbl = RECT {
        left: l.refresh_event_row.left + (18.0 * s) as i32,
        top: l.refresh_event_row.top,
        right: l.refresh_event_toggle.left - (12.0 * s) as i32,
        bottom: l.refresh_event_row.bottom,
    };
    let ic_lbl = RECT {
        left: l.refresh_icons_row.left + (18.0 * s) as i32,
        top: l.refresh_icons_row.top,
        right: l.refresh_icons_toggle.left - (12.0 * s) as i32,
        bottom: l.refresh_icons_row.bottom,
    };
    draw_text(dc, "安装 / 卸载事件驱动更新", dlg.font_body, &ev_lbl, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    draw_text(dc, "程序变更时自动更新图标", dlg.font_body, &ic_lbl, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    draw_toggle(
        sf,
        &l.refresh_event_toggle,
        app.settings.event_watch_enabled,
        dlg.hover == Hit::ToggleEventWatch,
        dlg.pressed == Hit::ToggleEventWatch,
        s,
    );
    draw_toggle(
        sf,
        &l.refresh_icons_toggle,
        app.settings.refresh_changed_icons,
        dlg.hover == Hit::ToggleRefreshChangedIcons,
        dlg.pressed == Hit::ToggleRefreshChangedIcons,
        s,
    );

    // 底部扫描状态与缓存操作卡片
    sf.fill_rrect(&l.refresh_status_card, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.refresh_status_card, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    let dot_cx = l.refresh_status_card.left as f32 + 22.0 * s;
    let dot_cy = (l.refresh_status_card.top + l.refresh_status_card.bottom) as f32 * 0.5;
    let dot_col = if app.scanning {
        rgb(0x1677FF, 1.0)
    } else if app.settings.event_watch_enabled && app.watcher.is_some() {
        rgb(0x16A34A, 1.0)
    } else {
        rgb(0x94A3B8, 1.0)
    };
    sf.fill_circle(dot_cx, dot_cy, 4.2 * s, dot_col);

    let status_text = if app.scanning {
        "正在扫描应用…".to_string()
    } else if !app.last_scan_time_text.is_empty() {
        match app.last_scan_kind {
            Some(k) => format!("最近扫描：{} · {}", app.last_scan_time_text, k.label()),
            None => format!("最近扫描：{}", app.last_scan_time_text),
        }
    } else {
        "等待首次扫描".to_string()
    };
    let status_rc = RECT {
        left: l.refresh_status_card.left + (34.0 * s) as i32,
        top: l.refresh_status_card.top,
        right: l.refresh_rebuild_btn.left - (10.0 * s) as i32,
        bottom: l.refresh_status_card.bottom,
    };
    draw_text(dc, &status_text, dlg.font_body, &status_rc, rgb(0x334155, 1.0), TextAlign::Left, true, false);
    draw_button(
        sf,
        dc,
        &l.refresh_rebuild_btn,
        "重建图标缓存",
        dlg.font_sub,
        BtnStyle::Secondary,
        dlg.hover == Hit::RebuildIconsCache,
        dlg.pressed == Hit::RebuildIconsCache,
        s,
    );
    draw_button(
        sf,
        dc,
        &l.refresh_reset_btn,
        "恢复默认",
        dlg.font_sub,
        BtnStyle::Secondary,
        dlg.hover == Hit::RefreshReset,
        dlg.pressed == Hit::RefreshReset,
        s,
    );
}

fn paint_tab_gestures(dlg: &Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: windows::Win32::Graphics::Gdi::HDC, s: f32) {
    sf.fill_rrect(&l.gesture_card, 10.0 * s, rgb(0xF8FAFC, 1.0));
    sf.stroke_rrect(&l.gesture_card, 10.0 * s, 1.0, rgb(0xE2E8F0, 1.0));
    if dlg.hover == Hit::ToggleGesture {
        sf.fill_rrect(&l.gesture_row1, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    if dlg.hover == Hit::ToggleGestureSwap {
        sf.fill_rrect(&l.gesture_row2, 10.0 * s, rgb(0xF1F5F9, 0.7));
    }
    for sep_y in [l.gesture_row2.top, l.gesture_row3.top] {
        sf.fill_rect(&RECT { left: l.gesture_card.left + (16.0 * s) as i32, top: sep_y, right: l.gesture_card.right - (16.0 * s) as i32, bottom: sep_y + 1 }, rgb(0xE2E8F0, 1.0));
    }

    let r1 = RECT { left: l.gesture_row1.left + (18.0 * s) as i32, top: l.gesture_row1.top, right: l.gesture_toggle.left - (12.0 * s) as i32, bottom: l.gesture_row1.bottom };
    let r2 = RECT { left: l.gesture_row2.left + (18.0 * s) as i32, top: l.gesture_row2.top, right: l.gesture_swap_toggle.left - (12.0 * s) as i32, bottom: l.gesture_row2.bottom };
    draw_text(dc, "启用全局右键手势", dlg.font_body, &r1, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    draw_text(dc, "左右手势切换桌面方向反转", dlg.font_body, &r2, rgb(0x111827, 1.0), TextAlign::Left, true, false);
    draw_toggle(sf, &l.gesture_toggle, app.settings.gestures_enabled, dlg.hover == Hit::ToggleGesture, dlg.pressed == Hit::ToggleGesture, s);
    draw_toggle(sf, &l.gesture_swap_toggle, app.settings.gesture_swap_lr, dlg.hover == Hit::ToggleGestureSwap, dlg.pressed == Hit::ToggleGestureSwap, s);

    // 复用通用步进控件展示手势识别最低匹配度百分比（50%–95%，默认 70%）
    let thresh = clamp_gesture_threshold(app.settings.gesture_threshold);
    let thresh_txt = format!("{thresh}%");
    draw_stepper_row(
        sf, dc, dlg, "手势识别匹配度", &l.gesture_row3,
        &l.gesture_thresh_def_btn, &l.gesture_thresh_dec_btn, &l.gesture_thresh_val_rc, &l.gesture_thresh_inc_btn,
        "默认", Hit::GestureThreshDef, Hit::GestureThreshDec, Hit::GestureThreshInc,
        thresh == DEFAULT_GESTURE_THRESHOLD, &thresh_txt, s,
    );

    let specs = gesture_card_specs(app.settings.gesture_swap_lr, &app.hotkey.display());
    for (i, spec) in specs.iter().enumerate() {
        let cr = &l.gesture_cards[i];
        let hov = dlg.hover == Hit::GestureCard(i);
        sf.fill_rrect(cr, 10.0 * s, if hov { rgb(0xF8FAFC, 1.0) } else { rgb(0xFFFFFF, 1.0) });
        sf.stroke_rrect(cr, 10.0 * s, (1.1 * s).max(1.0), if hov { rgb(0x93C5FD, 1.0) } else { rgb(0xE2E8F0, 1.0) });
        let gcx = (cr.left + cr.right) as f32 * 0.5;
        let gcy = cr.top as f32 + 34.0 * s;
        draw_gesture_glyph(sf, gcx, gcy, 42.0 * s, spec.glyph, s);
        let nr = RECT {
            left: cr.left + (8.0 * s).round() as i32,
            top: cr.top + (59.0 * s).round() as i32,
            right: cr.right - (8.0 * s).round() as i32,
            bottom: cr.top + (80.0 * s).round() as i32,
        };
        let ar = RECT {
            left: cr.left + (8.0 * s).round() as i32,
            top: cr.top + (79.0 * s).round() as i32,
            right: cr.right - (8.0 * s).round() as i32,
            bottom: cr.top + (97.0 * s).round() as i32,
        };
        draw_text(dc, spec.name, dlg.font_section, &nr, rgb(0x111827, 1.0), TextAlign::Center, true, false);
        draw_text(dc, spec.action, dlg.font_sub, &ar, rgb(0x64748B, 1.0), TextAlign::Center, true, false);

        // 只读快捷键徽标：采用浅灰底微描边胶囊样式，仅作按键说明展示，不可点击或修改
        let (sc_w, _) = measure_text(dc, &spec.shortcut, dlg.font_sub);
        let card_w = cr.right - cr.left;
        let pill_w = (sc_w + (18.0 * s).round() as i32)
            .clamp((72.0 * s).round() as i32, (card_w - (20.0 * s).round() as i32).max(1));
        let pill_cx = (cr.left + cr.right) / 2;
        let pill_rc = RECT {
            left: pill_cx - pill_w / 2,
            top: cr.top + (102.0 * s).round() as i32,
            right: pill_cx + (pill_w - pill_w / 2),
            bottom: cr.bottom - (11.0 * s).round() as i32,
        };
        let pill_tc = if hov { rgb(0x334155, 1.0) } else { rgb(0x475569, 1.0) };
        sf.fill_rrect(&pill_rc, 6.0 * s, rgb(0xF1F5F9, 1.0));
        sf.stroke_rrect(&pill_rc, 6.0 * s, (1.0 * s).max(1.0), rgb(0xE2E8F0, 1.0));
        let pill_text_rc = RECT {
            left: pill_rc.left + (6.0 * s).round() as i32,
            top: pill_rc.top,
            right: pill_rc.right - (6.0 * s).round() as i32,
            bottom: pill_rc.bottom,
        };
        draw_text(dc, &spec.shortcut, dlg.font_sub, &pill_text_rc, pill_tc, TextAlign::Center, true, true);
    }
}

/// 删除选中的过期条目（移除目录记录、清理图标缓存并尝试删除失效的本地 .lnk 文件）。
fn delete_expired_indices(dlg: &mut Dlg, app: &mut App, indices: Vec<usize>) {
    let mut removed_ids = Vec::new();
    let mut perm_warn = false;
    let icons_dir = app.store.icons_dir();

    for &idx in &indices {
        let Some(item) = dlg.expired_list.get(idx) else { continue };
        let e = &item.entry;
        removed_ids.push(e.id.clone());
        IconWorker::remove_disk_cache_for_key(&icons_dir, &e.icon_key);
        if e.target.to_lowercase().ends_with(".lnk") && std::path::Path::new(&e.target).is_file() {
            if std::fs::remove_file(&e.target).is_err() {
                perm_warn = true;
            }
        }
    }

    if !removed_ids.is_empty() {
        app.settings.manual.retain(|m| !removed_ids.contains(&m.id));
        for id in &removed_ids {
            app.settings.app_overrides.remove(id);
        }
        app.scan.entries.retain(|e| !removed_ids.contains(&e.id));
        let cache = crate::config::CatalogCache {
            version: 1,
            scanned_at: app.scan.scanned_at,
            entries: app.scan.entries.clone(),
        };
        app.store.save_catalog(&cache);
        app.store.save_settings(&app.settings);
        app.rebuild_view();
        dlg.rebuild_groups(app);
    }

    // 同时清理孤立的失效图标磁盘缓存（仅当全部清理时）
    if indices.len() == dlg.expired_list.len() {
        if let Ok(rd) = std::fs::read_dir(&icons_dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name.ends_with(".icache") && !name.starts_with("v4_") {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }

    dlg.expired_list = catalog::detect_expired(&app.scan.entries, &app.settings.manual);
    dlg.expired_scroll = dlg.expired_scroll.min(dlg.expired_list.len().saturating_sub(1));
    dlg.expired_status = Some(if perm_warn {
        "已从启动台与本地缓存删除（部分公共目录快捷方式需管理员权限）".into()
    } else {
        "已清理全部过期条目及本地失效快捷方式与图标缓存".into()
    });
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
            crate::log::log("设置窗口处理消息时 panic");
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
                dlg.search_edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD | WINDOW_STYLE(ES_LEFT as u32 | ES_AUTOHSCROLL as u32),
                    0,
                    0,
                    10,
                    10,
                    hwnd,
                    HMENU(ID_FOLDER_SEARCH as *mut _),
                    app.hinst,
                    None,
                )
                .unwrap_or_default();
                if !dlg.search_edit.is_invalid() {
                    // EM_SETCUEBANNER = 0x1501
                    let cue = wide("筛选应用或文件夹…");
                    let _ = SendMessageW(dlg.search_edit, 0x1501, WPARAM(1), LPARAM(cue.as_ptr() as isize));
                }
                dlg.rebuild_fonts();
                dlg.sync_search_edit_pos();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint_dlg(dlg, app);
                LRESULT(0)
            }
            WM_ACTIVATE => {
                // 从添加应用弹窗返回时同步最新的手动条目列表
                if loword(wparam.0) as u32 != WA_INACTIVE {
                    dlg.rebuild_groups(app);
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_APP_SCAN => {
                dlg.rebuild_groups(app);
                let l = dlg.layout();
                dlg.request_visible_app_icons(app, &l);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let new = lparam.0 as *const RECT;
                let r = *new;
                let dpi = hiword(wparam.0) as u32;
                dlg.scale = dpi as f32 / 96.0;
                dlg.rebuild_fonts();
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                dlg.sync_search_edit_pos();
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_CTLCOLOREDIT => {
                if lparam.0 as isize == dlg.search_edit.0 as isize {
                    let hdc = HDC(wparam.0 as *mut _);
                    SetTextColor(hdc, COLORREF(0x0027_1811));
                    SetBkColor(hdc, COLORREF(0x00FF_FFFF));
                    return LRESULT(dlg.search_brush.0 as isize);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                if loword(wparam.0) as i32 == ID_FOLDER_SEARCH && hiword(wparam.0) as u32 == EN_CHANGE {
                    dlg.on_search_changed();
                    let l = dlg.layout();
                    dlg.request_visible_app_icons(app, &l);
                    let _ = InvalidateRect(hwnd, None, false);
                }
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
                    let (done, restart) = dlg.about.poll();
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
                let x = get_x_lparam(lparam.0);
                let y = get_y_lparam(lparam.0);
                let hit = dlg.hit_test(&l, x, y);
                update_tip(dlg, app, &l, hit);
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
                if dlg.tab == Tab::About {
                    let l = dlg.layout();
                    let mut pt = POINT { x: get_x_lparam(lparam.0), y: get_y_lparam(lparam.0) };
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                    if dlg.about.scroll(&l.about, pt.x, pt.y, get_wheel_delta(wparam.0)) {
                        dlg.hover = dlg.hit_test(&l, pt.x, pt.y);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if dlg.tab == Tab::Folders {
                    let l = dlg.layout();
                    let mut pt = POINT { x: get_x_lparam(lparam.0), y: get_y_lparam(lparam.0) };
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                    let delta = get_wheel_delta(wparam.0);
                    let step = if delta > 0 { -2isize } else { 2isize };
                    if inside(&l.left_pane, pt.x, pt.y) {
                        let max_s = dlg.categories.len().saturating_sub(l.folder_vis) as isize;
                        dlg.cat_scroll = (dlg.cat_scroll as isize + step).clamp(0, max_s) as usize;
                    } else if inside(&l.right_pane, pt.x, pt.y) {
                        if dlg.show_expired {
                            let max_s = dlg.expired_list.len().saturating_sub(l.exp_vis) as isize;
                            dlg.expired_scroll = (dlg.expired_scroll as isize + step).clamp(0, max_s) as usize;
                        } else {
                            let max_s = dlg.tree_rows.len().saturating_sub(l.app_vis) as isize;
                            dlg.app_scroll = (dlg.app_scroll as isize + step).clamp(0, max_s) as usize;
                            dlg.request_visible_app_icons(app, &l);
                        }
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let l = dlg.layout();
                let x = get_x_lparam(lparam.0);
                let y = get_y_lparam(lparam.0);
                let hit = dlg.hit_test(&l, x, y);
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
                        Hit::Tab(t) => {
                            dlg.tab = t;
                            dlg.capture_hk = false;
                            dlg.hk_msg = None;
                            dlg.sync_search_edit_pos();
                            if t == Tab::Folders {
                                dlg.request_visible_app_icons(app, &l);
                            }
                        }
                        Hit::Rescan => {
                            app.start_scan();
                        }
                        Hit::CheckExpired => {
                            dlg.tab = Tab::Folders;
                            dlg.sync_search_edit_pos();
                            dlg.expired_list = catalog::detect_expired(&app.scan.entries, &app.settings.manual);
                            dlg.expired_scroll = 0;
                            dlg.expired_status = None;
                            dlg.show_expired = true;
                        }
                        Hit::ExpiredBack => {
                            dlg.show_expired = false;
                        }
                        Hit::ExpiredDeleteOne(idx) => {
                            delete_expired_indices(dlg, app, vec![idx]);
                        }
                        Hit::ExpiredDeleteAll => {
                            let all: Vec<usize> = (0..dlg.expired_list.len()).collect();
                            delete_expired_indices(dlg, app, all);
                        }
                        Hit::HkModify => {
                            dlg.capture_hk = !dlg.capture_hk;
                            dlg.hk_msg = None;
                            let _ = SetFocus(hwnd);
                        }
                        Hit::HkDefault => {
                            dlg.capture_hk = false;
                            dlg.hk_msg = None;
                            let def = HotkeySpec::default();
                            if let Err(e) = app.apply_hotkey(def) {
                                let m = wide(&e);
                                let _ = MessageBoxW(hwnd, PCWSTR(m.as_ptr()), w!("快捷键冲突"), MB_OK | MB_ICONWARNING);
                            } else {
                                app.store.save_settings(&app.settings);
                            }
                        }
                        Hit::ToggleStartup => {
                            let next = !dlg.start_with_windows;
                            match startup::set_enabled(next) {
                                Ok(_) => {
                                    dlg.start_with_windows = next;
                                    app.settings.start_with_windows = next;
                                    app.store.save_settings(&app.settings);
                                }
                                Err(e) => {
                                    let m = wide(&e);
                                    let _ = MessageBoxW(hwnd, PCWSTR(m.as_ptr()), w!("开机启动设置"), MB_OK | MB_ICONWARNING);
                                }
                            }
                        }
                        Hit::ToggleWallpaper => {
                            app.settings.use_wallpaper_blur = !app.settings.use_wallpaper_blur;
                            app.store.save_settings(&app.settings);
                            if app.settings.use_wallpaper_blur {
                                app.ensure_wallpaper_async();
                            }
                            if app.visible {
                                let _ = InvalidateRect(app.hwnd, None, false);
                            }
                        }
                        Hit::IconPreset(i) => {
                            if let Some(&(dp, _)) = ICON_SIZE_PRESETS.get(i) {
                                if app.settings.icon_size_dp != dp {
                                    app.settings.icon_size_dp = dp;
                                    dlg.commit_layout_settings(app);
                                }
                            }
                        }
                        Hit::RoundnessPreset(i) => {
                            if let Some(&(mode, _, _)) = ICON_ROUNDNESS_PRESETS.get(i) {
                                if app.settings.icon_roundness != mode {
                                    app.settings.icon_roundness = mode;
                                    dlg.commit_layout_settings(app);
                                }
                            }
                        }
                        Hit::ColsAuto => {
                            if app.settings.grid_cols != 0 {
                                app.settings.grid_cols = 0;
                                dlg.commit_layout_settings(app);
                            }
                        }
                        Hit::ColsDec | Hit::ColsInc => {
                            let (ac, _) = app.recommended_grid();
                            let base = if app.settings.grid_cols > 0 { app.settings.grid_cols } else { ac };
                            let delta = if hit == Hit::ColsDec { -1 } else { 1 };
                            let next = (base + delta).clamp(4, 10);
                            if next != app.settings.grid_cols {
                                app.settings.grid_cols = next;
                                dlg.commit_layout_settings(app);
                            }
                        }
                        Hit::RowsAuto => {
                            if app.settings.grid_rows != 0 {
                                app.settings.grid_rows = 0;
                                dlg.commit_layout_settings(app);
                            }
                        }
                        Hit::RowsDec | Hit::RowsInc => {
                            let (_, ar) = app.recommended_grid();
                            let base = if app.settings.grid_rows > 0 { app.settings.grid_rows } else { ar };
                            let delta = if hit == Hit::RowsDec { -1 } else { 1 };
                            let next = (base + delta).clamp(3, 8);
                            if next != app.settings.grid_rows {
                                app.settings.grid_rows = next;
                                dlg.commit_layout_settings(app);
                            }
                        }
                        Hit::LayoutReset => {
                            if app.settings.icon_size_dp != 56 || app.settings.icon_roundness != 1 || app.settings.grid_cols != 0 || app.settings.grid_rows != 0 {
                                app.settings.icon_size_dp = 56;
                                app.settings.icon_roundness = 1;
                                app.settings.grid_cols = 0;
                                app.settings.grid_rows = 0;
                                dlg.commit_layout_settings(app);
                            }
                        }
                        Hit::FolderSearchBox => {
                            let _ = SetFocus(dlg.search_edit);
                        }
                        Hit::FolderSelectAll => {
                            let all_checked = dlg.categories.iter()
                                .filter(|c| !c.is_manual)
                                .all(|c| dlg.category_check_state(app, c) == CheckState::Checked);
                            let next = !all_checked;
                            for cat in &dlg.categories {
                                if !cat.is_manual {
                                    for g in &cat.subfolders {
                                        app.settings.folder_overrides.insert(g.id.clone(), next);
                                        for e in &g.entries {
                                            app.settings.app_overrides.remove(&e.id);
                                        }
                                    }
                                }
                            }
                            dlg.commit_filter_settings(app);
                        }
                        Hit::FolderResetDefault => {
                            app.settings.folder_overrides.clear();
                            app.settings.app_overrides.clear();
                            dlg.commit_filter_settings(app);
                        }
                        Hit::CategoryRow(ci) => {
                            dlg.show_expired = false;
                            dlg.selected_cat = ci;
                            dlg.app_scroll = 0;
                            dlg.rebuild_tree();
                            dlg.request_visible_app_icons(app, &l);
                        }
                        Hit::CategoryCheck(ci) => {
                            dlg.show_expired = false;
                            dlg.toggle_category(app, ci);
                            dlg.selected_cat = ci;
                            dlg.app_scroll = 0;
                            dlg.rebuild_tree();
                            dlg.request_visible_app_icons(app, &l);
                        }
                        Hit::TreeExpandAll => {
                            let collapse = dlg.all_tree_expanded();
                            if let Some(cat) = dlg.categories.get(dlg.selected_cat) {
                                for g in &cat.subfolders {
                                    if collapse {
                                        dlg.expanded_folders.remove(&g.id);
                                    } else {
                                        dlg.expanded_folders.insert(g.id.clone());
                                    }
                                }
                            }
                            dlg.rebuild_tree();
                            dlg.request_visible_app_icons(app, &l);
                        }
                        Hit::AppSelectAllInCat => {
                            dlg.toggle_category(app, dlg.selected_cat);
                        }
                        Hit::TreeRowToggle(idx) => {
                            if let (Some(cat), Some(TreeRow::Folder { sub_idx })) =
                                (dlg.categories.get(dlg.selected_cat), dlg.tree_rows.get(idx))
                            {
                                if let Some(g) = cat.subfolders.get(*sub_idx) {
                                    let id = g.id.clone();
                                    if !dlg.expanded_folders.remove(&id) {
                                        dlg.expanded_folders.insert(id);
                                    }
                                }
                            }
                            dlg.rebuild_tree();
                            dlg.request_visible_app_icons(app, &l);
                        }
                        Hit::TreeRowCheck(idx) => {
                            if let Some(TreeRow::Folder { sub_idx }) = dlg.tree_rows.get(idx).cloned() {
                                dlg.toggle_subfolder(app, sub_idx);
                            }
                        }
                        Hit::TreeRowItem(idx) => {
                            if let Some(TreeRow::App { sub_idx, entry_idx, .. }) = dlg.tree_rows.get(idx).cloned() {
                                dlg.toggle_entry(app, sub_idx, entry_idx);
                            }
                        }
                        Hit::AppRemoveManual(idx) => {
                            if let (Some(cat), Some(TreeRow::App { sub_idx, entry_idx, .. })) =
                                (dlg.categories.get(dlg.selected_cat), dlg.tree_rows.get(idx).cloned())
                            {
                                if let Some(e) = cat.subfolders.get(sub_idx).and_then(|g| g.entries.get(entry_idx)) {
                                    let id = e.id.clone();
                                    app.settings.manual.retain(|m| m.id != id);
                                    app.store.save_settings(&app.settings);
                                    app.rebuild_view();
                                    dlg.rebuild_groups(app);
                                    let max_s = dlg.tree_rows.len().saturating_sub(l.app_vis);
                                    dlg.app_scroll = dlg.app_scroll.min(max_s);
                                }
                            }
                        }
                        Hit::AddManualBtn => {
                            crate::add::open(app);
                        }
                        Hit::RefreshIntervalPreset(i) => {
                            if let Some(&(secs, _)) = SCAN_INTERVAL_PRESETS.get(i) {
                                if app.settings.scan_interval_secs != secs {
                                    app.settings.scan_interval_secs = secs;
                                    app.store.save_settings(&app.settings);
                                    app.sync_scan_timer();
                                }
                            }
                        }
                        Hit::ToggleScanOnShow => {
                            app.settings.scan_on_show = !app.settings.scan_on_show;
                            app.store.save_settings(&app.settings);
                        }
                        Hit::ToggleEventWatch => {
                            app.settings.event_watch_enabled = !app.settings.event_watch_enabled;
                            app.store.save_settings(&app.settings);
                            app.sync_watcher();
                        }
                        Hit::ToggleRefreshChangedIcons => {
                            app.settings.refresh_changed_icons = !app.settings.refresh_changed_icons;
                            app.store.save_settings(&app.settings);
                        }
                        Hit::RebuildIconsCache => {
                            app.rebuild_all_icons();
                        }
                        Hit::RefreshReset => {
                            if app.settings.scan_interval_secs != 30
                                || !app.settings.event_watch_enabled
                                || app.settings.scan_on_show
                                || !app.settings.refresh_changed_icons
                            {
                                app.settings.scan_interval_secs = 30;
                                app.settings.event_watch_enabled = true;
                                app.settings.scan_on_show = false;
                                app.settings.refresh_changed_icons = true;
                                app.store.save_settings(&app.settings);
                                app.sync_scan_timer();
                                app.sync_watcher();
                            }
                        }
                        Hit::ToggleGesture => {
                            let next = !app.settings.gestures_enabled;
                            app.apply_gestures(next);
                            app.store.save_settings(&app.settings);
                        }
                        Hit::ToggleGestureSwap => {
                            app.settings.gesture_swap_lr = !app.settings.gesture_swap_lr;
                            app.store.save_settings(&app.settings);
                        }
                        Hit::GestureThreshDef => {
                            if app.settings.gesture_threshold != DEFAULT_GESTURE_THRESHOLD {
                                app.apply_gesture_threshold(DEFAULT_GESTURE_THRESHOLD);
                                app.store.save_settings(&app.settings);
                            }
                        }
                        Hit::GestureThreshDec | Hit::GestureThreshInc => {
                            let cur = clamp_gesture_threshold(app.settings.gesture_threshold);
                            let next = if hit == Hit::GestureThreshDec {
                                cur.saturating_sub(GESTURE_THRESHOLD_STEP).max(MIN_GESTURE_THRESHOLD)
                            } else {
                                cur.saturating_add(GESTURE_THRESHOLD_STEP).min(MAX_GESTURE_THRESHOLD)
                            };
                            if next != cur {
                                app.apply_gesture_threshold(next);
                                app.store.save_settings(&app.settings);
                            }
                        }
                        Hit::About(action) => match action {
                            about::Action::Version(index) => dlg.about.toggle(&l.about, index),
                            about::Action::GitHub => open_shell_path("open", "https://github.com/CrazyFigure/WinLaunchpad", None),
                            about::Action::Cnb => open_shell_path("open", "https://cnb.cool/crazy-figure/WinLaunchpad", None),
                            about::Action::DataDir => open_shell_path("open", &app.store.dir.to_string_lossy(), None),
                            about::Action::Check | about::Action::Install => {
                                // 更新期间清除悬停提示，仅在后台任务存在时轮询。
                                dlg.tip_pending = None;
                                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                                if dlg.about.start(action) {
                                    let _ = SetTimer(hwnd, TIMER_UPDATE, 100, None);
                                }
                            }
                        },
                        _ => {}
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN if dlg.capture_hk => {
                let vk = wparam.0 as u32;
                if vk == VK_ESCAPE.0 as u32 {
                    dlg.capture_hk = false;
                    dlg.hk_msg = None;
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if hotkey::is_modifier(vk) {
                    return LRESULT(0);
                }
                let mods = hotkey::current_mods();
                if mods & !(MOD_SHIFT.0 as u32) == 0 && vk != 0x09 {
                    let is_f = (0x70..=0x87).contains(&vk);
                    if mods == 0 && !is_f {
                        dlg.hk_msg = Some("请至少包含 Ctrl / Alt / Win 之一（Esc 取消）".into());
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                }
                let spec = HotkeySpec { mods: mods | MOD_NOREPEAT.0 as u32, vk };
                if let Err(e) = app.apply_hotkey(spec) {
                    let m = wide(&e);
                    let _ = MessageBoxW(hwnd, PCWSTR(m.as_ptr()), w!("快捷键冲突"), MB_OK | MB_ICONWARNING);
                    dlg.hk_msg = Some("快捷键冲突，请重按（Esc 取消）".into());
                } else {
                    dlg.capture_hk = false;
                    dlg.hk_msg = None;
                    app.store.save_settings(&app.settings);
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(hwnd, TIMER_TOOLTIP);
                let _ = KillTimer(hwnd, TIMER_UPDATE);
                app.modal = app.modal.saturating_sub(1);
                app.settings_hwnd = HWND::default();
                if !dlg.search_brush.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(dlg.search_brush.0));
                }
                dlg.destroy_fonts();
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

    #[test]
    fn gesture_card_specs_reflect_swap_and_hotkey() {
        let normal = gesture_card_specs(false, "Ctrl + Alt + Shift + A");
        assert_eq!(normal[0].action, "切换桌面 · 向右");
        assert_eq!(normal[0].shortcut, "Ctrl + Win + →");
        assert_eq!(normal[1].action, "切换桌面 · 向左");
        assert_eq!(normal[1].shortcut, "Ctrl + Win + ←");
        assert_eq!(normal[2].shortcut, "Ctrl + Alt + Tab");
        assert_eq!(normal[3].shortcut, "Ctrl + Alt + Shift + A");
        assert_eq!(normal[4].shortcut, "Win + Tab");

        let swapped = gesture_card_specs(true, "Alt + Space");
        assert_eq!(swapped[0].action, "切换桌面 · 向左");
        assert_eq!(swapped[0].shortcut, "Ctrl + Win + ←");
        assert_eq!(swapped[1].action, "切换桌面 · 向右");
        assert_eq!(swapped[1].shortcut, "Ctrl + Win + →");
        assert_eq!(swapped[3].shortcut, "Alt + Space");
    }

    #[test]
    fn gesture_shortcut_text_fits_card_width() {
        let mut target = DibTarget::default();
        assert!(target.ensure(240, 160));
        let dc = target.mem_dc.expect("测试 DIB DC 应已创建");
        let font = make_font(12, 400);
        let specs = gesture_card_specs(false, "Ctrl + Alt + Shift + A");
        // 标准窗口宽度下每张手势卡宽约 193px，预留左右内边距后快捷键文案应完整容纳
        for spec in &specs {
            let (tw, _) = measure_text(dc, &spec.shortcut, font);
            assert!(tw > 0 && tw + 18 <= 173, "快捷键过宽: {} ({tw}px)", spec.shortcut);
        }
        let (pw, _) = measure_text(dc, "95%", font);
        assert!(pw > 0 && pw + 12 <= 76, "手势匹配度数值框过宽 ({pw}px)");
        let (dw, _) = measure_text(dc, "默认", font);
        assert!(dw > 0 && dw + 10 <= 48, "默认按钮过宽 ({dw}px)");
        unsafe {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        target.release();
    }

    #[test]
    fn refresh_tab_labels_fit_buttons() {
        let mut target = DibTarget::default();
        assert!(target.ensure(240, 160));
        let dc = target.mem_dc.expect("测试 DIB DC 应已创建");
        let font = make_font(12, 400);
        for &(_, label) in SCAN_INTERVAL_PRESETS {
            let (tw, _) = measure_text(dc, label, font);
            assert!(tw > 0 && tw + 10 <= 56, "扫描档位标签过宽: {label} ({tw}px)");
        }
        let (rw, _) = measure_text(dc, "重建图标缓存", font);
        assert!(rw > 0 && rw + 14 <= 104, "重建图标缓存按钮过宽 ({rw}px)");
        unsafe {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        target.release();
    }
}
