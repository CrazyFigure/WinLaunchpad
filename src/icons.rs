//! 图标提取与持久化缓存：支持内容边界自适应缩放、方形图标圆角化与透明边缘归一化，
//! 优先命中本地二进制磁盘缓存，未命中项由短时子进程（`--worker-icons`）批量提取后退出。

use std::collections::{HashSet, VecDeque};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIBSECTION, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::UI::Shell::{
    IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY,
    SIIGBF_RESIZETOFIT,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::win32::{exe_path, fnv1a64, wide};

pub const WM_APP_ICON: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 3;
const ICON_CACHE_MAGIC: &[u8; 4] = b"WLI4";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub type IconKey = (String, u32);

pub struct IconResult {
    pub key: String,
    #[allow(dead_code)]
    pub size: u32,
    pub pixels: Option<Vec<u8>>,
    pub w: u32,
    pub h: u32,
}

struct Queue {
    items: VecDeque<IconKey>,
    pending: HashSet<IconKey>,
}

pub struct IconWorker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    cache_dir: PathBuf,
}

impl IconWorker {
    pub fn start(hwnd: HWND, cache_dir: PathBuf) -> Arc<IconWorker> {
        let _ = std::fs::create_dir_all(&cache_dir);
        let queue = Arc::new((
            Mutex::new(Queue {
                items: VecDeque::new(),
                pending: HashSet::new(),
            }),
            Condvar::new(),
        ));
        let q = queue.clone();
        let dir = cache_dir.clone();
        let hwnd_raw = hwnd.0 as isize;
        std::thread::Builder::new()
            .name("icon-worker".into())
            .spawn(move || icon_thread(q, hwnd_raw, dir))
            .ok();
        Arc::new(IconWorker { queue, cache_dir })
    }

    /// 归一化缓存尺寸档位，兼顾高分屏锐利度与内存控制。
    pub fn bucket_size(size: u32) -> u32 {
        if size <= 64 {
            64
        } else if size <= 96 {
            96
        } else if size <= 128 {
            128
        } else {
            160
        }
    }

    /// 计算磁盘缓存路径（v4 去外圈淡投影与动态圆角基础格式）。
    pub fn cache_path(cache_dir: &Path, key: &str, size: u32) -> PathBuf {
        let bucket = Self::bucket_size(size);
        let sig = format!("v4|{}|{}", key.to_lowercase(), bucket);
        let h = fnv1a64(sig.as_bytes());
        cache_dir.join(format!("v4_{h:016x}_{bucket}.icache"))
    }

    /// 删除指定 `icon_key` 在磁盘缓存中的全部尺寸条目。
    pub fn remove_disk_cache_for_key(cache_dir: &Path, key: &str) {
        if key.is_empty() {
            return;
        }
        for bucket in [64u32, 96, 128, 160] {
            let p = Self::cache_path(cache_dir, key, bucket);
            let _ = std::fs::remove_file(p);
            for prefix in ["v3_", "v2_", ""] {
                let sig = if prefix.is_empty() {
                    format!("{}|{}", key.to_lowercase(), bucket)
                } else {
                    format!("{}|{}|{}", prefix.trim_end_matches('_'), key.to_lowercase(), bucket)
                };
                let h = fnv1a64(sig.as_bytes());
                let _ = std::fs::remove_file(cache_dir.join(format!("{prefix}{h:016x}_{bucket}.icache")));
            }
        }
    }

    /// 同步尝试从磁盘缓存读取已提取图标（主线程快捷读取，无 Shell 调用）。
    pub fn read_disk_cache(&self, key: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
        read_cache_file(&Self::cache_path(&self.cache_dir, key, size))
    }

    /// 请求一个图标；已在队列/提取中的请求会合并。
    pub fn request(&self, key: &str, size: u32) {
        if key.is_empty() {
            return;
        }
        let bucket = Self::bucket_size(size);
        let k = (key.to_string(), bucket);
        let (lock, cv) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if q.pending.insert(k.clone()) {
            q.items.push_back(k);
            cv.notify_one();
        }
    }

