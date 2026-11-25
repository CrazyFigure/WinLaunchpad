//! WinLaunchpad 入口：单实例、命令转发、工作进程模式、窗口类注册、消息循环。
//!
//! 命令行：
//!   WinLaunchpad.exe                  打开/收起启动台（未运行则启动并显示）
//!   WinLaunchpad.exe --toggle         同上，供 Quicker 等调用
//!   WinLaunchpad.exe --background     仅驻留托盘（开机启动使用）
//!   WinLaunchpad.exe --settings       打开设置
//!   WinLaunchpad.exe --add            打开「添加应用」
//!   WinLaunchpad.exe --worker-scan    短时子进程：扫描已安装应用
//!   WinLaunchpad.exe --worker-icons   短时子进程：批量提取应用图标
//!   WinLaunchpad.exe --worker-wallpaper 短时子进程：生成壁纸毛玻璃缓存

// 从资源管理器、快捷方式或安装程序启动时，不分配控制台窗口。
#![windows_subsystem = "windows"]

mod add;
mod app;
mod catalog;
mod config;
mod gesture;
mod hotkey;
mod icons;
mod launch;
mod log;
mod render;
mod settings;
mod startup;
mod tray;
mod ui;
mod update;
mod wallpaper;
mod win32;

use std::path::PathBuf;
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use app::{App, MAIN_HWND, WM_APP_CMD};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 短时工作进程分支：隔离 UWP/Shell/WIC 内存开销，执行完毕立即退出
    if let Some(pos) = args.iter().position(|a| a == "--worker-scan") {
        if let Some(out) = args.get(pos + 1) {
            let ok = catalog::worker_scan_to_file(&PathBuf::from(out));
            std::process::exit(if ok { 0 } else { 1 });
        }
        std::process::exit(1);
    }
    if let Some(pos) = args.iter().position(|a| a == "--worker-icons") {
        if let Some(req) = args.get(pos + 1) {
            icons::worker_extract_from_file(&PathBuf::from(req));
            std::process::exit(0);
        }
        std::process::exit(1);
    }
    if let Some(pos) = args.iter().position(|a| a == "--worker-wallpaper") {
        if let Some(out) = args.get(pos + 1) {
            let ok = wallpaper::worker_build_cache(&PathBuf::from(out));
            std::process::exit(if ok { 0 } else { 1 });
        }
        std::process::exit(1);
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
    // 安装程序只维护现有启动项。这两个内部命令不启动主窗口或单实例服务。
    if args.len() == 1 {
        let result = match args[0].as_str() {
            "--installer-refresh-startup" => Some(startup::refresh_existing_shortcut()),
            "--installer-remove-startup" => Some(startup::remove_current_shortcut()),
            _ => None,
        };
        if let Some(result) = result {
            std::process::exit(i32::from(result.is_err()));
        }
    }
    let background = args.iter().any(|a| a == "--background");
    let settings = args.iter().any(|a| a == "--settings");
    let add = args.iter().any(|a| a == "--add");

    unsafe {
        let _mutex = CreateMutexW(None, true, w!("Local\\WinLaunchpad-SingleInstance")).ok();
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        if already
            && forward(match (settings, add, background) {
                (true, _, _) => 3,
                (_, true, _) => 4,
                (_, _, false) => 1,
                (_, _, true) => 0,
            })
        {
            return;
        }
    }

    let hinst = win32::hinstance();
    let app = match App::new(hinst) {
        Ok(a) => a,
        Err(e) => {
            let m = win32::wide(&format!("WinLaunchpad 初始化失败：{e}"));
            unsafe {
                let _ = MessageBoxW(
                    None,
                    windows::core::PCWSTR(m.as_ptr()),
                    w!("WinLaunchpad"),
                    MB_OK | MB_ICONERROR,
                );
            }
            return;
        }
    };
    unsafe {
        register_class(hinst);
        let ptr = Box::into_raw(app);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            w!("WinLaunchpadMain"),
            w!("WinLaunchpad"),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            100,
            100,
            None,
            None,
            hinst,
            Some(ptr as *const _),
        )
        .unwrap_or_default();
        if hwnd.is_invalid() {
            let e = win32::last_error_text();
            let m = win32::wide(&format!("创建主窗口失败：{e}"));
            let _ = MessageBoxW(
                None,
                windows::core::PCWSTR(m.as_ptr()),
                w!("WinLaunchpad"),
                MB_OK | MB_ICONERROR,
            );
            return;
        }
        MAIN_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::SeqCst);
        (*ptr).hwnd = hwnd;
        let app = &mut *ptr;
        app.post_init();
        if settings {
            crate::settings::open(app);
        } else if add {
            crate::add::open(app);
        } else if !background {
            app.show();
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = std::panic::catch_unwind(|| {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            });
        }
    }
}

unsafe fn register_class(hinst: HINSTANCE) {
    let hicon = LoadIconW(hinst, windows::core::PCWSTR(1usize as *const u16)).unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(app::wnd_proc),
        hInstance: hinst,
        hIcon: hicon,
        hIconSm: hicon,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: HBRUSH::default(),
        lpszClassName: w!("WinLaunchpadMain"),
        ..Default::default()
    };
    let _ = RegisterClassExW(&wc);
}

/// 已有实例在运行：发送命令（0 无操作 / 1 切换 / 2 显示 / 3 设置 / 4 添加）后退出本进程。
unsafe fn forward(cmd: usize) -> bool {
    for _ in 0..5 {
        if let Ok(hwnd) = FindWindowW(w!("WinLaunchpadMain"), None) {
            if !hwnd.is_invalid() {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid != 0 {
                    let _ = AllowSetForegroundWindow(pid);
                }
                if cmd != 0 {
                    let _ = PostMessageW(hwnd, WM_APP_CMD, WPARAM(cmd), LPARAM(0));
                }
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    log::log("未找到已有实例窗口，改为正常启动");
    false
}
