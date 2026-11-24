//! 用户设置与应用目录缓存的持久化（%LOCALAPPDATA%\WinLaunchpad）。
//! 写入方式：先写 .tmp 再 rename 覆盖，设置文件额外保留一份 .bak。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::catalog::AppEntry;
use crate::hotkey::HotkeySpec;
use crate::log;

pub const SETTINGS_VERSION: u32 = 2;

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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub hotkey: HotkeySpec,
    #[serde(default = "default_true")]
    pub start_with_windows: bool,
    #[serde(default)]
    pub gestures_enabled: bool,
    /// 左右手势切桌面方向反转
    #[serde(default)]
    pub gesture_swap_lr: bool,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: SETTINGS_VERSION,
            hotkey: HotkeySpec::default(),
            start_with_windows: true,
            gestures_enabled: false,
            gesture_swap_lr: false,
            use_wallpaper_blur: true,
            icon_size_dp: default_icon_size(),
            icon_roundness: default_roundness(),
            grid_cols: 0,
            grid_rows: 0,
            folder_overrides: HashMap::new(),
            app_overrides: HashMap::new(),
            hidden: Vec::new(),
            manual: Vec::new(),
        }
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