    /// 清空尚未开始的请求（翻页/搜索后丢弃过期任务）。
    pub fn clear(&self) {
        let (lock, _) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        q.items.clear();
        q.pending.clear();
    }
}

fn read_cache_file(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 12 || &data[0..4] != ICON_CACHE_MAGIC {
        return None;
    }
    let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let need = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if w == 0 || h == 0 || data.len() != 12 + need {
        return None;
    }
    Some((data[12..].to_vec(), w, h))
}

fn write_cache_file(path: &Path, px: &[u8], w: u32, h: u32) {
    let mut buf = Vec::with_capacity(12 + px.len());
    buf.extend_from_slice(ICON_CACHE_MAGIC);
    buf.extend_from_slice(&w.to_le_bytes());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.extend_from_slice(px);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn post_icon(hwnd: HWND, key: String, size: u32, extracted: Option<(Vec<u8>, u32, u32)>) {
    let result = Box::new(IconResult {
        key,
        size,
        pixels: extracted.as_ref().map(|e| e.0.clone()),
        w: extracted.as_ref().map(|e| e.1).unwrap_or(0),
        h: extracted.as_ref().map(|e| e.2).unwrap_or(0),
    });
    unsafe {
        let _ = PostMessageW(
            hwnd,
            WM_APP_ICON,
            WPARAM(0),
            LPARAM(Box::into_raw(result) as isize),
        );
    }
}

fn icon_thread(queue: Arc<(Mutex<Queue>, Condvar)>, hwnd_raw: isize, cache_dir: PathBuf) {
    let hwnd = HWND(hwnd_raw as *mut _);
    loop {
        let mut batch: Vec<IconKey> = Vec::new();
        {
            let (lock, cv) = &*queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            while q.items.is_empty() {
                q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            if q.items.len() < 16 {
                let (nq, _) = cv
                    .wait_timeout(q, Duration::from_millis(12))
                    .unwrap_or_else(|e| e.into_inner());
                q = nq;
            }
            while batch.len() < 32 {
                if let Some(k) = q.items.pop_front() {
                    batch.push(k);
                } else {
                    break;
                }
            }
        }

        let mut misses: Vec<(String, u32, PathBuf)> = Vec::new();
        for (key, size) in &batch {
            let p = IconWorker::cache_path(&cache_dir, key, *size);
            if let Some(hit) = read_cache_file(&p) {
                post_icon(hwnd, key.clone(), *size, Some(hit));
            } else {
                misses.push((key.clone(), *size, p));
            }
        }

        if !misses.is_empty() {
            run_worker_icons(&misses);
            for (key, size, path) in misses {
                let loaded = read_cache_file(&path);
                post_icon(hwnd, key, size, loaded);
            }
        }

        {
            let (lock, _) = &*queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            for k in batch {
                q.pending.remove(&k);
            }
        }
    }
}

/// 启动短时 `--worker-icons` 子进程批量提取缺失图标（超时 15 秒自动回收）。
fn run_worker_icons(items: &[(String, u32, PathBuf)]) {
    let Some(first_dir) = items.first().and_then(|x| x.2.parent()) else {
        return;
    };
    let req_path = first_dir.join(format!("req_{}.txt", std::process::id()));
    let mut lines = String::new();
    for (key, size, out) in items {
        let clean_key = key.replace(['\r', '\n', '\t'], " ");
        lines.push_str(&format!("{}\t{}\t{}\n", clean_key, size, out.display()));
    }
    if std::fs::write(&req_path, lines.as_bytes()).is_err() {
        return;
    }
    let exe = exe_path();
    if let Ok(mut child) = std::process::Command::new(exe)
        .arg("--worker-icons")
        .arg(&req_path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() > Duration::from_secs(15) {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break,
            }
        }
    }
    let _ = std::fs::remove_file(&req_path);
}

/// 短时工作进程入口：读取请求清单并提取所有图标落盘。
pub fn worker_extract_from_file(req_path: &Path) {
    let Ok(text) = std::fs::read_to_string(req_path) else {
        return;
    };
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let need_uninit = hr.is_ok();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(key) = parts.next() else { continue };
        let Some(sz_str) = parts.next() else { continue };
        let Some(out_str) = parts.next() else { continue };
        let size = sz_str.parse::<u32>().unwrap_or(96);
        let out_path = Path::new(out_str);
        if out_path.exists() {
            continue;
        }
        if let Some((px, w, h)) = std::panic::catch_unwind(|| extract(key, size)).ok().flatten() {
            write_cache_file(out_path, &px, w, h);
        }
    }
    if need_uninit {
        unsafe { CoUninitialize() };
    }
}

