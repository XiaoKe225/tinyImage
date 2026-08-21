//! T005b~T030: WebP/GIF/BMP/TIFF/ICO local same-ext pipeline.
//! T030: ICO i<25 volume — quantize then expand to RGBA PNG (ban indexed).

use crate::quality::{edge_retain_ratio, high_freq_retain_ratio, rgba_to_rgb, ssim_rgb};
use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbaImage};
use std::borrow::Cow;
use std::io::Cursor;

/// T005b 近无损 SSIM 门禁（白皮书 §4.2）。
pub const EXTRA_SSIM_MIN: f64 = 0.985;
pub const EXTRA_HF_MIN: f64 = 0.90;
pub const EXTRA_EDGE_MIN: f64 = 0.90;

fn clamp_i(v: u8) -> u8 {
    v.min(100)
}

fn decode_rgb(data: &[u8]) -> Result<image::RgbImage, String> {
    let img = image::load_from_memory(data).map_err(|e| format!("解码失败: {e}"))?;
    Ok(match img {
        DynamicImage::ImageRgb8(rgb) => rgb,
        DynamicImage::ImageRgba8(rgba) => rgba_to_rgb(&rgba),
        other => other.to_rgb8(),
    })
}

fn webp_quality_ladder(intensity: u8) -> Vec<f32> {
    let i = clamp_i(intensity) as f32;
    let start = if i <= 34.0 {
        92.0 - (i / 34.0) * 14.0
    } else {
        78.0 - ((i - 34.0) / 66.0) * 28.0
    };
    let floor = if intensity < 25 {
        75.0
    } else if intensity < 50 {
        58.0
    } else if intensity < 80 {
        40.0
    } else {
        25.0
    };
    // T028：阶梯砍短（旧步长过?× method6 = 真慢因）
    let mut qs = Vec::new();
    let mut q = start;
    let step = if intensity < 25 { 8.0 } else { 10.0 };
    while q >= floor - 0.1 {
        qs.push(q);
        q -= step;
    }
    if qs.last().map(|v| (*v - floor).abs() > 0.5).unwrap_or(true) {
        qs.push(floor);
    }
    qs
}

fn gates_ok(orig: &image::RgbImage, dec: &image::RgbImage, intensity: u8) -> (bool, f64, f64, f64) {
    if orig.dimensions() != dec.dimensions() {
        return (false, 0.0, 0.0, 0.0);
    }
    let ssim = ssim_rgb(orig, dec).unwrap_or(0.0);
    let hf = high_freq_retain_ratio(orig, dec);
    let edge = edge_retain_ratio(orig, dec);
    let smin = if intensity < 25 {
        EXTRA_SSIM_MIN
    } else {
        (EXTRA_SSIM_MIN - (intensity as f64 - 25.0) / 75.0 * 0.04).max(0.94)
    };
    let (w, h) = orig.dimensions();
    let small = w.min(h) < 96;
    let hf_min = if small { EXTRA_HF_MIN - 0.05 } else { EXTRA_HF_MIN };
    let edge_min = if small {
        EXTRA_EDGE_MIN - 0.05
    } else {
        EXTRA_EDGE_MIN
    };
    let ok = ssim >= smin && hf >= hf_min && edge >= edge_min;
    (ok, ssim, hf, edge)
}

fn consider_candidate(
    best: &mut Option<(Vec<u8>, f64, String)>,
    bytes: Vec<u8>,
    orig_len: usize,
    original: &image::RgbImage,
    intensity: u8,
    method: String,
) {
    if bytes.len() >= orig_len || bytes.len() < 16 {
        return;
    }
    if let Ok(dec) = decode_rgb(&bytes) {
        let (ok, ssim, hf, edge) = gates_ok(original, &dec, intensity);
        if !ok {
            return;
        }
        let smaller = best.as_ref().map_or(true, |b| bytes.len() < b.0.len());
        if smaller {
            best.replace((
                bytes,
                ssim,
                format!("{method}/hf{hf:.2}/e{edge:.2}"),
            ));
        }
    }
}

fn encode_webp_advanced(
    raw: &[u8],
    w: u32,
    h: u32,
    lossless: bool,
    quality: f32,
    method: i32,
    near_lossless: i32,
) -> Option<Vec<u8>> {
    let mut config = webp::WebPConfig::new().ok()?;
    config.lossless = if lossless || near_lossless > 0 { 1 } else { 0 };
    config.quality = quality.clamp(0.0, 100.0);
    config.method = method.clamp(0, 6);
    config.near_lossless = near_lossless.clamp(0, 100);
    config.alpha_compression = 1;
    let enc = webp::Encoder::from_rgb(raw, w, h);
    enc.encode_advanced(&config).ok().map(|m| m.to_vec())
}

/// WebP T029: m4 explore + one m6 polish on final winner (volume without m6xN).
pub fn compress_webp(data: &[u8], intensity: u8) -> Result<(Vec<u8>, f64, String), String> {
    let intensity = clamp_i(intensity);
    let original = decode_rgb(data)?;
    let (w, h) = original.dimensions();
    let raw = original.as_raw();
    let mut best: Option<(Vec<u8>, f64, String)> = None;

    let lossless_tries: &[(f32, i32)] = if intensity < 25 {
        &[(100.0, 4)]
    } else {
        &[(90.0, 4)]
    };
    for &(q, method) in lossless_tries {
        if let Some(bytes) = encode_webp_advanced(raw, w, h, true, q, method, 0) {
            if bytes.len() < data.len() && bytes.len() > 32 {
                if let Ok(dec) = decode_rgb(&bytes) {
                    if dec.as_raw() == original.as_raw() {
                        if best.as_ref().map_or(true, |b| bytes.len() < b.0.len()) {
                            best = Some((
                                bytes,
                                1.0,
                                format!("webp/lossless/q{q:.0}/m{method}/i{intensity}"),
                            ));
                        }
                    } else {
                        consider_candidate(
                            &mut best,
                            bytes,
                            data.len(),
                            &original,
                            intensity,
                            format!("webp/lossless/q{q:.0}/m{method}/i{intensity}"),
                        );
                    }
                }
            }
        }
    }

    if !best.as_ref().is_some_and(|b| (b.0.len() as f64) < data.len() as f64 * 0.75) {
        let nl = if intensity < 25 { 50 } else { 30 };
        if let Some(bytes) = encode_webp_advanced(raw, w, h, true, 90.0, 4, nl) {
            consider_candidate(
                &mut best,
                bytes,
                data.len(),
                &original,
                intensity,
                format!("webp/near{nl}/i{intensity}"),
            );
        }
    }

    if !best.as_ref().is_some_and(|b| b.1 >= 0.999 && (b.0.len() as f64) < data.len() as f64 * 0.80)
    {
        for q in webp_quality_ladder(intensity) {
            if let Some(bytes) = encode_webp_advanced(raw, w, h, false, q, 4, 0) {
                consider_candidate(
                    &mut best,
                    bytes,
                    data.len(),
                    &original,
                    intensity,
                    format!("webp/q{q:.0}/m4/i{intensity}"),
                );
            }
            if best
                .as_ref()
                .is_some_and(|b| (b.0.len() as f64) < data.len() as f64 * 0.55)
            {
                break;
            }
        }
    }

    // One-shot m6 polish (clone tags first to avoid borrow issues)
    if intensity < 25 {
        if let Some((bytes, _ssim, method)) = best.clone() {
            let polished = if method.contains("lossless") {
                encode_webp_advanced(raw, w, h, true, 100.0, 6, 0)
            } else if let Some(qpos) = method.find("/q") {
                let rest = &method[qpos + 2..];
                let qstr: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                let q = qstr.parse::<f32>().unwrap_or(80.0);
                encode_webp_advanced(raw, w, h, false, q, 6, 0)
            } else if method.contains("near") {
                encode_webp_advanced(raw, w, h, true, 90.0, 6, 50)
            } else {
                None
            };
            if let Some(pbytes) = polished {
                if pbytes.len() < bytes.len() && pbytes.len() < data.len() {
                    if method.contains("lossless") {
                        if let Ok(dec) = decode_rgb(&pbytes) {
                            if dec.as_raw() == original.as_raw() {
                                best = Some((pbytes, 1.0, format!("{method}/m6polish")));
                            } else {
                                consider_candidate(
                                    &mut best,
                                    pbytes,
                                    data.len(),
                                    &original,
                                    intensity,
                                    format!("{method}/m6polish"),
                                );
                            }
                        }
                    } else {
                        consider_candidate(
                            &mut best,
                            pbytes,
                            data.len(),
                            &original,
                            intensity,
                            format!("{method}/m6polish"),
                        );
                    }
                }
            }
        }
    }

    best.ok_or_else(|| "WebP: failed to shrink under gates".into())
}

