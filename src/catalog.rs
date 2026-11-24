//! 应用目录：自动发现已安装应用的可启动入口、过滤辅助程序、文件夹分类、去重与排序。
//!
//! 扫描来源：当前用户/公共开始菜单的 .lnk、shell:AppsFolder 中的打包应用、
//! HKCU/HKLM App Paths 中开始菜单没有登记的 exe。用户手动添加的条目在视图层合并。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::core::{w, GUID, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::Globalization::{CompareStringW, GetUserDefaultLCID, CSTR_GREATER_THAN, CSTR_LESS_THAN};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, IPersistFile, STGM_READ,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, REG_VALUE_TYPE,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_CommonPrograms, FOLDERID_CommonStartup, FOLDERID_Programs,
    FOLDERID_Startup, IEnumShellItems, IShellItem, IShellItem2, IShellLinkW,
    SHCreateItemFromParsingName, SHGetKnownFolderPath, ShellLink, SIGDN_DESKTOPABSOLUTEPARSING,
    SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY,
};

use crate::config::Settings;
use crate::win32::{from_wide, take_pwstr, wide};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Manual,
    UserMenu,
    CommonMenu,
    AppsFolder,
    AppPaths,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub source: Source,
    /// 启动对象：.lnk 路径、exe 路径或 shell:AppsFolder\AUMID
    pub target: String,
    #[serde(default)]
    pub args: String,
    /// 额外可搜索文本（exe 文件名等）
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 图标提取用的解析名
    #[serde(default)]
    pub icon_key: String,
    /// 所属自动发现文件夹标识（如 start_root / menu:wechat / apps_store / apps_system）
    #[serde(default)]
    pub folder_id: String,
    /// 所属文件夹展示名称
    #[serde(default)]
    pub folder_name: String,
}

