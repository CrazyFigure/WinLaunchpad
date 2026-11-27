//! 开机启动：普通模式下在当前用户的「启动」文件夹创建/删除指向本程序的快捷方式（--background）；
//! 管理员权限模式下通过任务计划程序注册高权限登录任务以实现开机静默提权启动（免 UAC 弹窗）。
//! 同时读取 Windows「启动应用」页面或任务计划程序的禁用状态。

use std::{path::PathBuf, ptr};

use windows::core::{w, Interface, BSTR, PCWSTR, VARIANT};
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, IPersistFile, STGM_READ,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY, REG_VALUE_TYPE,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ITaskFolder, ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE,
    TASK_LOGON_INTERACTIVE_TOKEN,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, FOLDERID_Startup, ShellLink, SHGetKnownFolderPath,
};

use crate::win32::{current_user_sid, error_string, exe_path, is_process_elevated, take_pwstr, wide};

pub const LNK_NAME: &str = "WinLaunchpad.lnk";
const ADMIN_TASK_NAME: &str = "WinLaunchpad";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupState {
    /// 快捷方式或高权限任务存在且未被系统禁用
    Enabled,
    /// 快捷方式或高权限任务存在，但被系统任务管理器/任务计划程序禁用
    DisabledBySystem,
    /// 没有开机启动入口
    Off,
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn build_admin_task_xml(exe: &std::path::Path, dir: Option<&std::path::Path>, user_sid: &str) -> String {
    let sid = xml_escape(user_sid);
    let exe_s = xml_escape(&exe.to_string_lossy());
    let dir_tag = match dir {
        Some(d) => format!("\n      <WorkingDirectory>{}</WorkingDirectory>", xml_escape(&d.to_string_lossy())),
        None => String::new(),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>WinLaunchpad 启动台（以管理员身份后台常驻）</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_s}</Command>
      <Arguments>--background</Arguments>{dir_tag}
    </Exec>
  </Actions>
</Task>"#
    )
}

fn with_task_folder<T>(f: impl FnOnce(&ITaskFolder) -> Result<T, String>) -> Result<T, String> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let need_uninit = hr.is_ok();
        let res = (|| -> Result<T, String> {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).map_err(error_string)?;
            let empty = VARIANT::default();
            service
                .Connect(&empty, &empty, &empty, &empty)
                .map_err(error_string)?;
            let folder = service
                .GetFolder(&BSTR::from("\\"))
                .map_err(error_string)?;
            f(&folder)
        })();
        if need_uninit {
            CoUninitialize();
        }
        res
    }
}

fn admin_task_state() -> Option<(StartupState, Option<PathBuf>)> {
    with_task_folder(|folder| unsafe {
        let task = folder
            .GetTask(&BSTR::from(ADMIN_TASK_NAME))
            .map_err(error_string)?;
        let enabled = task.Enabled().map(|v| v.0 != 0).unwrap_or(true);
        let st = if enabled {
            StartupState::Enabled
        } else {
            StartupState::DisabledBySystem
        };
        let exec_path = task
            .Definition()
            .ok()
            .and_then(|def| def.Actions().ok())
            .and_then(|acts| acts.get_Item(1).ok())
            .and_then(|act| act.cast::<IExecAction>().ok())
            .and_then(|exec| {
                let mut p = BSTR::default();
                exec.Path(&mut p).ok()?;
                let s = p.to_string();
                (!s.is_empty()).then(|| PathBuf::from(s))
            });
        Ok((st, exec_path))
    })
    .ok()
}

fn register_admin_task() -> Result<(), String> {
    if !is_process_elevated() {
        return Err("创建高权限开机启动任务需要管理员权限".into());
    }
    let user_sid = current_user_sid().ok_or_else(|| "无法获取当前用户 SID".to_string())?;
    let exe = exe_path();
    let xml = build_admin_task_xml(&exe, exe.parent(), &user_sid);
    // 授予当前用户完全控制权，便于卸载或降权切换时清理任务
    let sddl = format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})");
    with_task_folder(|folder| unsafe {
        let empty = VARIANT::default();
        folder
            .RegisterTask(
                &BSTR::from(ADMIN_TASK_NAME),
                &BSTR::from(xml.as_str()),
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &VARIANT::from(sddl.as_str()),
            )
            .map(|_| ())
            .map_err(|e| format!("注册管理员开机启动任务失败: {}", error_string(e)))
    })
}