/// 返回 (归一化预乘 BGRA 像素, 宽, 高)。仅在工作进程中调用。
pub fn extract(key: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    let target = size.clamp(32, 192);
    let raw = extract_item(key, target).or_else(|| {
        if Path::new(key)
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            if let Some(info) = crate::catalog::read_lnk(Path::new(key)) {
                if !info.target.is_empty() && Path::new(&info.target).exists() {
                    return extract_item(&info.target, target);
                }
            }
        }
        None
    })?;
    Some(normalize_and_premultiply(raw.0, raw.1, raw.2, target))
}

fn extract_item(key: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    let size = size.clamp(16, 256) as i32;
    unsafe {
        let w = wide(key);
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let fac: IShellItemImageFactory = item.cast().ok()?;
        let hbm = fac
            .GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY | SIIGBF_RESIZETOFIT)
            .or_else(|_| fac.GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY))
            .ok()?;
        let mut dib = DIBSECTION::default();
        let got = GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<DIBSECTION>() as i32,
            Some(&mut dib as *mut _ as *mut _),
        );
        if got == 0 {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            return None;
        }
        let w = dib.dsBmih.biWidth.max(1);
        let h = dib.dsBmih.biHeight.abs().max(1);
        let has_alpha = dib.dsBmih.biBitCount == 32;
        let hdc = CreateCompatibleDC(None);
        let mut bi = BITMAPINFO {
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
        let mut px = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(px.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(hdc);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        if lines == 0 {
            return None;
        }
        let mut any_alpha = false;
        for p in px.chunks_exact_mut(4) {
            if p[3] != 0 {
                any_alpha = true;
                break;
            }
        }
        if !has_alpha || !any_alpha {
            for p in px.chunks_exact_mut(4) {
                p[3] = 255;
            }
        }
        Some((px, w as u32, h as u32))
    }
}

