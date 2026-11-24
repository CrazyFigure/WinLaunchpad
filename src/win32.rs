//! Win32 小工具：宽字符串、错误信息、路径、字体、消息参数拆分等。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{w, HRESULT, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH,
    FF_DONTCARE, OUT_DEFAULT_PRECIS, HFONT,
};
use windows::Win32::System::Diagnostics::Debug::{
    FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
};
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

/// 转为以 0 结尾的 UTF-16。
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// 从以 0 结尾的缓冲区取字符串。
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// PWSTR -> String，并释放 Shell 分配的内存。
pub fn take_pwstr(p: PWSTR) -> String {
    let s = if p.0.is_null() { String::new() } else { unsafe { p.to_string().unwrap_or_default() } };
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// 把系统错误码转成可读文本。
pub fn error_text(err: u32) -> String {
    unsafe {
        let mut buf: [u16; 1024] = [0; 1024];
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            None,
            err,
            0,
            PWSTR(buf.as_mut_ptr()),
            buf.len() as u32,
            None,
        );
        if n == 0 {
            return format!("错误 {err}");
        }
        from_wide(&buf).trim().to_string()
    }
}

pub fn hresult_text(hr: HRESULT) -> String {
    if hr.is_ok() {
        return String::new();
    }
    let code = hr.0 as u32;
    if (code & 0xFFFF_0000) == 0x8007_0000 {
        // WIN32 错误被包在 HRESULT 里
        error_text(code & 0xFFFF)
    } else {
        format!("HRESULT 0x{code:08X}")
    }
}

/// windows crate 的 Error 转文本。
pub fn error_string(e: windows::core::Error) -> String {
    let hr = e.code();
    hresult_text(hr)
}

pub fn last_error_text() -> String {
    error_text(unsafe { GetLastError().0 })
}

/// 当前模块实例句柄。
pub fn hinstance() -> HINSTANCE {
    HINSTANCE(unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
        .map(|m| m.0)
        .unwrap_or(std::ptr::null_mut()))
}

/// 当前可执行文件路径。
pub fn exe_path() -> PathBuf {
    unsafe {
        let mut buf = [0u16; 1024];
        let n = windows::Win32::System::LibraryLoader::GetModuleFileNameW(None, &mut buf);
        PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// %LOCALAPPDATA%
pub fn local_app_data() -> PathBuf {
    unsafe {
        match SHGetKnownFolderPath(&FOLDERID_LocalAppData, Default::default(), None) {
            Ok(p) => PathBuf::from(take_pwstr(p)),
            Err(_) => std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

/// 路径是否指向一个存在的普通文件。
#[allow(dead_code)]
pub fn is_file(path: &str) -> bool {
    !path.is_empty() && Path::new(path).is_file()
}

/// 创建 GDI 字体（像素高度 = -pt*dpi/72 取整）。
pub fn create_font(dpi: u32, pt: i32, bold: bool) -> HFONT {
    let px = -((pt as i64 * dpi as i64 + 36) / 72) as i32;
    unsafe {
        CreateFontW(
            px,
            0,
            0,
            0,
            if bold { 600 } else { 400 },
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Microsoft YaHei UI"),
        )
    }
}

/// 给控件设置字体。
pub fn set_control_font(hwnd: HWND, font: HFONT) {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_SETFONT,
            WPARAM(font.0 as usize),
            LPARAM(1),
        );
    }
}

/// 屏幕上某点的 DPI（取所在显示器）。
pub fn dpi_at_point(x: i32, y: i32) -> u32 {
    unsafe {
        let mon = windows::Win32::Graphics::Gdi::MonitorFromPoint(
            POINT { x, y },
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );
        let mut dx = 96u32;
        let mut dy = 96u32;
        if windows::Win32::UI::HiDpi::GetDpiForMonitor(
            mon,
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut dx,
            &mut dy,
        )
        .is_ok()
        {
            return dx;
        }
        96
    }
}

/// 由客户区尺寸推算窗口外框尺寸（含标题栏）。
#[allow(dead_code)]
pub fn outer_size(cw: i32, ch: i32, style: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE, ex: windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE) -> (i32, i32) {
    unsafe {
        let mut r = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let dpi = dpi_scale_at_cursor() * 96.0;
        let _ = windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi as u32);
        (r.right - r.left, r.bottom - r.top)
    }
}

/// 鼠标所在显示器的缩放比例。
pub fn dpi_scale_at_cursor() -> f32 {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        dpi_at_point(pt.x, pt.y) as f32 / 96.0
    }
}

/// 取窗口 DPI（未就绪时回退 96）。
pub fn window_dpi(hwnd: HWND) -> u32 {
    let d = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    if d == 0 {
        96
    } else {
        d
    }
}

// ---------- lParam / wParam 拆分 ----------

pub fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}
pub fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}
pub fn get_x_lparam(l: isize) -> i32 {
    (l & 0xFFFF) as u16 as i16 as i32
}
pub fn get_y_lparam(l: isize) -> i32 {
    ((l >> 16) & 0xFFFF) as u16 as i16 as i32
}
pub fn get_wheel_delta(w: usize) -> i16 {
    hiword(w) as i16
}

