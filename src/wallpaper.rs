//! 桌面壁纸毛玻璃背景：短时工作进程解码缩放并做多层模糊后写入轻量缓存，
//! 主进程仅保留约 512×320 像素的模糊图，通过 GDI HALFTONE StretchBlt 毫秒级铺满全屏。

use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, SetBrushOrgEx,
    SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC,
    HGDIOBJ, HALFTONE, SRCCOPY,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};

use crate::win32::{fnv1a64, from_wide};

const CACHE_MAGIC: &[u8; 4] = b"WLPB";
const MAX_BLUR_DIM: u32 = 512;

/// 获取当前系统壁纸路径及文件指纹，用于判断缓存是否过期。
pub fn current_wallpaper_sig() -> Option<(PathBuf, u64)> {
    let mut path_buf = [0u16; 1024];
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            path_buf.len() as u32,
            Some(path_buf.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    let mut candidates = Vec::new();
    if ok.is_ok() {
        let p = PathBuf::from(from_wide(&path_buf));
        if p.is_file() {
            candidates.push(p);
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let transcoded = PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Themes")
            .join("TranscodedWallpaper");
        if transcoded.is_file() {
            candidates.push(transcoded);
        }
    }
    let path = candidates.into_iter().next()?;
    let meta = std::fs::metadata(&path).ok()?;
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut key = path.to_string_lossy().to_lowercase().into_bytes();
    key.extend_from_slice(&len.to_le_bytes());
    key.extend_from_slice(&mtime.to_le_bytes());
    Some((path, fnv1a64(&key)))
}

/// 工作进程入口：解码壁纸、缩放、三轮盒式高斯近似模糊并施加磨砂暗化后落盘。
pub fn worker_build_cache(out_path: &Path) -> bool {
    let Some((wp_path, sig)) = current_wallpaper_sig() else {
        return false;
    };
    let Some((w, h, mut px)) = decode_and_scale(&wp_path, MAX_BLUR_DIM) else {
        return false;
    };
    box_blur_3pass(&mut px, w as usize, h as usize, 18);
    apply_frosted_tint(&mut px, w as usize, h as usize);

    let mut out = Vec::with_capacity(20 + px.len());
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&sig.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&px);
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = out_path.with_extension("tmp");
    if std::fs::write(&tmp, &out).is_err() {
        return false;
    }
    std::fs::rename(&tmp, out_path).is_ok()
}

/// 用 WIC 解码壁纸并缩放到较小尺寸以控制内存峰值。
fn decode_and_scale(path: &Path, max_dim: u32) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let need_uninit = hr.is_ok();
        let res = decode_and_scale_inner(path, max_dim);
        if need_uninit {
            CoUninitialize();
        }
        res
    }
}

unsafe fn decode_and_scale_inner(path: &Path, max_dim: u32) -> Option<(u32, u32, Vec<u8>)> {
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
    let wpath = crate::win32::wide(path);
    let decoder = factory
        .CreateDecoderFromFilename(
            PCWSTR(wpath.as_ptr()),
            None,
            windows::Win32::Foundation::GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )
        .ok()?;
    let frame = decoder.GetFrame(0).ok()?;
    let mut ow = 0u32;
    let mut oh = 0u32;
    frame.GetSize(&mut ow, &mut oh).ok()?;
    if ow == 0 || oh == 0 {
        return None;
    }
    let scale = (max_dim as f32 / ow as f32).min(max_dim as f32 / oh as f32).min(1.0);
    let tw = ((ow as f32 * scale).round() as u32).max(64);
    let th = ((oh as f32 * scale).round() as u32).max(64);

    let scaler = factory.CreateBitmapScaler().ok()?;
    scaler.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant).ok()?;
    let conv = factory.CreateFormatConverter().ok()?;
    conv.Initialize(
        &scaler,
        &GUID_WICPixelFormat32bppBGRA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        WICBitmapPaletteTypeCustom,
    )
    .ok()?;
    let stride = tw * 4;
    let mut buf = vec![0u8; (stride * th) as usize];
    conv.CopyPixels(std::ptr::null(), stride, &mut buf).ok()?;
    Some((tw, th, buf))
}

/// 三轮分离式盒式模糊（滑动窗口 O(W×H)，三次叠加近似高斯模糊）。
fn box_blur_3pass(px: &mut [u8], w: usize, h: usize, radius: usize) {
    if w < 4 || h < 4 || radius == 0 {
        return;
    }
    let mut tmp = vec![0u8; px.len()];
    for _ in 0..3 {
        box_blur_h(px, &mut tmp, w, h, radius);
        box_blur_v(&tmp, px, w, h, radius);
    }
}

