//! 共享软件抗锯齿绘图与现代界面组件库（32bpp DIB 像素缓冲）。
//!
//! 统一封装圆角矩形、抗锯齿圆/线、图标双线性缩放与阴影、文字阴影与测量、
//! 现代复选框/开关/按钮/滚动条、矢量手势轨迹、火箭徽标与 100ms Tooltip 浮层。

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
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
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    GdiFlush, GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DRAW_TEXT_FORMAT, HDC, HFONT, HGDIOBJ, TRANSPARENT,
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

    /// 用整数混合预乘 BGRA 像素到指定坐标（0..255）。
    #[inline]
    fn blend_premul_u8(&mut self, x: i32, y: i32, b: u32, g: u32, r: u32, a: u32) {
        if a == 0 || !self.in_bounds(x, y) {
            return;
        }
        let p = unsafe { &mut *self.ptr.add(y as usize * self.w + x as usize) };
        if a >= 255 {
            *p = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            return;
        }
        let ia = 255 - a;
        let dst = *p;
        let dr = (dst >> 16) & 0xFF;
        let dg = (dst >> 8) & 0xFF;
        let db = dst & 0xFF;
        let nr = (r + (dr * ia + 127) / 255).min(255);
        let ng = (g + (dg * ia + 127) / 255).min(255);
        let nb = (b + (db * ia + 127) / 255).min(255);
        *p = 0xFF00_0000 | (nr << 16) | (ng << 8) | nb;
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

    /// 填充抗锯齿圆角矩形（中间核心矩形走高速填充，仅对边缘/四角做 SDF 抗锯齿）。
    pub fn fill_rrect(&mut self, r: &RECT, radius: f32, c: Rgba) {
        if c.3 <= 0.001 || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
        let max_r = ((rr - l).min(b - t) * 0.5).max(0.0);
        let radius = radius.min(max_r);
        if radius <= 0.5 {
            self.fill_rect(r, c);
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
            self.fill_rect(&core, c);
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
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    pub fn stroke_rrect(&mut self, r: &RECT, radius: f32, width: f32, c: Rgba) {
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
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
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

    /// 双线性缩放并绘制预乘 BGRA 图标，可选柔和投影（用于启动台图标质感增强）。
    pub fn draw_icon_bilinear(&mut self, icon: &IconPixels, cx: f32, cy: f32, target_size: f32, with_shadow: bool, shadow_offset: f32) {
        if icon.w == 0 || icon.h == 0 || target_size <= 1.0 {
            return;
        }
        let k = (target_size / icon.w as f32).min(target_size / icon.h as f32);
        let dw = (icon.w as f32 * k).round().max(1.0) as i32;
        let dh = (icon.h as f32 * k).round().max(1.0) as i32;
        let dx0 = (cx - dw as f32 * 0.5).round() as i32;
        let dy0 = (cy - dh as f32 * 0.5).round() as i32;

        let iw = icon.w as i32;
        let ih = icon.h as i32;
        let src = icon.px.as_slice();
        let inv_x = iw as f32 / dw as f32;
        let inv_y = ih as f32 / dh as f32;

        // 先绘制柔和阴影，再叠加图标本体
        if with_shadow && shadow_offset >= 1.0 {
            let so = shadow_offset.round() as i32;
            let (sx0, sy0, sx1, sy1) = self.clamp_bounds(dx0 - 1, dy0 + so - 1, dx0 + dw + 1, dy0 + dh + so + 2);
            for y in sy0..sy1 {
                let fy = ((y - (dy0 + so)) as f32 + 0.5) * inv_y - 0.5;
                let iy = (fy.round() as i32).clamp(0, ih - 1);
                let row_off = (iy as usize * icon.w as usize) * 4;
                for x in sx0..sx1 {
                    let fx = ((x - dx0) as f32 + 0.5) * inv_x - 0.5;
                    let ix = (fx.round() as i32).clamp(0, iw - 1);
                    let a = src[row_off + ix as usize * 4 + 3] as u32;
                    if a > 16 {
                        let sa = (a * 68) >> 8; // ~26% 柔和黑影
                        self.blend_premul_u8(x, y, 0, 0, 0, sa);
                    }
                }
            }
        }

        let (bx0, by0, bx1, by1) = self.clamp_bounds(dx0, dy0, dx0 + dw, dy0 + dh);
        if bx0 >= bx1 || by0 >= by1 {
            return;
        }
        for y in by0..by1 {
            let fy = ((y - dy0) as f32 + 0.5) * inv_y - 0.5;
            let y0 = (fy.floor() as i32).clamp(0, ih - 1);
            let y1 = (y0 + 1).min(ih - 1);
            let ty = (fy - y0 as f32).clamp(0.0, 1.0);
            let wy1 = (ty * 256.0).round() as u32;
            let wy0 = 256 - wy1;
            let row0 = (y0 as usize * icon.w as usize) * 4;
            let row1 = (y1 as usize * icon.w as usize) * 4;
            for x in bx0..bx1 {
                let fx = ((x - dx0) as f32 + 0.5) * inv_x - 0.5;
                let x0 = (fx.floor() as i32).clamp(0, iw - 1);
                let x1 = (x0 + 1).min(iw - 1);
                let tx = (fx - x0 as f32).clamp(0.0, 1.0);
                let wx1 = (tx * 256.0).round() as u32;
                let wx0 = 256 - wx1;

                let i00 = row0 + x0 as usize * 4;
                let i10 = row0 + x1 as usize * 4;
                let i01 = row1 + x0 as usize * 4;
                let i11 = row1 + x1 as usize * 4;

                let w00 = (wx0 * wy0) >> 8;
                let w10 = (wx1 * wy0) >> 8;
                let w01 = (wx0 * wy1) >> 8;
                let w11 = (wx1 * wy1) >> 8;

                let b = (src[i00] as u32 * w00 + src[i10] as u32 * w10 + src[i01] as u32 * w01 + src[i11] as u32 * w11 + 128) >> 8;
                let g = (src[i00 + 1] as u32 * w00 + src[i10 + 1] as u32 * w10 + src[i01 + 1] as u32 * w01 + src[i11 + 1] as u32 * w11 + 128) >> 8;
                let r = (src[i00 + 2] as u32 * w00 + src[i10 + 2] as u32 * w10 + src[i01 + 2] as u32 * w01 + src[i11 + 2] as u32 * w11 + 128) >> 8;
                let a = (src[i00 + 3] as u32 * w00 + src[i10 + 3] as u32 * w10 + src[i01 + 3] as u32 * w01 + src[i11 + 3] as u32 * w11 + 128) >> 8;
                self.blend_premul_u8(x, y, b, g, r, a);
            }
        }
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

/// 检测已缩放到目标正方形区域的图标是否为卡片型（四边中段呈直线分布），避免误裁圆形或异形图标。
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
    // 检测上下左右四条边的中间 24% 区间是否平坦贴边不透明（正圆形在左右偏移 12% 时边缘已向内回落，卡片则始终满透明阈值）
    let ox = ((cw * 0.14).round() as i32).max(2);
    let oy = ((ch * 0.14).round() as i32).max(2);
    let cx = (min_x + max_x) / 2;
    let cy = (min_y + max_y) / 2;
    let top_flat = at_a(cx - ox, min_y + 2) > 180 && at_a(cx, min_y + 2) > 180 && at_a(cx + ox, min_y + 2) > 180;
    let bot_flat = at_a(cx - ox, max_y - 2) > 180 && at_a(cx, max_y - 2) > 180 && at_a(cx + ox, max_y - 2) > 180;
    let left_flat = at_a(min_x + 2, cy - oy) > 180 && at_a(min_x + 2, cy) > 180 && at_a(min_x + 2, cy + oy) > 180;
    let right_flat = at_a(max_x - 2, cy - oy) > 180 && at_a(max_x - 2, cy) > 180 && at_a(max_x - 2, cy + oy) > 180;
    let flat_sides = [top_flat, bot_flat, left_flat, right_flat].iter().filter(|&&b| b).count();
    if flat_sides >= 3 {
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
                    fg[oi] = ((fg[oi] as u32 * c256) >> 8) as u8;
                    fg[oi + 1] = ((fg[oi + 1] as u32 * c256) >> 8) as u8;
                    fg[oi + 2] = ((fg[oi + 2] as u32 * c256) >> 8) as u8;
                    fg[oi + 3] = ((fg[oi + 3] as u32 * c256) >> 8) as u8;
                }
            }
        }
    }
}

/// 生成按指定圆润度实时裁切后的图标副本（用于悬停放大等直接双线性绘制场景）。
pub fn clip_icon_roundness(src: &IconPixels, round_ratio: f32) -> IconPixels {
    if round_ratio <= 0.125 || is_card_shape(&src.px, src.w as usize, src.h as usize).is_none() {
        return src.clone();
    }
    let mut buf = src.px.as_ref().clone();
    apply_card_roundness(&mut buf, src.w as usize, src.h as usize, round_ratio);
    IconPixels {
        w: src.w,
        h: src.h,
        px: std::sync::Arc::new(buf),
    }
}

/// 将源图标一次性缩放到目标尺寸、按配置圆润度裁切并烘焙柔和投影，供主网格高速 1:1 渲染使用。
pub fn bake_icon_sprite(src: &IconPixels, target_size: i32, shadow_offset: i32, round_ratio: f32) -> IconPixels {
    let sz = target_size.max(8) as u32;
    let so = shadow_offset.max(0) as u32;
    let bw = sz;
    let bh = sz + so + 2;
    let mut fg = vec![0u8; (sz * sz * 4) as usize];
    let iw = src.w as i32;
    let ih = src.h as i32;
    let sdata = src.px.as_slice();
    let k = (sz as f32 / src.w.max(1) as f32).min(sz as f32 / src.h.max(1) as f32);
    let dw = (src.w as f32 * k).round().max(1.0) as i32;
    let dh = (src.h as f32 * k).round().max(1.0) as i32;
    let dx0 = (sz as i32 - dw) / 2;
    let dy0 = (sz as i32 - dh) / 2;
    let inv_x = iw as f32 / dw as f32;
    let inv_y = ih as f32 / dh as f32;

    // 1. 双线性采样源图标到前景正方形图层
    for y in dy0.max(0)..(dy0 + dh).min(sz as i32) {
        let fy = ((y - dy0) as f32 + 0.5) * inv_y - 0.5;
        let y0 = (fy.floor() as i32).clamp(0, ih - 1);
        let y1 = (y0 + 1).min(ih - 1);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        let wy1 = (ty * 256.0).round() as u32;
        let wy0 = 256 - wy1;
        let row0 = (y0 as usize * src.w as usize) * 4;
        let row1 = (y1 as usize * src.w as usize) * 4;
        for x in dx0.max(0)..(dx0 + dw).min(sz as i32) {
            let fx = ((x - dx0) as f32 + 0.5) * inv_x - 0.5;
            let x0 = (fx.floor() as i32).clamp(0, iw - 1);
            let x1 = (x0 + 1).min(iw - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let wx1 = (tx * 256.0).round() as u32;
            let wx0 = 256 - wx1;

            let i00 = row0 + x0 as usize * 4;
            let i10 = row0 + x1 as usize * 4;
            let i01 = row1 + x0 as usize * 4;
            let i11 = row1 + x1 as usize * 4;
            let w00 = (wx0 * wy0) >> 8;
            let w10 = (wx1 * wy0) >> 8;
            let w01 = (wx0 * wy1) >> 8;
            let w11 = (wx1 * wy1) >> 8;

            let oi = (y as usize * sz as usize + x as usize) * 4;
            fg[oi] = ((sdata[i00] as u32 * w00 + sdata[i10] as u32 * w10 + sdata[i01] as u32 * w01 + sdata[i11] as u32 * w11 + 128) >> 8).min(255) as u8;
            fg[oi + 1] = ((sdata[i00 + 1] as u32 * w00 + sdata[i10 + 1] as u32 * w10 + sdata[i01 + 1] as u32 * w01 + sdata[i11 + 1] as u32 * w11 + 128) >> 8).min(255) as u8;
            fg[oi + 2] = ((sdata[i00 + 2] as u32 * w00 + sdata[i10 + 2] as u32 * w10 + sdata[i01 + 2] as u32 * w01 + sdata[i11 + 2] as u32 * w11 + 128) >> 8).min(255) as u8;
            fg[oi + 3] = ((sdata[i00 + 3] as u32 * w00 + sdata[i10 + 3] as u32 * w10 + sdata[i01 + 3] as u32 * w01 + sdata[i11 + 3] as u32 * w11 + 128) >> 8).min(255) as u8;
        }
    }

    // 2. 卡片型图标按当前选定的圆角圆润度实时抗锯齿裁切四角
    apply_card_roundness(&mut fg, sz as usize, sz as usize, round_ratio);

    // 3. 基于裁切后的前景 alpha 生成同步轮廓的柔和黑影并合成前景
    let mut buf = vec![0u8; (bw * bh * 4) as usize];
    if so >= 1 {
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

/// 基于 Direct2D / DirectWrite 的单行彩色文本渲染器。
/// GDI `DrawTextW` 不支持彩色字体表（COLR），会回退为黑白线框字形；
/// 启用 `D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT` 后可在现有 DIB DC 上直接渲染原生彩色 Emoji。
pub struct ColorTextRenderer {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    cached_fmt: Option<(i32, IDWriteTextFormat, IDWriteInlineObject)>,
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

    /// 在裁剪区 `clip` 内批量绘制带彩色 Emoji 的单行文本；若渲染目标失效则返回 false 供调用方回退 GDI。
    pub fn draw_lines(
        &mut self,
        dc: HDC,
        clip: &RECT,
        px: i32,
        c: Rgba,
        items: &[(RECT, &str)],
    ) -> bool {
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
            let Ok(fmt) = self.text_format(px).cloned() else {
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
    if text.is_empty() {
        return;
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
    surface.fill_rrect(&shadow_rect, 7.0 * scale, (0.0, 0.0, 0.0, 0.25));
    surface.fill_rrect(&rect, 7.0 * scale, rgb(0x181C24, 0.94));
    surface.stroke_rrect(&rect, 7.0 * scale, (1.0 * scale).max(1.0), (1.0, 1.0, 1.0, 0.16));

    let text_rc = RECT {
        left: rect.left + pad_x,
        top: rect.top,
        right: rect.right - pad_x,
        bottom: rect.bottom,
    };
    draw_text(dc, text, font, &text_rc, rgb(0xF8FAFC, 0.98), TextAlign::Center, true, false);
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
    draw_text(sdc, text, font, &local_rc, rgb(0xFFFFFF, 1.0), TextAlign::Left, true, false);
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

    // 先叠加轻微下沉投影，确保在极暗或极亮壁纸上都能锐利识别
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
        target.release();
    }
}