fn delete_admin_task() -> Result<(), String> {
    if admin_task_state().is_none() {
        return Ok(());
    }
    with_task_folder(|folder| unsafe {
        folder
            .DeleteTask(&BSTR::from(ADMIN_TASK_NAME), 0)
            .map_err(|e| format!("删除管理员开机启动任务失败: {}", error_string(e)))
    })
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
    if let Some((task_state, _)) = admin_task_state() {
        return task_state;
    }
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

/// 安装到新位置时迁移已有启动入口，保留关闭状态、提权任务及 StartupApproved 设置。
pub fn refresh_existing_shortcut() -> Result<(), String> {
    if admin_task_state().is_some() {
        register_admin_task()?;
    }
    if shortcut_path().is_some_and(|link| link.is_file()) {
        create_startup_shortcut()?;
    }
    Ok(())
}

fn same_executable(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

/// 卸载时仅清理指向被卸载程序的启动项与管理员任务，保留指向其他位置的入口。
pub fn remove_current_shortcut() -> Result<(), String> {
    let current = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| error.to_string())?;
    if let Some((_, Some(task_exe))) = admin_task_state() {
        if same_executable(&task_exe, &current) && delete_admin_task().is_err() && !is_process_elevated() {
            // 普通权限卸载器删除高权限计划任务被拒时，按需提权清理
            let _ = run_elevated_schtasks_delete();
        }
    }
    let Some(path) = shortcut_path().filter(|link| link.is_file()) else {
        return Ok(());
    };
    let target = unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(error_string)?;
        let file: IPersistFile = link.cast().map_err(error_string)?;
        file.Load(PCWSTR(wide(&path).as_ptr()), STGM_READ)
            .map_err(error_string)?;
        // GetPath 只读取目标，不解析或启动目标程序。
        let mut buffer = [0u16; 1024];
        link.GetPath(&mut buffer, ptr::null_mut(), 0)
            .map_err(error_string)?;
        let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
        PathBuf::from(String::from_utf16_lossy(&buffer[..length]))
    };
    if same_executable(&target, &current) {
        std::fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn run_elevated_schtasks_delete() -> Result<(), String> {
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let args = format!(r#"/Delete /TN "{ADMIN_TASK_NAME}" /F"#);
    let w_args = wide(&args);
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: w!("schtasks.exe"),
        lpParameters: PCWSTR(w_args.as_ptr()),
        nShow: SW_HIDE.0 as i32,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut sei).map_err(error_string)?;
        if !sei.hProcess.is_invalid() {
            let _ = WaitForSingleObject(sei.hProcess, 5000);
            let _ = windows::Win32::Foundation::CloseHandle(sei.hProcess);
        }
    }
    Ok(())
}

fn create_startup_shortcut() -> Result<(), String> {
    let Some(lnk) = shortcut_path() else {
        return Err("无法定位启动文件夹".into());
    };
    let exe = exe_path();
    unsafe {
        let link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| {
                format!("创建快捷方式失败: {}", error_string(e))
            })?;
        link.SetPath(PCWSTR(wide(&exe).as_ptr()))
            .map_err(error_string)?;
        let _ = link.SetArguments(w!("--background"));
        if let Some(dir) = exe.parent() {
            let _ = link.SetWorkingDirectory(PCWSTR(wide(dir).as_ptr()));
        }
        let _ = link.SetDescription(w!("WinLaunchpad 启动台（托盘常驻）"));
        let _ = link.SetIconLocation(PCWSTR(wide(&exe).as_ptr()), 0);
        let pf: IPersistFile = link.cast().map_err(error_string)?;
        pf.Save(PCWSTR(wide(&lnk).as_ptr()), true)
            .map_err(|e| format!("保存启动项失败: {}", error_string(e)))?;
    }
    Ok(())
}

/// 按目标权限模式创建或清理开机启动项（提权模式使用计划任务避免每次登录弹 UAC）。
pub fn set_enabled_with_elevation(enabled: bool, elevate: bool) -> Result<(), String> {
    if !enabled {
        if let Some(lnk) = shortcut_path() {
            if lnk.exists() {
                std::fs::remove_file(&lnk).map_err(|e| format!("删除启动项失败: {e}"))?;
            }
        }
        if admin_task_state().is_some() {
            delete_admin_task()?;
        }
        return Ok(());
    }
    if elevate {
        register_admin_task()?;
        // 清理普通快捷方式，避免登录时同时触发普通实例与高权限任务
        if let Some(lnk) = shortcut_path() {
            if lnk.exists() {
                let _ = std::fs::remove_file(&lnk);
            }
        }
        return Ok(());
    }
    if admin_task_state().is_some() {
        let _ = delete_admin_task();
    }
    create_startup_shortcut()
}

/// 创建或删除启动快捷方式。
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    set_enabled_with_elevation(enabled, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_task_xml_escapes_special_characters() {
        let xml = build_admin_task_xml(
            std::path::Path::new(r"C:\Apps\A&B\WinLaunchpad.exe"),
            Some(std::path::Path::new(r"C:\Apps\A&B")),
            "S-1-5-21-1000",
        );
        assert!(xml.contains("A&amp;B\\WinLaunchpad.exe"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<Arguments>--background</Arguments>"));
        assert!(xml.contains("<UserId>S-1-5-21-1000</UserId>"));
    }
}
