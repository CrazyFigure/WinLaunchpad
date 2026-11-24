//! 全局快捷键：注册、显示名、按键名称。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::KeyboardAndMouse::MAPVK_VK_TO_VSC;

pub const HOTKEY_ID: i32 = 1;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct HotkeySpec {
    pub mods: u32,
    pub vk: u32,
}

impl Default for HotkeySpec {
    fn default() -> Self {
        // 默认 Ctrl+Alt+Shift+A，避免抢占常见的 Alt+字母 菜单快捷键。
        HotkeySpec {
            mods: (MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT).0 as u32,
            vk: 'A' as u32,
        }
    }
}

impl HotkeySpec {
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mods & MOD_WIN.0 as u32 != 0 {
            parts.push("Win".into());
        }
        if self.mods & MOD_CONTROL.0 as u32 != 0 {
            parts.push("Ctrl".into());
        }
        if self.mods & MOD_ALT.0 as u32 != 0 {
            parts.push("Alt".into());
        }
        if self.mods & MOD_SHIFT.0 as u32 != 0 {
            parts.push("Shift".into());
        }
        parts.push(key_name(self.vk));
        parts.join(" + ")
    }
}

/// 当前按下的修饰键（用于设置窗口录入）。
pub fn current_mods() -> u32 {
    unsafe {
        let mut m = 0u32;
        if GetKeyState(VK_CONTROL.0 as i32) < 0 {
            m |= MOD_CONTROL.0 as u32;
        }
        if GetKeyState(VK_MENU.0 as i32) < 0 {
            m |= MOD_ALT.0 as u32;
        }
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            m |= MOD_SHIFT.0 as u32;
        }
        if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 {
            m |= MOD_WIN.0 as u32;
        }
        m
    }
}

pub fn is_modifier(vk: u32) -> bool {
    matches!(
        vk,
        x if x == VK_SHIFT.0 as u32
            || x == VK_CONTROL.0 as u32
            || x == VK_MENU.0 as u32
            || x == VK_LWIN.0 as u32
            || x == VK_RWIN.0 as u32
    )
}

/// 读取系统按键名称，例如 "A"、"F5"、"Tab"。
pub fn key_name(vk: u32) -> String {
    unsafe {
        let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let mut buf = [0u16; 64];
        let n = GetKeyNameTextW(((sc as i32) << 16) as i32, &mut buf);
        if n > 0 {
            let s = crate::win32::from_wide(&buf);
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
        }
        if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
            return ((vk as u8) as char).to_string();
        }
        format!("VK_{vk}")
    }
}

/// 注册全局热键。成功返回 true。
pub fn register(hwnd: HWND, spec: HotkeySpec) -> bool {
    unsafe {
        let mods = spec.mods & !(MOD_NOREPEAT.0 as u32);
        RegisterHotKey(
            hwnd,
            HOTKEY_ID,
            HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0 as u32),
            spec.vk,
        )
        .is_ok()
    }
}

pub fn unregister(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(hwnd, HOTKEY_ID);
    }
}

