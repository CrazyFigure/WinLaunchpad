//! 开机启动：在当前用户的「启动」文件夹创建/删除指向本程序的快捷方式（--background）。
//! 同时读取 Windows「启动应用」页面里的禁用状态（StartupApproved）。

use std::{path::PathBuf, ptr};

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER, IPersistFile, STGM_READ};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY, REG_VALUE_TYPE,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, FOLDERID_Startup, ShellLink, SHGetKnownFolderPath,
};

use crate::win32::{take_pwstr, wide};

pub const LNK_NAME: &str = "WinLaunchpad.lnk";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupState {
    /// 快捷方式存在且未被系统禁用
    Enabled,
    /// 快捷方式存在，但用户在任务管理器「启动应用」里禁用了它
    DisabledBySystem,
    /// 没有快捷方式
    Off,
}

pub fn shortcut_path() -> Option<PathBuf> {
    unsafe { SHGetKnownFolderPath(&FOLDERID_Startup, Default::default(), None) }
        .ok()
        .map(take_pwstr)
        .map(|d| PathBuf::from(d).join(LNK_NAME))
}

fn disabled_by_system() -> bool {
    let mut data = [0u8; 64];
    let mut size = data.len() as u32;
    let r: WIN32_ERROR = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder"),
            PCWSTR(wide(LNK_NAME).as_ptr()),
            RRF_RT_REG_BINARY,
            Some(&mut REG_VALUE_TYPE(0)),
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS || size == 0 {
        return false;
    }
    // 首字节为奇数表示被禁用（02/06 启用，03/07 禁用）
    data[0] & 1 == 1
}

pub fn state() -> StartupState {
    match shortcut_path() {
        Some(p) if p.exists() => {
            if disabled_by_system() {
                StartupState::DisabledBySystem
            } else {
                StartupState::Enabled
            }
        }
        _ => StartupState::Off,
    }
}

/// 安装到新位置时迁移已有快捷方式，保留关闭状态及 Windows 的 StartupApproved 设置。
pub fn refresh_existing_shortcut() -> Result<(), String> {
    if shortcut_path().is_some_and(|link| link.is_file()) {
        set_enabled(true)?;
    }
    Ok(())
}

/// 卸载时仅清理指向被卸载程序的启动项，保留指向其他位置的快捷方式。
pub fn remove_current_shortcut() -> Result<(), String> {
    let Some(path) = shortcut_path().filter(|link| link.is_file()) else {
        return Ok(());
    };
    let current = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| error.to_string())?;
    let target = unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(crate::win32::error_string)?;
        let file: IPersistFile = link.cast().map_err(crate::win32::error_string)?;
        file.Load(PCWSTR(wide(&path).as_ptr()), STGM_READ)
            .map_err(crate::win32::error_string)?;
        // GetPath 只读取目标，不解析或启动目标程序。
        let mut buffer = [0u16; 1024];
        link.GetPath(&mut buffer, ptr::null_mut(), 0)
            .map_err(crate::win32::error_string)?;
        let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
        PathBuf::from(String::from_utf16_lossy(&buffer[..length]))
    };
    if target.canonicalize().is_ok_and(|target| target == current) {
        std::fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// 创建或删除启动快捷方式。
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let Some(lnk) = shortcut_path() else {
        return Err("无法定位启动文件夹".into());
    };
    if !enabled {
        if lnk.exists() {
            std::fs::remove_file(&lnk).map_err(|e| format!("删除启动项失败: {e}"))?;
        }
        return Ok(());
    }
    let exe = crate::win32::exe_path();
    unsafe {
        let link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| {
                format!("创建快捷方式失败: {}", crate::win32::error_string(e))
            })?;
        link.SetPath(PCWSTR(wide(&exe).as_ptr()))
            .map_err(|e| crate::win32::error_string(e))?;
        let _ = link.SetArguments(w!("--background"));
        if let Some(dir) = exe.parent() {
            let _ = link.SetWorkingDirectory(PCWSTR(wide(dir).as_ptr()));
        }
        let _ = link.SetDescription(w!("WinLaunchpad 启动台（托盘常驻）"));
        let _ = link.SetIconLocation(PCWSTR(wide(&exe).as_ptr()), 0);
        let pf: IPersistFile = link.cast().map_err(|e| crate::win32::error_string(e))?;
        pf.Save(PCWSTR(wide(&lnk).as_ptr()), true)
            .map_err(|e| format!("保存启动项失败: {}", crate::win32::error_string(e)))?;
    }
    Ok(())
}