fn box_blur_h(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    let div = (r * 2 + 1) as u32;
    for y in 0..h {
        let row = y * w * 4;
        let mut sb = 0u32;
        let mut sg = 0u32;
        let mut sr = 0u32;
        for dx in -(r as isize)..=(r as isize) {
            let x = dx.clamp(0, (w - 1) as isize) as usize;
            let i = row + x * 4;
            sb += src[i] as u32;
            sg += src[i + 1] as u32;
            sr += src[i + 2] as u32;
        }
        for x in 0..w {
            let di = row + x * 4;
            dst[di] = (sb / div) as u8;
            dst[di + 1] = (sg / div) as u8;
            dst[di + 2] = (sr / div) as u8;
            dst[di + 3] = 255;
            let add_x = (x + r + 1).min(w - 1);
            let sub_x = x.saturating_sub(r);
            let ai = row + add_x * 4;
            let si = row + sub_x * 4;
            sb = sb + src[ai] as u32 - src[si] as u32;
            sg = sg + src[ai + 1] as u32 - src[si + 1] as u32;
            sr = sr + src[ai + 2] as u32 - src[si + 2] as u32;
        }
    }
}

fn box_blur_v(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    let div = (r * 2 + 1) as u32;
    let stride = w * 4;
    for x in 0..w {
        let col = x * 4;
        let mut sb = 0u32;
        let mut sg = 0u32;
        let mut sr = 0u32;
        for dy in -(r as isize)..=(r as isize) {
            let y = dy.clamp(0, (h - 1) as isize) as usize;
            let i = y * stride + col;
            sb += src[i] as u32;
            sg += src[i + 1] as u32;
            sr += src[i + 2] as u32;
        }
        for y in 0..h {
            let di = y * stride + col;
            dst[di] = (sb / div) as u8;
            dst[di + 1] = (sg / div) as u8;
            dst[di + 2] = (sr / div) as u8;
            dst[di + 3] = 255;
            let add_y = (y + r + 1).min(h - 1);
            let sub_y = y.saturating_sub(r);
            let ai = add_y * stride + col;
            let si = sub_y * stride + col;
            sb = sb + src[ai] as u32 - src[si] as u32;
            sg = sg + src[ai + 1] as u32 - src[si + 1] as u32;
            sr = sr + src[ai + 2] as u32 - src[si + 2] as u32;
        }
    }
}

/// 叠加柔和深色半透明蒙版与边缘暗角，确保浅色桌面壁纸下白色图标文字依然清晰。
fn apply_frosted_tint(px: &mut [u8], w: usize, h: usize) {
    for y in 0..h {
        let vy = (y as f32 / h.max(1) as f32 - 0.5) * 2.0;
        for x in 0..w {
            let vx = (x as f32 / w.max(1) as f32 - 0.5) * 2.0;
            let dist = ((vx * vx + vy * vy) * 0.5).min(1.0);
            let dim = 0.34 + 0.18 * dist;
            let keep = 1.0 - dim;
            let i = (y * w + x) * 4;
            px[i] = (px[i] as f32 * keep + 22.0 * dim).min(255.0) as u8;
            px[i + 1] = (px[i + 1] as f32 * keep + 16.0 * dim).min(255.0) as u8;
            px[i + 2] = (px[i + 2] as f32 * keep + 12.0 * dim).min(255.0) as u8;
            px[i + 3] = 255;
        }
    }
}

/// 主进程持有的轻量毛玻璃壁纸纹理（仅约 600KB~1MB）。
pub struct WallpaperTexture {
    pub sig: u64,
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
    dc: Option<HDC>,
    bmp: Option<HGDIOBJ>,
    old: Option<HGDIOBJ>,
}

impl Default for WallpaperTexture {
    fn default() -> Self {
        Self {
            sig: 0,
            w: 0,
            h: 0,
            px: Vec::new(),
            dc: None,
            bmp: None,
            old: None,
        }
    }
}

impl WallpaperTexture {
    pub fn load_from_file(&mut self, path: &Path) -> bool {
        let Ok(data) = std::fs::read(path) else {
            return false;
        };
        if data.len() < 20 || &data[0..4] != CACHE_MAGIC {
            return false;
        }
        let sig = u64::from_le_bytes(data[4..12].try_into().unwrap());
        let w = u32::from_le_bytes(data[12..16].try_into().unwrap());
        let h = u32::from_le_bytes(data[16..20].try_into().unwrap());
        let expected = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if w == 0 || h == 0 || data.len() != 20 + expected {
            return false;
        }
        self.release_gdi();
        self.sig = sig;
        self.w = w;
        self.h = h;
        self.px = data[20..].to_vec();
        self.build_gdi()
    }