/// 图标内容边界归一化：
/// 1. 裁剪多余透明留白，消除部分图标过小的问题；
/// 2. 识别尖角矩形图标及外圈白边，裁切为统一的 macOS 风格圆角；
/// 3. 双线性缩放到目标尺寸并完成 alpha 预乘。
fn normalize_and_premultiply(
    px: Vec<u8>,
    w: u32,
    h: u32,
    target: u32,
) -> (Vec<u8>, u32, u32) {
    let iw = w as i32;
    let ih = h as i32;
    let at = |x: i32, y: i32| -> [u8; 4] {
        let i = ((y * iw + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    };

    // 1. 优先以核心不透明像素（alpha >= 170）定位图形边界，裁掉微信输入法等原图外圈淡透明投影，避免视觉缩水
    let (mut cmin_x, mut cmin_y, mut cmax_x, mut cmax_y) = (iw, ih, -1i32, -1i32);
    let (mut lmin_x, mut lmin_y, mut lmax_x, mut lmax_y) = (iw, ih, -1i32, -1i32);
    for y in 0..ih {
        for x in 0..iw {
            let a = at(x, y)[3];
            if a >= 170 {
                cmin_x = cmin_x.min(x);
                cmin_y = cmin_y.min(y);
                cmax_x = cmax_x.max(x);
                cmax_y = cmax_y.max(y);
            }
            if a > 18 {
                lmin_x = lmin_x.min(x);
                lmin_y = lmin_y.min(y);
                lmax_x = lmax_x.max(x);
                lmax_y = lmax_y.max(y);
            }
        }
    }
    let (mut min_x, mut min_y, mut max_x, mut max_y) = if cmax_x >= cmin_x && cmax_y >= cmin_y {
        // 仅向外保留 1px 抗锯齿缓冲边，防止硬切边缘
        (
            (cmin_x - 1).max(0),
            (cmin_y - 1).max(0),
            (cmax_x + 1).min(iw - 1),
            (cmax_y + 1).min(ih - 1),
        )
    } else if lmax_x >= lmin_x && lmax_y >= lmin_y {
        (lmin_x, lmin_y, lmax_x, lmax_y)
    } else {
        (0, 0, iw - 1, ih - 1)
    };

    // 2. 检测是否带尖角矩形底板（四个内角均不透明），若是则平滑圆角化并微缩至 84% 保持视觉等大
    let cw = (max_x - min_x + 1).max(1);
    let ch = (max_y - min_y + 1).max(1);
    let inset_c = (cw.min(ch) / 24).max(1);
    let sharp_corners = [
        at(min_x + inset_c, min_y + inset_c)[3] > 200,
        at(max_x - inset_c, min_y + inset_c)[3] > 200,
        at(min_x + inset_c, max_y - inset_c)[3] > 200,
        at(max_x - inset_c, max_y - inset_c)[3] > 200,
    ];
    let sharp_count = sharp_corners.iter().filter(|&&b| b).count();
    let is_sharp_rect = sharp_count >= 3;

    // 3. 若是带 1-2px 纯白外框的方图标且内部非白，轻微收边消除多余白边
    if is_sharp_rect && cw > 24 && ch > 24 {
        let is_white_ring = |d: i32| -> bool {
            let samples = [
                at(min_x + d, min_y + ch / 2),
                at(max_x - d, min_y + ch / 2),
                at(min_x + cw / 2, min_y + d),
                at(min_x + cw / 2, max_y - d),
            ];
            samples
                .iter()
                .all(|p| p[3] > 235 && p[0] > 240 && p[1] > 240 && p[2] > 240)
        };
        let inner = at(min_x + cw / 4, min_y + ch / 4);
        let inner_not_white = inner[0] < 230 || inner[1] < 230 || inner[2] < 230;
        if inner_not_white && is_white_ring(0) && is_white_ring(1) && !is_white_ring(4) {
            min_x += 2;
            min_y += 2;
            max_x -= 2;
            max_y -= 2;
        }
    }

    let cw_i = (max_x - min_x + 1).max(1);
    let ch_i = (max_y - min_y + 1).max(1);
    let cw = cw_i as f32;
    let ch = ch_i as f32;
    // 判断是否已经是圆角方形（中心边缘不透明但角落透明）
    let edge_opaque = at(min_x + cw_i / 2, min_y + 2)[3] > 200
        && at(min_x + 2, min_y + ch_i / 2)[3] > 200;

    // 检测是否为“透明背景 + 纯深色剪影/线稿”图标（如 ChatGPT），避免在深色毛玻璃上隐身
    let mut trans_px = 0usize;
    let mut vis_px = 0usize;
    let mut bright_or_color_px = 0usize;
    let mut luma_sum = 0.0f32;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = at(x, y);
            if p[3] <= 18 {
                trans_px += 1;
            } else if p[3] >= 60 {
                vis_px += 1;
                let (b, g, r) = (p[0] as f32, p[1] as f32, p[2] as f32);
                let luma = (0.114 * b + 0.587 * g + 0.299 * r) / 255.0;
                let sat = (b.max(g).max(r) - b.min(g).min(r)) / 255.0;
                if luma > 0.40 || sat > 0.25 {
                    bright_or_color_px += 1;
                }
                luma_sum += luma;
            }
        }
    }
    let total_box = (cw_i * ch_i).max(1) as f32;
    let trans_ratio = trans_px as f32 / total_box;
    let avg_luma = if vis_px > 0 { luma_sum / vis_px as f32 } else { 1.0 };
    let bright_ratio = bright_or_color_px as f32 / vis_px.max(1) as f32;
    let needs_light_card =
        !is_sharp_rect && trans_ratio > 0.28 && avg_luma < 0.28 && bright_ratio < 0.14;

    let mut out = vec![0u8; (target * target * 4) as usize];
    if needs_light_card {
        // 为纯暗色透明图标自动铺设 macOS 风格浅银白圆角卡片，居中嵌放原始图标
        let card_sz = (target as f32 * 0.85).round().max(16.0);
        let card_x0 = (target as f32 - card_sz) * 0.5;
        let card_y0 = (target as f32 - card_sz) * 0.5;
        let card_rcx = card_x0 + card_sz * 0.5;
        let card_rcy = card_y0 + card_sz * 0.5;
        // 缓存落盘统一采用基础微圆角（0.12），主面板渲染时可实时叠加三档圆角弧度
        let card_rad = (card_sz * 0.12).max(3.0);
        let card_hw = card_sz * 0.5 - card_rad;
        let card_hh = card_sz * 0.5 - card_rad;

        let inner_fit = card_sz * 0.62;
        let iscale = (inner_fit / cw).min(inner_fit / ch);
        let dw = (cw * iscale).round().max(1.0);
        let dh = (ch * iscale).round().max(1.0);
        let dx0 = (target as f32 - dw) * 0.5;
        let dy0 = (target as f32 - dh) * 0.5;

        for y in 0..target as i32 {
            let fy = y as f32 + 0.5;
            let grad_t = ((fy - card_y0) / card_sz).clamp(0.0, 1.0);
            // 顶部 #FAFAFD -> 底部 #E6EBF3
            let base_r = 250.0 - 20.0 * grad_t;
            let base_g = 250.0 - 15.0 * grad_t;
            let base_b = 253.0 - 10.0 * grad_t;

            for x in 0..target as i32 {
                let fx = x as f32 + 0.5;
                let qx = (fx - card_rcx).abs() - card_hw;
                let qy = (fy - card_rcy).abs() - card_hh;
                let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - card_rad;
                let card_cov = (0.5 - sd).clamp(0.0, 1.0);
                if card_cov <= 0.0 {
                    continue;
                }

                // 卡片边缘微弱高光，增加层次感
                let rim = if sd > -1.3 && sd < 0.2 { 1.0 - ((sd + 0.55).abs() / 0.85).clamp(0.0, 1.0) } else { 0.0 };
                let mut r = base_r + (255.0 - base_r) * rim * 0.45;
                let mut g = base_g + (255.0 - base_g) * rim * 0.45;
                let mut b = base_b + (255.0 - base_b) * rim * 0.45;

                if fx >= dx0 - 0.5 && fx <= dx0 + dw + 0.5 && fy >= dy0 - 0.5 && fy <= dy0 + dh + 0.5 {
                    let sy = min_y as f32 + ((fy - dy0) / dh) * ch - 0.5;
                    let sx = min_x as f32 + ((fx - dx0) / dw) * cw - 0.5;
                    let y0 = (sy.floor() as i32).clamp(min_y, max_y);
                    let y1 = (y0 + 1).min(max_y);
                    let ty = (sy - y0 as f32).clamp(0.0, 1.0);
                    let x0 = (sx.floor() as i32).clamp(min_x, max_x);
                    let x1 = (x0 + 1).min(max_x);
                    let tx = (sx - x0 as f32).clamp(0.0, 1.0);
                    let p00 = at(x0, y0);
                    let p10 = at(x1, y0);
                    let p01 = at(x0, y1);
                    let p11 = at(x1, y1);
                    let w00 = (1.0 - tx) * (1.0 - ty);
                    let w10 = tx * (1.0 - ty);
                    let w01 = (1.0 - tx) * ty;
                    let w11 = tx * ty;
                    let fg_b = p00[0] as f32 * w00 + p10[0] as f32 * w10 + p01[0] as f32 * w01 + p11[0] as f32 * w11;
                    let fg_g = p00[1] as f32 * w00 + p10[1] as f32 * w10 + p01[1] as f32 * w01 + p11[1] as f32 * w11;
                    let fg_r = p00[2] as f32 * w00 + p10[2] as f32 * w10 + p01[2] as f32 * w01 + p11[2] as f32 * w11;
                    let fg_a = ((p00[3] as f32 * w00 + p10[3] as f32 * w10 + p01[3] as f32 * w01 + p11[3] as f32 * w11) / 255.0).clamp(0.0, 1.0);
                    r = fg_r * fg_a + r * (1.0 - fg_a);
                    g = fg_g * fg_a + g * (1.0 - fg_a);
                    b = fg_b * fg_a + b * (1.0 - fg_a);
                }

                let ai = ((card_cov * 255.0).round() as u32).min(255);
                let oi = ((y as u32 * target + x as u32) * 4) as usize;
                out[oi] = ((b.round() as u32 * ai + 127) / 255).min(255) as u8;
                out[oi + 1] = ((g.round() as u32 * ai + 127) / 255).min(255) as u8;
                out[oi + 2] = ((r.round() as u32 * ai + 127) / 255).min(255) as u8;
                out[oi + 3] = ai as u8;
            }
        }
        return (out, target, target);
    }

    let fill_ratio = if is_sharp_rect || edge_opaque { 0.85 } else { 0.90 };
    let fit = target as f32 * fill_ratio;
    let scale = (fit / cw).min(fit / ch);
    let dw = (cw * scale).round().max(1.0);
    let dh = (ch * scale).round().max(1.0);
    let dx0 = (target as f32 - dw) * 0.5;
    let dy0 = (target as f32 - dh) * 0.5;

    let rad = (dw.min(dh) * 0.12).max(3.0);
    let half_w = dw * 0.5 - rad;
    let half_h = dh * 0.5 - rad;
    let rcx = dx0 + dw * 0.5;
    let rcy = dy0 + dh * 0.5;

    for y in 0..target as i32 {
        let fy_out = y as f32 + 0.5;
        if fy_out < dy0 - 0.5 || fy_out > dy0 + dh + 0.5 {
            continue;
        }
        let sy = min_y as f32 + ((fy_out - dy0) / dh) * ch - 0.5;
        let y0 = (sy.floor() as i32).clamp(min_y, max_y);
        let y1 = (y0 + 1).min(max_y);
        let ty = (sy - y0 as f32).clamp(0.0, 1.0);

        for x in 0..target as i32 {
            let fx_out = x as f32 + 0.5;
            if fx_out < dx0 - 0.5 || fx_out > dx0 + dw + 0.5 {
                continue;
            }
            let sx = min_x as f32 + ((fx_out - dx0) / dw) * cw - 0.5;
            let x0 = (sx.floor() as i32).clamp(min_x, max_x);
            let x1 = (x0 + 1).min(max_x);
            let tx = (sx - x0 as f32).clamp(0.0, 1.0);

            let p00 = at(x0, y0);
            let p10 = at(x1, y0);
            let p01 = at(x0, y1);
            let p11 = at(x1, y1);
            let w00 = (1.0 - tx) * (1.0 - ty);
            let w10 = tx * (1.0 - ty);
            let w01 = (1.0 - tx) * ty;
            let w11 = tx * ty;

            let b = p00[0] as f32 * w00 + p10[0] as f32 * w10 + p01[0] as f32 * w01 + p11[0] as f32 * w11;
            let g = p00[1] as f32 * w00 + p10[1] as f32 * w10 + p01[1] as f32 * w01 + p11[1] as f32 * w11;
            let r = p00[2] as f32 * w00 + p10[2] as f32 * w10 + p01[2] as f32 * w01 + p11[2] as f32 * w11;
            let mut a = p00[3] as f32 * w00 + p10[3] as f32 * w10 + p01[3] as f32 * w01 + p11[3] as f32 * w11;

            // 若原始图像边缘不透明，使用抗锯齿圆角遮罩消除直角或白框边角
            if is_sharp_rect {
                let qx = (fx_out - rcx).abs() - half_w;
                let qy = (fy_out - rcy).abs() - half_h;
                let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad;
                let cov = (0.5 - sd).clamp(0.0, 1.0);
                a *= cov;
            }

            let ai = (a.round() as u32).min(255);
            let oi = ((y as u32 * target + x as u32) * 4) as usize;
            out[oi] = ((b.round() as u32 * ai + 127) / 255).min(255) as u8;
            out[oi + 1] = ((g.round() as u32 * ai + 127) / 255).min(255) as u8;
            out[oi + 2] = ((r.round() as u32 * ai + 127) / 255).min(255) as u8;
            out[oi + 3] = ai as u8;
        }
    }
    (out, target, target)
}

