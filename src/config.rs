//! 用户设置与应用目录缓存的持久化（%LOCALAPPDATA%\WinLaunchpad）。
//! 写入方式：先写 .tmp 再 rename 覆盖，设置文件额外保留一份 .bak。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::AppEntry;
use crate::hotkey::HotkeySpec;
use crate::log;

pub const SETTINGS_VERSION: u32 = 2;

/// 默认启动后首次自动检测更新延迟（10 秒，避免与启动扫描争抢资源）。
pub const AUTO_UPDATE_STARTUP_DELAY_MS: u32 = 10_000;
/// 后台自动检测更新默认周期（4 小时）。
pub const AUTO_UPDATE_INTERVAL_SECS: u64 = 4 * 3600;

fn default_true() -> bool {
    true
}

fn default_icon_size() -> i32 {
    56
}

/// 启动台图标尺寸预设档位（5 档，默认 56 = 标准）。
pub const ICON_SIZE_PRESETS: &[(i32, &str)] = &[
    (44, "极小"),
    (50, "偏小"),
    (56, "标准"),
    (64, "偏大"),
    (72, "超大"),
];

/// 将任意尺寸吸附到最接近的预设档位。
pub fn snap_icon_size_dp(dp: i32) -> i32 {
    ICON_SIZE_PRESETS
        .iter()
        .min_by_key(|&&(v, _)| (v - dp).abs())
        .map(|&(v, _)| v)
        .unwrap_or(56)
}

fn default_roundness() -> u8 {
    1
}

/// 默认手势识别最低匹配度百分比（70%）。
pub const DEFAULT_GESTURE_THRESHOLD: u8 = 70;
/// 手势识别匹配度下限（50%）。
pub const MIN_GESTURE_THRESHOLD: u8 = 50;
/// 手势识别匹配度上限（95%）。
pub const MAX_GESTURE_THRESHOLD: u8 = 95;
/// 设置面板步进调节步长（5%）。
pub const GESTURE_THRESHOLD_STEP: u8 = 5;

fn default_gesture_threshold() -> u8 {
    DEFAULT_GESTURE_THRESHOLD
}

/// 将手势匹配度百分比限制在有效范围（50..=95），0 视为旧配置缺省并回落到默认值 70。
pub fn clamp_gesture_threshold(pct: u8) -> u8 {
    if pct == 0 {
        DEFAULT_GESTURE_THRESHOLD
    } else {
        pct.clamp(MIN_GESTURE_THRESHOLD, MAX_GESTURE_THRESHOLD)
    }
}

fn default_scan_interval_secs() -> u32 {
    30
}

/// 后台定时自动扫描间隔预设档位（0 表示关闭定时扫描，默认 30 秒）。
pub const SCAN_INTERVAL_PRESETS: &[(u32, &str)] = &[
    (10, "10 秒"),
    (20, "20 秒"),
    (30, "30 秒"),
    (60, "1 分钟"),
    (300, "5 分钟"),
    (0, "关闭"),
];

/// 将任意秒数吸附到最接近的定时扫描间隔预设档位（0 保留为关闭）。
pub fn snap_scan_interval_secs(secs: u32) -> u32 {
    if secs == 0 {
        return 0;
    }
    SCAN_INTERVAL_PRESETS
        .iter()
        .filter(|&&(v, _)| v > 0)
        .min_by_key(|&&(v, _)| (v as i64 - secs as i64).abs())
        .map(|&(v, _)| v)
        .unwrap_or(30)
}

/// 图标圆角圆润度 3 档设置（0=微圆角, 1=标准, 2=圆润）。
pub const ICON_ROUNDNESS_PRESETS: &[(u8, &str, f32)] = &[
    (0, "微圆角", 0.12),
    (1, "标准", 0.225),
    (2, "圆润", 0.34),
];

/// 返回圆角档位对应的圆角半径比例。
pub fn roundness_ratio(mode: u8) -> f32 {
    ICON_ROUNDNESS_PRESETS
        .get(mode as usize)
        .map(|x| x.2)
        .unwrap_or(0.225)
}

/// 用户在启动台中创建的自定义文件夹。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaunchpadFolder {
    /// 文件夹稳定唯一标识
    pub id: String,
    /// 文件夹显示名称
    pub name: String,
    /// 归属到该文件夹内的应用 ID（对应 `AppEntry::id`）
    #[serde(default)]
    pub app_ids: Vec<String>,
}