impl AppEntry {
    pub fn manual_from_path(path: &Path) -> AppEntry {
        let target = path.to_string_lossy().to_string();
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| target.clone());
        let mut args = String::new();
        if path.extension().map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
            if let Some(info) = read_lnk(path) {
                args = info.args;
                let _ = &info.desc;
            }
        }
        let lower = target.to_lowercase();
        AppEntry {
            id: format!("manual:{lower}|{args}"),
            name,
            source: Source::Manual,
            target: target.clone(),
            args,
            aliases: vec![],
            icon_key: target,
            folder_id: "manual".into(),
            folder_name: "手动添加".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanResult {
    #[serde(default)]
    pub entries: Vec<AppEntry>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub scanned_at: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    /// 开始菜单快捷方式：完整规则
    Full,
    /// 打包应用：只排除卸载/安装类
    Light,
}

// ---------- 名称过滤 ----------

const EN_STOP: &[&str] = &[
    "uninstall", "uninstaller", "setup", "install", "installer", "configuration", "config",
    "configure", "settings", "setting", "options", "repair", "help", "readme", "license",
    "update", "upgrade", "website", "homepage", "manual", "release", "notes", "documentation",
    "docs", "guide", "tutorial", "unregister", "register", "activation", "activate",
];
const EN_STOP_LIGHT: &[&str] = &["uninstall", "uninstaller", "setup", "install", "installer"];
const ZH_STOP: &[&str] = &[
    "卸载", "卸載", "安装", "安裝", "配置", "设置", "設定", "修复", "修復", "帮助", "說明", "说明",
    "更新", "升级", "升級", "官网", "网站", "網站", "主页", "主頁", "许可", "許可", "手册", "手冊",
    "用户指南", "教程", "注册", "註冊", "反馈", "回饋", "激活", "卸载工具",
];
const ZH_STOP_LIGHT: &[&str] = &["卸载", "卸載", "安装", "安裝"];

fn tokens(s: &str) -> Vec<String> {
    let lower = s.to_lowercase();
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 判断名称是否属于辅助入口（卸载、配置、安装、帮助等）。
fn is_aux_name(name: &str, mode: Filter) -> bool {
    let (en, zh): (&[&str], &[&str]) = match mode {
        Filter::Full => (EN_STOP, ZH_STOP),
        Filter::Light => (EN_STOP_LIGHT, ZH_STOP_LIGHT),
    };
    let toks = tokens(name);
    // 例外：Uninstall Tool 本身是产品
    if toks.len() == 2 && toks[0] == "uninstall" && toks[1] == "tool" {
        return false;
    }
    for t in &toks {
        if t.starts_with("unins") && t[5..].chars().all(|c| c.is_ascii_digit()) && t.len() > 5 {
            return true;
        }
        if en.contains(&t.as_str()) {
            return true;
        }
    }
    zh.iter().any(|k| name.contains(k))
}

/// 综合判断一个候选入口是否应该被排除。
fn is_aux(display_name: &str, target: &str, args: &str, mode: Filter) -> bool {
    if is_aux_name(display_name, mode) {
        return true;
    }
    let stem = Path::new(target)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !stem.is_empty() && is_aux_name(&stem, Filter::Light) {
        return true;
    }
    let a = args.to_lowercase();
    if a.contains("/uninstall")
        || a.contains("-uninstall")
        || a.contains("/remove")
        || a.contains("--uninstall")
    {
        return true;
    }
    false
}

/// 名称排序：交给系统区域规则（中文按拼音），忽略大小写。
pub fn cmp_names(a: &str, b: &str) -> std::cmp::Ordering {
    let la: Vec<u16> = a.encode_utf16().collect();
    let lb: Vec<u16> = b.encode_utf16().collect();
    let r = unsafe { CompareStringW(GetUserDefaultLCID(), 1, &la, &lb) };
    if r == CSTR_LESS_THAN {
        std::cmp::Ordering::Less
    } else if r == CSTR_GREATER_THAN {
        std::cmp::Ordering::Greater
    } else {
        a.cmp(b)
    }
}

// ---------- 文件夹分类与默认可见策略 ----------

/// 将 AppsFolder 条目归入「常用商店应用」或「Windows 系统与管理工具」。
fn classify_apps_folder(name: &str, aumid: &str) -> (&'static str, &'static str) {
    let ln = name.trim().to_lowercase();
    let la = aumid.to_lowercase();
    const SYS_NAMES: &[&str] = &[
        "windows 安全中心", "windows 备份", "windows 工具", "windows backup", "windows security",
        "windows tools", "本地安全策略", "单击以执行", "反馈中心", "feedback hub", "计算机管理",
        "检查更新", "控制面板", "任务管理器", "任务计划程序", "入门", "get started", "事件查看器",
        "性能监视器", "用户指南", "运行", "资源监视器", "设置", "settings", "get help",
    ];
    if SYS_NAMES.iter().any(|&k| ln == k)
        || ln.contains("troubleshooter")
        || ln.contains("module docs")
        || la.contains("microsoft.windowscommunicationsapps")
    {
        ("apps_system", "Windows 系统与管理工具")
    } else {
        ("apps_store", "Microsoft Store 与常用应用")
    }
}

/// 根据文件夹标识与名称判断其是否属于默认勾选的常用应用文件夹。
pub fn is_folder_default_enabled(folder_id: &str, folder_name: &str) -> bool {
    match folder_id {
        "start_root" | "apps_store" | "manual" => true,
        "apps_system" | "app_paths" => false,
        _ => {
            let lower = folder_name.trim().to_lowercase();
            const UNCOMMON_EXACT: &[&str] = &[
                "administrative tools",
                "windows administrative tools",
                "管理工具",
                "windows 管理工具",
                "system tools",
                "windows system",
                "系统工具",
                "windows 系统",
                "accessories",
                "windows accessories",
                "附件",
                "windows 附件",
                "accessibility",
                "ease of access",
                "windows ease of access",
                "轻松使用",
                "辅助功能",
                "windows powershell",
                "windows kits",
                "maintenance",
                "维护",
                "microsoft office 工具",
                "microsoft office tools",
                "java",
                "node.js",
                "git",
            ];
            if UNCOMMON_EXACT.iter().any(|&k| lower == k) {
                return false;
            }
            const UNCOMMON_PREFIX: &[&str] = &[
                "strawberry perl",
                "python ",
                "visual studio",
                "windows sdk",
                "windows driver kit",
            ];
            if UNCOMMON_PREFIX.iter().any(|&p| lower.starts_with(p)) {
                return false;
            }
            true
        }
    }
}

/// 为任意条目（含旧版缓存迁移场景）解析稳定的文件夹 `(folder_id, folder_name)`。
pub fn resolve_folder(entry: &AppEntry) -> (String, String) {
    if !entry.folder_id.is_empty() && !entry.folder_name.is_empty() {
        return (entry.folder_id.clone(), entry.folder_name.clone());
    }
    match entry.source {
        Source::Manual => ("manual".into(), "手动添加".into()),
        Source::AppPaths => ("app_paths".into(), "注册表 App Paths".into()),
        Source::AppsFolder => {
            let (id, name) = classify_apps_folder(&entry.name, &entry.target);
            (id.into(), name.into())
        }
        Source::UserMenu | Source::CommonMenu => {
            // 从路径中抽取 Start Menu\Programs 后的首层目录名
            let norm = entry.target.replace('/', "\\");
            let lower = norm.to_lowercase();
            if let Some(pos) = lower.find("\\start menu\\programs\\") {
                let rest = &norm[pos + "\\start menu\\programs\\".len()..];
                if let Some(slash) = rest.find('\\') {
                    let sub = rest[..slash].trim();
                    if !sub.is_empty() {
                        return (format!("menu:{}", sub.to_lowercase()), sub.to_string());
                    }
                }
            }
            ("start_root".into(), "开始菜单根目录".into())
        }
    }
}

/// 判断单个条目在当前用户配置下是否应该在启动台展示。
pub fn is_entry_visible(entry: &AppEntry, settings: &Settings) -> bool {
    if entry.source == Source::Manual {
        return true;
    }
    if let Some(&v) = settings.app_overrides.get(&entry.id) {
        return v;
    }
    let (fid, fname) = resolve_folder(entry);
    if let Some(&v) = settings.folder_overrides.get(&fid) {
        return v;
    }
    is_folder_default_enabled(&fid, &fname)
}

// ---------- 快捷方式 ----------

pub struct LnkInfo {
    pub target: String,
    pub args: String,
    pub desc: String,
}

/// 读取 .lnk 的目标、参数、描述（不解析网络路径，失败返回 None）。
pub fn read_lnk(path: &Path) -> Option<LnkInfo> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let pf: IPersistFile = link.cast().ok()?;
        if pf.Load(PCWSTR(wide(path).as_ptr()), STGM_READ).is_err() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let target = if link
            .GetPath(
                &mut buf,
                std::ptr::null_mut(),
                windows::Win32::UI::Shell::SLGP_RAWPATH.0 as u32,
            )
            .is_ok()
        {
            from_wide(&buf)
        } else {
            String::new()
        };
        let mut abuf = [0u16; 1024];
        let args = if link.GetArguments(&mut abuf).is_ok() {
            from_wide(&abuf)
        } else {
            String::new()
        };
        let mut dbuf = [0u16; 512];
        let desc = if link.GetDescription(&mut dbuf).is_ok() {
            from_wide(&dbuf)
        } else {
            String::new()
        };
        Some(LnkInfo { target, args, desc })
    }
}