#[cfg(test)]
mod tests {
    use super::normalize_and_premultiply;

    #[test]
    fn dark_transparent_silhouette_gets_light_squircle_backdrop() {
        // 构造一个类似 ChatGPT 的黑色镂空圆环（周围及内部中心透明）
        let sz = 64u32;
        let mut px = vec![0u8; (sz * sz * 4) as usize];
        for y in 0..sz {
            for x in 0..sz {
                let dx = x as f32 - 31.5;
                let dy = y as f32 - 31.5;
                let r = (dx * dx + dy * dy).sqrt();
                if (14.0..=24.0).contains(&r) {
                    let i = ((y * sz + x) * 4) as usize;
                    px[i] = 10;
                    px[i + 1] = 10;
                    px[i + 2] = 10;
                    px[i + 3] = 255;
                }
            }
        }
        let (out, ow, oh) = normalize_and_premultiply(px, sz, sz, 64);
        assert_eq!((ow, oh), (64, 64));
        // 中心原本透明，现在应该衬有明亮浅白背景；四个最外角仍应保持透明圆角裁切
        let center = ((32 * 64 + 32) * 4) as usize;
        assert!(out[center + 3] > 240 && out[center + 2] > 220);
        let corner = 0usize;
        assert_eq!(out[corner + 3], 0);
    }

