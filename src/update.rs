//! 检测正式发行版、收集未安装版本日志，并在验签后由独立进程运行 Windows 安装包。

mod http;

use std::{
    collections::HashSet,
    fs,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::Command,
};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use windows::core::w;
use windows::Win32::System::{SystemInformation::GetSystemDirectoryW, Threading::CREATE_NO_WINDOW};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const PUBLIC_KEY: &str = include_str!("../assets/update-public-key.hex");
const SIGNATURE_CONTEXT: &[u8] = b"WinLaunchpad update v1\0";
const RELEASE_LIMIT: usize = 1024 * 1024;
const SETUP_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    GitHub,
    Cnb,
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::Cnb => "腾讯云 CNB",
        }
    }

    /// 固定项目与路径，客户端不使用发布元数据里提供的下载地址。
    fn download(self, version: &str, name: &str, limit: usize) -> Result<Vec<u8>, String> {
        let (host, prefix) = match self {
            Self::GitHub => ("github.com", "/CrazyFigure/WinLaunchpad/releases/download"),
            Self::Cnb => ("cnb.cool", "/crazy-figure/WinLaunchpad/-/releases/download"),
        };
        http::get(
            host,
            &format!("{prefix}/v{version}/{name}"),
            w!("application/octet-stream"),
            limit,
        )
    }

    fn fallback(self) -> Self {
        match self {
            Self::GitHub => Self::Cnb,
            Self::Cnb => Self::GitHub,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub channel: Channel,
}

/// 单个版本的日志分组（与 assets/release-notes.json 结构一致）。
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ReleaseSection {
    pub title: String,
    pub items: Vec<String>,
}

/// 单条版本发布日志（可来自内置 JSON 或远端 Release 正文解析）。
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ReleaseNote {
    pub version: String,
    pub date: String,
    pub sections: Vec<ReleaseSection>,
}

/// 可安装更新信息，包含最高完整正式版目标与所有未安装版本的更新日志。
#[derive(Clone, Debug)]
pub struct UpdateBundle {
    pub latest: Release,
    pub uninstalled: Vec<ReleaseNote>,
}

#[derive(Deserialize)]
struct RemoteRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<RemoteAsset>,
}

#[derive(Deserialize)]
struct RemoteAsset {
    name: String,
    state: Option<String>,
}

/// 只接受三段正式版本，按数值比较，并排除可进入 URL 的非规范内容。
fn stable_version(value: &str) -> Option<[u64; 3]> {
    let mut numbers = value.split('.');
    let mut next = || {
        let part = numbers.next()?;
        let number = part.parse::<u64>().ok()?;
        (number.to_string() == part).then_some(number)
    };
    let result = [next()?, next()?, next()?];
    numbers.next().is_none().then_some(result)
}

fn asset_name(version: &str) -> String {
    format!("WinLaunchpad_{version}_windows_x64_setup.exe")
}

/// 提取发布日期（YYYY-MM-DD），无效时返回空字符串。
fn parse_release_date(published_at: Option<&str>, created_at: Option<&str>) -> String {
    let raw = published_at
        .filter(|s| !s.trim().is_empty())
        .or_else(|| created_at.filter(|s| !s.trim().is_empty()))
        .unwrap_or("");
    let prefix = raw.get(..10).unwrap_or("");
    let bytes = prefix.as_bytes();
    if bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        prefix.to_string()
    } else {
        String::new()
    }
}

/// 解析 GitHub / CNB Release 正文 Markdown 为结构化日志分组。
pub fn parse_release_body(body: &str) -> Vec<ReleaseSection> {
    let mut sections: Vec<ReleaseSection> = Vec::new();
    let mut seen_heading = false;

    let push_item = |sections: &mut Vec<ReleaseSection>, text: &str| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if sections.is_empty() {
            sections.push(ReleaseSection {
                title: "更新内容".into(),
                items: Vec::new(),
            });
        }
        if let Some(last) = sections.last_mut() {
            last.items.push(trimmed.to_string());
        }
    };

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // 跳过发布工作流在正文顶部注入的下载引导语，只保留实际更新内容。
        if !seen_heading && line.starts_with("首次安装请下载文件名以") {
            continue;
        }
        if let Some(title) = line.strip_prefix("##") {
            let title = title.trim_start_matches('#').trim();
            if !title.is_empty() {
                seen_heading = true;
                sections.push(ReleaseSection {
                    title: title.to_string(),
                    items: Vec::new(),
                });
            }
            continue;
        }
        if let Some(item) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("• "))
        {
            push_item(&mut sections, item);
            continue;
        }
        push_item(&mut sections, line);
    }

    sections.retain(|s| !s.items.is_empty());
    if sections.is_empty() {
        sections.push(ReleaseSection {
            title: "更新说明".into(),
            items: vec!["暂无详细更新说明".into()],
        });
    }
    sections
}

