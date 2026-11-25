//! 托盘图标与右键菜单。

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics, LoadImageW,
    SetForegroundWindow, TrackPopupMenu, HICON, IMAGE_FLAGS, IMAGE_ICON, MF_STRING, SM_CXSMICON,
    TPM_RIGHTBUTTON,
};

use crate::win32::wide;

pub const CMD_TOGGLE: u32 = 1;
pub const CMD_RESCAN: u32 = 2;
pub const CMD_SETTINGS: u32 = 3;
pub const CMD_EXIT: u32 = 5;

pub struct Tray {
    pub hwnd: HWND,
    pub hicon: HICON,
    pub added: bool,
}

impl Tray {
    pub fn new(hwnd: HWND, hinst: windows::Win32::Foundation::HINSTANCE) -> Tray {
        let sz = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let hicon = unsafe {
            LoadImageW(
                hinst,
                PCWSTR(1usize as *const u16),
                IMAGE_ICON,
                sz,
                sz,
                IMAGE_FLAGS(0x8000), // LR_DEFAULTCOLOR
            )
            .map(|h| HICON(h.0))
            .unwrap_or_default()
        };
        Tray { hwnd, hicon, added: false }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut tip = [0u16; 128];
        for (i, c) in wide("WinLaunchpad 启动台").into_iter().take(127).enumerate() {
            tip[i] = c;
        }
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: crate::app::WM_APP_TRAY,
            hIcon: self.hicon,
            szTip: tip,
            ..Default::default()
        }
    }

    pub fn add(&mut self) {
        unsafe {
            if self.added {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data());
            } else {
                self.added = Shell_NotifyIconW(NIM_ADD, &self.data()).as_bool();
            }
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
            self.added = false;
        }
    }

    /// 弹出右键菜单。选择结果通过 WM_COMMAND（lparam == 0）送到窗口。
    pub fn show_menu(&self, visible: bool) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let toggle_label = wide(if visible { "收起启动台" } else { "打开启动台" });
            let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE as usize, PCWSTR(toggle_label.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, CMD_RESCAN as usize, w!("刷新应用图标"));
            // 托盘菜单仅保留常用操作入口，开机启动统一归入设置页管理
            let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS as usize, w!("设置"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT as usize, w!("退出"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, self.hwnd, None);
            let _ = DestroyMenu(menu);
        }
    }
}