// ---------- 扫描 ----------

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe { SHGetKnownFolderPath(id, Default::default(), None) }
        .ok()
        .map(take_pwstr)
        .map(PathBuf::from)
}

fn is_reparse(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .map(|m| m.file_attributes() & 0x400 != 0)
        .unwrap_or(false)
}

fn walk_lnk(dir: &Path, skip: &[PathBuf], out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 5 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let p = entry.path();
        if ft.is_dir() {
            if is_reparse(&p) || skip.iter().any(|s| s == &p) {
                continue;
            }
            walk_lnk(&p, skip, out, depth + 1);
        } else if p.extension().map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
            out.push(p);
        }
    }
}

fn add_entry(entries: &mut Vec<AppEntry>, e: AppEntry) {
    if !entries.iter().any(|x| x.id == e.id) {
        entries.push(e);
    }
}

fn folder_from_rel(root: &Path, lnk: &Path) -> (String, String) {
    if let Ok(rel) = lnk.strip_prefix(root) {
        let mut comps = rel.components();
        if let (Some(first), Some(_second)) = (comps.next(), comps.next()) {
            let sub = first.as_os_str().to_string_lossy().trim().to_string();
            if !sub.is_empty() {
                return (format!("menu:{}", sub.to_lowercase()), sub);
            }
        }
    }
    ("start_root".into(), "开始菜单根目录".into())
}

fn scan_start_menu(root: &Path, skip: &[PathBuf], source: Source, out: &mut ScanResult) {
    let mut lnks = Vec::new();
    walk_lnk(root, skip, &mut lnks, 0);
    for p in lnks {
        let name = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let info = read_lnk(&p);
        let (target, args) = match &info {
            Some(i) => (i.target.clone(), i.args.clone()),
            None => (String::new(), String::new()),
        };
        if info.is_none() {
            out.errors.push(format!("无法读取快捷方式 {}", p.display()));
            continue;
        }
        if is_aux(&name, &target, &args, Filter::Full) {
            continue;
        }
        let lp = p.to_string_lossy().to_string();
        let id = if target.is_empty() {
            format!("lnkfile:{}", lp.to_lowercase())
        } else {
            format!("lnk:{}|{}", target.to_lowercase(), args)
        };
        let (folder_id, folder_name) = folder_from_rel(root, &p);
        let aliases = vec![target.to_lowercase(), name.to_lowercase()];
        add_entry(
            &mut out.entries,
            AppEntry {
                id,
                name,
                source,
                target: lp.clone(),
                args,
                aliases,
                icon_key: lp,
                folder_id,
                folder_name,
            },
        );
    }
}

