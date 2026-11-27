//! 启动应用：桌面程序/快捷方式走 ShellExecuteExW，打包应用优先 Shell 激活，失败时回退到
//! IApplicationActivationManager。调用在短时线程里执行，避免 Shell 偶发阻塞卡住界面。

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoInitializeEx, CoCreateInstance, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::UI::Shell::{
    ApplicationActivationManager, IApplicationActivationManager, SEE_MASK_NOASYNC,
    SEE_MASK_NO_CONSOLE, SHELLEXECUTEINFOW, ShellExecuteExW, AO_NONE,
};

use crate::catalog::AppEntry;
use crate::win32::wide;

pub fn launch_async(entry: AppEntry, on_result: impl FnOnce(Result<(), String>) + Send + 'static) {
    std::thread::spawn(move || {
        let r = launch(&entry);
        on_result(r);
    });
}

pub fn launch(entry: &AppEntry) -> Result<(), String> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let need_uninit = hr.is_ok();
        let r = launch_inner(entry);
        if need_uninit {
            windows::Win32::System::Com::CoUninitialize();
        }
        r
    }
}

unsafe fn launch_inner(entry: &AppEntry) -> Result<(), String> {
    let show = windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL.0 as i32;
    let explicit_missing = !entry.target.is_empty()
        && !entry.target.starts_with("shell:")
        && !entry.target.contains('%')
        && (entry.target.contains('\\') || entry.target.contains('/'))
        && !std::path::Path::new(&entry.target).exists();
    // 主进程以管理员身份运行时，优先委托桌面 Explorer 以普通用户权限拉起存在的目标应用
    if !explicit_missing
        && crate::win32::is_process_elevated()
        && crate::win32::shell_execute_unelevated(
            &entry.target,
            (!entry.args.is_empty()).then_some(entry.args.as_str()),
            None,
            None,
            show,
        )
        .is_ok()
    {
        return Ok(());
    }
    let target = wide(&entry.target);
    let args = wide(&entry.args);
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_NO_CONSOLE,
        lpFile: PCWSTR(target.as_ptr()),
        nShow: show,
        ..Default::default()
    };
    if !entry.args.is_empty() {
        sei.lpParameters = PCWSTR(args.as_ptr());
    }
    if ShellExecuteExW(&mut sei).is_ok() {
        return Ok(());
    }
    // 打包应用：回退到激活管理器
    if let Some(aumid) = entry.target.strip_prefix("shell:AppsFolder\\") {
        if let Ok(mgr) = CoCreateInstance::<_, IApplicationActivationManager>(
            &ApplicationActivationManager,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            let a = wide(aumid);
            if mgr.ActivateApplication(PCWSTR(a.as_ptr()), PCWSTR::null(), AO_NONE).is_ok() {
                return Ok(());
            }
        }
    }
    let err = windows::Win32::Foundation::GetLastError().0;
    if err == 0 {
        Err("系统拒绝启动该程序".into())
    } else {
        Err(crate::win32::error_text(err))
    }
}

/// 确保窗口句柄参数存在（保留签名，供未来 Shell 需要父窗口时使用）。
#[allow(dead_code)]
fn _owner(_: HWND) {}