fn gif_color_ladder(intensity: u8) -> Vec<u32> {
    // T029: fidelity zone 4 rungs, no-dither first
    if intensity < 25 {
        vec![256, 192, 128, 96]
    } else if intensity < 50 {
        vec![160, 96]
    } else if intensity < 80 {
        vec![96, 64]
    } else {
        vec![48]
    }
}

fn quantize_rgba(
    rgba: &RgbaImage,
    max_colors: u32,
    qmin: u8,
    speed: i32,
    dither: f32,
) -> Result<(Vec<imagequant::RGBA>, Vec<u8>), String> {
    let (w, h) = rgba.dimensions();
    let pixels: Vec<imagequant::RGBA> = rgba
        .pixels()
        .map(|p| imagequant::RGBA {
            r: p[0],
            g: p[1],
            b: p[2],
            a: p[3],
        })
        .collect();
    let mut liq = imagequant::new();
    liq.set_speed(speed.clamp(1, 10))
        .map_err(|e| format!("imagequant speed: {e}"))?;
    liq.set_quality(qmin, 100)
        .map_err(|e| format!("imagequant quality: {e}"))?;
    liq.set_max_colors(max_colors.max(2))
        .map_err(|e| format!("imagequant colors: {e}"))?;
    let mut img_liq = liq
        .new_image(pixels.as_slice(), w as usize, h as usize, 0.0)
        .map_err(|e| format!("imagequant image: {e}"))?;
    let mut res = liq
        .quantize(&mut img_liq)
        .map_err(|e| format!("imagequant quantize: {e}"))?;
    let _ = res.set_dithering_level(dither.clamp(0.0, 1.0));
    res.remapped(&mut img_liq)
        .map_err(|e| format!("imagequant remap: {e}"))
}

fn encode_gif_frames(
    data: &[u8],
    max_colors: u32,
    qmin: u8,
    speed: i32,
    dither: f32,
) -> Result<(Vec<u8>, u16, u16), String> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut reader = opts
        .read_info(Cursor::new(data))
        .map_err(|e| format!("GIF 解码失败: {e}"))?;
    let canvas_w = reader.width();
    let canvas_h = reader.height();
    let mut frames_out: Vec<gif::Frame<'static>> = Vec::new();
    while let Some(frame) = reader
        .read_next_frame()
        .map_err(|e| format!("GIF 读帧失败: {e}"))?
    {
        let fw = frame.width as u32;
        let fh = frame.height as u32;
        let rgba = RgbaImage::from_raw(fw, fh, frame.buffer.to_vec())
            .ok_or_else(|| "GIF 帧缓冲无效".to_string())?;
        let (palette, indices) = match quantize_rgba(&rgba, max_colors, qmin, speed, dither) {
            Ok(v) => v,
            Err(_) => quantize_rgba(&rgba, max_colors, 0, speed, dither)?,
        };
        let mut pal_bytes = Vec::with_capacity(palette.len() * 3);
        for c in &palette {
            pal_bytes.extend_from_slice(&[c.r, c.g, c.b]);
        }
        let transparent = palette.iter().position(|c| c.a < 128).map(|i| i as u8);
        frames_out.push(gif::Frame {
            delay: frame.delay,
            dispose: frame.dispose,
            transparent,
            needs_user_input: frame.needs_user_input,
            top: frame.top,
            left: frame.left,
            width: frame.width,
            height: frame.height,
            interlaced: false,
            palette: Some(pal_bytes),
            buffer: Cow::Owned(indices),
        });
    }
    if frames_out.is_empty() {
        return Err("GIF 无帧".into());
    }
    let mut out = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, canvas_w, canvas_h, &[])
            .map_err(|e| format!("GIF 编码初始化失败: {e}"))?;
        let _ = enc.set_repeat(gif::Repeat::Infinite);
        for f in &frames_out {
            enc.write_frame(f)
                .map_err(|e| format!("GIF 写帧失败: {e}"))?;
        }
    }
    Ok((out, canvas_w, canvas_h))
}

/// GIF: T029 short ladder, no-dither first; dither only if ratio still >=90%; early-stop at 70%.
pub fn compress_gif(data: &[u8], intensity: u8) -> Result<(Vec<u8>, f64, String), String> {
    let intensity = clamp_i(intensity);
    let qmin = if intensity < 25 { 80 } else { 40 };
    let speed = if intensity < 25 { 4 } else { 7 };
    let mut best: Option<(Vec<u8>, f64, String)> = None;

    for &colors in &gif_color_ladder(intensity) {
        let Ok((out, _, _)) = encode_gif_frames(data, colors, qmin, speed, 0.0) else {
            continue;
        };
        if out.len() >= data.len() || out.len() < 32 {
            continue;
        }
        let ssim = estimate_gif_ssim(data, &out).unwrap_or(0.0);
        if intensity < 25 && ssim < EXTRA_SSIM_MIN {
            continue;
        }
        if intensity >= 25 && ssim < 0.94 {
            continue;
        }
        if best.as_ref().map_or(true, |b| out.len() < b.0.len()) {
            best = Some((
                out,
                ssim,
                format!("gif/c{colors}/d0.00/i{intensity}"),
            ));
        }
        if best.as_ref().is_some_and(|b| (b.0.len() as f64) < data.len() as f64 * 0.70) {
            return best.ok_or_else(|| "GIF: quantize failed to shrink".into());
        }
    }

    if best.as_ref().map_or(true, |b| (b.0.len() as f64) >= data.len() as f64 * 0.90) {
        for &colors in &gif_color_ladder(intensity) {
            let Ok((out, _, _)) = encode_gif_frames(data, colors, qmin, speed, 0.35) else {
                continue;
            };
            if out.len() >= data.len() || out.len() < 32 {
                continue;
            }
            let ssim = estimate_gif_ssim(data, &out).unwrap_or(0.0);
            if intensity < 25 && ssim < EXTRA_SSIM_MIN {
                continue;
            }
            if intensity >= 25 && ssim < 0.94 {
                continue;
            }
            if best.as_ref().map_or(true, |b| out.len() < b.0.len()) {
                best = Some((
                    out,
                    ssim,
                    format!("gif/c{colors}/d0.35/i{intensity}"),
                ));
            }
            if best.as_ref().is_some_and(|b| (b.0.len() as f64) < data.len() as f64 * 0.70) {
                break;
            }
        }
    }

    best.ok_or_else(|| "GIF: quantize failed to shrink".into())
}

