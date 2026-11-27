//! 共享软件抗锯齿绘图与现代界面组件库（32bpp DIB 像素缓冲）。
//!
//! 统一封装圆角矩形、抗锯齿圆/线、图标 Lanczos3 高清重采样、文字阴影与测量、
//! 现代复选框/开关/按钮/滚动条、矢量手势轨迹、火箭徽标与 100ms Tooltip 浮层。

use std::sync::OnceLock;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
    D2D1_DRAW_TEXT_OPTIONS, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteInlineObject, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_EMERGENCY_BREAK,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, GdiFlush, GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    DRAW_TEXT_FORMAT, HBRUSH, HDC, HFONT, HGDIOBJ, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindow, RegisterClassExW, SetWindowPos,
    ShowWindow, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNA, ULW_ALPHA, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

pub type Rgba = (f32, f32, f32, f32);

/// RGB u32 颜色常量辅助构造（alpha 0..1）。
#[inline]
pub const fn rgb(hex: u32, a: f32) -> Rgba {
    (
        ((hex >> 16) & 0xFF) as f32 / 255.0,
        ((hex >> 8) & 0xFF) as f32 / 255.0,
        (hex & 0xFF) as f32 / 255.0,
        a,
    )
}

pub fn to_colorref(c: Rgba) -> COLORREF {
    let r = (c.0.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.1.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.2.clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF((b << 16) | (g << 8) | r)
}

#[inline]
fn pack_bgra(r: f32, g: f32, b: f32) -> u32 {
    let ri = (r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let gi = (g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let bi = (b.clamp(0.0, 1.0) * 255.0).round() as u32;
    0xFF00_0000 | (ri << 16) | (gi << 8) | bi
}

#[derive(Clone)]
pub struct IconPixels {
    pub w: u32,
    pub h: u32,
    pub px: std::sync::Arc<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Unchecked,
    Checked,
    Indeterminate,
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnStyle {
    Primary,
    Secondary,
    Ghost,
    DangerGhost,
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GestureGlyph {
    Left,
    Right,
    Up,
    UpDown,
    DownUp,
}

/// 创建 ClearType 抗锯齿字体（像素高度，负值表示字符高度）。
pub fn make_font(px: i32, weight: i32) -> HFONT {
    let px = px.max(9);
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            1, // DEFAULT_CHARSET
            0,
            0,
            5, // CLEARTYPE_QUALITY
            0,
            w!("Microsoft YaHei UI"),
        )
    }
}

/// 可重用的 32bpp DIB 后备缓冲，提供显式释放以严格控制常驻内存。
pub struct DibTarget {
    pub mem_dc: Option<HDC>,
    pub dib: Option<HGDIOBJ>,
    pub old_bmp: Option<HGDIOBJ>,
    pub ptr: *mut std::ffi::c_void,
    pub w: i32,
    pub h: i32,
}

impl Default for DibTarget {
    fn default() -> Self {
        Self {
            mem_dc: None,
            dib: None,
            old_bmp: None,
            ptr: std::ptr::null_mut(),
            w: 0,
            h: 0,
        }
    }
}

impl DibTarget {
    /// 确保后备缓冲至少达到指定宽高，按 128px 分档扩容以避免实时手势拖动期间频繁重建 DIB。
    pub fn ensure_at_least(&mut self, w: i32, h: i32) -> bool {
        let w = w.max(1);
        let h = h.max(1);
        if self.mem_dc.is_some() && self.w >= w && self.h >= h && !self.ptr.is_null() {
            return true;
        }
        let bw = (((w + 127) / 128) * 128).max(128);
        let bh = (((h + 127) / 128) * 128).max(128);
        self.ensure(bw, bh)
    }

    pub fn ensure(&mut self, w: i32, h: i32) -> bool {
        let w = w.max(1);
        let h = h.max(1);
        if self.mem_dc.is_some() && self.w == w && self.h == h && !self.ptr.is_null() {
            return true;
        }
        self.release();
        unsafe {
            let dc = match CreateCompatibleDC(None) {
                dc if !dc.is_invalid() => dc,
                _ => return false,
            };
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
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
            let old = SelectObject(dc, bmp);
            self.mem_dc = Some(dc);
            self.dib = Some(HGDIOBJ(bmp.0));
            self.old_bmp = Some(old);
            self.ptr = ptr;
            self.w = w;
            self.h = h;
            true
        }
    }

    pub fn release(&mut self) {
        unsafe {
            if let (Some(dc), Some(old)) = (self.mem_dc, self.old_bmp) {
                SelectObject(dc, old);
            }
            if let Some(d) = self.dib {
                let _ = DeleteObject(d);
            }
            if let Some(dc) = self.mem_dc {
                let _ = DeleteDC(dc);
            }
        }
        self.mem_dc = None;
        self.dib = None;
        self.old_bmp = None;
        self.ptr = std::ptr::null_mut();
        self.w = 0;
        self.h = 0;
    }

    pub fn surface(&mut self) -> Option<Surface> {
        if self.ptr.is_null() || self.w <= 0 || self.h <= 0 {
            return None;
        }
        Some(Surface {
            w: self.w as usize,
            h: self.h as usize,
            ptr: self.ptr as *mut u32,
            clip: None,
        })
    }
}

impl Drop for DibTarget {
    fn drop(&mut self) {
        self.release();
    }
}

/// 指向 32bpp BGRA 内存的轻量绘制表面，支持可选矩形裁剪。
pub struct Surface {
    pub w: usize,
    pub h: usize,
    pub ptr: *mut u32,
    pub clip: Option<RECT>,
}

impl Surface {
    pub fn set_clip(&mut self, r: Option<RECT>) {
        self.clip = r;
    }

    #[inline]
    fn in_bounds(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return false;
        }
        if let Some(c) = self.clip {
            return x >= c.left && x < c.right && y >= c.top && y < c.bottom;
        }
        true
    }

    pub fn clear(&mut self, c: Rgba) {
        let px = pack_bgra(c.0, c.1, c.2);
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        slice.fill(px);
    }

    /// 将指定矩形区域清空为全透明像素（0x00000000），供 UpdateLayeredWindow 透明浮窗复用缓冲。
    pub fn clear_rect_transparent(&mut self, w: i32, h: i32) {
        let cw = (w.max(0) as usize).min(self.w);
        let ch = (h.max(0) as usize).min(self.h);
        if cw == 0 || ch == 0 {
            return;
        }
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..ch {
            let row = y * self.w;
            slice[row..row + cw].fill(0);
        }
    }

    /// 将指定矩形区域清空为纯黑不透明像素，供 GDI 文字生成灰度 Alpha 遮罩使用。
    pub fn clear_rect_black(&mut self, w: i32, h: i32) {
        let cw = (w.max(0) as usize).min(self.w);
        let ch = (h.max(0) as usize).min(self.h);
        if cw == 0 || ch == 0 {
            return;
        }
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..ch {
            let row = y * self.w;
            slice[row..row + cw].fill(0xFF00_0000);
        }
    }

    /// 在透明表面上执行标准预乘 BGRA Porter-Duff OVER 合成，保证 RGB 始终 <= A。
    #[inline]
    pub fn blend_premul_over(&mut self, x: i32, y: i32, b: u32, g: u32, r: u32, a: u32) {
        if a == 0 || !self.in_bounds(x, y) {
            return;
        }
        let p = unsafe { &mut *self.ptr.add(y as usize * self.w + x as usize) };
        if a >= 255 {
            *p = 0xFF00_0000 | (r.min(255) << 16) | (g.min(255) << 8) | b.min(255);
            return;
        }
        let dst = *p;
        if dst == 0 {
            *p = (a << 24) | (r.min(a) << 16) | (g.min(a) << 8) | b.min(a);
            return;
        }
        let ia = 255 - a;
        let da = (dst >> 24) & 0xFF;
        let dr = (dst >> 16) & 0xFF;
        let dg = (dst >> 8) & 0xFF;
        let db = dst & 0xFF;
        let out_a = (a + (da * ia + 127) / 255).min(255);
        let out_r = (r + (dr * ia + 127) / 255).min(out_a);
        let out_g = (g + (dg * ia + 127) / 255).min(out_a);
        let out_b = (b + (db * ia + 127) / 255).min(out_a);
        *p = (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b;
    }

    pub fn clear_vertical_gradient(&mut self, top: (f32, f32, f32), mid: (f32, f32, f32), bottom: (f32, f32, f32)) {
        let h = self.h.max(1);
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..self.h {
            let t = y as f32 / h as f32;
            let (r, g, b) = if t < 0.55 {
                let k = t / 0.55;
                (
                    top.0 + (mid.0 - top.0) * k,
                    top.1 + (mid.1 - top.1) * k,
                    top.2 + (mid.2 - top.2) * k,
                )
            } else {
                let k = (t - 0.55) / 0.45;
                (
                    mid.0 + (bottom.0 - mid.0) * k,
                    mid.1 + (bottom.1 - mid.1) * k,
                    mid.2 + (bottom.2 - mid.2) * k,
                )
            };
            let c = pack_bgra(r, g, b);
            let row = y * self.w;
            slice[row..row + self.w].fill(c);
        }
    }

    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba) {
        if !self.in_bounds(x, y) {
            return;
        }
        let a = c.3;
        if a <= 0.001 {
            return;
        }
        let p = unsafe { &mut *self.ptr.add(y as usize * self.w + x as usize) };
        if a >= 0.999 {
            *p = pack_bgra(c.0, c.1, c.2);
            return;
        }
        let ia = 1.0 - a;
        let pr = ((*p >> 16) & 0xFF) as f32;
        let pg = ((*p >> 8) & 0xFF) as f32;
        let pb = (*p & 0xFF) as f32;
        let nr = pr * ia + c.0 * 255.0 * a;
        let ng = pg * ia + c.1 * 255.0 * a;
        let nb = pb * ia + c.2 * 255.0 * a;
        *p = 0xFF00_0000
            | ((nr.min(255.0) as u32) << 16)
            | ((ng.min(255.0) as u32) << 8)
            | (nb.min(255.0) as u32);
    }

    /// 计算当前裁剪区与目标矩形的交集边界。
    fn clamp_bounds(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> (i32, i32, i32, i32) {
        let mut lx0 = x0.max(0);
        let mut ly0 = y0.max(0);
        let mut lx1 = x1.min(self.w as i32);
        let mut ly1 = y1.min(self.h as i32);
        if let Some(c) = self.clip {
            lx0 = lx0.max(c.left);
            ly0 = ly0.max(c.top);
            lx1 = lx1.min(c.right);
            ly1 = ly1.min(c.bottom);
        }
        (lx0, ly0, lx1, ly1)
    }

    pub fn fill_rect(&mut self, r: &RECT, c: Rgba) {
        if c.3 <= 0.001 {
            return;
        }
        let (x0, y0, x1, y1) = self.clamp_bounds(r.left, r.top, r.right, r.bottom);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        if c.3 >= 0.999 {
            let packed = pack_bgra(c.0, c.1, c.2);
            let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
            for y in y0..y1 {
                let row = y as usize * self.w;
                slice[row + x0 as usize..row + x1 as usize].fill(packed);
            }
            return;
        }
        let a = c.3;
        let ia = 1.0 - a;
        let cr = c.0 * 255.0 * a;
        let cg = c.1 * 255.0 * a;
        let cb = c.2 * 255.0 * a;
        for y in y0..y1 {
            let row = y as usize * self.w;
            for x in x0..x1 {
                let p = unsafe { &mut *self.ptr.add(row + x as usize) };
                let pr = ((*p >> 16) & 0xFF) as f32;
                let pg = ((*p >> 8) & 0xFF) as f32;
                let pb = (*p & 0xFF) as f32;
                *p = 0xFF00_0000
                    | (((pr * ia + cr).min(255.0) as u32) << 16)
                    | (((pg * ia + cg).min(255.0) as u32) << 8)
                    | ((pb * ia + cb).min(255.0) as u32);
            }
        }
    }

    #[inline]
    fn sd_rrect(px: f32, py: f32, left: f32, top: f32, right: f32, bottom: f32, r: f32) -> f32 {
        let cx = (left + right) * 0.5;
        let cy = (top + bottom) * 0.5;
        let hw = ((right - left) * 0.5 - r).max(0.0);
        let hh = ((bottom - top) * 0.5 - r).max(0.0);
        let qx = (px - cx).abs() - hw;
        let qy = (py - cy).abs() - hh;
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
    }

    /// 将浮点 RGBA 颜色转换为预乘 BGRA 并执行 Porter-Duff OVER 合成。
    #[inline]
    fn blend_premul_rgba(&mut self, x: i32, y: i32, c: Rgba) {
        let a = (c.3.clamp(0.0, 1.0) * 255.0).round() as u32;
        if a == 0 {
            return;
        }
        let r = ((c.0.clamp(0.0, 1.0) * a as f32).round() as u32).min(a);
        let g = ((c.1.clamp(0.0, 1.0) * a as f32).round() as u32).min(a);
        let b = ((c.2.clamp(0.0, 1.0) * a as f32).round() as u32).min(a);
        self.blend_premul_over(x, y, b, g, r, a);
    }

    #[inline]
    fn blend_pixel(&mut self, x: i32, y: i32, c: Rgba, premul: bool) {
        if premul {
            self.blend_premul_rgba(x, y, c);
        } else {
            self.blend(x, y, c);
        }
    }

    /// 填充抗锯齿圆角矩形（中间核心矩形走高速填充，仅对边缘/四角做 SDF 抗锯齿）。
    pub fn fill_rrect(&mut self, r: &RECT, radius: f32, c: Rgba) {
        self.fill_rrect_impl(r, radius, c, false);
    }

    /// 在透明分层窗口表面上以预乘 Alpha 填充抗锯齿圆角矩形。
    pub fn fill_rrect_premul(&mut self, r: &RECT, radius: f32, c: Rgba) {
        self.fill_rrect_impl(r, radius, c, true);
    }

    fn fill_rrect_impl(&mut self, r: &RECT, radius: f32, c: Rgba, premul: bool) {
        if c.3 <= 0.001 || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
        let max_r = ((rr - l).min(b - t) * 0.5).max(0.0);
        let radius = radius.min(max_r);
        if radius <= 0.5 {
            if !premul {
                self.fill_rect(r, c);
            } else {
                let (x0, y0, x1, y1) = self.clamp_bounds(r.left, r.top, r.right, r.bottom);
                for y in y0..y1 {
                    for x in x0..x1 {
                        self.blend_premul_rgba(x, y, c);
                    }
                }
            }
            return;
        }
        let pad = radius.ceil() as i32 + 1;
        let inner_top = r.top + pad;
        let inner_bot = r.bottom - pad;
        let inner_left = r.left + pad;
        let inner_right = r.right - pad;

        // 中心大块区域直接填充，无需逐像素算 SDF
        if inner_top < inner_bot && inner_left < inner_right {
            let core = RECT {
                left: r.left + 1,
                top: inner_top,
                right: r.right - 1,
                bottom: inner_bot,
            };
            if !premul {
                self.fill_rect(&core, c);
            } else {
                let (cx0, cy0, cx1, cy1) = self.clamp_bounds(core.left, core.top, core.right, core.bottom);
                for y in cy0..cy1 {
                    for x in cx0..cx1 {
                        self.blend_premul_rgba(x, y, c);
                    }
                }
            }
        }

        let (x0, y0, x1, y1) = self.clamp_bounds(r.left - 1, r.top - 1, r.right + 1, r.bottom + 1);
        for y in y0..y1 {
            let in_core_y = inner_top < inner_bot && y >= inner_top && y < inner_bot;
            for x in x0..x1 {
                if in_core_y && x >= r.left + 1 && x < r.right - 1 {
                    continue;
                }
                let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, l, t, rr, b, radius);
                if d < 0.5 {
                    let cov = (0.5 - d).clamp(0.0, 1.0);
                    self.blend_pixel(x, y, (c.0, c.1, c.2, c.3 * cov), premul);
                }
            }
        }
    }

    pub fn stroke_rrect(&mut self, r: &RECT, radius: f32, width: f32, c: Rgba) {
        self.stroke_rrect_impl(r, radius, width, c, false);
    }

    /// 在透明分层窗口表面上以预乘 Alpha 绘制抗锯齿圆角边框。
    pub fn stroke_rrect_premul(&mut self, r: &RECT, radius: f32, width: f32, c: Rgba) {
        self.stroke_rrect_impl(r, radius, width, c, true);
    }

    fn stroke_rrect_impl(&mut self, r: &RECT, radius: f32, width: f32, c: Rgba, premul: bool) {
        if c.3 <= 0.001 || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (
            r.left as f32 + width * 0.5,
            r.top as f32 + width * 0.5,
            r.right as f32 - width * 0.5,
            r.bottom as f32 - width * 0.5,
        );
        let rad = (radius - width * 0.5).max(0.0);
        let (x0, y0, x1, y1) = self.clamp_bounds(
            l.floor() as i32 - 2,
            t.floor() as i32 - 2,
            rr.ceil() as i32 + 2,
            b.ceil() as i32 + 2,
        );
        let half = width * 0.5;
        let pad = (radius + width).ceil() as i32 + 2;
        let skip_l = r.left + pad;
        let skip_r = r.right - pad;
        let skip_t = r.top + pad;
        let skip_b = r.bottom - pad;
        for y in y0..y1 {
            let in_skip_y = skip_t < skip_b && y >= skip_t && y < skip_b;
            for x in x0..x1 {
                if in_skip_y && skip_l < skip_r && x >= skip_l && x < skip_r {
                    continue;
                }
                let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, l, t, rr, b, rad).abs();
                if d < half + 0.5 {
                    let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                    self.blend_pixel(x, y, (c.0, c.1, c.2, c.3 * cov), premul);
                }
            }
        }
    }

    /// 虚线圆角矩形描边（用于拖放区提示）。
    pub fn stroke_rrect_dashed(&mut self, r: &RECT, radius: f32, width: f32, dash: i32, c: Rgba) {
        if c.3 <= 0.001 || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (
            r.left as f32 + width * 0.5,
            r.top as f32 + width * 0.5,
            r.right as f32 - width * 0.5,
            r.bottom as f32 - width * 0.5,
        );
        let rad = (radius - width * 0.5).max(0.0);
        let (x0, y0, x1, y1) = self.clamp_bounds(
            l.floor() as i32 - 2,
            t.floor() as i32 - 2,
            rr.ceil() as i32 + 2,
            b.ceil() as i32 + 2,
        );
        let half = width * 0.5;
        let pad = (radius + width).ceil() as i32 + 2;
        let skip_l = r.left + pad;
        let skip_r = r.right - pad;
        let skip_t = r.top + pad;
        let skip_b = r.bottom - pad;
        let period = (dash * 2).max(2);
        for y in y0..y1 {
            let in_skip_y = skip_t < skip_b && y >= skip_t && y < skip_b;
            for x in x0..x1 {
                if in_skip_y && skip_l < skip_r && x >= skip_l && x < skip_r {
                    continue;
                }
                let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, l, t, rr, b, rad).abs();
                if d < half + 0.5 {
                    let phase = (x + y).rem_euclid(period);
                    if phase < dash {
                        let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                        self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                    }
                }
            }
        }
    }

    pub fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba) {
        if c.3 <= 0.001 || radius <= 0.0 {
            return;
        }
        let (x0, y0, x1, y1) = self.clamp_bounds(
            (cx - radius - 1.0).floor() as i32,
            (cy - radius - 1.0).floor() as i32,
            (cx + radius + 1.0).ceil() as i32,
            (cy + radius + 1.0).ceil() as i32,
        );
        for y in y0..y1 {
            for x in x0..x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() - radius;
                if d < 0.5 {
                    let cov = (0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    pub fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, width: f32, c: Rgba) {
        let outer = radius + width * 0.5;
        let (x0, y0, x1, y1) = self.clamp_bounds(
            (cx - outer - 1.0).floor() as i32,
            (cy - outer - 1.0).floor() as i32,
            (cx + outer + 1.0).ceil() as i32,
            (cy + outer + 1.0).ceil() as i32,
        );
        let half = width * 0.5;
        for y in y0..y1 {
            for x in x0..x1 {
                let d = (((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() - radius).abs();
                if d < half + 0.5 {
                    let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    /// 抗锯齿粗线段（圆头端点）。
    pub fn thick_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Rgba) {
        let half = width * 0.5;
        let pad = half + 1.5;
        let (bx0, by0, bx1, by1) = self.clamp_bounds(
            (x0.min(x1) - pad).floor() as i32,
            (y0.min(y1) - pad).floor() as i32,
            (x0.max(x1) + pad).ceil() as i32,
            (y0.max(y1) + pad).ceil() as i32,
        );
        let vx = x1 - x0;
        let vy = y1 - y0;
        let len2 = vx * vx + vy * vy;
        for y in by0..by1 {
            for x in bx0..bx1 {
                let px = x as f32 + 0.5 - x0;
                let py = y as f32 + 0.5 - y0;
                let h = if len2 > 1e-4 { ((px * vx + py * vy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let dx = px - vx * h;
                let dy = py - vy * h;
                let d = (dx * dx + dy * dy).sqrt();
                if d < half + 0.5 {
                    let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    /// 使用 Lanczos3 高品质重采样并绘制预乘 BGRA 图标（无投影，用于设置列表与添加弹窗）。
    pub fn draw_icon_resampled(&mut self, icon: &IconPixels, cx: f32, cy: f32, target_size: f32) {
        if icon.w == 0 || icon.h == 0 || target_size <= 1.0 {
            return;
        }
        let k = (target_size / icon.w as f32).min(target_size / icon.h as f32);
        let dw = (icon.w as f32 * k).round().max(1.0) as i32;
        let dh = (icon.h as f32 * k).round().max(1.0) as i32;
        if dw == icon.w as i32 && dh == icon.h as i32 {
            self.draw_icon_1to1(icon, cx, cy, dh);
            return;
        }
        let buf = resample_premul_bgra(
            icon.px.as_slice(),
            icon.w as i32,
            icon.h as i32,
            0,
            0,
            icon.w as i32 - 1,
            icon.h as i32 - 1,
            dw,
            dh,
            false,
        );
        let sprite = IconPixels {
            w: dw as u32,
            h: dh as u32,
            px: std::sync::Arc::new(buf),
        };
        self.draw_icon_1to1(&sprite, cx, cy, dh);
    }

    /// 1:1 直接贴图（用于预烘焙含阴影图标，避免逐帧双线性重采样，保障左右滑页丝滑 60+ FPS）。
    pub fn draw_icon_1to1(&mut self, sprite: &IconPixels, cx: f32, cy: f32, icon_h: i32) {
        if sprite.w == 0 || sprite.h == 0 {
            return;
        }
        let dx0 = (cx - sprite.w as f32 * 0.5).round() as i32;
        let dy0 = (cy - icon_h as f32 * 0.5).round() as i32;
        let (bx0, by0, bx1, by1) = self.clamp_bounds(dx0, dy0, dx0 + sprite.w as i32, dy0 + sprite.h as i32);
        if bx0 >= bx1 || by0 >= by1 {
            return;
        }
        let src = sprite.px.as_slice();
        let sw = sprite.w as usize;
        for y in by0..by1 {
            let sy = (y - dy0) as usize;
            let row = sy * sw;
            let dst_row = y as usize * self.w;
            for x in bx0..bx1 {
                let sx = (x - dx0) as usize;
                let si = (row + sx) * 4;
                let a = src[si + 3] as u32;
                if a == 0 {
                    continue;
                }
                let p = unsafe { &mut *self.ptr.add(dst_row + x as usize) };
                if a >= 255 {
                    *p = 0xFF00_0000
                        | ((src[si + 2] as u32) << 16)
                        | ((src[si + 1] as u32) << 8)
                        | (src[si] as u32);
                    continue;
                }
                let ia = 255 - a;
                let dst = *p;
                let dr = (dst >> 16) & 0xFF;
                let dg = (dst >> 8) & 0xFF;
                let db = dst & 0xFF;
                let nr = (src[si + 2] as u32 + (dr * ia + 127) / 255).min(255);
                let ng = (src[si + 1] as u32 + (dg * ia + 127) / 255).min(255);
                let nb = (src[si] as u32 + (db * ia + 127) / 255).min(255);
                *p = 0xFF00_0000 | (nr << 16) | (ng << 8) | nb;
            }
        }
    }
}

#[inline]
fn lanczos3(x: f32) -> f32 {
    let ax = x.abs();
    if ax < 1e-6 {
        return 1.0;
    }
    if ax >= 3.0 {
        return 0.0;
    }
    let px = std::f32::consts::PI * ax;
    (px.sin() / px) * ((px / 3.0).sin() / (px / 3.0))
}

/// 对预乘 BGRA 图像指定区域执行自适应支持域 Lanczos3 两阶段（水平+垂直）高清重采样。
/// `clamp_edge`: true 时边界向外钳位（配合随后 SDF 圆角裁切的实心方卡）；false 时源矩形外按透明零值填充（消除圆形/异形图标边缘突出）。
pub fn resample_premul_bgra(
    src: &[u8],
    sw: i32,
    sh: i32,
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    dw: i32,
    dh: i32,
    clamp_edge: bool,
) -> Vec<u8> {
    if sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 {
        return Vec::new();
    }
    let min_x = min_x.clamp(0, sw - 1);
    let min_y = min_y.clamp(0, sh - 1);
    let max_x = max_x.clamp(min_x, sw - 1);
    let max_y = max_y.clamp(min_y, sh - 1);
    let cw = (max_x - min_x + 1) as usize;
    let ch = (max_y - min_y + 1) as usize;
    let dw_u = dw as usize;
    let dh_u = dh as usize;

    // 尺寸与区域完全一致时直接返回副本，避免不必要的滤波损耗
    if min_x == 0 && min_y == 0 && max_x == sw - 1 && max_y == sh - 1 && dw == sw && dh == sh {
        return src.to_vec();
    }

    let ratio_x = cw as f32 / dw as f32;
    let ratio_y = ch as f32 / dh as f32;
    let scale_x = ratio_x.max(1.0);
    let scale_y = ratio_y.max(1.0);
    let sup_x = 3.0 * scale_x;
    let sup_y = 3.0 * scale_y;
    let inv_kx = 1.0 / scale_x;
    let inv_ky = 1.0 / scale_y;

    // 预计算水平方向每一目标列的采样起点与归一化 Lanczos3 权重表
    let mut x_taps: Vec<(i32, Vec<f32>)> = Vec::with_capacity(dw_u);
    for ox in 0..dw {
        let cx = min_x as f32 + ((ox as f32 + 0.5) / dw as f32) * cw as f32 - 0.5;
        let ix0 = (cx - sup_x).ceil() as i32;
        let ix1 = (cx + sup_x).floor() as i32;
        let mut weights = Vec::with_capacity((ix1 - ix0 + 1).max(0) as usize);
        let mut wsum = 0.0f32;
        for ix in ix0..=ix1 {
            let w = lanczos3((ix as f32 - cx) * inv_kx);
            weights.push(w);
            wsum += w;
        }
        if wsum.abs() > 1e-6 {
            let inv = 1.0 / wsum;
            for w in &mut weights {
                *w *= inv;
            }
        }
        x_taps.push((ix0, weights));
    }

    let sw_u = sw as usize;
    let mut inter = vec![0.0f32; ch * dw_u * 4];
    for (row_i, sy) in (min_y..=max_y).enumerate() {
        let srow = sy as usize * sw_u * 4;
        let drow = row_i * dw_u * 4;
        for (ox, (ix0, weights)) in x_taps.iter().enumerate() {
            let (mut b, mut g, mut r, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for (k, &w) in weights.iter().enumerate() {
                if w == 0.0 {
                    continue;
                }
                let ix = *ix0 + k as i32;
                let sx = if clamp_edge {
                    ix.clamp(min_x, max_x)
                } else if ix < min_x || ix > max_x {
                    continue;
                } else {
                    ix
                };
                let si = srow + sx as usize * 4;
                b += src[si] as f32 * w;
                g += src[si + 1] as f32 * w;
                r += src[si + 2] as f32 * w;
                a += src[si + 3] as f32 * w;
            }
            let di = drow + ox * 4;
            inter[di] = b;
            inter[di + 1] = g;
            inter[di + 2] = r;
            inter[di + 3] = a;
        }
    }

    // 预计算垂直方向每一目标行的采样起点与归一化 Lanczos3 权重表
    let mut y_taps: Vec<(i32, Vec<f32>)> = Vec::with_capacity(dh_u);
    for oy in 0..dh {
        let cy = ((oy as f32 + 0.5) / dh as f32) * ch as f32 - 0.5;
        let iy0 = (cy - sup_y).ceil() as i32;
        let iy1 = (cy + sup_y).floor() as i32;
        let mut weights = Vec::with_capacity((iy1 - iy0 + 1).max(0) as usize);
        let mut wsum = 0.0f32;
        for iy in iy0..=iy1 {
            let w = lanczos3((iy as f32 - cy) * inv_ky);
            weights.push(w);
            wsum += w;
        }
        if wsum.abs() > 1e-6 {
            let inv = 1.0 / wsum;
            for w in &mut weights {
                *w *= inv;
            }
        }
        y_taps.push((iy0, weights));
    }

    let mut out = vec![0u8; dw_u * dh_u * 4];
    for (oy, (iy0, weights)) in y_taps.iter().enumerate() {
        let drow = oy * dw_u * 4;
        for ox in 0..dw_u {
            let (mut b, mut g, mut r, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for (k, &w) in weights.iter().enumerate() {
                if w == 0.0 {
                    continue;
                }
                let iy = *iy0 + k as i32;
                let ry = if clamp_edge {
                    iy.clamp(0, ch as i32 - 1)
                } else if iy < 0 || iy >= ch as i32 {
                    continue;
                } else {
                    iy
                };
                let si = (ry as usize * dw_u + ox) * 4;
                b += inter[si] * w;
                g += inter[si + 1] * w;
                r += inter[si + 2] * w;
                a += inter[si + 3] * w;
            }
            let ai = a.round().clamp(0.0, 255.0) as u8;
            let af = ai as f32;
            let oi = drow + ox * 4;
            out[oi] = b.round().clamp(0.0, af) as u8;
            out[oi + 1] = g.round().clamp(0.0, af) as u8;
            out[oi + 2] = r.round().clamp(0.0, af) as u8;
            out[oi + 3] = ai;
        }
    }
    out
}

/// 检测已缩放到目标正方形区域的图标是否为近似方形卡片（四边中段平直且长宽比接近 1:1），避免误裁圆形或记事本等异形图标。
fn is_card_shape(px: &[u8], w: usize, h: usize) -> Option<(f32, f32, f32, f32)> {
    let at_a = |x: i32, y: i32| -> u8 {
        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
            return 0;
        }
        px[(y as usize * w + x as usize) * 4 + 3]
    };
    let mut min_y = h as i32;
    let mut max_y = -1i32;
    'top: for y in 0..h as i32 {
        let row = y as usize * w * 4;
        for x in 0..w {
            if px[row + x * 4 + 3] > 120 {
                min_y = y;
                break 'top;
            }
        }
    }
    if min_y >= h as i32 {
        return None;
    }
    'bot: for y in (min_y..h as i32).rev() {
        let row = y as usize * w * 4;
        for x in 0..w {
            if px[row + x * 4 + 3] > 120 {
                max_y = y;
                break 'bot;
            }
        }
    }
    let mut min_x = w as i32;
    let mut max_x = -1i32;
    for y in min_y..=max_y {
        let row = y as usize * w * 4;
        for x in 0..min_x {
            if px[row + x as usize * 4 + 3] > 120 {
                min_x = x;
                break;
            }
        }
        for x in (max_x + 1..w as i32).rev() {
            if px[row + x as usize * 4 + 3] > 120 {
                max_x = x;
                break;
            }
        }
    }
    if max_x - min_x < 12 || max_y - min_y < 12 {
        return None;
    }
    let cw = (max_x - min_x + 1) as f32;
    let ch = (max_y - min_y + 1) as f32;
    // 排除记事本、计算器等非方卡比例图标，防止误裁顶部装订环或底部圆角
    if cw.max(ch) > cw.min(ch) * 1.18 {
        return None;
    }
    // 按尺寸等比内缩并检查四边 ±22% 及 ±11% 五点是否平坦不透明，严格区分对称圆角方卡与圆形/异形图标
    let in_x = (cw * 0.02).round().clamp(1.0, 3.0) as i32;
    let in_y = (ch * 0.02).round().clamp(1.0, 3.0) as i32;
    let ox = ((cw * 0.22).round() as i32).max(2);
    let oy = ((ch * 0.22).round() as i32).max(2);
    let ox2 = ((cw * 0.11).round() as i32).max(1);
    let oy2 = ((ch * 0.11).round() as i32).max(1);
    let cx = (min_x + max_x) / 2;
    let cy = (min_y + max_y) / 2;
    let top_flat = [-ox, -ox2, 0, ox2, ox].iter().all(|&d| at_a(cx + d, min_y + in_y) > 180);
    let bot_flat = [-ox, -ox2, 0, ox2, ox].iter().all(|&d| at_a(cx + d, max_y - in_y) > 180);
    let left_flat = [-oy, -oy2, 0, oy2, oy].iter().all(|&d| at_a(min_x + in_x, cy + d) > 180);
    let right_flat = [-oy, -oy2, 0, oy2, oy].iter().all(|&d| at_a(max_x - in_x, cy + d) > 180);
    if top_flat && bot_flat && left_flat && right_flat {
        Some((min_x as f32, min_y as f32, cw, ch))
    } else {
        None
    }
}

/// 按给定圆角弧度对正方形前景层（卡片型图标）就地执行抗锯齿圆角遮罩裁切（仅计算四角区域）。
fn apply_card_roundness(fg: &mut [u8], w: usize, h: usize, round_ratio: f32) {
    let Some((bx, by, cw, ch)) = is_card_shape(fg, w, h) else {
        return;
    };
    let rad = (cw.min(ch) * round_ratio).max(2.5);
    let rcx = bx + cw * 0.5;
    let rcy = by + ch * 0.5;
    let half_w = (cw * 0.5 - rad).max(0.0);
    let half_h = (ch * 0.5 - rad).max(0.0);
    let pad = rad.ceil() as i32 + 2;
    let inner_l = (bx as i32 + pad).max(0);
    let inner_r = ((bx + cw).ceil() as i32 - pad).min(w as i32);
    let inner_t = (by as i32 + pad).max(0);
    let inner_b = ((by + ch).ceil() as i32 - pad).min(h as i32);
    for y in 0..h as i32 {
        let in_mid_y = inner_t < inner_b && y >= inner_t && y < inner_b;
        let fy = y as f32 + 0.5;
        for x in 0..w as i32 {
            // 卡片中心十字区一定在圆角内部，跳过无谓的逐像素 SDF 开方运算
            if in_mid_y && inner_l < inner_r && x >= inner_l && x < inner_r {
                continue;
            }
            if (x as f32 + 0.5 >= bx + 1.0 && (x as f32 + 0.5) < bx + cw - 1.0)
                && (fy >= by + 1.0 && fy < by + ch - 1.0)
                && ((x >= inner_l && x < inner_r) || (y >= inner_t && y < inner_b))
            {
                continue;
            }
            let fx = x as f32 + 0.5;
            let qx = (fx - rcx).abs() - half_w;
            let qy = (fy - rcy).abs() - half_h;
            let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad;
            if sd > -0.5 {
                let cov = (0.5 - sd).clamp(0.0, 1.0);
                let oi = (y as usize * w + x as usize) * 4;
                if cov <= 0.0 {
                    fg[oi..oi + 4].fill(0);
                } else if cov < 1.0 {
                    let c256 = (cov * 256.0).round() as u32;
                    let na = (((fg[oi + 3] as u32 * c256 + 128) >> 8).min(255)) as u8;
                    let nam = na as u32;
                    fg[oi] = (((fg[oi] as u32 * c256 + 128) >> 8).min(nam)) as u8;
                    fg[oi + 1] = (((fg[oi + 1] as u32 * c256 + 128) >> 8).min(nam)) as u8;
                    fg[oi + 2] = (((fg[oi + 2] as u32 * c256 + 128) >> 8).min(nam)) as u8;
                    fg[oi + 3] = na;
                }
            }
        }
    }
}

/// 将源图标一次性通过 Lanczos3 缩放到目标尺寸、按配置圆润度裁切并可选烘焙柔和投影，供主网格高速 1:1 渲染使用。
pub fn bake_icon_sprite(src: &IconPixels, target_size: i32, shadow_offset: i32, round_ratio: f32) -> IconPixels {
    let sz = target_size.max(8) as u32;
    let so = shadow_offset.max(0) as u32;
    let bw = sz;
    let bh = if so > 0 { sz + so + 2 } else { sz };
    let iw = src.w.max(1) as i32;
    let ih = src.h.max(1) as i32;
    let k = (sz as f32 / iw as f32).min(sz as f32 / ih as f32);
    let dw = (iw as f32 * k).round().max(1.0) as i32;
    let dh = (ih as f32 * k).round().max(1.0) as i32;
    let dx0 = (sz as i32 - dw) / 2;
    let dy0 = (sz as i32 - dh) / 2;

    // 1. 使用自适应支持域 Lanczos3 采样源图标到前景正方形图层
    let sampled = resample_premul_bgra(
        src.px.as_slice(),
        iw,
        ih,
        0,
        0,
        iw - 1,
        ih - 1,
        dw,
        dh,
        false,
    );
    let mut fg = if dw == sz as i32 && dh == sz as i32 && dx0 == 0 && dy0 == 0 {
        sampled
    } else {
        let mut canvas = vec![0u8; (sz * sz * 4) as usize];
        for y in 0..dh {
            let oy = dy0 + y;
            if oy < 0 || oy >= sz as i32 {
                continue;
            }
            let srow = y as usize * dw as usize * 4;
            let drow = oy as usize * sz as usize * 4;
            for x in 0..dw {
                let ox = dx0 + x;
                if ox < 0 || ox >= sz as i32 {
                    continue;
                }
                let si = srow + x as usize * 4;
                let di = drow + ox as usize * 4;
                canvas[di..di + 4].copy_from_slice(&sampled[si..si + 4]);
            }
        }
        canvas
    };

    // 2. 卡片型图标按当前选定的圆角圆润度实时抗锯齿裁切四角
    apply_card_roundness(&mut fg, sz as usize, sz as usize, round_ratio);

    // 3. 无投影时直接返回紧凑正方形精灵，避免额外内存与合成开销
    if so == 0 {
        return IconPixels {
            w: bw,
            h: bh,
            px: std::sync::Arc::new(fg),
        };
    }

    let mut buf = vec![0u8; (bw * bh * 4) as usize];
    let so_i = so as i32;
    for y in 0..sz as i32 {
        let dy = y + so_i;
        if dy < 0 || dy >= bh as i32 {
            continue;
        }
        for x in 0..sz as i32 {
            let si = (y as usize * sz as usize + x as usize) * 4;
            let a = fg[si + 3] as u32;
            if a > 16 {
                let di = (dy as usize * bw as usize + x as usize) * 4;
                buf[di + 3] = ((a * 64) >> 8) as u8;
            }
        }
    }

    for y in 0..sz as usize {
        for x in 0..sz as usize {
            let i = (y * sz as usize + x) * 4;
            let a = fg[i + 3] as u32;
            if a == 0 {
                continue;
            }
            let da = buf[i + 3] as u32;
            let ia = 255 - a;
            buf[i] = fg[i];
            buf[i + 1] = fg[i + 1];
            buf[i + 2] = fg[i + 2];
            buf[i + 3] = (a + (da * ia + 127) / 255).min(255) as u8;
        }
    }

    IconPixels {
        w: bw,
        h: bh,
        px: std::sync::Arc::new(buf),
    }
}

/// 在 GDI DC 上绘制单行文字（支持左/中/右对齐、省略号与可选投影）。
pub fn draw_text(
    dc: HDC,
    s: &str,
    font: HFONT,
    r: &RECT,
    c: Rgba,
    align: TextAlign,
    ellipsis: bool,
    shadow: bool,
) {
    if s.is_empty() {
        return;
    }
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let align_flag = match align {
        TextAlign::Left => 0x0,
        TextAlign::Center => 0x1,
        TextAlign::Right => 0x2,
    };
    let fmt = DRAW_TEXT_FORMAT(
        0x20 // DT_SINGLELINE
            | 0x4 // DT_VCENTER
            | 0x800 // DT_NOPREFIX
            | align_flag
            | if ellipsis { 0x8000 } else { 0 }, // DT_END_ELLIPSIS
    );
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        if shadow {
            let mut sr = RECT {
                left: r.left,
                top: r.top + 1,
                right: r.right,
                bottom: r.bottom + 1,
            };
            SetTextColor(dc, COLORREF(0x0000_0000));
            let _ = DrawTextW(dc, &mut w, &mut sr, fmt);
        }
        let mut rc = *r;
        SetTextColor(dc, to_colorref(c));
        let _ = DrawTextW(dc, &mut w, &mut rc, fmt);
        SelectObject(dc, old);
    }
}

/// 在 GDI DC 上测量单行文字像素宽度与高度。
pub fn measure_text(dc: HDC, s: &str, font: HFONT) -> (i32, i32) {
    if s.is_empty() {
        return (0, 0);
    }
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut sz = SIZE::default();
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let _ = GetTextExtentPoint32W(dc, &w, &mut sz);
        SelectObject(dc, old);
    }
    (sz.cx, sz.cy)
}

/// 在 GDI DC 上测量多行自动换行文字的实际高度。
pub fn measure_wrapped_text_height(dc: HDC, s: &str, font: HFONT, max_w: i32) -> i32 {
    if s.is_empty() || max_w <= 0 {
        return 0;
    }
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: max_w.max(1),
        bottom: 0,
    };
    let fmt = DRAW_TEXT_FORMAT(0x10 | 0x400 | 0x800 | 0x2000); // DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX | DT_EDITCONTROL
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let _ = DrawTextW(dc, &mut w, &mut rc, fmt);
        SelectObject(dc, old);
    }
    (rc.bottom - rc.top).max(1)
}

/// 在 GDI DC 上绘制支持自动换行的多行文字（顶部左对齐）。
pub fn draw_wrapped_text(dc: HDC, s: &str, font: HFONT, r: &RECT, c: Rgba) {
    if s.is_empty() || r.right <= r.left || r.bottom <= r.top {
        return;
    }
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let fmt = DRAW_TEXT_FORMAT(0x10 | 0x800 | 0x2000); // DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        let mut rc = *r;
        SetTextColor(dc, to_colorref(c));
        let _ = DrawTextW(dc, &mut w, &mut rc, fmt);
        SelectObject(dc, old);
    }
}

/// 基于 Direct2D / DirectWrite 的彩色文本渲染器。
/// GDI `DrawTextW` 不支持彩色字体表（COLR），会回退为黑白线框字形；
/// 启用 `D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT` 后可在现有 DIB DC 上直接渲染原生彩色 Emoji。
pub struct ColorTextRenderer {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    cached_fmt: Option<(i32, IDWriteTextFormat, IDWriteInlineObject)>,
    cached_wrap_fmt: Option<(i32, IDWriteTextFormat)>,
}

impl ColorTextRenderer {
    /// 创建可绑定 GDI HDC 的 Direct2D 渲染目标与 DirectWrite 文字工厂。
    pub fn new() -> windows::core::Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let (rt, brush) = Self::create_target_and_brush(&d2d)?;
            Ok(Self {
                d2d,
                dwrite,
                rt,
                brush,
                cached_fmt: None,
                cached_wrap_fmt: None,
            })
        }
    }

    unsafe fn create_target_and_brush(
        d2d: &ID2D1Factory,
    ) -> windows::core::Result<(ID2D1DCRenderTarget, ID2D1SolidColorBrush)> {
        // 使用 96 DPI 与 DIB 像素坐标 1:1 映射，alphaMode 设为 IGNORE 以直接合成到不透明窗口表面。
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        let brush = rt.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            None,
        )?;
        Ok((rt, brush))
    }

    /// 缓存指定像素字号的单行居中、末尾省略号格式。
    unsafe fn text_format(&mut self, px: i32) -> windows::core::Result<&IDWriteTextFormat> {
        let px = px.max(9);
        let needs_rebuild = !matches!(self.cached_fmt, Some((cur, _, _)) if cur == px);
        if needs_rebuild {
            let fmt = self.dwrite.CreateTextFormat(
                w!("Microsoft YaHei UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px as f32,
                w!("zh-CN"),
            )?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let sign = self.dwrite.CreateEllipsisTrimmingSign(&fmt)?;
            let trimming = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            };
            fmt.SetTrimming(&trimming, &sign)?;
            self.cached_fmt = Some((px, fmt, sign));
        }
        Ok(&self.cached_fmt.as_ref().expect("format cached").1)
    }

    /// 缓存指定像素字号的自动换行格式（顶部对齐，防止长更新条目被截断）。
    unsafe fn wrapped_format(&mut self, px: i32) -> windows::core::Result<&IDWriteTextFormat> {
        let px = px.max(9);
        let needs_rebuild = !matches!(self.cached_wrap_fmt, Some((cur, _)) if cur == px);
        if needs_rebuild {
            let fmt = self.dwrite.CreateTextFormat(
                w!("Microsoft YaHei UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px as f32,
                w!("zh-CN"),
            )?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
            fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
            self.cached_wrap_fmt = Some((px, fmt));
        }
        Ok(&self.cached_wrap_fmt.as_ref().expect("wrap format cached").1)
    }

    /// 测量多行自动换行文本在给定最大宽度下的像素高度，用于精准计算折叠日志展开高度。
    pub fn measure_wrapped_height(&mut self, px: i32, max_w: i32, text: &str) -> Option<i32> {
        if text.is_empty() || max_w <= 0 {
            return Some(0);
        }
        unsafe {
            let dwrite = self.dwrite.clone();
            let fmt = self.wrapped_format(px).ok()?.clone();
            let utf16: Vec<u16> = text.encode_utf16().collect();
            let layout = dwrite
                .CreateTextLayout(&utf16, &fmt, max_w as f32, 10_000.0)
                .ok()?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics).ok()?;
            Some(metrics.height.ceil() as i32)
        }
    }

    fn draw_batch(&mut self, dc: HDC, clip: &RECT, px: i32, c: Rgba, items: &[(RECT, &str)], wrap: bool) -> bool {
        if items.is_empty() || clip.right <= clip.left || clip.bottom <= clip.top {
            return true;
        }
        unsafe {
            // 先刷新前置 GDI/DIB 绘制，确保 BindDC 读取到最新背景像素。
            let _ = GdiFlush();
            if self.rt.BindDC(dc, clip).is_err() {
                if let Ok((rt, brush)) = Self::create_target_and_brush(&self.d2d) {
                    self.rt = rt;
                    self.brush = brush;
                }
                if self.rt.BindDC(dc, clip).is_err() {
                    return false;
                }
            }
            let dwrite = self.dwrite.clone();
            let fmt = if wrap {
                self.wrapped_format(px).cloned()
            } else {
                self.text_format(px).cloned()
            };
            let Ok(fmt) = fmt else {
                return false;
            };
            self.brush.SetColor(&D2D1_COLOR_F {
                r: c.0,
                g: c.1,
                b: c.2,
                a: c.3,
            });
            let opts = D2D1_DRAW_TEXT_OPTIONS(
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT.0 | D2D1_DRAW_TEXT_OPTIONS_CLIP.0,
            );
            self.rt.BeginDraw();
            for (rc, text) in items {
                if text.is_empty() || rc.right <= rc.left || rc.bottom <= rc.top {
                    continue;
                }
                let utf16: Vec<u16> = text.encode_utf16().collect();
                let max_w = (rc.right - rc.left) as f32;
                let max_h = (rc.bottom - rc.top) as f32;
                if let Ok(layout) = dwrite.CreateTextLayout(&utf16, &fmt, max_w, max_h) {
                    // BindDC 将渲染目标原点映射到 clip 左上角，需把坐标平移到 clip 局部空间。
                    let origin = D2D_POINT_2F {
                        x: (rc.left - clip.left) as f32,
                        y: (rc.top - clip.top) as f32,
                    };
                    self.rt.DrawTextLayout(origin, &layout, &self.brush, opts);
                }
            }
            if self.rt.EndDraw(None, None).is_err() {
                if let Ok((rt, brush)) = Self::create_target_and_brush(&self.d2d) {
                    self.rt = rt;
                    self.brush = brush;
                }
                return false;
            }
        }
        true
    }

    /// 在裁剪区 `clip` 内批量绘制带彩色 Emoji 的单行文本；若渲染目标失效则返回 false 供调用方回退 GDI。
    #[allow(dead_code)]
    pub fn draw_lines(
        &mut self,
        dc: HDC,
        clip: &RECT,
        px: i32,
        c: Rgba,
        items: &[(RECT, &str)],
    ) -> bool {
        self.draw_batch(dc, clip, px, c, items, false)
    }

    /// 在裁剪区 `clip` 内批量绘制支持自动换行与彩色 Emoji 的多行文本。
    pub fn draw_wrapped_lines(
        &mut self,
        dc: HDC,
        clip: &RECT,
        px: i32,
        c: Rgba,
        items: &[(RECT, &str)],
    ) -> bool {
        self.draw_batch(dc, clip, px, c, items, true)
    }
}

/// 现代复选框绘制（支持选中、半选、未选及 Hover/Active 状态）。
pub fn draw_checkbox(
    surface: &mut Surface,
    r: &RECT,
    state: CheckState,
    hover: bool,
    pressed: bool,
    scale: f32,
) {
    let rad = 5.0 * scale;
    let active_blue = if pressed {
        rgb(0x0958D9, 1.0)
    } else if hover {
        rgb(0x4096FF, 1.0)
    } else {
        rgb(0x1677FF, 1.0)
    };
    match state {
        CheckState::Unchecked => {
            let bg = if pressed {
                rgb(0xE5E7EB, 1.0)
            } else if hover {
                rgb(0xF3F4F6, 1.0)
            } else {
                rgb(0xFFFFFF, 1.0)
            };
            let border = if hover || pressed {
                rgb(0x1677FF, 1.0)
            } else {
                rgb(0xCBD5E1, 1.0)
            };
            surface.fill_rrect(r, rad, bg);
            surface.stroke_rrect(r, rad, (1.4 * scale).max(1.0), border);
        }
        CheckState::Checked => {
            surface.fill_rrect(r, rad, active_blue);
            let l = r.left as f32;
            let t = r.top as f32;
            let w = (r.right - r.left) as f32;
            let h = (r.bottom - r.top) as f32;
            let stroke = (1.8 * scale).max(1.2);
            let white = rgb(0xFFFFFF, 1.0);
            surface.thick_line(l + w * 0.24, t + h * 0.52, l + w * 0.43, t + h * 0.71, stroke, white);
            surface.thick_line(l + w * 0.43, t + h * 0.71, l + w * 0.76, t + h * 0.30, stroke, white);
        }
        CheckState::Indeterminate => {
            surface.fill_rrect(r, rad, active_blue);
            let l = r.left as f32;
            let t = r.top as f32;
            let w = (r.right - r.left) as f32;
            let h = (r.bottom - r.top) as f32;
            surface.thick_line(
                l + w * 0.26,
                t + h * 0.50,
                l + w * 0.74,
                t + h * 0.50,
                (1.8 * scale).max(1.2),
                rgb(0xFFFFFF, 1.0),
            );
        }
    }
}

/// 现代胶囊拨动开关（Toggle Switch）。
pub fn draw_toggle(surface: &mut Surface, r: &RECT, on: bool, hover: bool, pressed: bool, scale: f32) {
    let h = (r.bottom - r.top) as f32;
    let rad = h * 0.5;
    let track = if on {
        if pressed {
            rgb(0x0958D9, 1.0)
        } else if hover {
            rgb(0x4096FF, 1.0)
        } else {
            rgb(0x1677FF, 1.0)
        }
    } else if pressed {
        rgb(0x9CA3AF, 1.0)
    } else if hover {
        rgb(0xB8C0CC, 1.0)
    } else {
        rgb(0xCBD5E1, 1.0)
    };
    surface.fill_rrect(r, rad, track);
    let pad = 2.5 * scale;
    let kr = (h * 0.5 - pad).max(2.0);
    let cx = if on {
        r.right as f32 - pad - kr
    } else {
        r.left as f32 + pad + kr
    };
    let cy = (r.top + r.bottom) as f32 * 0.5;
    surface.fill_circle(cx, cy + 0.6 * scale, kr, (0.0, 0.0, 0.0, 0.14));
    surface.fill_circle(cx, cy, kr, rgb(0xFFFFFF, 1.0));
}

/// 现代按钮（支持主色、描边、幽灵态及 Hover/Active 反馈）。
pub fn draw_button(
    surface: &mut Surface,
    dc: HDC,
    r: &RECT,
    label: &str,
    font: HFONT,
    style: BtnStyle,
    hover: bool,
    pressed: bool,
    scale: f32,
) {
    let rad = 8.0 * scale;
    let (bg, border, fg) = match style {
        BtnStyle::Primary => {
            let b = if pressed {
                rgb(0x0958D9, 1.0)
            } else if hover {
                rgb(0x3B82F6, 1.0)
            } else {
                rgb(0x1677FF, 1.0)
            };
            (b, None, rgb(0xFFFFFF, 1.0))
        }
        BtnStyle::Secondary => {
            let b = if pressed {
                rgb(0xE5E7EB, 1.0)
            } else if hover {
                rgb(0xF3F4F6, 1.0)
            } else {
                rgb(0xFFFFFF, 1.0)
            };
            let br = if hover || pressed {
                rgb(0xCBD5E1, 1.0)
            } else {
                rgb(0xE2E8F0, 1.0)
            };
            (b, Some(br), rgb(0x1F2937, 1.0))
        }
        BtnStyle::Ghost => {
            let b = if pressed {
                rgb(0xE2E8F0, 1.0)
            } else if hover {
                rgb(0xF1F5F9, 1.0)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            (b, None, rgb(0x334155, 1.0))
        }
        BtnStyle::DangerGhost => {
            let b = if pressed {
                rgb(0xFEE2E2, 1.0)
            } else if hover {
                rgb(0xFEF2F2, 1.0)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            let fg = if hover || pressed { rgb(0xDC2626, 1.0) } else { rgb(0x64748B, 1.0) };
            (b, None, fg)
        }
    };
    if bg.3 > 0.001 {
        surface.fill_rrect(r, rad, bg);
    }
    if let Some(br) = border {
        surface.stroke_rrect(r, rad, (1.0 * scale).max(1.0), br);
    }
    draw_text(dc, label, font, r, fg, TextAlign::Center, true, false);
}

/// 现代细长圆角滚动条。
pub fn draw_scrollbar(
    surface: &mut Surface,
    track: &RECT,
    total: usize,
    visible: usize,
    offset: usize,
    hover: bool,
    scale: f32,
) {
    if total <= visible || visible == 0 {
        return;
    }
    let th = (track.bottom - track.top) as f32;
    let min_thumb = 24.0 * scale;
    let thumb_h = ((visible as f32 / total as f32) * th).max(min_thumb).min(th);
    let max_off = (total - visible) as f32;
    let frac = (offset as f32 / max_off).clamp(0.0, 1.0);
    let top = track.top as f32 + frac * (th - thumb_h);
    let r = RECT {
        left: track.left,
        top: top.round() as i32,
        right: track.right,
        bottom: (top + thumb_h).round() as i32,
    };
    let w = (r.right - r.left) as f32;
    let c = if hover { rgb(0x94A3B8, 0.9) } else { rgb(0xCBD5E1, 0.8) };
    surface.fill_rrect(&r, w * 0.5, c);
}

#[derive(Clone, Copy)]
struct TooltipLayout {
    rect: RECT,
    shadow_rect: RECT,
    text_rc: RECT,
    radius: f32,
    border_w: f32,
}

impl TooltipLayout {
    fn offset(&self, dx: i32, dy: i32) -> Self {
        let shift = |r: RECT| RECT {
            left: r.left + dx,
            top: r.top + dy,
            right: r.right + dx,
            bottom: r.bottom + dy,
        };
        Self {
            rect: shift(self.rect),
            shadow_rect: shift(self.shadow_rect),
            text_rc: shift(self.text_rc),
            radius: self.radius,
            border_w: self.border_w,
        }
    }
}

/// 计算 Tooltip 的气泡、阴影与文本矩形布局，自动约束在宿主窗口边界内。
fn compute_tooltip_layout(
    dc: HDC,
    font: HFONT,
    anchor: &RECT,
    text: &str,
    bounds_w: i32,
    bounds_h: i32,
    scale: f32,
) -> Option<TooltipLayout> {
    if text.is_empty() || bounds_w <= 0 || bounds_h <= 0 {
        return None;
    }
    let (tw, th) = measure_text(dc, text, font);
    let pad_x = (10.0 * scale).round() as i32;
    let pad_y = (5.0 * scale).round() as i32;
    let max_w = (bounds_w - (24.0 * scale) as i32).max((120.0 * scale) as i32);
    let box_w = (tw + pad_x * 2).min(max_w);
    let box_h = th + pad_y * 2;
    let margin = (8.0 * scale).round() as i32;

    let cx = (anchor.left + anchor.right) / 2;
    let mut left = (cx - box_w / 2).clamp(margin, (bounds_w - box_w - margin).max(margin));
    let gap = (6.0 * scale).round() as i32;
    // 优先放在锚点上方，空间不足时放到下方
    let top = if anchor.top - box_h - gap >= margin {
        anchor.top - box_h - gap
    } else {
        (anchor.bottom + gap).min((bounds_h - box_h - margin).max(margin))
    };
    if left < margin {
        left = margin;
    }
    let rect = RECT {
        left,
        top,
        right: left + box_w,
        bottom: top + box_h,
    };
    let shadow_rect = RECT {
        left: rect.left,
        top: rect.top + (2.0 * scale).round() as i32,
        right: rect.right,
        bottom: rect.bottom + (3.0 * scale).round() as i32,
    };
    let text_rc = RECT {
        left: rect.left + pad_x,
        top: rect.top,
        right: rect.right - pad_x,
        bottom: rect.bottom,
    };
    Some(TooltipLayout {
        rect,
        shadow_rect,
        text_rc,
        radius: 7.0 * scale,
        border_w: (1.0 * scale).max(1.0),
    })
}

/// 绘制自定义圆角深色磨砂 Tooltip（自动约束在窗口边界内，替代原生 title）。
pub fn draw_tooltip(
    surface: &mut Surface,
    dc: HDC,
    font: HFONT,
    anchor: &RECT,
    text: &str,
    bounds_w: i32,
    bounds_h: i32,
    scale: f32,
) {
    let Some(tl) = compute_tooltip_layout(dc, font, anchor, text, bounds_w, bounds_h, scale) else {
        return;
    };
    surface.fill_rrect(&tl.shadow_rect, tl.radius, (0.0, 0.0, 0.0, 0.25));
    surface.fill_rrect(&tl.rect, tl.radius, rgb(0x181C24, 0.94));
    surface.stroke_rrect(&tl.rect, tl.radius, tl.border_w, (1.0, 1.0, 1.0, 0.16));
    draw_text(dc, text, font, &tl.text_rc, rgb(0xF8FAFC, 0.98), TextAlign::Center, true, false);
}

/// 将 Tooltip 按预乘 Alpha 渲染到透明分层表面（用于独立悬浮窗，不被原生子控件遮挡）。
fn render_tooltip_surface(
    surface: &mut Surface,
    text_scratch: &mut DibTarget,
    font: HFONT,
    text: &str,
    tl: &TooltipLayout,
    scale: f32,
) {
    surface.fill_rrect_premul(&tl.shadow_rect, tl.radius, (0.0, 0.0, 0.0, 0.25));
    surface.fill_rrect_premul(&tl.rect, tl.radius, rgb(0x181C24, 0.94));
    surface.stroke_rrect_premul(&tl.rect, tl.radius, tl.border_w, (1.0, 1.0, 1.0, 0.16));
    draw_transparent_text_ex(
        surface,
        text_scratch,
        text,
        font,
        &tl.text_rc,
        rgb(0xF8FAFC, 0.98),
        TextAlign::Center,
        false,
        scale,
    );
}

unsafe extern "system" fn tooltip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 独立透明分层 Tooltip 浮窗：始终悬浮于宿主窗口及其原生子控件（如 EDIT 输入框）之上，
/// 且显隐无需重绘宿主窗口，从而消除控件遮挡与输入框闪烁问题。
#[derive(Default)]
pub struct TooltipPopup {
    hwnd: HWND,
    target: DibTarget,
    text_scratch: DibTarget,
    visible: bool,
}

impl TooltipPopup {
    fn ensure_window(&mut self, owner: HWND, hinst: HINSTANCE) -> bool {
        if !self.hwnd.is_invalid() && unsafe { IsWindow(self.hwnd).as_bool() } {
            return true;
        }
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| unsafe {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(tooltip_wnd_proc),
                hInstance: hinst,
                hbrBackground: HBRUSH::default(),
                lpszClassName: w!("WinLaunchpadTooltip"),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);
        });
        unsafe {
            self.hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT
                    | WS_EX_LAYERED,
                w!("WinLaunchpadTooltip"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                owner,
                None,
                hinst,
                None,
            )
            .unwrap_or_default();
        }
        !self.hwnd.is_invalid()
    }

    /// 根据宿主窗口客户区坐标的 `anchor` 与尺寸约束，定位并显示悬浮 Tooltip。
    pub fn show(
        &mut self,
        owner: HWND,
        hinst: HINSTANCE,
        font: HFONT,
        anchor: &RECT,
        text: &str,
        bounds_w: i32,
        bounds_h: i32,
        scale: f32,
    ) {
        if text.is_empty() || owner.is_invalid() || bounds_w <= 0 || bounds_h <= 0 {
            self.hide();
            return;
        }
        if !self.ensure_window(owner, hinst) {
            return;
        }
        if !self.text_scratch.ensure_at_least(16, 16) {
            return;
        }
        let Some(measure_dc) = self.text_scratch.mem_dc else {
            return;
        };
        let Some(tl) = compute_tooltip_layout(measure_dc, font, anchor, text, bounds_w, bounds_h, scale) else {
            self.hide();
            return;
        };

        // 预留 2px 抗锯齿边界，确保圆角与外阴影平滑不裁边
        let edge_pad = 2;
        let box_l = tl.rect.left.min(tl.shadow_rect.left) - edge_pad;
        let box_t = tl.rect.top.min(tl.shadow_rect.top) - edge_pad;
        let box_r = tl.rect.right.max(tl.shadow_rect.right) + edge_pad;
        let box_b = tl.rect.bottom.max(tl.shadow_rect.bottom) + edge_pad;
        let win_w = (box_r - box_l).max(1);
        let win_h = (box_b - box_t).max(1);
        let local_tl = tl.offset(-box_l, -box_t);

        if !self.target.ensure_at_least(win_w, win_h) {
            return;
        }
        let Some(dc) = self.target.mem_dc else {
            return;
        };
        {
            let Some(mut sf) = self.target.surface() else {
                return;
            };
            sf.clear_rect_transparent(win_w, win_h);
            sf.set_clip(Some(RECT {
                left: 0,
                top: 0,
                right: win_w,
                bottom: win_h,
            }));
            render_tooltip_surface(&mut sf, &mut self.text_scratch, font, text, &local_tl, scale);
        }

        unsafe {
            let mut dst_pt = POINT { x: box_l, y: box_t };
            let _ = ClientToScreen(owner, &mut dst_pt);
            let dst_sz = SIZE { cx: win_w, cy: win_h };
            let src_pt = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&dst_pt),
                Some(&dst_sz),
                dc,
                Some(&src_pt),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            if !self.visible {
                let _ = ShowWindow(self.hwnd, SW_SHOWNA);
                self.visible = true;
            }
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// 隐藏浮窗并释放临时 DIB 缓冲以控制内存占用。
    pub fn hide(&mut self) {
        if self.visible && !self.hwnd.is_invalid() {
            unsafe {
                if IsWindow(self.hwnd).as_bool() {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
            }
            self.visible = false;
        }
        self.target.release();
        self.text_scratch.release();
    }

    /// 销毁浮窗窗口句柄与相关 GDI 资源。
    pub fn destroy(&mut self) {
        self.hide();
        unsafe {
            if !self.hwnd.is_invalid() && IsWindow(self.hwnd).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
        self.hwnd = HWND::default();
    }
}

impl Drop for TooltipPopup {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// 绘制软件自身图标徽标（复刻 assets/app.ico：45° 紫蓝渐变圆角方框 + 3×3 白色网格方块）。
pub fn draw_app_badge(surface: &mut Surface, cx: f32, cy: f32, size: f32, scale: f32) {
    let half = size * 0.5;
    let r = RECT {
        left: (cx - half).round() as i32,
        top: (cy - half).round() as i32,
        right: (cx + half).round() as i32,
        bottom: (cy + half).round() as i32,
    };
    let shadow = RECT {
        left: r.left,
        top: r.top + (3.0 * scale).round() as i32,
        right: r.right,
        bottom: r.bottom + (4.0 * scale).round() as i32,
    };
    let rad = size * (29.0 / 240.0);
    surface.fill_rrect(&shadow, rad, (0.0, 0.0, 0.0, 0.15));

    // 45° 对角线渐变底板（#4F7CFF → #7B3FE4）
    let x0 = r.left.max(0);
    let y0 = r.top.max(0);
    let x1 = r.right.min(surface.w as i32);
    let y1 = r.bottom.min(surface.h as i32);
    let (fl, ft, fr, fb) = (r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
    let span = (size * 2.0).max(1.0);
    let (c1r, c1g, c1b) = (0x4F as f32 / 255.0, 0x7C as f32 / 255.0, 1.0);
    let (c2r, c2g, c2b) = (0x7B as f32 / 255.0, 0x3F as f32 / 255.0, 0xE4 as f32 / 255.0);
    for y in y0..y1 {
        let fy = y as f32 + 0.5;
        for x in x0..x1 {
            let fx = x as f32 + 0.5;
            let d = Surface::sd_rrect(fx, fy, fl, ft, fr, fb, rad);
            if d < 0.5 {
                let cov = (0.5 - d).clamp(0.0, 1.0);
                let t = (((fx - fl) + (fy - ft)) / span).clamp(0.0, 1.0);
                let cr = c1r + (c2r - c1r) * t;
                let cg = c1g + (c2g - c1g) * t;
                let cb = c1b + (c2b - c1b) * t;
                surface.blend(x, y, (cr, cg, cb, cov));
            }
        }
    }
    surface.stroke_rrect(&r, rad, (1.0 * scale).max(1.0), (1.0, 1.0, 1.0, 0.24));

    // 居中 3×3 白色圆角网格（比例与 assets/app.ico 一致）
    let cell = size * (52.0 / 240.0);
    let gap = size * (16.0 / 240.0);
    let cell_rad = (size * (6.0 / 240.0)).max(1.4);
    let total = cell * 3.0 + gap * 2.0;
    let ox = cx - total * 0.5;
    let oy = cy - total * 0.5;
    let white = rgb(0xFFFFFF, 0.98);
    for row in 0..3 {
        for col in 0..3 {
            let gx = ox + col as f32 * (cell + gap);
            let gy = oy + row as f32 * (cell + gap);
            let gr = RECT {
                left: gx.round() as i32,
                top: gy.round() as i32,
                right: (gx + cell).round() as i32,
                bottom: (gy + cell).round() as i32,
            };
            surface.fill_rrect(&gr, cell_rad, white);
        }
    }
}

/// 在透明 DIB 上以最大覆盖度缓冲绘制 Catmull-Rom 平滑亮青手势轨迹（消除转角透明叠加暗斑）。
pub fn draw_gesture_trail(
    surface: &mut Surface,
    pts: &[(f32, f32)],
    stroke_w: f32,
    col: Rgba,
    view_w: i32,
    view_h: i32,
    cov_buf: &mut Vec<f32>,
) {
    if pts.is_empty() || view_w <= 0 || view_h <= 0 {
        return;
    }
    let vw = (view_w as usize).min(surface.w);
    let vh = (view_h as usize).min(surface.h);
    let need = vw * vh;
    if cov_buf.len() < need {
        cov_buf.resize(need, 0.0);
    }
    cov_buf[..need].fill(0.0);

    // 1. 对采样点执行 Catmull-Rom 样条细分，使快速划动曲线顺滑无折角
    let mut smooth = Vec::with_capacity(pts.len() * 4);
    if pts.len() <= 2 {
        smooth.extend_from_slice(pts);
    } else {
        for i in 0..pts.len() - 1 {
            let p0 = if i == 0 { pts[0] } else { pts[i - 1] };
            let p1 = pts[i];
            let p2 = pts[i + 1];
            let p3 = if i + 2 < pts.len() { pts[i + 2] } else { pts[i + 1] };
            let seg_len = ((p2.0 - p1.0).powi(2) + (p2.1 - p1.1).powi(2)).sqrt();
            let steps = ((seg_len / 3.0).ceil() as usize).clamp(1, 8);
            if i == 0 {
                smooth.push(p1);
            }
            for s in 1..=steps {
                let t = s as f32 / steps as f32;
                let t2 = t * t;
                let t3 = t2 * t;
                let x = 0.5
                    * (2.0 * p1.0
                        + (-p0.0 + p2.0) * t
                        + (2.0 * p0.0 - 5.0 * p1.0 + 4.0 * p2.0 - p3.0) * t2
                        + (-p0.0 + 3.0 * p1.0 - 3.0 * p2.0 + p3.0) * t3);
                let y = 0.5
                    * (2.0 * p1.1
                        + (-p0.1 + p2.1) * t
                        + (2.0 * p0.1 - 5.0 * p1.1 + 4.0 * p2.1 - p3.1) * t2
                        + (-p0.1 + 3.0 * p1.1 - 3.0 * p2.1 + p3.1) * t3);
                smooth.push((x, y));
            }
        }
    }

    // 2. 写入最大覆盖度缓冲（保证线段连接处绝不产生重复透明加深）
    let half = stroke_w * 0.5;
    let pad = half + 1.5;
    if smooth.len() == 1 {
        let (cx, cy) = smooth[0];
        let x0 = (cx - pad).floor().max(0.0) as i32;
        let y0 = (cy - pad).floor().max(0.0) as i32;
        let x1 = (cx + pad).ceil().min(vw as f32) as i32;
        let y1 = (cy + pad).ceil().min(vh as f32) as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let cov = (half + 0.6 - d).clamp(0.0, 1.0);
                let idx = y as usize * vw + x as usize;
                if cov > cov_buf[idx] {
                    cov_buf[idx] = cov;
                }
            }
        }
    } else {
        for seg in smooth.windows(2) {
            let (ax, ay) = seg[0];
            let (bx, by) = seg[1];
            let vx = bx - ax;
            let vy = by - ay;
            let len2 = vx * vx + vy * vy;
            let x0 = (ax.min(bx) - pad).floor().max(0.0) as i32;
            let y0 = (ay.min(by) - pad).floor().max(0.0) as i32;
            let x1 = (ax.max(bx) + pad).ceil().min(vw as f32) as i32;
            let y1 = (ay.max(by) + pad).ceil().min(vh as f32) as i32;
            for y in y0..y1 {
                let py = y as f32 + 0.5 - ay;
                let row = y as usize * vw;
                for x in x0..x1 {
                    let px = x as f32 + 0.5 - ax;
                    let h = if len2 > 1e-4 {
                        ((px * vx + py * vy) / len2).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let dx = px - vx * h;
                    let dy = py - vy * h;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d < half + 0.6 {
                        let cov = (half + 0.6 - d).clamp(0.0, 1.0);
                        let idx = row + x as usize;
                        if cov > cov_buf[idx] {
                            cov_buf[idx] = cov;
                        }
                    }
                }
            }
        }
    }

    // 3. 将覆盖度转化为合法预乘 BGRA 写入透明 Surface
    for y in 0..vh {
        let row = y * vw;
        for x in 0..vw {
            let cov = cov_buf[row + x];
            if cov <= 0.002 {
                continue;
            }
            let a = (cov * col.3 * 255.0).round().clamp(0.0, 255.0) as u32;
            if a == 0 {
                continue;
            }
            let r = ((col.0 * a as f32).round() as u32).min(a);
            let g = ((col.1 * a as f32).round() as u32).min(a);
            let b = ((col.2 * a as f32).round() as u32).min(a);
            surface.blend_premul_over(x as i32, y as i32, b, g, r, a);
        }
    }
}

/// 将 GDI 文字按灰度遮罩合成到全透明分层窗口 Surface（带柔和微阴影，兼顾深浅壁纸清晰度）。
pub fn draw_transparent_text(
    surface: &mut Surface,
    scratch: &mut DibTarget,
    text: &str,
    font: HFONT,
    rect: &RECT,
    col: Rgba,
    scale: f32,
) {
    draw_transparent_text_ex(surface, scratch, text, font, rect, col, TextAlign::Left, true, scale);
}

/// 将 GDI 文字按指定对齐方式与可选微阴影合成到透明分层表面。
fn draw_transparent_text_ex(
    surface: &mut Surface,
    scratch: &mut DibTarget,
    text: &str,
    font: HFONT,
    rect: &RECT,
    col: Rgba,
    align: TextAlign,
    shadow: bool,
    scale: f32,
) {
    if text.is_empty() {
        return;
    }
    let tw = (rect.right - rect.left).max(1);
    let th = (rect.bottom - rect.top).max(1);
    if !scratch.ensure_at_least(tw, th) {
        return;
    }
    if let Some(mut ssf) = scratch.surface() {
        ssf.clear_rect_black(tw, th);
    }
    let Some(sdc) = scratch.mem_dc else { return };
    let local_rc = RECT {
        left: 0,
        top: 0,
        right: tw,
        bottom: th,
    };
    draw_text(sdc, text, font, &local_rc, rgb(0xFFFFFF, 1.0), align, true, false);
    unsafe {
        let _ = GdiFlush();
    }
    if scratch.ptr.is_null() {
        return;
    }
    let src_stride = scratch.w as usize;
    let src_slice = unsafe {
        std::slice::from_raw_parts(scratch.ptr as *const u32, src_stride * scratch.h as usize)
    };
    let sdy = (1.0 * scale).round().max(1.0) as i32;
    let cr_f = col.0.clamp(0.0, 1.0);
    let cg_f = col.1.clamp(0.0, 1.0);
    let cb_f = col.2.clamp(0.0, 1.0);
    let ca_f = col.3.clamp(0.0, 1.0);

    // 可选叠加轻微下沉投影，确保在极暗或极亮壁纸上都能锐利识别
    if shadow {
        for ly in 0..th {
            let row = ly as usize * src_stride;
            for lx in 0..tw {
                let mask = (src_slice[row + lx as usize] >> 8) & 0xFF;
                if mask > 12 {
                    let sa = (mask * 72) >> 8;
                    surface.blend_premul_over(rect.left + lx, rect.top + ly + sdy, 0, 0, 0, sa);
                }
            }
        }
    }
    for ly in 0..th {
        let row = ly as usize * src_stride;
        for lx in 0..tw {
            let mask = (src_slice[row + lx as usize] >> 8) & 0xFF;
            if mask == 0 {
                continue;
            }
            let a = ((mask as f32 * ca_f).round() as u32).min(255);
            if a == 0 {
                continue;
            }
            let r = ((cr_f * a as f32).round() as u32).min(a);
            let g = ((cg_f * a as f32).round() as u32).min(a);
            let b = ((cb_f * a as f32).round() as u32).min(a);
            surface.blend_premul_over(rect.left + lx, rect.top + ly, b, g, r, a);
        }
    }
}

/// 绘制参考图 6 风格的蓝色手势矢量轨迹与箭头。
pub fn draw_gesture_glyph(surface: &mut Surface, cx: f32, cy: f32, size: f32, glyph: GestureGlyph, scale: f32) {
    let blue = rgb(0x1677FF, 1.0);
    let sw = (2.6 * scale).max(1.8);
    let r = size * 0.36;
    let head = size * 0.14;
    let arrow = |sf: &mut Surface, tx: f32, ty: f32, ux: f32, uy: f32| {
        // ux, uy 为箭头指向的单位方向向量
        let px = -uy;
        let py = ux;
        sf.thick_line(tx, ty, tx - ux * head + px * head * 0.65, ty - uy * head + py * head * 0.65, sw, blue);
        sf.thick_line(tx, ty, tx - ux * head - px * head * 0.65, ty - uy * head - py * head * 0.65, sw, blue);
    };
    match glyph {
        GestureGlyph::Left => {
            surface.thick_line(cx + r, cy, cx - r, cy, sw, blue);
            arrow(surface, cx - r, cy, -1.0, 0.0);
        }
        GestureGlyph::Right => {
            surface.thick_line(cx - r, cy, cx + r, cy, sw, blue);
            arrow(surface, cx + r, cy, 1.0, 0.0);
        }
        GestureGlyph::Up => {
            surface.thick_line(cx, cy + r, cx, cy - r, sw, blue);
            arrow(surface, cx, cy - r, 0.0, -1.0);
        }
        GestureGlyph::UpDown => {
            let x0 = cx - r * 0.78;
            let y0 = cy + r * 0.72;
            let xm = cx;
            let ym = cy - r * 0.76;
            let x1 = cx + r * 0.78;
            let y1 = cy + r * 0.72;
            surface.thick_line(x0, y0, xm, ym, sw, blue);
            surface.thick_line(xm, ym, x1, y1, sw, blue);
            let dx = x1 - xm;
            let dy = y1 - ym;
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            arrow(surface, x1, y1, dx / len, dy / len);
        }
        GestureGlyph::DownUp => {
            let x0 = cx - r * 0.78;
            let y0 = cy - r * 0.72;
            let xm = cx;
            let ym = cy + r * 0.76;
            let x1 = cx + r * 0.78;
            let y1 = cy - r * 0.72;
            surface.thick_line(x0, y0, xm, ym, sw, blue);
            surface.thick_line(xm, ym, x1, y1, sw, blue);
            let dx = x1 - xm;
            let dy = y1 - ym;
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            arrow(surface, x1, y1, dx / len, dy / len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_trail_produces_valid_premultiplied_alpha() {
        let (w, h) = (160, 160);
        let mut buf = vec![0u32; w * h];
        let mut sf = Surface {
            w,
            h,
            ptr: buf.as_mut_ptr(),
            clip: None,
        };
        let mut cov = Vec::new();
        let pts = [(20.0, 20.0), (50.0, 120.0), (90.0, 130.0), (135.0, 30.0)];
        draw_gesture_trail(&mut sf, &pts, 6.0, rgb(0x00AEFF, 0.96), w as i32, h as i32, &mut cov);

        let mut non_zero = 0usize;
        for &px in &buf {
            let a = (px >> 24) & 0xFF;
            let r = (px >> 16) & 0xFF;
            let g = (px >> 8) & 0xFF;
            let b = px & 0xFF;
            assert!(r <= a && g <= a && b <= a, "premultiplied invariant violated: {px:#010X}");
            if a > 0 {
                non_zero += 1;
            }
        }
        assert!(non_zero > 50);
        // 远离轨迹的角落像素应保持全透明
        assert_eq!(buf[0], 0);
    }

    #[test]
    fn app_badge_renders_white_center_grid_and_gradient() {
        let (w, h) = (128, 128);
        let mut buf = vec![0xFFFF_FFFFu32; w * h];
        let mut sf = Surface {
            w,
            h,
            ptr: buf.as_mut_ptr(),
            clip: None,
        };
        draw_app_badge(&mut sf, 64.0, 64.0, 64.0, 1.0);
        // 正中心属于九宫格第 (1,1) 个白色方块，应呈纯白近似像素
        let center = buf[64 * w + 64];
        assert!(((center >> 16) & 0xFF) > 240);
        assert!(((center >> 8) & 0xFF) > 240);
        assert!((center & 0xFF) > 240);
        // 左上角圆角区域内部应带蓝色渐变分量
        let tl = buf[40 * w + 40];
        assert!((tl & 0xFF) > ((tl >> 16) & 0xFF));
    }

    #[test]
    fn color_text_renderer_draws_colored_emoji_pixels() {
        let mut target = DibTarget::default();
        assert!(target.ensure(360, 48));
        if let Some(mut sf) = target.surface() {
            sf.clear(rgb(0xFFFFFF, 1.0));
        }
        let dc = target.mem_dc.expect("测试 DIB DC 应已创建");
        let mut renderer = ColorTextRenderer::new().expect("Direct2D 彩色文本渲染器应创建成功");
        let clip = RECT { left: 0, top: 0, right: 360, bottom: 48 };
        let text_rc = RECT { left: 8, top: 8, right: 352, bottom: 40 };
        let ok = renderer.draw_lines(
            dc,
            &clip,
            20,
            rgb(0x52647D, 1.0),
            &[(text_rc, "📦 🚀 🎨 ✨ 测试")],
        );
        assert!(ok, "Direct2D 彩色文本渲染应返回成功");

        let slice = unsafe {
            std::slice::from_raw_parts(target.ptr as *const u32, (target.w * target.h) as usize)
        };
        // 前景色 #52647D 满足 R < B；若出现明显的暖色像素（R - B > 30），则证明 Emoji 已按彩色字形输出。
        let warm_colored_pixels = slice
            .iter()
            .filter(|&&px| {
                let r = ((px >> 16) & 0xFF) as i32;
                let b = (px & 0xFF) as i32;
                r - b > 30
            })
            .count();
        assert!(
            warm_colored_pixels > 20,
            "期望渲染出彩色 Emoji 像素，实际暖色像素数: {warm_colored_pixels}"
        );

        let single_h = renderer
            .measure_wrapped_height(13, 400, "📦 短文本")
            .unwrap();
        let multi_h = renderer
            .measure_wrapped_height(
                13,
                120,
                "📦 这是一段足够长的测试文本用于验证自动换行计算高度确实大于单行高度",
            )
            .unwrap();
        assert!(multi_h > single_h, "期望换行高度 {multi_h} > 单行高度 {single_h}");
        assert!(renderer.draw_wrapped_lines(
            dc,
            &clip,
            13,
            rgb(0x52647D, 1.0),
            &[(text_rc, "🔔 自动换行彩色文本测试")],
        ));
        target.release();
    }

    #[test]
    fn bake_icon_sprite_clips_square_cards_only_and_omits_shadow() {
        // 1. 对称方形卡片应被按圆润度裁切四角，且 shadow_offset=0 时尺寸紧凑为 sz×sz
        let sz = 128u32;
        let mut card = vec![0u8; (sz * sz * 4) as usize];
        for y in 12..116 {
            for x in 12..116 {
                let i = ((y * sz + x) * 4) as usize;
                card[i] = 220;
                card[i + 1] = 140;
                card[i + 2] = 60;
                card[i + 3] = 255;
            }
        }
        let card_src = IconPixels { w: sz, h: sz, px: std::sync::Arc::new(card) };
        let baked_card = bake_icon_sprite(&card_src, 112, 0, 0.225);
        assert_eq!((baked_card.w, baked_card.h), (112, 112));
        // 卡片四角（相对于卡片角点 (10,10) 内缩 3px）应已被圆角裁成透明，中心则保持实心
        assert_eq!(baked_card.px[(12 * 112 + 12) * 4 + 3], 0);
        assert_eq!(baked_card.px[(56 * 112 + 56) * 4 + 3], 255);

        // 2. 正圆形图标不得被误判为卡片或产生四向突出
        let mut circle = vec![0u8; (sz * sz * 4) as usize];
        for y in 0..sz {
            for x in 0..sz {
                let d = ((x as f32 - 63.5).powi(2) + (y as f32 - 63.5).powi(2)).sqrt();
                let cov = (54.0 + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let i = ((y * sz + x) * 4) as usize;
                    let a = (cov * 255.0).round() as u8;
                    circle[i] = a;
                    circle[i + 1] = (a as f32 * 0.5).round() as u8;
                    circle[i + 2] = a;
                    circle[i + 3] = a;
                }
            }
        }
        assert!(is_card_shape(&circle, sz as usize, sz as usize).is_none());
        let circle_src = IconPixels { w: sz, h: sz, px: std::sync::Arc::new(circle) };
        let baked_circle = bake_icon_sprite(&circle_src, 112, 0, 0.34);
        assert_eq!((baked_circle.w, baked_circle.h), (112, 112));
        for ch in baked_circle.px.chunks_exact(4) {
            assert!(ch[0] <= ch[3] && ch[1] <= ch[3] && ch[2] <= ch[3]);
        }

        // 3. 三角实心+右上角留空的文件夹形状不得被误判为方卡
        let mut folder = vec![0u8; (sz * sz * 4) as usize];
        for y in 16..112 {
            for x in 12..116 {
                if y < 32 && x > 62 {
                    continue;
                }
                let i = ((y * sz + x) * 4) as usize;
                folder[i + 1] = 200;
                folder[i + 2] = 250;
                folder[i + 3] = 255;
            }
        }
        assert!(is_card_shape(&folder, sz as usize, sz as usize).is_none());
    }

    #[test]
    fn tooltip_surface_produces_valid_premultiplied_alpha() {
        let mut text_scratch = DibTarget::default();
        assert!(text_scratch.ensure(240, 64));
        let dc = text_scratch.mem_dc.expect("测试 DIB DC 应已创建");
        let font = make_font(12, 400);
        let anchor = RECT { left: 420, top: 120, right: 780, bottom: 156 };
        let tl = compute_tooltip_layout(
            dc,
            font,
            &anchor,
            r"Winc · C:\Users\demo\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Winc.lnk",
            820,
            560,
            1.0,
        )
        .expect("应成功计算 Tooltip 布局");
        // 长路径居中时在宿主边界内保持有效宽高
        assert!(tl.rect.left >= 8 && tl.rect.right <= 812);
        assert!(tl.rect.bottom <= anchor.top);

        let edge_pad = 2;
        let box_l = tl.rect.left.min(tl.shadow_rect.left) - edge_pad;
        let box_t = tl.rect.top.min(tl.shadow_rect.top) - edge_pad;
        let box_r = tl.rect.right.max(tl.shadow_rect.right) + edge_pad;
        let box_b = tl.rect.bottom.max(tl.shadow_rect.bottom) + edge_pad;
        let win_w = (box_r - box_l) as usize;
        let win_h = (box_b - box_t) as usize;
        let local_tl = tl.offset(-box_l, -box_t);

        let mut buf = vec![0u32; win_w * win_h];
        let mut sf = Surface {
            w: win_w,
            h: win_h,
            ptr: buf.as_mut_ptr(),
            clip: None,
        };
        render_tooltip_surface(
            &mut sf,
            &mut text_scratch,
            font,
            r"Winc · C:\Users\demo\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Winc.lnk",
            &local_tl,
            1.0,
        );

        let mut bright_text_pixels = 0usize;
        for &px in &buf {
            let a = (px >> 24) & 0xFF;
            let r = (px >> 16) & 0xFF;
            let g = (px >> 8) & 0xFF;
            let b = px & 0xFF;
            assert!(r <= a && g <= a && b <= a, "premultiplied invariant violated: {px:#010X}");
            if a > 220 && r > 180 && g > 180 && b > 180 {
                bright_text_pixels += 1;
            }
        }
        assert!(bright_text_pixels > 30, "应渲染出清晰的浅色文字像素，实际: {bright_text_pixels}");
        // 外边缘角点保持全透明
        assert_eq!(buf[0], 0);
        unsafe {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        text_scratch.release();
    }
}