fn scan_apps_folder(out: &mut ScanResult) {
    unsafe {
        const PKEY_AUMID: PROPERTYKEY = PROPERTYKEY {
            fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
            pid: 5,
        };
        let apps: IShellItem = match SHCreateItemFromParsingName(w!("shell:AppsFolder"), None) {
            Ok(v) => v,
            Err(e) => {
                out.errors
                    .push(format!("无法打开 AppsFolder: {}", crate::win32::error_string(e)));
                return;
            }
        };
        let en: IEnumShellItems = match apps.BindToHandler(None, &BHID_EnumItems) {
            Ok(v) => v,
            Err(e) => {
                crate::log::log(&format!(
                    "AppsFolder BindToHandler 失败: {}",
                    crate::win32::error_string(e)
                ));
                return;
            }
        };
        let (mut total, mut with_aumid, mut aux) = (0usize, 0usize, 0usize);
        loop {
            let mut items: [Option<IShellItem>; 16] = std::array::from_fn(|_| None);
            let mut fetched = 0u32;
            if en.Next(&mut items, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            for it in items.into_iter().take(fetched as usize).flatten() {
                let Ok(it2) = it.cast::<IShellItem2>() else { continue };
                let name = match it.GetDisplayName(SIGDN_NORMALDISPLAY) {
                    Ok(p) => take_pwstr(p),
                    Err(_) => continue,
                };
                let parsing = match it.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) {
                    Ok(p) => take_pwstr(p),
                    Err(_) => String::new(),
                };
                total += 1;
                if parsing.contains(':')
                    || parsing.contains('\\')
                    || it.GetDisplayName(SIGDN_FILESYSPATH).is_ok()
                {
                    continue;
                }
                let Ok(aumid_ptr) = it2.GetString(&PKEY_AUMID) else { continue };
                with_aumid += 1;
                let aumid = take_pwstr(aumid_ptr);
                if aumid.is_empty() || name.trim().is_empty() {
                    continue;
                }
                if is_aux(&name, "", "", Filter::Light) {
                    aux += 1;
                    continue;
                }
                let clean_name = name.trim().to_string();
                let (fid, fname) = classify_apps_folder(&clean_name, &aumid);
                let target = format!("shell:AppsFolder\\{aumid}");
                add_entry(
                    &mut out.entries,
                    AppEntry {
                        id: format!("uwp:{aumid}"),
                        name: clean_name,
                        source: Source::AppsFolder,
                        target: target.clone(),
                        args: String::new(),
                        aliases: vec![aumid.to_lowercase()],
                        icon_key: target,
                        folder_id: fid.into(),
                        folder_name: fname.into(),
                    },
                );
            }
            if (fetched as usize) < 16 {
                break;
            }
        }
        crate::log::log(&format!(
            "AppsFolder: 共 {total} 项，打包应用 {with_aumid} 个，过滤辅助项 {aux} 个"
        ));
    }
}