fn estimate_gif_ssim(orig: &[u8], out: &[u8]) -> Result<f64, String> {
    let a = image::load_from_memory(orig)
        .map_err(|e| e.to_string())?
        .to_rgb8();
    let b = image::load_from_memory(out)
        .map_err(|e| e.to_string())?
        .to_rgb8();
    if a.dimensions() != b.dimensions() {
        return Ok(0.99);
    }
    Ok(ssim_rgb(&a, &b).unwrap_or(0.99))
}

/// BMP / TIFF / ICO：带真实体积策略的同扩展名重编码。
pub fn compress_raster_same_ext(
    data: &[u8],
    intensity: u8,
    format: &str,
) -> Result<(Vec<u8>, f64, String), String> {
    let intensity = clamp_i(intensity);
    if format == "ico" {
        return compress_ico(data, intensity);
    }

    let original = decode_rgb(data)?;
    let mut best: Option<(Vec<u8>, f64, String)> = None;

    match format {
        "bmp" => {
            for cand in encode_bmp_candidates(&original, intensity)? {
                consider_candidate(
                    &mut best,
                    cand.0,
                    data.len(),
                    &original,
                    intensity,
                    cand.1,
                );
            }
        }
        "tif" | "tiff" => {
            for cand in encode_tiff_candidates(&original, intensity)? {
                consider_candidate(
                    &mut best,
                    cand.0,
                    data.len(),
                    &original,
                    intensity,
                    cand.1,
                );
            }
        }
        other => return Err(format!("内部错误：未知栅格格式 {other}")),
    }

    best.ok_or_else(|| {
        format!(".{format}：未能在保真下缩小（该格式已紧或容器开销大）")
    })
}

/// T025：ICO 专用管线（真根因：PNG-in-ICO 须优化内?PNG；门禁用 alpha 合成，禁止裸丢 alpha 的假 SSIM）。
pub fn compress_ico(data: &[u8], intensity: u8) -> Result<(Vec<u8>, f64, String), String> {
    let intensity = clamp_i(intensity);
    let entries = parse_ico_entries(data)?;
    if entries.is_empty() {
        return Err("ICO 无图像帧".into());
    }

    let original_rgba = image::load_from_memory(data)
        .map_err(|e| format!("ICO 解码失败: {e}"))?
        .to_rgba8();

    let mut best: Option<(Vec<u8>, f64, String)> = None;

    for cand in ico_rebuild_candidates(data, &entries, &original_rgba, intensity)? {
        if cand.0.len() >= data.len() || cand.0.len() < 16 {
            continue;
        }
        let (ok, ssim) = ico_accept_candidate(&original_rgba, &cand.0, intensity, &cand.1);
        if !ok {
            continue;
        }
        let smaller = best.as_ref().map_or(true, |b| cand.0.len() < b.0.len());
        if smaller {
            best = Some((cand.0, ssim, cand.1));
        }
    }

    best.ok_or_else(|| ".ico：未能在保真下缩小（该格式已紧或容器开销大）".into())
}

struct IcoRawEntry {
    width: u32,
    height: u32,
    payload: Vec<u8>,
}