    fn build_gdi(&mut self) -> bool {
        unsafe {
            let dc = match CreateCompatibleDC(None) {
                dc if !dc.is_invalid() => dc,
                _ => return false,
            };
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: self.w as i32,
                    biHeight: -(self.h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0 as u32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut ptr, None, 0).unwrap_or_default();
            if bmp.is_invalid() || ptr.is_null() {
                let _ = DeleteDC(dc);
                return false;
            }
            std::ptr::copy_nonoverlapping(self.px.as_ptr(), ptr as *mut u8, self.px.len());
            let old = SelectObject(dc, bmp);
            self.dc = Some(dc);
            self.bmp = Some(HGDIOBJ(bmp.0));
            self.old = Some(old);
            true
        }
    }

    pub fn is_ready(&self) -> bool {
        self.dc.is_some() && self.w > 0 && self.h > 0
    }

    /// 按桌面 Fill 模式等比裁剪放大到目标窗口 DC。
    pub fn blit_cover(&self, dst_dc: HDC, dst_w: i32, dst_h: i32) -> bool {
        let Some(src_dc) = self.dc else {
            return false;
        };
        if dst_w <= 0 || dst_h <= 0 || self.w == 0 || self.h == 0 {
            return false;
        }
        let (sx, sy, sw, sh) = self.cover_crop(dst_w, dst_h);
        unsafe {
            let _ = SetStretchBltMode(dst_dc, HALFTONE);
            let _ = SetBrushOrgEx(dst_dc, 0, 0, None);
            let _ = StretchBlt(dst_dc, 0, 0, dst_w, dst_h, src_dc, sx, sy, sw, sh, SRCCOPY);
        }
        true
    }

    fn cover_crop(&self, dst_w: i32, dst_h: i32) -> (i32, i32, i32, i32) {
        let src_ar = self.w as f32 / self.h as f32;
        let dst_ar = dst_w.max(1) as f32 / dst_h.max(1) as f32;
        if src_ar > dst_ar {
            let sw = ((self.h as f32 * dst_ar).round() as i32).clamp(1, self.w as i32);
            let sx = (self.w as i32 - sw) / 2;
            (sx, 0, sw, self.h as i32)
        } else {
            let sh = ((self.w as f32 / dst_ar).round() as i32).clamp(1, self.h as i32);
            let sy = (self.h as i32 - sh) / 2;
            (0, sy, self.w as i32, sh)
        }
    }

    /// 采样目标搜索框区域的平均背景色，用于计算 EDIT 控件无接缝混合底色。
    pub fn sample_region_rgb(&self, r: &RECT, dst_w: i32, dst_h: i32) -> (f32, f32, f32) {
        if self.px.is_empty() || dst_w <= 0 || dst_h <= 0 {
            return (0.14, 0.17, 0.24);
        }
        let (sx, sy, sw, sh) = self.cover_crop(dst_w, dst_h);
        let x0 = sx + ((r.left.max(0) as f32 / dst_w as f32) * sw as f32).round() as i32;
        let x1 = sx + ((r.right.min(dst_w) as f32 / dst_w as f32) * sw as f32).round() as i32;
        let y0 = sy + ((r.top.max(0) as f32 / dst_h as f32) * sh as f32).round() as i32;
        let y1 = sy + ((r.bottom.min(dst_h) as f32 / dst_h as f32) * sh as f32).round() as i32;
        let x0 = x0.clamp(0, self.w as i32 - 1);
        let x1 = x1.clamp(x0 + 1, self.w as i32);
        let y0 = y0.clamp(0, self.h as i32 - 1);
        let y1 = y1.clamp(y0 + 1, self.h as i32);
        let mut sr = 0u64;
        let mut sg = 0u64;
        let mut sb = 0u64;
        let mut cnt = 0u64;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y as usize * self.w as usize + x as usize) * 4;
                sb += self.px[i] as u64;
                sg += self.px[i + 1] as u64;
                sr += self.px[i + 2] as u64;
                cnt += 1;
            }
        }
        if cnt == 0 {
            return (0.14, 0.17, 0.24);
        }
        (
            sr as f32 / cnt as f32 / 255.0,
            sg as f32 / cnt as f32 / 255.0,
            sb as f32 / cnt as f32 / 255.0,
        )
    }

    pub fn release_gdi(&mut self) {
        unsafe {
            if let (Some(dc), Some(old)) = (self.dc, self.old) {
                SelectObject(dc, old);
            }
            if let Some(b) = self.bmp {
                let _ = DeleteObject(b);
            }
            if let Some(dc) = self.dc {
                let _ = DeleteDC(dc);
            }
        }
        self.dc = None;
        self.bmp = None;
        self.old = None;
    }
}

impl Drop for WallpaperTexture {
    fn drop(&mut self) {
        self.release_gdi();
    }
}