static NEXT_FOLDER_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub hotkey: HotkeySpec,
    #[serde(default = "default_true")]
    pub start_with_windows: bool,
    /// 是否启用全局右键手势（默认开启）。
    #[serde(default = "default_true")]
    pub gestures_enabled: bool,
    /// 左右手势切换桌面方向是否反转（默认关闭）。
    #[serde(default)]
    pub gesture_swap_lr: bool,
    /// 手势识别最低匹配度百分比（50..=95，默认 70）
    #[serde(default = "default_gesture_threshold")]
    pub gesture_threshold: u8,
    /// 启动台使用桌面壁纸毛玻璃背景
    #[serde(default = "default_true")]
    pub use_wallpaper_blur: bool,
    /// 启动台图标基准尺寸（逻辑 dp，取 ICON_SIZE_PRESETS 档位，默认 56）
    #[serde(default = "default_icon_size")]
    pub icon_size_dp: i32,
    /// 启动台图标圆角圆润度（0=微圆角, 1=标准, 2=圆润）
    #[serde(default = "default_roundness")]
    pub icon_roundness: u8,
    /// 启动台每页列数（0 表示按屏幕分辨率自动计算；非 0 范围 4..=10）
    #[serde(default)]
    pub grid_cols: i32,
    /// 启动台每页行数（0 表示按屏幕分辨率自动计算；非 0 范围 3..=8）
    #[serde(default)]
    pub grid_rows: i32,
    /// 文件夹级展示开关覆盖（folder_id -> 是否显示，未配置时按默认常用策略决定）
    #[serde(default)]
    pub folder_overrides: HashMap<String, bool>,
    /// 单应用级展示开关覆盖（entry_id -> 是否显示，优先级高于文件夹级设置）
    #[serde(default)]
    pub app_overrides: HashMap<String, bool>,
    /// 兼容旧版：自动发现条目隐藏列表（加载时自动迁移到 app_overrides）
    #[serde(default)]
    pub hidden: Vec<String>,
    /// 用户手动添加的条目
    #[serde(default)]
    pub manual: Vec<AppEntry>,
    /// 用户在启动台中创建的文件夹列表
    #[serde(default)]
    pub folders: Vec<LaunchpadFolder>,
    /// 后台定时扫描刷新间隔（秒，0 表示关闭定时扫描，默认 30）
    #[serde(default = "default_scan_interval_secs")]
    pub scan_interval_secs: u32,
    /// 是否启用安装/卸载事件驱动监听（默认开启）
    #[serde(default = "default_true")]
    pub event_watch_enabled: bool,
    /// 展开启动台面板时是否在后台静默刷新一次（默认关闭）
    #[serde(default)]
    pub scan_on_show: bool,
    /// 检测到快捷方式或程序变更时自动更新图标缓存（默认开启）
    #[serde(default = "default_true")]
    pub refresh_changed_icons: bool,
    /// 是否定时自动检测软件新版本（默认开启）
    #[serde(default = "default_true")]
    pub auto_check_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: SETTINGS_VERSION,
            hotkey: HotkeySpec::default(),
            start_with_windows: true,
            gestures_enabled: true,
            gesture_swap_lr: false,
            gesture_threshold: DEFAULT_GESTURE_THRESHOLD,
            use_wallpaper_blur: true,
            icon_size_dp: default_icon_size(),
            icon_roundness: default_roundness(),
            grid_cols: 0,
            grid_rows: 0,
            folder_overrides: HashMap::new(),
            app_overrides: HashMap::new(),
            hidden: Vec::new(),
            manual: Vec::new(),
            folders: Vec::new(),
            scan_interval_secs: default_scan_interval_secs(),
            event_watch_enabled: true,
            scan_on_show: false,
            refresh_changed_icons: true,
            auto_check_update: true,
        }
    }
}