    #[test]
    fn colorful_transparent_icon_keeps_transparent_background() {
        let sz = 64u32;
        let mut px = vec![0u8; (sz * sz * 4) as usize];
        for y in 0..sz {
            for x in 0..sz {
                let dx = x as f32 - 31.5;
                let dy = y as f32 - 31.5;
                let r = (dx * dx + dy * dy).sqrt();
                if (14.0..=24.0).contains(&r) {
                    let i = ((y * sz + x) * 4) as usize;
                    px[i] = 240; // 鲜蓝 BGRA
                    px[i + 1] = 120;
                    px[i + 2] = 30;
                    px[i + 3] = 255;
                }
            }
        }
        let (out, _, _) = normalize_and_premultiply(px, sz, sz, 64);
        let center = ((32 * 64 + 32) * 4) as usize;
        assert_eq!(out[center + 3], 0);
    }

    #[test]
    fn faint_outer_shadow_is_trimmed_to_full_card_size() {
        let sz = 128u32;
        let mut px = vec![0u8; (sz * sz * 4) as usize];
        for y in 6..=121 {
            for x in 6..=121 {
                let i = ((y * sz + x) * 4) as usize;
                if (22..=105).contains(&x) && (22..=105).contains(&y) {
                    px[i + 1] = 200; // 实心绿色卡片本体
                    px[i + 3] = 255;
                } else {
                    px[i + 3] = 45; // 模拟外圈半透明淡阴影
                }
            }
        }
        let (out, _, _) = normalize_and_premultiply(px, sz, sz, 100);
        // 卡片本体应该被放大填充至接近目标边长 85%（中心 10..89 内应全为绿色实体）
        let edge_pt = ((50 * 100 + 12) * 4) as usize;
        assert!(out[edge_pt + 3] > 230 && out[edge_pt + 1] > 180);
    }
}