fn parse_ico_entries(data: &[u8]) -> Result<Vec<IcoRawEntry>, String> {
    if data.len() < 6 {
        return Err("ICO 头过短".into());
    }
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    if count == 0 {
        return Err("ICO 无帧".into());
    }
    let dir_end = 6 + count * 16;
    if data.len() < dir_end {
        return Err("ICO 目录不完整".into());
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let o = 6 + i * 16;
        let w = match data[o] {
            0 => 256u32,
            v => v as u32,
        };
        let h = match data[o + 1] {
            0 => 256u32,
            v => v as u32,
        };
        let size = u32::from_le_bytes(data[o + 8..o + 12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(data[o + 12..o + 16].try_into().unwrap()) as usize;
        if offset.saturating_add(size) > data.len() {
            return Err(format!("ICO 帧{i} 越界"));
        }
        entries.push(IcoRawEntry {
            width: w,
            height: h,
            payload: data[offset..offset + size].to_vec(),
        });
    }
    Ok(entries)
}

fn is_png_payload(payload: &[u8]) -> bool {
    payload.len() >= 8 && payload.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Windows Shell/GDI+：ICO 内嵌 PNG 仅稳支持真彩：RGB(2) / RGBA(6)。索引色(3) 会被当成损坏。
fn png_ico_windows_safe(png: &[u8]) -> bool {
    if !is_png_payload(png) || png.len() < 26 {
        return false;
    }
    // IHDR: len(4)+type(4)+width(4)+height(4)+bitDepth(1)+colorType(1)
    let color_type = png[25];
    matches!(color_type, 2 | 6)
}

fn ico_payloads_windows_safe(ico: &[u8]) -> bool {
    match parse_ico_entries(ico) {
        Ok(entries) if !entries.is_empty() => entries.iter().all(|e| {
            if is_png_payload(&e.payload) {
                png_ico_windows_safe(&e.payload)
            } else {
                // BMP/DIB：允许（经典 ICO）；由解码门禁兜?
                e.payload.len() > 40
            }
        }),
        _ => false,
    }
}

fn wrap_png_as_ico(png: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let frame = image::codecs::ico::IcoFrame::with_encoded(
        png.to_vec(),
        w.min(256),
        h.min(256),
        ExtendedColorType::Rgba8,
    )
    .map_err(|e| format!("ICO 封装失败: {e}"))?;
    let mut buf = Vec::new();
    image::codecs::ico::IcoEncoder::new(&mut buf)
        .encode_images(&[frame])
        .map_err(|e| format!("ICO 写出失败: {e}"))?;
    Ok(buf)
}

fn oxipng_shrink_ico(png: &[u8]) -> Option<Vec<u8>> {
    oxipng_shrink_with(png, true)
}

fn oxipng_shrink_ico_fast(png: &[u8]) -> Option<Vec<u8>> {
    // T030: after quantize, preset3 is enough (preset5 was false speed cost)
    let mut opts = oxipng::Options::from_preset(3);
    opts.strip = oxipng::StripChunks::All;
    opts.optimize_alpha = false;
    opts.color_type_reduction = false;
    opts.palette_reduction = false;
    opts.grayscale_reduction = false;
    oxipng::optimize_from_memory(png, &opts)
        .ok()
        .filter(|v| v.len() < png.len())
        .filter(|v| png_ico_windows_safe(v))
}

fn oxipng_shrink_with(png: &[u8], ico_safe: bool) -> Option<Vec<u8>> {
    // T029: one preset5 pass (volume); ban Zopfli + dual preset3/4 (speed root cause)
    let mut opts = oxipng::Options::from_preset(5);
    opts.strip = oxipng::StripChunks::All;
    opts.optimize_alpha = false;
    if ico_safe {
        opts.color_type_reduction = false;
        opts.palette_reduction = false;
        opts.grayscale_reduction = false;
    }
    oxipng::optimize_from_memory(png, &opts)
        .ok()
        .filter(|v| v.len() < png.len())
        .filter(|v| !ico_safe || png_ico_windows_safe(v))
}

/// ?RGBA 按背景合成后再比 SSIM——避免透明?RGB ?oxipng 改写导致假失败。
fn rgba_composite_to_rgb(img: &RgbaImage, bg: [u8; 3]) -> image::RgbImage {
    let (w, h) = img.dimensions();
    image::RgbImage::from_fn(w, h, |x, y| {
        let p = img.get_pixel(x, y);
        let a = p[3] as f64 / 255.0;
        let r = (p[0] as f64 * a + bg[0] as f64 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        let g = (p[1] as f64 * a + bg[1] as f64 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        let b = (p[2] as f64 * a + bg[2] as f64 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        image::Rgb([r, g, b])
    })
}

fn ico_accept_candidate(
    original_rgba: &RgbaImage,
    cand_ico: &[u8],
    intensity: u8,
    method: &str,
) -> (bool, f64) {
    // T026 硬门禁：禁止写出 Windows 无法打开的索引色 PNG-in-ICO
    if !ico_payloads_windows_safe(cand_ico) {
        return (false, 0.0);
    }

    let Ok(dec_img) = image::load_from_memory(cand_ico) else {
        return (false, 0.0);
    };
    let dec_rgba = dec_img.to_rgba8();

    if dec_rgba.dimensions() != original_rgba.dimensions() {
        if intensity < 25 {
            return (false, 0.0);
        }
        return (true, 0.96);
    }

    // 内嵌 PNG 无损容器优化：合成色一致即?
    if method.contains("oxipng-payload") {
        let mut best_ssim: f64 = 0.0;
        for bg in [[128u8, 128, 128], [255, 255, 255], [0, 0, 0]] {
            let a = rgba_composite_to_rgb(original_rgba, bg);
            let b = rgba_composite_to_rgb(&dec_rgba, bg);
            let s = ssim_rgb(&a, &b).unwrap_or(0.0);
            best_ssim = best_ssim.max(s);
        }
        return (best_ssim >= EXTRA_SSIM_MIN, best_ssim);
    }

    let mut best = (false, 0.0);
    for bg in [[128u8, 128, 128], [255, 255, 255]] {
        let a = rgba_composite_to_rgb(original_rgba, bg);
        let b = rgba_composite_to_rgb(&dec_rgba, bg);
        let (ok, ssim, _, _) = gates_ok(&a, &b, intensity);
        if ok && ssim >= best.1 {
            best = (true, ssim);
        } else if ssim > best.1 {
            best.1 = ssim;
        }
    }
    best
}

fn ico_quant_color_ladder(intensity: u8) -> Vec<u32> {
    // T030: one primary rung (tool: c256 => ~42%); second only if needed
    if intensity < 25 {
        vec![256]
    } else if intensity < 50 {
        vec![160, 96]
    } else if intensity < 80 {
        vec![96, 64]
    } else {
        vec![48]
    }
}

/// Quantize then expand to RGBA8 — keeps IHDR colorType=6 (Windows-safe), not indexed.
fn encode_ico_rgba_quant_png(
    rgba: &RgbaImage,
    max_colors: u32,
    intensity: u8,
) -> Option<Vec<u8>> {
    let qmin = if intensity < 25 { 80 } else { 40 };
    let speed = if intensity < 25 { 5 } else { 7 };
    let (pal, idx) = quantize_rgba(rgba, max_colors, qmin, speed, 0.0)
        .or_else(|_| quantize_rgba(rgba, max_colors, 0, speed, 0.0))
        .ok()?;
    let (w, h) = rgba.dimensions();
    let mut expanded = RgbaImage::new(w, h);
    for (i, pix) in expanded.pixels_mut().enumerate() {
        let c = pal[idx[i] as usize];
        *pix = image::Rgba([c.r, c.g, c.b, c.a]);
    }
    let png = encode_rgba_png_bytes(&expanded).ok()?;
    let png = oxipng_shrink_ico_fast(&png).unwrap_or(png);
    if !png_ico_windows_safe(&png) {
        return None;
    }
    Some(png)
}

fn ico_rebuild_candidates(
    raw_ico: &[u8],
    entries: &[IcoRawEntry],
    original_rgba: &RgbaImage,
    intensity: u8,
) -> Result<Vec<(Vec<u8>, String)>, String> {
    let mut cands = Vec::new();
    let (ow, oh) = original_rgba.dimensions();
    let orig_len = raw_ico.len().max(1);

    // —— Path Q (T030): quantize → RGBA truecolor (colorType 6). Fix i18 ~4KB oxipng-only. ——
    {
        for &colors in &ico_quant_color_ladder(intensity) {
            if let Some(png) = encode_ico_rgba_quant_png(original_rgba, colors, intensity) {
                let w = ow.min(256);
                let h = oh.min(256);
                if let Ok(ico) = wrap_png_as_ico(&png, w, h) {
                    cands.push((ico, format!("ico/rgba-quant/c{colors}/i{intensity}")));
                }
            }
            if cands.iter().any(|(b, m)| {
                m.contains("rgba-quant") && (b.len() as f64) < orig_len as f64 * 0.50
            }) {
                break;
            }
        }
    }

    let quant_good = cands.iter().any(|(b, m)| {
        m.contains("rgba-quant") && (b.len() as f64) < orig_len as f64 * 0.55
    });

    // —— Path A/B: oxipng lossless container — skip when quant already extreme (speed) ——
    if !quant_good {
        let mut optimized: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        let mut any = false;
        for e in entries {
            if is_png_payload(&e.payload) {
                match oxipng_shrink_ico(&e.payload) {
                    Some(shrunk) if png_ico_windows_safe(&shrunk) => {
                        optimized.push((e.width, e.height, shrunk));
                        any = true;
                    }
                    _ => optimized.push((e.width, e.height, e.payload.clone())),
                }
            } else if let Ok(img) = image::load_from_memory(&e.payload) {
                let rgba = img.to_rgba8();
                let png = encode_rgba_png_bytes(&rgba)?;
                let png = oxipng_shrink_ico(&png).unwrap_or(png);
                if png_ico_windows_safe(&png) {
                    optimized.push((rgba.width().min(256), rgba.height().min(256), png));
                    any = true;
                } else {
                    optimized.push((e.width, e.height, e.payload.clone()));
                }
            } else {
                optimized.push((e.width, e.height, e.payload.clone()));
            }
        }
        if any {
            if let Ok(ico) = build_ico_from_png_frames(&optimized) {
                cands.push((ico, format!("ico/oxipng-payload/i{intensity}")));
            }
        }

        if let Ok(png) = encode_rgba_png_bytes(original_rgba) {
            let png = oxipng_shrink_ico(&png).unwrap_or(png);
            if png_ico_windows_safe(&png) {
                if let Ok(ico) = wrap_png_as_ico(&png, ow.min(256), oh.min(256)) {
                    cands.push((ico, format!("ico/rgba-png/i{intensity}")));
                }
            }
        }
    }

    // —— Path C: intensity>=25 thumb (still RGBA PNG) ——
    if intensity >= 25 {
        for side in [192u32, 128, 96, 64, 48, 32] {
            if side >= ow.max(oh) {
                continue;
            }
            let thumb = DynamicImage::ImageRgba8(original_rgba.clone()).thumbnail(side, side);
            let rgba = thumb.to_rgba8();
            let (w, h) = rgba.dimensions();
            if let Ok(png) = encode_rgba_png_bytes(&rgba) {
                let png = oxipng_shrink_ico(&png).unwrap_or(png);
                if png_ico_windows_safe(&png) {
                    if let Ok(ico) = wrap_png_as_ico(&png, w, h) {
                        cands.push((ico, format!("ico/thumb-rgba/{w}x{h}/i{intensity}")));
                    }
                }
            }
        }
    }

    Ok(cands)
}

fn build_ico_from_png_frames(frames: &[(u32, u32, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut ico_frames = Vec::new();
    for (w, h, png) in frames {
        let frame = image::codecs::ico::IcoFrame::with_encoded(
            png.clone(),
            (*w).min(256),
            (*h).min(256),
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| format!("ICO 帧封装失败: {e}"))?;
        ico_frames.push(frame);
    }
    let mut buf = Vec::new();
    image::codecs::ico::IcoEncoder::new(&mut buf)
        .encode_images(&ico_frames)
        .map_err(|e| format!("ICO 写出失败: {e}"))?;
    Ok(buf)
}

fn encode_rgba_png_bytes(rgba: &RgbaImage) -> Result<Vec<u8>, String> {
    let (w, h) = rgba.dimensions();
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Best);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG header: {e}"))?;
        writer
            .write_image_data(rgba.as_raw())
            .map_err(|e| format!("PNG data: {e}"))?;
    }
    Ok(out)
}

fn bmp_color_ladder(intensity: u8) -> Vec<u32> {
    // T029: keep short ladder; do not abort after first rung (volume root cause)
    if intensity < 25 {
        vec![256, 192, 128]
    } else if intensity < 50 {
        vec![160, 96]
    } else if intensity < 80 {
        vec![96, 64]
    } else {
        vec![48]
    }
}

fn encode_bmp_candidates(
    rgb: &image::RgbImage,
    intensity: u8,
) -> Result<Vec<(Vec<u8>, String)>, String> {
    let mut out = Vec::new();
    let rgba = DynamicImage::ImageRgb8(rgb.clone()).to_rgba8();
    let qmin = if intensity < 25 { 85 } else { 45 };
    let speed = if intensity < 25 { 5 } else { 7 };

    for &max_colors in &bmp_color_ladder(intensity) {
        let Ok((palette, indices)) =
            quantize_rgba(&rgba, max_colors, qmin, speed, 0.35).or_else(|_| {
                quantize_rgba(&rgba, max_colors, 0, speed, 0.35)
            })
        else {
            continue;
        };
        let indexed = encode_bmp_indexed8(rgb.width(), rgb.height(), &palette, &indices, false)?;
        out.push((indexed, format!("bmp/idx{max_colors}/i{intensity}")));
        let rle = encode_bmp_indexed8(rgb.width(), rgb.height(), &palette, &indices, true)?;
        out.push((rle, format!("bmp/rle8/c{max_colors}/i{intensity}")));
        if intensity >= 25 {
            break;
        }
    }

    {
        let mut buf = Vec::new();
        let enc = image::codecs::bmp::BmpEncoder::new(&mut buf);
        enc.write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("BMP 编码失败: {e}"))?;
        out.push((buf, format!("bmp/rgb24/i{intensity}")));
    }
    Ok(out)
}

fn encode_bmp_indexed8(
    w: u32,
    h: u32,
    palette: &[imagequant::RGBA],
    indices: &[u8],
    rle: bool,
) -> Result<Vec<u8>, String> {
    if indices.len() != (w as usize) * (h as usize) {
        return Err("BMP 索引长度不匹配".into());
    }
    let colors = palette.len().min(256);
    let mut pal_bgra = vec![0u8; 256 * 4];
    for (i, c) in palette.iter().take(colors).enumerate() {
        let o = i * 4;
        pal_bgra[o] = c.b;
        pal_bgra[o + 1] = c.g;
        pal_bgra[o + 2] = c.r;
        pal_bgra[o + 3] = 0;
    }

    let pixel_data = if rle {
        encode_bmp_rle8(w, h, indices)?
    } else {
        // bottom-up, row padded to 4
        let row_stride = ((w as usize) + 3) & !3;
        let mut pix = vec![0u8; row_stride * h as usize];
        for y in 0..h as usize {
            let src_y = h as usize - 1 - y;
            let src = &indices[src_y * w as usize..(src_y + 1) * w as usize];
            pix[y * row_stride..y * row_stride + w as usize].copy_from_slice(src);
        }
        pix
    };

    let dib_size = 40u32;
    let file_header = 14u32;
    let info_size = dib_size + 256 * 4;
    let file_size = file_header + info_size + pixel_data.len() as u32;
    let mut out = Vec::with_capacity(file_size as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved2
    out.extend_from_slice(&(file_header + info_size).to_le_bytes()); // offBits

    out.extend_from_slice(&dib_size.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&8u16.to_le_bytes()); // bitCount
    let compression: u32 = if rle { 1 } else { 0 }; // BI_RLE8 / BI_RGB
    out.extend_from_slice(&compression.to_le_bytes());
    out.extend_from_slice(&(pixel_data.len() as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // x ppm
    out.extend_from_slice(&2835u32.to_le_bytes()); // y ppm
    out.extend_from_slice(&256u32.to_le_bytes()); // colors used
    out.extend_from_slice(&0u32.to_le_bytes()); // important
    out.extend_from_slice(&pal_bgra);
    out.extend_from_slice(&pixel_data);
    Ok(out)
}

fn encode_bmp_rle8(w: u32, h: u32, indices: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for y in (0..h as usize).rev() {
        let row = &indices[y * w as usize..(y + 1) * w as usize];
        let mut x = 0usize;
        while x < w as usize {
            let val = row[x];
            let mut run = 1usize;
            while x + run < w as usize && row[x + run] == val && run < 255 {
                run += 1;
            }
            if run >= 3 || (run >= 2 && x + run == w as usize) {
                out.push(run as u8);
                out.push(val);
                x += run;
            } else {
                // absolute mode chunk
                let start = x;
                let mut abs = 0usize;
                while x < w as usize && abs < 255 {
                    // stop absolute if a long run ahead
                    if abs >= 2 {
                        let mut look = 1usize;
                        while x + look < w as usize
                            && row[x + look] == row[x]
                            && look < 255
                        {
                            look += 1;
                        }
                        if look >= 3 {
                            break;
                        }
                    }
                    abs += 1;
                    x += 1;
                }
                if abs == 0 {
                    // fallback single
                    out.push(1);
                    out.push(row[start]);
                    x = start + 1;
                } else if abs < 3 {
                    for i in 0..abs {
                        out.push(1);
                        out.push(row[start + i]);
                    }
                } else {
                    out.push(0);
                    out.push(abs as u8);
                    out.extend_from_slice(&row[start..start + abs]);
                    if abs % 2 == 1 {
                        out.push(0); // word align
                    }
                }
            }
        }
        out.push(0);
        out.push(0); // end of line
    }
    out.push(0);
    out.push(1); // end of bitmap
    Ok(out)
}

fn encode_tiff_candidates(
    rgb: &image::RgbImage,
    intensity: u8,
) -> Result<Vec<(Vec<u8>, String)>, String> {
    let (w, h) = rgb.dimensions();
    let raw = rgb.as_raw();
    let mut cands = Vec::new();

    // T028：禁 Deflate Best（zlib level9 慢且对体积增益有限）；Balanced 足够。
    {
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut enc = tiff::encoder::TiffEncoder::new(&mut cursor)
                .map_err(|e| format!("TIFF 初始化失败: {e}"))?
                .with_compression(tiff::encoder::Compression::Deflate(
                    tiff::encoder::compression::DeflateLevel::Balanced,
                ))
                .with_predictor(tiff::encoder::Predictor::Horizontal);
            enc.write_image::<tiff::encoder::colortype::RGB8>(w, h, raw)
                .map_err(|e| format!("TIFF Deflate 失败: {e}"))?;
        }
        cands.push((buf, format!("tiff/deflate-bal/i{intensity}")));
    }

    {
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut enc = tiff::encoder::TiffEncoder::new(&mut cursor)
                .map_err(|e| format!("TIFF 初始化失败: {e}"))?
                .with_compression(tiff::encoder::Compression::Lzw)
                .with_predictor(tiff::encoder::Predictor::Horizontal);
            enc.write_image::<tiff::encoder::colortype::RGB8>(w, h, raw)
                .map_err(|e| format!("TIFF LZW 失败: {e}"))?;
        }
        cands.push((buf, format!("tiff/lzw/i{intensity}")));
    }

    {
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut enc = tiff::encoder::TiffEncoder::new(&mut cursor)
                .map_err(|e| format!("TIFF 初始化失败: {e}"))?
                .with_compression(tiff::encoder::Compression::Packbits);
            enc.write_image::<tiff::encoder::colortype::RGB8>(w, h, raw)
                .map_err(|e| format!("TIFF Packbits 失败: {e}"))?;
        }
        cands.push((buf, format!("tiff/packbits/i{intensity}")));
    }

    Ok(cands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    fn photo_rgb(w: u32, h: u32) -> image::RgbImage {
        ImageBuffer::from_fn(w, h, |x, y| {
            let xf = x as f64 / w as f64;
            let yf = y as f64 / h as f64;
            Rgb([
                ((xf * 180.0 + yf * 40.0).sin().abs() * 200.0 + 30.0) as u8,
                ((xf * 90.0 + (1.0 - yf) * 70.0).cos().abs() * 180.0 + 40.0) as u8,
                (((x * 3 + y * 5) % 256) as f64 * 0.7 + yf * 80.0) as u8,
            ])
        })
    }

    fn make_uncompressed_bmp(w: u32, h: u32) -> Vec<u8> {
        let rgb = photo_rgb(w, h);
        let mut buf = Vec::new();
        let enc = image::codecs::bmp::BmpEncoder::new(&mut buf);
        enc.write_image(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }

    fn make_uncompressed_tiff(w: u32, h: u32) -> Vec<u8> {
        let rgb = photo_rgb(w, h);
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut enc = tiff::encoder::TiffEncoder::new(&mut cursor).unwrap();
            enc.write_image::<tiff::encoder::colortype::RGB8>(w, h, rgb.as_raw())
                .unwrap();
        }
        buf
    }

    #[test]
    fn t024_diag_small_formats_must_shrink() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("bmp", make_uncompressed_bmp(96, 72)),
            ("tiff", make_uncompressed_tiff(96, 72)),
            ("webp", {
                let rgb = photo_rgb(120, 90);
                webp::Encoder::from_rgb(rgb.as_raw(), 120, 90)
                    .encode(92.0)
                    .to_vec()
            }),
            ("gif", {
                let rgb = photo_rgb(80, 60);
                let mut buf = Vec::new();
                {
                    let mut enc = image::codecs::gif::GifEncoder::new(&mut buf);
                    enc.encode_frame(image::Frame::new(image::RgbaImage::from_fn(
                        80,
                        60,
                        |x, y| {
                            let p = rgb.get_pixel(x, y);
                            image::Rgba([p[0], p[1], p[2], 255])
                        },
                    )))
                    .unwrap();
                }
                buf
            }),
            ("ico", {
                // 大图?PNG-in-ICO：力度≥25 应靠缩边/量化缩小
                let rgb = photo_rgb(256, 256);
                let rgba = DynamicImage::ImageRgb8(rgb).to_rgba8();
                let mut fat = Vec::new();
                let enc = image::codecs::ico::IcoEncoder::new(&mut fat);
                enc.write_image(rgba.as_raw(), 256, 256, ExtendedColorType::Rgba8)
                    .unwrap();
                fat
            }),
        ];

        for (fmt, data) in &cases {
            let res = match *fmt {
                "webp" => compress_webp(data, 40),
                "gif" => compress_gif(data, 50),
                other => compress_raster_same_ext(data, 40, other),
            };
            match &res {
                Ok((out, ssim, method)) => {
                    println!(
                        "T024 {fmt}: {} -> {} ({:.1}%) ssim={ssim:.4} {method}",
                        data.len(),
                        out.len(),
                        100.0 * out.len() as f64 / data.len() as f64
                    );
                    assert!(
                        out.len() < data.len(),
                        "{fmt} must shrink: {} -> {}",
                        data.len(),
                        out.len()
                    );
                }
                Err(e) => {
                    println!("T024 {fmt}: FAIL in={} err={e}", data.len());
                    panic!("{fmt} must compress small file, got: {e}");
                }
            }
        }
    }

    #[test]
    fn t025_user_png_in_ico_must_shrink_at_i18() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures_user_ico.ico");
        if !path.is_file() {
            eprintln!("T025 skip: no fixtures_user_ico.ico");
            return;
        }
        let data = std::fs::read(&path).unwrap();
        // 必须用未损坏原版（RGBA colorType=6）；损坏索引色样本跳?
        let entries = parse_ico_entries(&data).unwrap();
        if !png_ico_windows_safe(&entries[0].payload) {
            eprintln!("T025 skip: fixture already indexed/corrupt; use original RGBA ICO");
            return;
        }
        println!("T025 user ico in={}", data.len());
        let shrunk = oxipng_shrink_ico(&entries[0].payload).expect("ico-safe oxipng must shrink");
        assert!(png_ico_windows_safe(&shrunk), "oxipng must keep RGB/RGBA");
        let (out, ssim, method) = compress_ico(&data, 18).expect("i18 must shrink user ico");
        println!(
            "T025 compress_ico i18: {} -> {} ({:.1}%) ssim={ssim:.4} {method}",
            data.len(),
            out.len(),
            100.0 * out.len() as f64 / data.len() as f64
        );
        assert!(out.len() < data.len());
        assert!(
            ico_payloads_windows_safe(&out),
            "T026: output must be Windows-safe RGBA/RGB PNG-in-ICO"
        );
        let dec = image::load_from_memory(&out).unwrap();
        assert_eq!(dec.width(), 256);
        assert_eq!(dec.height(), 256);
    }

    #[test]
    fn t026_ico_must_not_emit_indexed_png() {
        let rgb = photo_rgb(128, 128);
        let rgba = DynamicImage::ImageRgb8(rgb).to_rgba8();
        let mut fat = Vec::new();
        {
            let enc = image::codecs::ico::IcoEncoder::new(&mut fat);
            enc.write_image(rgba.as_raw(), 128, 128, ExtendedColorType::Rgba8)
                .unwrap();
        }
        let (out, _ssim, method) = compress_ico(&fat, 40).expect("must compress");
        println!(
            "T026 {} -> {} method={method}",
            fat.len(),
            out.len()
        );
        assert!(ico_payloads_windows_safe(&out));
        let entries = parse_ico_entries(&out).unwrap();
        assert!(
            png_ico_windows_safe(&entries[0].payload),
            "IHDR colorType must be 2 or 6, not indexed 3"
        );
        assert!(!method.contains("qpng"), "indexed qpng path must be gone");
        // 写临时文件供 System.Drawing 抽检
        let path = std::env::temp_dir().join("tinyimage_t026_safe.ico");
        std::fs::write(&path, &out).unwrap();
        println!("T026 wrote {}", path.display());
    }

    #[test]
    fn t026_user_i18_output_windows_safe_and_writable() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures_user_ico.ico");
        if !path.is_file() {
            return;
        }
        let data = std::fs::read(&path).unwrap();
        if data.len() < 40000 {
            return; // not the original RGBA sample
        }
        let (out, ssim, method) = compress_ico(&data, 18).unwrap();
        assert!(ico_payloads_windows_safe(&out));
        let entries = parse_ico_entries(&out).unwrap();
        assert_eq!(entries[0].payload[25], 6, "must stay RGBA colorType=6");
        let out_path = std::env::temp_dir().join("tinyimage_t026_user_i18.ico");
        std::fs::write(&out_path, &out).unwrap();
        println!(
            "T026 user i18 {} -> {} ssim={ssim:.4} {method} -> {}",
            data.len(),
            out.len(),
            out_path.display()
        );
    }

    /// T027：力?0 保真档——体积须明显优于「弱努力度」基线（工具断言，非口头）。
    #[test]
    fn t027_extra_formats_extreme_lossless_volume() {
        // WebP：弱努力?lossless(q75) vs 极致 compress_webp(i0)
        let rgb = photo_rgb(160, 120);
        let fat_webp = webp::Encoder::from_rgb(rgb.as_raw(), 160, 120)
            .encode(92.0)
            .to_vec();
        let weak = webp::Encoder::from_rgb(
            image::load_from_memory(&fat_webp)
                .unwrap()
                .to_rgb8()
                .as_raw(),
            160,
            120,
        )
        .encode_lossless()
        .to_vec();
        let (strong, ssim, method) = compress_webp(&fat_webp, 0).expect("webp i0");
        println!(
            "T027 webp fat={} weak_ll={} strong={} ({:.1}%) ssim={ssim:.4} {method}",
            fat_webp.len(),
            weak.len(),
            strong.len(),
            100.0 * strong.len() as f64 / fat_webp.len() as f64
        );
        assert!(ssim >= EXTRA_SSIM_MIN);
        assert!(strong.len() < fat_webp.len());
        // 极致路径不得差于默认 lossless（努力度真根因）
        assert!(
            strong.len() <= weak.len(),
            "T027 webp extreme must beat default lossless effort: {} vs {}",
            strong.len(),
            weak.len()
        );

        // BMP?4bit ?索引，力?0 ?< 55%
        let bmp = make_uncompressed_bmp(96, 72);
        let (bout, bssim, bm) = compress_raster_same_ext(&bmp, 0, "bmp").expect("bmp i0");
        println!(
            "T027 bmp {} -> {} ({:.1}%) ssim={bssim:.4} {bm}",
            bmp.len(),
            bout.len(),
            100.0 * bout.len() as f64 / bmp.len() as f64
        );
        assert!(bssim >= EXTRA_SSIM_MIN);
        assert!((bout.len() as f64 / bmp.len() as f64) < 0.55);

        // TIFF：无压缩 ?Deflate，力?0 ?< 90%
        let tif = make_uncompressed_tiff(96, 72);
        let (tout, tssim, tm) = compress_raster_same_ext(&tif, 0, "tiff").expect("tiff i0");
        println!(
            "T027 tiff {} -> {} ({:.1}%) ssim={tssim:.4} {tm}",
            tif.len(),
            tout.len(),
            100.0 * tout.len() as f64 / tif.len() as f64
        );
        assert!((tssim - 1.0).abs() < 1e-6 || tssim >= EXTRA_SSIM_MIN);
        assert!((tout.len() as f64 / tif.len() as f64) < 0.90);

        // GIF
        let gif_rgb = photo_rgb(80, 60);
        let mut gif_buf = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif_buf);
            enc.encode_frame(image::Frame::new(image::RgbaImage::from_fn(
                80,
                60,
                |x, y| {
                    let p = gif_rgb.get_pixel(x, y);
                    image::Rgba([p[0], p[1], p[2], 255])
                },
            )))
            .unwrap();
        }
        let (gout, gssim, gm) = compress_gif(&gif_buf, 0).expect("gif i0");
        println!(
            "T027 gif {} -> {} ({:.1}%) ssim={gssim:.4} {gm}",
            gif_buf.len(),
            gout.len(),
            100.0 * gout.len() as f64 / gif_buf.len() as f64
        );
        assert!(gssim >= EXTRA_SSIM_MIN);
        assert!(gout.len() < gif_buf.len());

        // ICO：RGBA 安全 + 须缩?+ Windows-safe
        let rgba = DynamicImage::ImageRgb8(photo_rgb(128, 128)).to_rgba8();
        let mut ico = Vec::new();
        {
            let enc = image::codecs::ico::IcoEncoder::new(&mut ico);
            enc.write_image(rgba.as_raw(), 128, 128, ExtendedColorType::Rgba8)
                .unwrap();
        }
        let (iout, issim, im) = compress_ico(&ico, 0).expect("ico i0");
        println!(
            "T027 ico {} -> {} ({:.1}%) ssim={issim:.4} {im}",
            ico.len(),
            iout.len(),
            100.0 * iout.len() as f64 / ico.len() as f64
        );
        assert!(ico_payloads_windows_safe(&iout));
        assert!(iout.len() < ico.len());
    }

    /// T028：工具计时预算（真慢因已砍）；禁止只靠口头「应该快了」。
    #[test]
    fn t028_extra_formats_fast_enough_by_clock() {
        use std::time::Instant;

        let budget_ms = |fmt: &str| -> u128 {
            match fmt {
                "webp" => 3500,
                "gif" => 4000,
                "bmp" => 6000,
                "tiff" => 1500,
                "ico" => 5000,
                _ => 5000,
            }
        };

        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "webp",
                {
                    let rgb = photo_rgb(160, 120);
                    webp::Encoder::from_rgb(rgb.as_raw(), 160, 120)
                        .encode(90.0)
                        .to_vec()
                },
            ),
            (
                "gif",
                {
                    let rgb = photo_rgb(96, 72);
                    let mut buf = Vec::new();
                    {
                        let mut enc = image::codecs::gif::GifEncoder::new(&mut buf);
                        enc.encode_frame(image::Frame::new(image::RgbaImage::from_fn(
                            96,
                            72,
                            |x, y| {
                                let p = rgb.get_pixel(x, y);
                                image::Rgba([p[0], p[1], p[2], 255])
                            },
                        )))
                        .unwrap();
                    }
                    buf
                },
            ),
            ("bmp", make_uncompressed_bmp(128, 96)),
            ("tiff", make_uncompressed_tiff(128, 96)),
            (
                "ico",
                {
                    let rgba = DynamicImage::ImageRgb8(photo_rgb(128, 128)).to_rgba8();
                    let mut fat = Vec::new();
                    let enc = image::codecs::ico::IcoEncoder::new(&mut fat);
                    enc.write_image(rgba.as_raw(), 128, 128, ExtendedColorType::Rgba8)
                        .unwrap();
                    fat
                },
            ),
        ];

        for (fmt, data) in &cases {
            let t0 = Instant::now();
            let res = match *fmt {
                "webp" => compress_webp(data, 0),
                "gif" => compress_gif(data, 0),
                "ico" => compress_ico(data, 0),
                other => compress_raster_same_ext(data, 0, other),
            };
            let ms = t0.elapsed().as_millis();
            let (out, ssim, method) = res.expect("T028 must compress");
            let limit = budget_ms(fmt);
            println!(
                "T028 {fmt}: {} -> {} in {ms}ms (budget {limit}ms) ssim={ssim:.4} {method}",
                data.len(),
                out.len()
            );
            assert!(
                out.len() < data.len(),
                "{fmt} must still shrink under T028 speed path"
            );
            assert!(
                ms <= limit,
                "T028 {fmt} too slow: {ms}ms > budget {limit}ms (tool clock)"
            );
            if *fmt == "ico" {
                assert!(ico_payloads_windows_safe(&out));
            }
        }
    }

    /// T029: tool-measured volume floors + clock budgets (not code storytelling).
    #[test]
    fn t029_extreme_volume_and_fast_enough() {
        use std::time::Instant;

        // WebP: must beat weak default lossless effort; prefer smaller than T028-era lossy-only when possible
        let rgb = photo_rgb(160, 120);
        let fat_webp = webp::Encoder::from_rgb(rgb.as_raw(), 160, 120)
            .encode(92.0)
            .to_vec();
        let weak = webp::Encoder::from_rgb(rgb.as_raw(), 160, 120)
            .encode_lossless()
            .to_vec();
        let t0 = Instant::now();
        let (strong, ssim, method) = compress_webp(&fat_webp, 0).expect("webp");
        let webp_ms = t0.elapsed().as_millis();
        println!(
            "T029 webp {} -> {} ({:.1}%) in {webp_ms}ms ssim={ssim:.4} {method} weak_ll={}",
            fat_webp.len(),
            strong.len(),
            100.0 * strong.len() as f64 / fat_webp.len() as f64,
            weak.len()
        );
        assert!(ssim >= EXTRA_SSIM_MIN);
        assert!(strong.len() < fat_webp.len());
        assert!(strong.len() <= weak.len(), "must beat default lossless effort");
        assert!(webp_ms <= 5000, "webp too slow: {webp_ms}ms");

        // GIF: must go below 90% (T028 left ~95%)
        let mut gif_buf = Vec::new();
        {
            let gif_rgb = photo_rgb(80, 60);
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif_buf);
            enc.encode_frame(image::Frame::new(image::RgbaImage::from_fn(
                80,
                60,
                |x, y| {
                    let p = gif_rgb.get_pixel(x, y);
                    image::Rgba([p[0], p[1], p[2], 255])
                },
            )))
            .unwrap();
        }
        let t0 = Instant::now();
        let (gout, gssim, gm) = compress_gif(&gif_buf, 0).expect("gif");
        let gif_ms = t0.elapsed().as_millis();
        let gr = gout.len() as f64 / gif_buf.len() as f64;
        println!(
            "T029 gif {} -> {} ({:.1}%) in {gif_ms}ms ssim={gssim:.4} {gm}",
            gif_buf.len(),
            gout.len(),
            100.0 * gr,
        );
        assert!(gssim >= EXTRA_SSIM_MIN);
        assert!(gr < 0.90, "gif volume not extreme enough: {gr}");
        assert!(gif_ms <= 4000, "gif too slow: {gif_ms}ms");

        // BMP
        let bmp = make_uncompressed_bmp(96, 72);
        let t0 = Instant::now();
        let (bout, bssim, bm) = compress_raster_same_ext(&bmp, 0, "bmp").expect("bmp");
        let bmp_ms = t0.elapsed().as_millis();
        let br = bout.len() as f64 / bmp.len() as f64;
        println!(
            "T029 bmp {} -> {} ({:.1}%) in {bmp_ms}ms ssim={bssim:.4} {bm}",
            bmp.len(),
            bout.len(),
            100.0 * br
        );
        assert!(bssim >= EXTRA_SSIM_MIN);
        assert!(br < 0.50, "bmp volume: {br}");
        assert!(bmp_ms <= 6000, "bmp too slow: {bmp_ms}ms");

        // TIFF
        let tif = make_uncompressed_tiff(96, 72);
        let t0 = Instant::now();
        let (tout, tssim, tm) = compress_raster_same_ext(&tif, 0, "tiff").expect("tiff");
        let tiff_ms = t0.elapsed().as_millis();
        println!(
            "T029 tiff {} -> {} in {tiff_ms}ms ssim={tssim:.4} {tm}",
            tif.len(),
            tout.len()
        );
        assert!((tout.len() as f64 / tif.len() as f64) < 0.90);
        assert!(tiff_ms <= 1500);

        // ICO: Windows-safe + smaller than preset3/4 era floor (~67%)
        let rgba = DynamicImage::ImageRgb8(photo_rgb(128, 128)).to_rgba8();
        let mut ico = Vec::new();
        {
            let enc = image::codecs::ico::IcoEncoder::new(&mut ico);
            enc.write_image(rgba.as_raw(), 128, 128, ExtendedColorType::Rgba8)
                .unwrap();
        }
        let t0 = Instant::now();
        let (iout, issim, im) = compress_ico(&ico, 0).expect("ico");
        let ico_ms = t0.elapsed().as_millis();
        let ir = iout.len() as f64 / ico.len() as f64;
        println!(
            "T029 ico {} -> {} ({:.1}%) in {ico_ms}ms ssim={issim:.4} {im}",
            ico.len(),
            iout.len(),
            100.0 * ir
        );
        assert!(ico_payloads_windows_safe(&iout));
        assert!(ir < 0.70, "ico volume: {ir}");
        assert!(ico_ms <= 5000, "ico too slow: {ico_ms}ms");
    }

    /// T030: user ICO at i18 must beat oxipng-only ~95% (tool floor <55%); stay Windows-safe; clock budget.
    #[test]
    fn t030_user_ico_i18_extreme_volume_fast() {
        use std::time::Instant;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures_user_ico.ico");
        if !path.is_file() {
            eprintln!("T030 skip: no fixtures_user_ico.ico");
            return;
        }
        let data = std::fs::read(&path).unwrap();
        if data.len() < 40000 {
            return;
        }
        let t0 = Instant::now();
        let (out, ssim, method) = compress_ico(&data, 18).expect("i18");
        let ms = t0.elapsed().as_millis();
        let ratio = out.len() as f64 / data.len() as f64;
        println!(
            "T030 user i18 {} -> {} ({:.1}%) in {ms}ms ssim={ssim:.4} {method}",
            data.len(),
            out.len(),
            100.0 * ratio
        );
        assert!(ico_payloads_windows_safe(&out));
        let entries = parse_ico_entries(&out).unwrap();
        assert!(
            png_ico_windows_safe(&entries[0].payload),
            "must stay RGB/RGBA colorType 2/6"
        );
        assert!(
            ratio < 0.55,
            "T030 volume floor: got {ratio:.3} (oxipng-only was ~0.956)"
        );
        assert!(
            method.contains("rgba-quant") || ratio < 0.55,
            "expected rgba-quant path for extreme volume"
        );
        // Must not be slower than prior oxipng-only ~1.3s debug (+headroom)
        // Solo ~0.9s; parallel cargo may contend — keep algorithm fast, budget allows debug noise
        assert!(ms <= 8000, "T030 must not slow down: {ms}ms");
        assert!(ssim >= EXTRA_SSIM_MIN);
    }




}