/// 筛选最高可一键安装的正式版，并按版本号降序收集所有未安装版本的日志。
fn collect_updates(
    releases: Vec<RemoteRelease>,
    channel: Channel,
    current: [u64; 3],
) -> Option<UpdateBundle> {
    let mut candidates: Vec<([u64; 3], RemoteRelease, bool)> = releases
        .into_iter()
        .filter_map(|release| {
            if release.draft || release.prerelease {
                return None;
            }
            let version = release.tag_name.strip_prefix('v')?;
            let parts = stable_version(version)?;
            if parts <= current {
                return None;
            }
            let setup = asset_name(version);
            let uploaded = |name: &str| {
                release.assets.iter().any(|asset| {
                    asset.name == name
                        && asset
                            .state
                            .as_deref()
                            .is_none_or(|state| state == "uploaded")
                })
            };
            // 安装包和签名同时可下载后才允许作为目标版本进行一键更新。
            let installable = uploaded(&setup) && uploaded(&format!("{setup}.sig"));
            Some((parts, release, installable))
        })
        .collect();

    let (latest_parts, latest_version) = candidates
        .iter()
        .filter(|(_, _, installable)| *installable)
        .max_by_key(|(parts, _, _)| *parts)
        .map(|(parts, rel, _)| (*parts, rel.tag_name.strip_prefix('v').unwrap().to_string()))?;

    // 按版本号从新到旧排序并去重，汇总当前版本之后到目标版本之间的全部未安装更新日志。
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let mut seen = HashSet::new();
    let mut uninstalled = Vec::new();
    for (parts, rel, _) in candidates {
        if parts > latest_parts || !seen.insert(parts) {
            continue;
        }
        let version = rel.tag_name.strip_prefix('v').unwrap_or("").to_string();
        let date = parse_release_date(rel.published_at.as_deref(), rel.created_at.as_deref());
        let sections = parse_release_body(rel.body.as_deref().unwrap_or(""));
        uninstalled.push(ReleaseNote {
            version,
            date,
            sections,
        });
    }

    Some(UpdateBundle {
        latest: Release {
            version: latest_version,
            channel,
        },
        uninstalled,
    })
}

fn check_channel(channel: Channel) -> Result<Option<UpdateBundle>, String> {
    let (host, path, accept) = match channel {
        Channel::GitHub => (
            "api.github.com",
            "/repos/CrazyFigure/WinLaunchpad/releases?per_page=30",
            w!("application/vnd.github+json"),
        ),
        Channel::Cnb => (
            "cnb.cool",
            "/crazy-figure/WinLaunchpad/-/releases?page=1&page_size=30",
            w!("application/json"),
        ),
    };
    let data = http::get(host, path, accept, RELEASE_LIMIT)?;
    let releases =
        serde_json::from_slice(&data).map_err(|error| format!("发布列表格式错误：{error}"))?;
    let current = stable_version(VERSION).ok_or("当前不是正式版本")?;
    Ok(collect_updates(releases, channel, current))
}

/// 默认检查 GitHub。连接或接口失败时自动尝试 CNB，并保留两边失败原因。
pub fn check() -> Result<(Option<UpdateBundle>, Channel), String> {
    match check_channel(Channel::GitHub) {
        Ok(bundle) => Ok((bundle, Channel::GitHub)),
        Err(github) => check_channel(Channel::Cnb)
            .map(|bundle| (bundle, Channel::Cnb))
            .map_err(|cnb| format!("GitHub：{github}；腾讯云 CNB：{cnb}")),
    }
}

/// 公钥随应用构建。签名覆盖协议前缀、完整文件名和二进制，版本与架构无法互换。
fn verify(name: &str, setup: &[u8], signature: &[u8]) -> Result<(), String> {
    let hex = PUBLIC_KEY.trim();
    let mut bytes = [0u8; 32];
    if hex.len() != 64 || !hex.is_ascii() {
        return Err("内置更新公钥格式错误".into());
    }
    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let pair = std::str::from_utf8(pair).map_err(|_| "内置更新公钥格式错误")?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| "内置更新公钥格式错误")?;
    }
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_| "内置更新公钥无效")?;
    let signature = Signature::from_slice(signature).map_err(|_| "更新签名格式错误")?;
    let mut signed = Vec::with_capacity(SIGNATURE_CONTEXT.len() + name.len() + 1 + setup.len());
    signed.extend_from_slice(SIGNATURE_CONTEXT);
    signed.extend_from_slice(name.as_bytes());
    signed.push(0);
    signed.extend_from_slice(setup);
    key.verify_strict(&signed, &signature)
        .map_err(|_| "更新包签名校验失败".into())
}

/// 下载后必须完成验签才能进入安装步骤，下载渠道失败时只回退到同名官方镜像。
pub fn download(release: &Release) -> Result<Vec<u8>, String> {
    let name = asset_name(&release.version);
    let fetch = |channel: Channel| {
        let signature = channel.download(&release.version, &format!("{name}.sig"), 64)?;
        let setup = channel.download(&release.version, &name, SETUP_LIMIT)?;
        verify(&name, &setup, &signature)?;
        Ok(setup)
    };
    fetch(release.channel).or_else(|first: String| {
        let fallback = release.channel.fallback();
        fetch(fallback).map_err(|second| {
            format!(
                "{}：{first}；{}：{second}",
                release.channel.name(),
                fallback.name()
            )
        })
    })
}