fn scan_app_paths(known_targets: &mut Vec<String>, out: &mut ScanResult) {
    let sub = w!(r"Software\Microsoft\Windows\CurrentVersion\App Paths");
    let roots: Vec<(HKEY, u32)> = vec![
        (HKEY_CURRENT_USER, KEY_READ.0),
        (HKEY_LOCAL_MACHINE, KEY_READ.0),
        (HKEY_LOCAL_MACHINE, KEY_READ.0 | KEY_WOW64_32KEY.0),
    ];
    for (root, access) in roots {
        let mut key = HKEY::default();
        let r: WIN32_ERROR = unsafe {
            RegOpenKeyExW(
                root,
                sub,
                0,
                windows::Win32::System::Registry::REG_SAM_FLAGS(access),
                &mut key,
            )
        };
        if r != ERROR_SUCCESS {
            continue;
        }
        let mut idx = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut len = 256u32;
            let r = unsafe {
                RegEnumKeyExW(
                    key,
                    idx,
                    windows::core::PWSTR(name.as_mut_ptr()),
                    &mut len,
                    None,
                    PWSTR::null(),
                    None,
                    None,
                )
            };
            if r != ERROR_SUCCESS {
                break;
            }
            idx += 1;
            let exe_name = from_wide(&name);
            if is_aux(&exe_name, "", "", Filter::Light) {
                continue;
            }
            let mut val = [0u16; 1024];
            let mut vt = REG_VALUE_TYPE(0);
            let mut vsz = (val.len() * 2) as u32;
            let r = unsafe {
                RegQueryValueExW(
                    key,
                    PCWSTR::null(),
                    None,
                    Some(&mut vt),
                    Some(val.as_mut_ptr() as *mut u8),
                    Some(&mut vsz),
                )
            };
            if r != ERROR_SUCCESS {
                continue;
            }
            let path = from_wide(&val);
            let path = expand_env(&path);
            let lower = path.to_lowercase();
            if lower.is_empty() || known_targets.contains(&lower) || !Path::new(&path).is_file() {
                continue;
            }
            known_targets.push(lower.clone());
            let stem = Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| exe_name.clone());
            add_entry(
                &mut out.entries,
                AppEntry {
                    id: format!("exe:{lower}"),
                    name: stem,
                    source: Source::AppPaths,
                    target: path.clone(),
                    args: String::new(),
                    aliases: vec![exe_name.to_lowercase()],
                    icon_key: path,
                    folder_id: "app_paths".into(),
                    folder_name: "注册表 App Paths".into(),
                },
            );
        }
        unsafe {
            let _ = RegCloseKey(key);
        }
    }
}

/// 展开 %ProgramFiles% 之类的环境变量。
pub fn expand_env(s: &str) -> String {
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    unsafe {
        let w = wide(s);
        let mut buf = vec![0u16; 1024];
        let n = ExpandEnvironmentStringsW(PCWSTR(w.as_ptr()), Some(&mut buf));
        if n == 0 || n as usize > buf.len() {
            return s.to_string();
        }
        from_wide(&buf)
    }
}

/// 校验条目的本地可执行文件或 `.lnk` 指向目标是否仍然有效。
pub fn is_entry_target_valid(entry: &AppEntry) -> bool {
    if matches!(entry.source, Source::AppsFolder) {
        return true;
    }
    let target_exp = if entry.target.contains('%') {
        expand_env(&entry.target)
    } else {
        entry.target.clone()
    };
    if target_exp.is_empty() || !Path::new(&target_exp).exists() {
        return false;
    }
    if target_exp.to_lowercase().ends_with(".lnk") {
        if let Some(info) = read_lnk(Path::new(&target_exp)) {
            let dest = info.target.trim();
            if !dest.is_empty() {
                let exp = if dest.contains('%') {
                    expand_env(dest)
                } else {
                    dest.to_string()
                };
                // 仅校验本地盘符路径，防止网络共享/虚拟 shell 目标误报
                if exp.as_bytes().get(1) == Some(&b':') && !Path::new(&exp).exists() {
                    return false;
                }
            }
        }
    }
    true
}

#[derive(Clone, Debug)]
pub struct ExpiredItem {
    pub entry: AppEntry,
    pub reason: String,
}

/// 检测所有已失效/过期的启动入口（含失效的 `.lnk` 指向目标与已丢失的本地 `.exe`）。
pub fn detect_expired(scan_entries: &[AppEntry], manual_entries: &[AppEntry]) -> Vec<ExpiredItem> {
    let mut out = Vec::new();
    for e in scan_entries.iter().chain(manual_entries.iter()) {
        if !is_entry_target_valid(e) {
            let lnk_exists = e.target.to_lowercase().ends_with(".lnk") && Path::new(&e.target).is_file();
            let reason = if lnk_exists {
                "快捷方式指向的目标程序不存在".to_string()
            } else {
                "本地启动入口文件已丢失".to_string()
            };
            out.push(ExpiredItem {
                entry: e.clone(),
                reason,
            });
        }
    }
    out
}

