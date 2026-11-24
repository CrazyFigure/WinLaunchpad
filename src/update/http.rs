//! 用系统 WinHTTP 跟随代理与 HTTPS 下载重定向，保留原生程序的依赖规模。

use std::{ffi::c_void, ptr};
use windows::core::{w, Error, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

use crate::win32::wide;

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> windows::core::Result<Self> {
        if raw.is_null() {
            return Err(Error::from_win32());
        }
        Ok(Self(raw))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// 在后台线程读取官方服务器响应。限制超时及实际读入大小，避免异常响应占用过多内存。
pub(super) fn get(host: &str, path: &str, accept: PCWSTR, limit: usize) -> Result<Vec<u8>, String> {
    let host = wide(host);
    let path = wide(path);
    unsafe {
        let session = Handle::new(WinHttpOpen(
            w!("WinLaunchpad"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))
        .map_err(|err| err.to_string())?;
        WinHttpSetTimeouts(session.0, 5_000, 8_000, 12_000, 20_000)
            .map_err(|err| err.to_string())?;
        let connection = Handle::new(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), 443, 0))
            .map_err(|err| err.to_string())?;
        let types = [accept, PCWSTR::null()];
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            types.as_ptr(),
            WINHTTP_FLAG_SECURE,
        ))
        .map_err(|err| err.to_string())?;
        let redirect = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP.to_ne_bytes();
        WinHttpSetOption(
            Some(request.0),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&redirect),
        )
        .map_err(|err| err.to_string())?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|err| err.to_string())?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).map_err(|err| err.to_string())?;
        let mut status = 0u32;
        let mut status_bytes = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(ptr::from_mut(&mut status).cast()),
            &mut status_bytes,
            ptr::null_mut(),
        )
        .map_err(|err| err.to_string())?;
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }
        let mut out = Vec::new();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let mut read = 0;
            WinHttpReadData(
                request.0,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut read,
            )
            .map_err(|err| err.to_string())?;
            if read == 0 {
                return Ok(out);
            }
            if out.len() + read as usize > limit {
                return Err("发布服务器返回的数据超过大小限制".into());
            }
            out.extend_from_slice(&buffer[..read as usize]);
        }
    }
}