/// 将验签后的安装包暂存到程序所在卷。更新进程启动后才允许主程序退出。
pub fn start_installation(setup: &[u8]) -> Result<(), String> {
    let target = std::env::current_exe().map_err(|err| format!("无法定位当前程序：{err}"))?;
    let parent = target.parent().ok_or("无法定位当前程序目录")?;
    let temporary = tempfile::Builder::new()
        .prefix("WinLaunchpad-update-")
        .tempdir_in(parent)
        .map_err(|err| format!("无法在程序目录准备更新：{err}"))?;
    let script = temporary.path().join("install.ps1");
    fs::write(temporary.path().join("setup.exe"), setup)
        .map_err(|err| format!("写入更新安装包失败：{err}"))?;
    // Windows PowerShell 5.1 通过 BOM 识别脚本中的 UTF-8 中文。
    let script_source = [
        b"\xef\xbb\xbf".as_slice(),
        include_bytes!("update/install.ps1"),
    ]
    .concat();
    fs::write(&script, script_source).map_err(|err| format!("写入更新脚本失败：{err}"))?;
    let mut system_dir = [0u16; 260];
    let length = unsafe { GetSystemDirectoryW(Some(&mut system_dir)) } as usize;
    if length == 0 || length >= system_dir.len() {
        return Err("无法定位 Windows PowerShell".into());
    }
    let powershell = PathBuf::from(String::from_utf16_lossy(&system_dir[..length]))
        .join("WindowsPowerShell/v1.0/powershell.exe");
    Command::new(powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .args(["-ParentId", &std::process::id().to_string(), "-TargetExe"])
        .arg(&target)
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|err| format!("启动更新进程失败：{err}"))?;
    // 脚本负责清理暂存文件，退出前保留安装包以供更新。
    let _ = temporary.keep();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_highest_complete_stable_release_and_collects_uninstalled_logs() {
        let create = |tag: &str, signed: bool, prerelease: bool, date: &str, body: &str| {
            RemoteRelease {
                tag_name: tag.into(),
                draft: false,
                prerelease,
                published_at: Some(format!("{date}T03:06:14Z")),
                created_at: None,
                body: Some(body.into()),
                assets: [
                    asset_name(&tag[1..]),
                    format!("{}.sig", asset_name(&tag[1..])),
                ]
                .into_iter()
                .take(if signed { 2 } else { 1 })
                .map(|name| RemoteAsset { name, state: None })
                .collect(),
            }
        };
        // 较新的旧式便携版附件与未签名安装包都不作为更新候选。
        let portable = RemoteRelease {
            tag_name: "v0.0.13".into(),
            draft: false,
            prerelease: false,
            published_at: Some("2026-11-30T00:00:00Z".into()),
            created_at: None,
            body: Some("## 便携版\n\n- 旧包".into()),
            assets: [
                "WinLaunchpad_0.0.13_windows_x64.exe",
                "WinLaunchpad_0.0.13_windows_x64.exe.sig",
            ]
            .into_iter()
            .map(|name| RemoteAsset {
                name: name.into(),
                state: None,
            })
            .collect(),
        };
        let releases = vec![
            create("v0.0.1", true, false, "2026-11-24", "## 启动台\n\n- 首版"),
            create("v0.0.9", true, false, "2026-11-25", "## 更新\n\n- 📦 九版"),
            create("v0.0.11", false, false, "2026-11-27", "## 未签名\n\n- 不应选中"),
            create("v0.0.10", true, false, "2026-11-26", "## 体验\n\n- 🚀 十版"),
            create("v0.0.12-beta", true, true, "2026-11-28", "## 测试版\n\n- beta"),
            portable,
        ];
        let bundle = collect_updates(releases, Channel::Cnb, [0, 0, 1]).unwrap();
        assert_eq!(bundle.latest.version, "0.0.10");
        assert_eq!(bundle.uninstalled.len(), 2);
        assert_eq!(bundle.uninstalled[0].version, "0.0.10");
        assert_eq!(bundle.uninstalled[0].date, "2026-11-26");
        assert_eq!(bundle.uninstalled[0].sections[0].title, "体验");
        assert_eq!(bundle.uninstalled[0].sections[0].items, vec!["🚀 十版"]);
        assert_eq!(bundle.uninstalled[1].version, "0.0.9");
        assert_eq!(bundle.uninstalled[1].date, "2026-11-25");
    }

    #[test]
    fn parse_release_body_handles_markdown_and_skips_boilerplate() {
        let body = "首次安装请下载文件名以 `_setup.exe` 结尾的安装程序。\r\n\r\n## 安装与更新\r\n\r\n- 📦 第一条\r\n* ✨ 第二条\r\n\r\n## 手势\r\n\r\n- 🎯 第三条\r\n";
        let sections = parse_release_body(body);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "安装与更新");
        assert_eq!(sections[0].items, vec!["📦 第一条", "✨ 第二条"]);
        assert_eq!(sections[1].title, "手势");
        assert_eq!(sections[1].items, vec!["🎯 第三条"]);

        let empty = parse_release_body("   \n\n ");
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].items, vec!["暂无详细更新说明"]);
    }
}
