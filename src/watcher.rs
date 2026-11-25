//! 安装/卸载事件驱动监听：在独立后台线程零功耗阻塞监听开始菜单快捷方式目录、
//! App Paths、Uninstall 注册表及商店应用包变更，通过防抖合并后向主窗口派发重扫通知。

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Storage::FileSystem::{
    FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegNotifyChangeKeyValue, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_NOTIFY, KEY_WOW64_32KEY, REG_NOTIFY_CHANGE_LAST_SET,
    REG_NOTIFY_CHANGE_NAME, REG_SAM_FLAGS,
};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows::Win32::UI::Shell::{FOLDERID_CommonPrograms, FOLDERID_Programs, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::log;
use crate::win32::{take_pwstr, wide};

pub const WM_APP_WATCH_EVENT: u32 = WM_APP + 11;

/// wParam 标志位：开始菜单或传统桌面安装/卸载键发生变更。
pub const WATCH_FLAG_DESKTOP: usize = 1 << 0;
/// wParam 标志位：Microsoft Store / MSIX / UWP 应用包注册表发生变更。
pub const WATCH_FLAG_UWP: usize = 1 << 1;

const DEBOUNCE_MS: u32 = 1500;

pub struct AppWatcher {
    stop_event_raw: isize,
    thread: Option<JoinHandle<()>>,
}

impl AppWatcher {
    /// 启动后台事件驱动监听线程；所有目录和注册表监听句柄均在工作线程内创建与回收。
    pub fn start(hwnd: HWND) -> Option<Self> {
        let stop_event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                log::log("创建事件监听停止信号失败");
                return None;
            }
        };
        let stop_raw = stop_event.0 as isize;
        let hwnd_raw = hwnd.0 as isize;
        let thread = std::thread::Builder::new()
            .name("app-watcher".into())
            .spawn(move || watcher_loop(hwnd_raw, stop_raw))
            .ok();
        if thread.is_none() {
            unsafe {
                let _ = CloseHandle(stop_event);
            }
            log::log("启动事件监听线程失败");
            return None;
        }
        Some(AppWatcher {
            stop_event_raw: stop_raw,
            thread,
        })
    }

    pub fn stop(&mut self) {
        if self.stop_event_raw != 0 {
            unsafe {
                let _ = SetEvent(HANDLE(self.stop_event_raw as *mut _));
            }
        }
        if let Some(th) = self.thread.take() {
            let _ = th.join();
        }
        if self.stop_event_raw != 0 {
            unsafe {
                let _ = CloseHandle(HANDLE(self.stop_event_raw as *mut _));
            }
            self.stop_event_raw = 0;
        }
    }
}

impl Drop for AppWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

struct DirWatch {
    handle: HANDLE,
}

impl DirWatch {
    fn open(dir: &PathBuf) -> Option<Self> {
        // 跳过不存在或 UNC 网络路径，避免阻塞或无效监听
        let s = dir.to_string_lossy();
        if s.starts_with("\\\\") || !dir.is_dir() {
            return None;
        }
        let w = wide(dir);
        let flags =
            FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE;
        let h = unsafe { FindFirstChangeNotificationW(PCWSTR(w.as_ptr()), true, flags) }.ok()?;
        if h.is_invalid() || h == INVALID_HANDLE_VALUE {
            return None;
        }
        Some(DirWatch { handle: h })
    }

    fn rearm(&self) -> bool {
        unsafe { FindNextChangeNotification(self.handle).is_ok() }
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        if !self.handle.is_invalid() && self.handle != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = FindCloseChangeNotification(self.handle);
            }
        }
    }
}

struct RegWatch {
    key: HKEY,
    event: HANDLE,
    is_uwp: bool,
}

impl RegWatch {
    fn open(root: HKEY, subkey: PCWSTR, extra_sam: u32, is_uwp: bool) -> Option<Self> {
        let mut key = HKEY::default();
        let sam = REG_SAM_FLAGS(KEY_NOTIFY.0 | extra_sam);
        let rc = unsafe { RegOpenKeyExW(root, subkey, 0, sam, &mut key) };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                unsafe {
                    let _ = RegCloseKey(key);
                }
                return None;
            }
        };
        let watch = RegWatch { key, event, is_uwp };
        if !watch.rearm() {
            return None;
        }
        Some(watch)
    }

    /// 注册一次性异步键变更通知；必须在同一存活工作线程中调用以遵守 Win32 生命周期契约。
    fn rearm(&self) -> bool {
        unsafe {
            let _ = ResetEvent(self.event);
            let r = RegNotifyChangeKeyValue(
                self.key,
                true,
                REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                self.event,
                true,
            );
            r == ERROR_SUCCESS
        }
    }
}

