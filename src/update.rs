//! 检测正式发行版，验签后由独立进程运行 Windows 安装包。

mod http;

use std::{fs, os::windows::process::CommandExt, path::PathBuf, process::Command};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use windows::core::w;
use windows::Win32::System::{SystemInformation::GetSystemDirectoryW, Threading::CREATE_NO_WINDOW};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const PUBLIC_KEY: &str = include_str!("../assets/update-public-key.hex");
const SIGNATURE_CONTEXT: &[u8] = b"WinLaunchpad update v1\0";
const RELEASE_LIMIT: usize = 1024 * 1024;
const SETUP_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
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

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub channel: Channel,
}

#[derive(Deserialize)]
struct RemoteRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
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

fn latest_update(
    releases: Vec<RemoteRelease>,
    channel: Channel,
    current: [u64; 3],
) -> Option<Release> {
    releases
        .into_iter()
        .filter_map(|release| {
            if release.draft || release.prerelease {
                return None;
            }
            let version = release.tag_name.strip_prefix('v')?;
            let parts = stable_version(version)?;
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
            // 安装包和签名同时可下载后才显示一键更新。
            (parts > current && uploaded(&setup) && uploaded(&format!("{setup}.sig"))).then(|| {
                (
                    parts,
                    Release {
                        version: version.into(),
                        channel,
                    },
                )
            })
        })
        .max_by_key(|(parts, _)| *parts)
        .map(|(_, release)| release)
}

fn check_channel(channel: Channel) -> Result<Option<Release>, String> {
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
    Ok(latest_update(releases, channel, current))
}

/// 默认检查 GitHub。连接或接口失败时自动尝试 CNB，并保留两边失败原因。
pub fn check() -> Result<(Option<Release>, Channel), String> {
    match check_channel(Channel::GitHub) {
        Ok(release) => Ok((release, Channel::GitHub)),
        Err(github) => check_channel(Channel::Cnb)
            .map(|release| (release, Channel::Cnb))
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
    fn picks_highest_complete_stable_release() {
        let create = |tag: &str, signed: bool, prerelease: bool| RemoteRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            assets: [
                asset_name(&tag[1..]),
                format!("{}.sig", asset_name(&tag[1..])),
            ]
            .into_iter()
            .take(if signed { 2 } else { 1 })
            .map(|name| RemoteAsset { name, state: None })
            .collect(),
        };
        // 较新的旧式便携版附件与未签名安装包都不作为更新候选。
        let portable = RemoteRelease {
            tag_name: "v0.0.13".into(),
            draft: false,
            prerelease: false,
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
            create("v0.0.9", true, false),
            create("v0.0.11", false, false),
            create("v0.0.10", true, false),
            create("v0.0.12-beta", true, true),
            portable,
        ];
        assert_eq!(
            latest_update(releases, Channel::Cnb, [0, 0, 1])
                .unwrap()
                .version,
            "0.0.10"
        );
    }
}