/// 执行一次完整扫描（应在短时工作进程 `--worker-scan` 中运行以保持主进程低内存）。
pub fn scan() -> ScanResult {
    let mut out = ScanResult::default();
    out.scanned_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let need_uninit = hr.is_ok();

    let programs = known_folder(&FOLDERID_Programs);
    let common_programs = known_folder(&FOLDERID_CommonPrograms);
    let mut skip: Vec<PathBuf> = Vec::new();
    skip.extend(known_folder(&FOLDERID_Startup));
    skip.extend(known_folder(&FOLDERID_CommonStartup));

    if let Some(p) = &programs {
        scan_start_menu(p, &skip, Source::UserMenu, &mut out);
    }
    if let Some(p) = &common_programs {
        if Some(p) != programs.as_ref() {
            scan_start_menu(p, &skip, Source::CommonMenu, &mut out);
        }
    }
    scan_apps_folder(&mut out);
    // 保险：与开始菜单快捷方式同名的 AppsFolder 项视为同一应用的 Win32 版本
    let lnk_names: std::collections::HashSet<String> = out
        .entries
        .iter()
        .filter(|e| matches!(e.source, Source::UserMenu | Source::CommonMenu))
        .map(|e| e.name.trim().to_lowercase())
        .collect();
    out.entries.retain(|e| {
        e.source != Source::AppsFolder || !lnk_names.contains(&e.name.trim().to_lowercase())
    });

    let mut known_targets: Vec<String> = out
        .entries
        .iter()
        .filter(|e| matches!(e.source, Source::UserMenu | Source::CommonMenu))
        .filter_map(|e| e.aliases.first().cloned().filter(|s| !s.is_empty()))
        .collect();
    scan_app_paths(&mut known_targets, &mut out);

    out.entries.sort_by(|a, b| cmp_names(&a.name, &b.name));
    if need_uninit {
        unsafe { CoUninitialize() };
    }
    out
}

/// 短时工作进程入口：扫描结果序列化写入指定临时文件后直接退出。
pub fn worker_scan_to_file(out_path: &Path) -> bool {
    let result = scan();
    let Ok(json) = serde_json::to_string(&result) else {
        return false;
    };
    let tmp = out_path.with_extension("tmp");
    if std::fs::write(&tmp, json.as_bytes()).is_err() {
        return false;
    }
    std::fs::rename(&tmp, out_path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_uninstall_and_helpers() {
        assert!(is_aux_name("卸载 Win顺", Filter::Full));
        assert!(is_aux_name("Uninstall Tool Help", Filter::Full));
        assert!(is_aux_name("unins000", Filter::Full));
        assert!(is_aux_name("Notepad++ 配置", Filter::Full));
        assert!(is_aux_name("Git Bash Help", Filter::Full));
        assert!(is_aux_name("检查更新", Filter::Full));
        assert!(!is_aux_name("7-Zip File Manager", Filter::Full));
    }

    #[test]
    fn keeps_real_products() {
        assert!(!is_aux_name("Uninstall Tool", Filter::Full));
        assert!(!is_aux_name("UniGetUI", Filter::Full));
        assert!(!is_aux_name("Microsoft Edge", Filter::Full));
        assert!(!is_aux_name("完美解码", Filter::Full));
        assert!(!is_aux_name("设置", Filter::Light));
        assert!(is_aux_name("卸载 微信", Filter::Light));
    }

    #[test]
    fn aux_by_target_and_args() {
        assert!(is_aux("Foo", r"C:\App\unins000.exe", "", Filter::Full));
        assert!(is_aux("Foo", r"C:\App\app.exe", "/uninstall", Filter::Full));
        assert!(!is_aux("Foo", r"C:\App\app.exe", "--minimized", Filter::Full));
    }

    #[test]
    fn sort_puts_latin_before_cjk() {
        assert_eq!(cmp_names("Alpha", "Beta"), std::cmp::Ordering::Less);
        let names = ["微信", "Chrome", "计算器", "7-Zip"];
        let mut sorted = names.to_vec();
        sorted.sort_by(|a, b| cmp_names(a, b));
        assert_eq!(sorted[0], "7-Zip");
        assert_eq!(sorted[1], "Chrome");
    }

    #[test]
    fn folder_defaults_distinguish_common_vs_system() {
        assert!(is_folder_default_enabled("start_root", "开始菜单根目录"));
        assert!(is_folder_default_enabled("apps_store", "Microsoft Store 与常用应用"));
        assert!(is_folder_default_enabled("menu:wechat", "微信"));
        assert!(is_folder_default_enabled("menu:jetbrains", "JetBrains"));
        assert!(!is_folder_default_enabled("apps_system", "Windows 系统与管理工具"));
        assert!(!is_folder_default_enabled("menu:windows kits", "Windows Kits"));
        assert!(!is_folder_default_enabled("menu:administrative tools", "Administrative Tools"));
        assert!(!is_folder_default_enabled("menu:strawberry perl (64-bit)", "Strawberry Perl (64-bit)"));
    }
}