impl Drop for RegWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.key);
            if !self.event.is_invalid() {
                let _ = CloseHandle(self.event);
            }
        }
    }
}

fn known_dir(id: &GUID) -> Option<PathBuf> {
    unsafe { SHGetKnownFolderPath(id, Default::default(), None) }
        .ok()
        .map(take_pwstr)
        .map(PathBuf::from)
}

fn watcher_loop(hwnd_raw: isize, stop_raw: isize) {
    let stop_event = HANDLE(stop_raw as *mut _);
    let mut dirs: Vec<DirWatch> = Vec::new();
    let p_user = known_dir(&FOLDERID_Programs);
    let p_common = known_dir(&FOLDERID_CommonPrograms);
    if let Some(ref p) = p_user {
        if let Some(w) = DirWatch::open(p) {
            dirs.push(w);
        }
    }
    if let Some(ref p) = p_common {
        if Some(p) != p_user.as_ref() {
            if let Some(w) = DirWatch::open(p) {
                dirs.push(w);
            }
        }
    }

    let mut regs: Vec<RegWatch> = Vec::new();
    let app_paths = w!(r"Software\Microsoft\Windows\CurrentVersion\App Paths");
    let uninstall = w!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall");
    let app_model =
        w!(r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages");

    for (root, sub, sam, is_uwp) in [
        (HKEY_CURRENT_USER, app_paths, 0u32, false),
        (HKEY_LOCAL_MACHINE, app_paths, 0u32, false),
        (HKEY_LOCAL_MACHINE, app_paths, KEY_WOW64_32KEY.0, false),
        (HKEY_CURRENT_USER, uninstall, 0u32, false),
        (HKEY_LOCAL_MACHINE, uninstall, 0u32, false),
        (HKEY_LOCAL_MACHINE, uninstall, KEY_WOW64_32KEY.0, false),
        (HKEY_CURRENT_USER, app_model, 0u32, true),
    ] {
        if let Some(rw) = RegWatch::open(root, sub, sam, is_uwp) {
            regs.push(rw);
        }
    }

    if dirs.is_empty() && regs.is_empty() {
        log::log("事件监听未找到可用目录或注册表路径，线程退出");
        return;
    }

    let hwnd = HWND(hwnd_raw as *mut _);
    loop {
        let mut handles: Vec<HANDLE> = Vec::with_capacity(1 + dirs.len() + regs.len());
        handles.push(stop_event);
        for d in &dirs {
            handles.push(d.handle);
        }
        for r in &regs {
            handles.push(r.event);
        }

        let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait.0 < WAIT_OBJECT_0.0 || wait.0 >= WAIT_OBJECT_0.0 + handles.len() as u32 {
            // 异常返回时短暂退避并检查停止信号，防止 CPU 空转
            if unsafe { WaitForSingleObject(stop_event, 500) } == WAIT_OBJECT_0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        // 进入 1.5s 防抖窗口：仅等待停止信号，合并安装/卸载过程中的突发批量文件与注册表写入
        if unsafe { WaitForSingleObject(stop_event, DEBOUNCE_MS) } == WAIT_OBJECT_0 {
            break;
        }

        let mut flags = 0usize;
        // 统一检查并重新挂载所有已触发句柄，若目标已失效则自动剔除防死循环
        dirs.retain(|d| {
            let signaled = unsafe { WaitForSingleObject(d.handle, 0) } == WAIT_OBJECT_0;
            if signaled {
                flags |= WATCH_FLAG_DESKTOP;
                d.rearm()
            } else {
                true
            }
        });
        regs.retain(|r| {
            let signaled = unsafe { WaitForSingleObject(r.event, 0) } == WAIT_OBJECT_0;
            if signaled {
                flags |= if r.is_uwp {
                    WATCH_FLAG_UWP
                } else {
                    WATCH_FLAG_DESKTOP
                };
                r.rearm()
            } else {
                true
            }
        });

        if flags != 0 {
            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_WATCH_EVENT, WPARAM(flags), LPARAM(0));
            }
        }
        if dirs.is_empty() && regs.is_empty() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_watch_signals_on_file_creation_and_rearms() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_path_buf();
        let watch = DirWatch::open(&p).expect("DirWatch 应成功打开临时目录");

        let lnk = p.join("demo.lnk");
        std::fs::write(&lnk, b"test").unwrap();
        let wait = unsafe { WaitForSingleObject(watch.handle, 1000) };
        assert_eq!(wait, WAIT_OBJECT_0);
        assert!(watch.rearm());

        std::fs::remove_file(&lnk).unwrap();
        let wait2 = unsafe { WaitForSingleObject(watch.handle, 1000) };
        assert_eq!(wait2, WAIT_OBJECT_0);
    }
}