/// 为无边框弹窗启用 Windows 11 原生圆角与阴影。
pub fn enable_rounded_window(hwnd: HWND) {
    use windows::Win32::Graphics::Dwm::{
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
        DWMWCP_ROUND,
    };
    use windows::Win32::UI::Controls::MARGINS;
    unsafe {
        let pref = DWMWCP_ROUND.0 as u32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
        // 扩展 1px 底边帧以获得 Windows 11 原生窗口投影
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 0,
            cyBottomHeight: 1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

/// 在资源管理器中选中文件或打开目录/链接。
pub fn open_shell_path(verb: &str, file: &str, params: Option<&str>) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        let wv = wide(verb);
        let wf = wide(file);
        let wp = params.map(wide);
        let _ = ShellExecuteW(
            None,
            PCWSTR(wv.as_ptr()),
            PCWSTR(wf.as_ptr()),
            wp.as_ref().map(|v| PCWSTR(v.as_ptr())).unwrap_or(PCWSTR::null()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// 打开多文件选择对话框，返回选中的 `.exe` / `.lnk` 路径列表。
pub fn pick_app_files(owner: HWND) -> Vec<PathBuf> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
        OPENFILENAMEW,
    };
    let mut buf = vec![0u16; 16384];
    let filter = "程序 (*.exe;*.lnk)\0*.exe;*.lnk\0所有文件 (*.*)\0*.*\0\0";
    let wfilter: Vec<u16> = filter.encode_utf16().collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(wfilter.as_ptr()),
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: w!("选择要添加的程序"),
        Flags: OFN_EXPLORER | OFN_ALLOWMULTISELECT | OFN_FILEMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    unsafe {
        if !GetOpenFileNameW(&mut ofn).as_bool() {
            return Vec::new();
        }
    }
    // 解析多选缓冲区：单选时仅 1 个以 0 结尾的路径；多选时先是目录，后续为文件名，以双 0 结尾
    let mut parts = Vec::new();
    let mut start = 0usize;
    for i in 0..buf.len() {
        if buf[i] == 0 {
            if i == start {
                break;
            }
            parts.push(String::from_utf16_lossy(&buf[start..i]));
            start = i + 1;
        }
    }
    if parts.is_empty() {
        Vec::new()
    } else if parts.len() == 1 {
        vec![PathBuf::from(&parts[0])]
    } else {
        let dir = PathBuf::from(&parts[0]);
        parts[1..].iter().map(|name| dir.join(name)).collect()
    }
}

/// 稳定的 FNV-1a 64 位哈希（用于图标和壁纸缓存文件名）。
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