impl Settings {
    /// 创建一个新的启动台文件夹并返回其唯一标识；名称为空白时返回 `None`。
    pub fn create_folder(&mut self, name: &str) -> Option<String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return None;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq = NEXT_FOLDER_SEQ.fetch_add(1, Ordering::Relaxed);
        let id = format!("fld:{ts:x}_{seq}");
        self.folders.push(LaunchpadFolder {
            id: id.clone(),
            name: trimmed.to_string(),
            app_ids: Vec::new(),
        });
        Some(id)
    }

    /// 重命名指定文件夹；名称空白或未找到文件夹或名称未变更时返回 `false`。
    pub fn rename_folder(&mut self, folder_id: &str, name: &str) -> bool {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Some(folder) = self.folders.iter_mut().find(|f| f.id == folder_id) else {
            return false;
        };
        if folder.name == trimmed {
            return false;
        }
        folder.name = trimmed.to_string();
        true
    }

    /// 删除指定文件夹，其内部应用 ID 随之恢复到启动台主网格展示。
    pub fn delete_folder(&mut self, folder_id: &str) -> bool {
        let before = self.folders.len();
        self.folders.retain(|f| f.id != folder_id);
        self.folders.len() != before
    }

    /// 将指定应用移动至目标文件夹（先从其他文件夹移除，保证单应用最多归属一个文件夹）。
    pub fn move_app_to_folder(&mut self, app_id: &str, folder_id: &str) -> bool {
        if !self.folders.iter().any(|f| f.id == folder_id) {
            return false;
        }
        let mut changed = false;
        for f in &mut self.folders {
            if f.id == folder_id {
                if !f.app_ids.iter().any(|id| id == app_id) {
                    f.app_ids.push(app_id.to_string());
                    changed = true;
                }
            } else {
                let prev = f.app_ids.len();
                f.app_ids.retain(|id| id != app_id);
                if f.app_ids.len() != prev {
                    changed = true;
                }
            }
        }
        changed
    }

    /// 将指定应用从所有文件夹中移除（恢复到启动台主网格）。
    pub fn remove_app_from_folders(&mut self, app_id: &str) -> bool {
        let mut changed = false;
        for f in &mut self.folders {
            let prev = f.app_ids.len();
            f.app_ids.retain(|id| id != app_id);
            if f.app_ids.len() != prev {
                changed = true;
            }
        }
        changed
    }

    /// 清理文件夹列表中的空名称、重复文件夹 ID 及跨文件夹重复应用 ID。
    pub fn sanitize_folders(&mut self) {
        let mut seen_folders = HashSet::new();
        let mut seen_apps = HashSet::new();
        self.folders.retain_mut(|f| {
            let name = f.name.trim().to_string();
            if f.id.is_empty() || name.is_empty() || !seen_folders.insert(f.id.clone()) {
                return false;
            }
            f.name = name;
            f.app_ids.retain(|aid| !aid.is_empty() && seen_apps.insert(aid.clone()));
            true
        });
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CatalogCache {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub scanned_at: u64,
    #[serde(default)]
    pub entries: Vec<AppEntry>,
}

pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Store { dir }
    }

    pub fn settings_exists(&self) -> bool {
        self.dir.join("settings.json").exists()
    }

    pub fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    pub fn catalog_path(&self) -> PathBuf {
        self.dir.join("catalog.json")
    }

    pub fn icons_dir(&self) -> PathBuf {
        self.dir.join("icons")
    }

    pub fn wallpaper_cache_path(&self) -> PathBuf {
        self.dir.join("wallpaper_blur.bin")
    }

    fn read_json<T: serde::de::DeserializeOwned>(path: &Path, what: &str) -> Option<T> {
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str::<T>(&text) {
            Ok(v) => Some(v),
            Err(e) => {
                log::log(&format!("{what} 解析失败 {}: {e}", path.display()));
                None
            }
        }
    }

    /// 读取设置；主文件损坏时尝试 .bak，并迁移旧版 hidden 列表到 app_overrides。
    pub fn load_settings(&self) -> Settings {
        let p = self.settings_path();
        let mut s = Store::read_json::<Settings>(&p, "设置")
            .or_else(|| Store::read_json::<Settings>(&self.bak_of(&p), "设置备份"))
            .unwrap_or_default();
        if !s.hidden.is_empty() {
            for id in std::mem::take(&mut s.hidden) {
                s.app_overrides.entry(id).or_insert(false);
            }
        }
        if s.version < SETTINGS_VERSION {
            s.version = SETTINGS_VERSION;
        }
        s.icon_size_dp = snap_icon_size_dp(s.icon_size_dp);
        s.icon_roundness = s.icon_roundness.min(2);
        s.grid_cols = if s.grid_cols <= 0 { 0 } else { s.grid_cols.clamp(4, 10) };
        s.grid_rows = if s.grid_rows <= 0 { 0 } else { s.grid_rows.clamp(3, 8) };
        s.scan_interval_secs = snap_scan_interval_secs(s.scan_interval_secs);
        s.gesture_threshold = clamp_gesture_threshold(s.gesture_threshold);
        s.sanitize_folders();
        s
    }

    fn bak_of(&self, p: &Path) -> PathBuf {
        let mut name = p.file_name().unwrap().to_os_string();
        name.push(".bak");
        p.with_file_name(name)
    }

    /// 写入 JSON：先 .tmp，再覆盖；设置文件先把旧文件复制为 .bak。
    fn write_json<T: serde::Serialize>(&self, path: &Path, value: &T, keep_bak: bool) -> bool {
        let text = match serde_json::to_string_pretty(value) {
            Ok(t) => t,
            Err(e) => {
                log::log(&format!("序列化失败 {}: {e}", path.display()));
                return false;
            }
        };
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, text.as_bytes()) {
            log::log(&format!("写入失败 {}: {e}", tmp.display()));
            return false;
        }
        if keep_bak && path.exists() {
            let _ = std::fs::copy(path, self.bak_of(path));
        }
        match std::fs::rename(&tmp, path) {
            Ok(_) => true,
            Err(e) => {
                log::log(&format!("替换失败 {}: {e}", path.display()));
                let _ = std::fs::remove_file(&tmp);
                false
            }
        }
    }

    pub fn save_settings(&self, s: &Settings) -> bool {
        self.write_json(&self.settings_path(), s, true)
    }

    pub fn load_catalog(&self) -> Option<CatalogCache> {
        Store::read_json::<CatalogCache>(&self.catalog_path(), "应用目录缓存")
    }

    pub fn save_catalog(&self, c: &CatalogCache) -> bool {
        self.write_json(&self.catalog_path(), c, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_interval_snaps_to_presets() {
        assert_eq!(snap_scan_interval_secs(0), 0);
        assert_eq!(snap_scan_interval_secs(10), 10);
        assert_eq!(snap_scan_interval_secs(20), 20);
        assert_eq!(snap_scan_interval_secs(27), 30);
        assert_eq!(snap_scan_interval_secs(60), 60);
        assert_eq!(snap_scan_interval_secs(290), 300);
    }

    #[test]
    fn legacy_settings_json_gets_refresh_defaults() {
        let s: Settings = serde_json::from_str(r#"{"version":2}"#).unwrap();
        assert_eq!(s.scan_interval_secs, 30);
        assert!(s.event_watch_enabled);
        assert!(!s.scan_on_show);
        assert!(s.refresh_changed_icons);
        assert!(s.auto_check_update);
        assert_eq!(s.gesture_threshold, 70);
    }

    #[test]
    fn gesture_settings_default_to_enabled_without_reversing_desktops() {
        // 首次运行和旧配置缺少手势字段时采用同一组默认值。
        let fresh = Settings::default();
        let legacy: Settings = serde_json::from_str(r#"{"version":2}"#).unwrap();

        assert!(fresh.gestures_enabled);
        assert!(!fresh.gesture_swap_lr);
        assert!(legacy.gestures_enabled);
        assert!(!legacy.gesture_swap_lr);
    }

    #[test]
    fn gesture_settings_respect_saved_preferences() {
        // 配置中的显式开关值优先于默认值。
        let saved: Settings = serde_json::from_str(
            r#"{"version":2,"gestures_enabled":false,"gesture_swap_lr":true}"#,
        )
        .unwrap();

        assert!(!saved.gestures_enabled);
        assert!(saved.gesture_swap_lr);
    }

    #[test]
    fn gesture_threshold_clamps_within_valid_range() {
        assert_eq!(clamp_gesture_threshold(0), 70);
        assert_eq!(clamp_gesture_threshold(10), 50);
        assert_eq!(clamp_gesture_threshold(70), 70);
        assert_eq!(clamp_gesture_threshold(85), 85);
        assert_eq!(clamp_gesture_threshold(100), 95);
    }

    #[test]
    fn folder_crud_and_uniqueness_work() {
        let mut s = Settings::default();
        assert!(s.create_folder("   ").is_none());
        let f1 = s.create_folder(" 办公 ").unwrap();
        let f2 = s.create_folder("游戏").unwrap();
        assert_ne!(f1, f2);
        assert_eq!(s.folders[0].name, "办公");

        assert!(s.move_app_to_folder("app1", &f1));
        assert!(!s.move_app_to_folder("app1", &f1));
        assert_eq!(s.folders[0].app_ids, vec!["app1"]);

        // 移动到新文件夹时应自动从旧文件夹迁出
        assert!(s.move_app_to_folder("app1", &f2));
        assert!(s.folders[0].app_ids.is_empty());
        assert_eq!(s.folders[1].app_ids, vec!["app1"]);

        assert!(s.rename_folder(&f2, "娱乐工具"));
        assert_eq!(s.folders[1].name, "娱乐工具");
        assert!(!s.rename_folder(&f2, "   "));

        assert!(s.remove_app_from_folders("app1"));
        assert!(s.folders[1].app_ids.is_empty());

        assert!(s.delete_folder(&f1));
        assert_eq!(s.folders.len(), 1);
    }
}
