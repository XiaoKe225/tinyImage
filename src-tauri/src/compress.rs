//! T022：压缩加速 + 窗口最小常态 + 用户文案去黑话。
//! T038：在不改压缩率/体积前提下再加速——保真区并行试路（无损+有损、PNG 无损+quant）。
//! - 真根因（慢）：①中高档 Jpegli+Moz 双跑且 Moz trellis；②0 档 q 阶梯过长；③无损 progressive 双试；④sizeRefine 多次；⑤image 慢解 JPEG 做 SSIM 门禁；⑥PNG 无损与 quant 串行。
//! - 假根因：单纯「再加线程」不解决单张编码路径过重。
//! - 策略：全程 Jpegli 优先；MozJPEG 快解；保真区并行试路；缩短 0 档阶梯；精炼/恢复次数砍半；观感达标可跳过无损。

use crate::jpeg_lossless::jpeg_lossless_optimize_ex;
use crate::quality::{
    edge_retain_ratio, high_freq_retain_ratio, psnr_rgb, rgba_to_rgb, ssim_rgb,
};
use image::{DynamicImage, RgbImage};
use serde::Serialize;
use std::fs;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};

/// 默认力度：观感/像素近无损档。
pub const DEFAULT_INTENSITY: u8 = 0;

/// &lt; 此力度：保真区；≥：4:2:0 换体积。
pub const JPEG_DETAIL_THRESHOLD: u8 = 25;

/// ≥ 此力度：体积精炼。
pub const JPEG_SIZE_REFINE_THRESHOLD: u8 = 50;

/// &lt; 此力度：PNG 保真；0 档允许高质近无损量化。
pub const PNG_LOSSY_THRESHOLD: u8 = 25;

/// 保真区有损质量下限。
pub const JPEG_FIDELITY_Q_FLOOR: u8 = 72;

/// 观感路径相对原图体积上限（至少约省 12%）。
pub const VISUAL_MAX_SIZE_RATIO: f64 = 0.88;

/// 有损相对无损：须严格更小才替换。
pub const VISUAL_BEAT_LOSSLESS_RATIO: f64 = 1.0;

/// 0 档锐度门禁。
pub const ZERO_SSIM_MIN: f64 = 0.9925;
pub const ZERO_HF_MIN: f64 = 0.97;
pub const ZERO_EDGE_MIN: f64 = 0.97;
pub const ZERO_PSNR_MIN: f64 = 34.0;

/// 0 档观感阶梯（缩短：跳过极少过体积门的超高 q，加速 first-fit）。
pub const ZERO_VISUAL_QUALITIES: [u8; 4] = [92, 90, 88, 86];

/// 观感已明显小于原图时跳过昂贵无损竞速。
pub const ZERO_SKIP_LOSSLESS_RATIO: f64 = 0.85;

/// 高频门禁：i=0→0.95；i=100→0.50（已压缩含颗粒图代际再编码 HF 常≈0.50）。
pub fn hf_min_for_intensity(intensity: u8) -> f64 {
    let i = clamp_intensity(intensity) as f64;
    0.95 - (i / 100.0) * 0.45
}

/// SSIM：i=0→0.995；i=100→0.900。
pub fn ssim_min_for_intensity(intensity: u8) -> f64 {
    let i = clamp_intensity(intensity) as f64;
    0.995 - (i / 100.0) * 0.095
}

/// 力度→质量%展示/有损映射：0→100；34→75；100→42。
pub fn jpeg_quality_from_intensity(intensity: u8) -> u8 {
    let i = clamp_intensity(intensity) as u32;
    if i == 0 {
        return 100;
    }
    if i <= 34 {
        (96 - (i * 21) / 34) as u8
    } else {
        (75 - ((i - 34) * 33) / 66) as u8
    }
}

/// 等效「压缩质量%」展示。
pub fn quality_percent_from_intensity(intensity: u8) -> u8 {
    jpeg_quality_from_intensity(intensity)
}

fn jpeg_bits_per_pixel(data_len: usize, w: u32, h: u32) -> f64 {
    let px = (w as f64) * (h as f64);
    if px < 1.0 {
        99.0
    } else {
        (data_len as f64) * 8.0 / px
    }
}

pub fn jpeg_use_420(intensity: u8) -> bool {
    clamp_intensity(intensity) >= JPEG_DETAIL_THRESHOLD
}

/// pngquant：仅 ≥PNG_LOSSY_THRESHOLD 使用；门槛处 qmin90 → 100 处 qmin40。
pub fn pngquant_quality_min(intensity: u8) -> u8 {
    let i = clamp_intensity(intensity).saturating_sub(PNG_LOSSY_THRESHOLD) as u32;
    let span = (100 - PNG_LOSSY_THRESHOLD) as u32;
    (90 - (i * 50) / span.max(1)) as u8
}

pub fn png_max_colors(intensity: u8) -> u32 {
    let i = clamp_intensity(intensity).saturating_sub(PNG_LOSSY_THRESHOLD) as u32;
    let span = (100 - PNG_LOSSY_THRESHOLD) as u32;
    256 - (i * 200) / span.max(1)
}

pub fn png_dither_level(intensity: u8) -> f32 {
    let i = clamp_intensity(intensity) as f32;
    0.08 + (i / 100.0) * 0.35
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CompressResult {
    pub path: String,
    pub input_size: u64,
    pub output_size: u64,
    pub saved_bytes: i64,
    pub format: String,
    pub skipped: bool,
    pub method: String,
    pub ssim: Option<f64>,
    pub intensity: u8,
}

pub fn clamp_intensity(v: u8) -> u8 {
    v.min(100)
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

fn decode_to_rgb(data: &[u8]) -> Result<RgbImage, String> {
    let img = image::load_from_memory(data).map_err(|e| format!("解码失败: {e}"))?;
    Ok(match img {
        DynamicImage::ImageRgb8(rgb) => rgb,
        DynamicImage::ImageRgba8(rgba) => rgba_to_rgb(&rgba),
        other => other.to_rgb8(),
    })
}

pub fn encode_jpeg_fallback(rgb: &RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("JPEG 回退编码失败: {e}"))?;
    Ok(out)
}

/// Jpegli：`progressive=false` 用于 0 档观感锐利路径。
pub fn encode_jpeg_jpegli(
    rgb: &RgbImage,
    quality: u8,
    subsample_420: bool,
) -> Result<Vec<u8>, String> {
    encode_jpeg_jpegli_ex(rgb, quality, subsample_420, true)
}

pub fn encode_jpeg_jpegli_ex(
    rgb: &RgbImage,
    quality: u8,
    subsample_420: bool,
    progressive: bool,
) -> Result<Vec<u8>, String> {
    let (w, h) = rgb.dimensions();
    let raw = rgb.as_raw().to_vec();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut comp = jpegli::Compress::new(jpegli::ColorSpace::JCS_RGB);
        comp.set_size(w as usize, h as usize);
        comp.set_quality(quality as f32);
        if progressive {
            comp.set_progressive_mode();
        }
        comp.set_optimize_coding(true);
        if subsample_420 {
            comp.set_chroma_sampling_pixel_sizes((2, 2), (2, 2));
        } else {
            comp.set_chroma_sampling_pixel_sizes((1, 1), (1, 1));
        }
        let mut started = comp
            .start_compress(Vec::new())
            .map_err(|e| format!("Jpegli start: {e}"))?;
        started
            .write_scanlines(&raw)
            .map_err(|e| format!("Jpegli write: {e}"))?;
        started.finish().map_err(|e| format!("Jpegli finish: {e}"))
    }))
    .map_err(|_| "Jpegli 编码 panic".to_string())?
}

/// MozJPEG：`heavy=true` 开 trellis（慢、更小）；低档禁用。
pub fn encode_jpeg_moz(
    rgb: &RgbImage,
    quality: u8,
    subsample_420: bool,
) -> Result<Vec<u8>, String> {
    encode_jpeg_moz_opts(rgb, quality, subsample_420, true)
}

fn encode_jpeg_moz_opts(
    rgb: &RgbImage,
    quality: u8,
    subsample_420: bool,
    heavy: bool,
) -> Result<Vec<u8>, String> {
    let (w, h) = rgb.dimensions();
    let raw = rgb.as_raw().to_vec();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        comp.set_size(w as usize, h as usize);
        comp.set_scan_optimization_mode(mozjpeg::ScanMode::AllComponentsTogether);
        comp.set_progressive_mode();
        comp.set_quality(quality as f32);
        comp.set_optimize_coding(true);
        if heavy {
            comp.set_optimize_scans(true);
            comp.set_use_scans_in_trellis(true);
        }
        if subsample_420 {
            comp.set_chroma_sampling_pixel_sizes((2, 2), (2, 2));
        } else {
            comp.set_chroma_sampling_pixel_sizes((1, 1), (1, 1));
        }
        let mut comp = comp
            .start_compress(Vec::new())
            .map_err(|e| format!("MozJPEG start: {e}"))?;
        comp.write_scanlines(&raw)
            .map_err(|e| format!("MozJPEG write: {e}"))?;
        comp.finish()
            .map_err(|e| format!("MozJPEG finish: {e}"))
    }))
    .map_err(|_| "MozJPEG 编码 panic".to_string())?
}

#[derive(Clone, Copy)]
enum EncodeRace {
    /// 低档：只跑 Jpegli（快）；失败才 Moz 轻量回退。
    JpegliFirst,
    /// 中高档：双编码器竞速取小。
    Both,
}

fn encode_jpeg_candidates(
    rgb: &RgbImage,
    quality: u8,
    subsample_420: bool,
    race: EncodeRace,
) -> Vec<(Vec<u8>, &'static str)> {
    let mut out = Vec::with_capacity(2);
    if let Ok(b) = encode_jpeg_jpegli(rgb, quality, subsample_420) {
        out.push((b, "jpegli"));
    }
    match race {
        EncodeRace::JpegliFirst => {
            if out.is_empty() {
                if let Ok(b) = encode_jpeg_moz_opts(rgb, quality, subsample_420, false) {
                    out.push((b, "mozjpeg"));
                }
            }
        }
        EncodeRace::Both => {
            if let Ok(b) = encode_jpeg_moz_opts(rgb, quality, subsample_420, true) {
                out.push((b, "mozjpeg"));
            }
        }
    }
    if out.is_empty() {
        if let Ok(b) = encode_jpeg_fallback(rgb, quality) {
            out.push((b, "image-jpeg"));
        }
    }
    out
}

/// 无损去掉 EXIF/缩略图/注释等体积杂质（不改 DCT 系数，画质不变）。
/// 保留 APP0(JFIF)、APP2(ICC)、APP14(Adobe) 以免色偏。
pub fn jpeg_strip_metadata(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut i = 2usize;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        while i + 1 < data.len() && data[i] == 0xFF && data[i + 1] == 0xFF {
            i += 1;
        }
        if i + 1 >= data.len() {
            break;
        }
        let marker = data[i + 1];
        if marker == 0xD9 {
            out.extend_from_slice(&[0xFF, 0xD9]);
            break;
        }
        if marker == 0xDA {
            out.extend_from_slice(&data[i..]);
            break;
        }
        // RST / TEM：无长度段
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&data[i..i + 2]);
            i += 2;
            continue;
        }
        if i + 3 >= data.len() {
            return None;
        }
        let seglen = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if seglen < 2 {
            return None;
        }
        let end = i + 2 + seglen;
        if end > data.len() {
            return None;
        }
        // 剥离：APP1 EXIF、COM、APP13 Photoshop、以及其它杂 APP（保留 0/2/14）
        let strip = marker == 0xE1
            || marker == 0xFE
            || marker == 0xED
            || (marker >= 0xE3 && marker <= 0xEF && marker != 0xEE);
        if !strip {
            out.extend_from_slice(&data[i..end]);
        }
        i = end;
    }
    if out.len() + 32 < data.len() && out.len() > 64 {
        Some(out)
    } else if out.len() < data.len() && out.len() > 64 {
        Some(out)
    } else {
        None
    }
}

fn unique_qualities(qs: impl IntoIterator<Item = u8>) -> Vec<u8> {
    let mut v: Vec<u8> = qs.into_iter().collect();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v.dedup();
    v
}

/// 收集无损候选（速度优先：只跑非 progressive 熵优化 + strip，禁止 progressive 双试）。
fn jpeg_lossless_candidates(
    data: &[u8],
    intensity: u8,
    original: &RgbImage,
) -> Vec<(Vec<u8>, f64, String)> {
    let mut cands = Vec::new();
    let push_if_pixel_ok = |cands: &mut Vec<(Vec<u8>, f64, String)>, bytes: Vec<u8>, tag: &str| {
        if bytes.len() >= data.len() || bytes.len() <= 64 {
            return;
        }
        let Ok(dec) = decode_to_rgb(&bytes) else {
            return;
        };
        if dec.as_raw() != original.as_raw() {
            return;
        }
        cands.push((
            bytes,
            1.0,
            format!("jpeg/{tag}/i{intensity}+lossless"),
        ));
    };

    // 只用基线熵优化（非 progressive）；jpeg_lossless_best 会双试 progressive，过慢。
    if let Some(slim) = jpeg_strip_metadata(data) {
        push_if_pixel_ok(&mut cands, slim.clone(), "strip-meta");
        if let Some(opt) = jpeg_lossless_optimize_ex(&slim, false) {
            push_if_pixel_ok(&mut cands, opt, "strip+entropy");
        }
    }
    if let Some(opt) = jpeg_lossless_optimize_ex(data, false) {
        push_if_pixel_ok(&mut cands, opt, "entropy-opt");
    }
    cands
}

/// 0 档观感：Jpegli 基线 444；**高质优先 first-fit**（达标即返回，禁止贪最小体积落到发糊 q）。
fn jpeg_visual_zero(
    data: &[u8],
    original: &RgbImage,
) -> Result<(Vec<u8>, f64, String), String> {
    let mut last_err = "力度 0 观感路径无可用结果".to_string();
    for q in ZERO_VISUAL_QUALITIES {
        let Ok(out) = encode_jpeg_jpegli_ex(original, q, false, false) else {
            last_err = format!("力度 0（jpegli/q{q}）编码失败");
            continue;
        };
        if (out.len() as f64) > (data.len() as f64) * VISUAL_MAX_SIZE_RATIO {
            last_err = format!("力度 0（jpegli/q{q}）体积收益不足");
            continue;
        }
        let Ok(decoded) = decode_to_rgb(&out) else {
            continue;
        };
        let ssim = ssim_rgb(original, &decoded).unwrap_or(0.0);
        let hf = high_freq_retain_ratio(original, &decoded);
        let edge = edge_retain_ratio(original, &decoded);
        let psnr = psnr_rgb(original, &decoded);
        if ssim < ZERO_SSIM_MIN {
            last_err = format!("jpegli/q{q} SSIM={ssim:.4}<{ZERO_SSIM_MIN}");
            continue;
        }
        if hf < ZERO_HF_MIN {
            last_err = format!("jpegli/q{q} HF={hf:.3}<{ZERO_HF_MIN}");
            continue;
        }
        if edge < ZERO_EDGE_MIN {
            last_err = format!("jpegli/q{q} edge={edge:.3}<{ZERO_EDGE_MIN}");
            continue;
        }
        if psnr < ZERO_PSNR_MIN {
            last_err = format!("jpegli/q{q} PSNR={psnr:.1}<{ZERO_PSNR_MIN}");
            continue;
        }
        let method = format!(
            "jpegli/q{q}/444/i0+visual/hf{hf:.2}/e{edge:.2}/p{psnr:.0}"
        );
        return Ok((out, ssim, method));
    }
    Err(last_err)
}

fn pick_zero_jpeg(
    data: &[u8],
    _original: &RgbImage,
    lossless: Option<(Vec<u8>, f64, String)>,
    visual: Option<(Vec<u8>, f64, String)>,
) -> Result<(Vec<u8>, f64, String), String> {
    let _ = data;
    match (lossless, visual) {
        (Some(l), Some(v)) => {
            // 高质观感已过锐度门：只要比无损更小就采用（体积优先于「多省 8%」假门槛）
            if v.0.len() < l.0.len() {
                Ok(v)
            } else {
                Ok(l)
            }
        }
        (Some(l), None) => Ok(l),
        (None, Some(v)) => Ok(v),
        (None, None) => Err("力度 0：无损与观感路径均未能在保锐下缩小".into()),
    }
}

/// JPEG：0=锐利无损+观感体积；保真区无损优先；中高档竞速。
pub fn compress_jpeg(data: &[u8], intensity: u8) -> Result<(Vec<u8>, f64, String), String> {
    let intensity = clamp_intensity(intensity);
    let quality = jpeg_quality_from_intensity(intensity).min(96);
    let gate = ssim_min_for_intensity(intensity);
    let hf_gate = hf_min_for_intensity(intensity);
    let fidelity_first = intensity < JPEG_DETAIL_THRESHOLD;
    // T022：全程 Jpegli 优先；Moz 仅作失败回退（禁双跑+trellis 拖慢）
    let race = EncodeRace::JpegliFirst;

    let original = decode_to_rgb(data)?;

    let attempt = |q: u8, use_420: bool, tag: &str| -> Result<(Vec<u8>, f64, String), String> {
        let mut best: Option<(Vec<u8>, f64, f64, &'static str, String)> = None;
        let mut last_err = format!("力度 {intensity} q{q} 无可用编码");

        for (out, eng) in encode_jpeg_candidates(&original, q, use_420, race) {
            if out.len() >= data.len() {
                last_err = format!("力度 {intensity}（{eng} q{q}）未能缩小体积");
                continue;
            }
            let decoded = match decode_to_rgb(&out) {
                Ok(d) => d,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            let ssim = ssim_rgb(&original, &decoded).unwrap_or(0.0);
            if ssim < gate {
                last_err = format!(
                    "力度 {intensity}（{eng} q{q}）SSIM={ssim:.4}<{gate:.3}，未写回"
                );
                continue;
            }
            let hf = high_freq_retain_ratio(&original, &decoded);
            if hf < hf_gate {
                last_err = format!(
                    "力度 {intensity}（{eng} q{q}）高频保留={hf:.3}<{hf_gate:.3}，未写回"
                );
                continue;
            }
            let samp = if use_420 { "420" } else { "444" };
            let method = format!("{eng}/q{q}/{samp}/i{intensity}+{tag}/hf{hf:.2}");
            let take = match &best {
                None => true,
                Some((b_out, _, _, _, _)) => out.len() < b_out.len(),
            };
            if take {
                best = Some((out, ssim, hf, eng, method));
            }
        }

        best.map(|(o, s, _, _, m)| (o, s, m)).ok_or(last_err)
    };

    if intensity == 0 {
        let visual = jpeg_visual_zero(data, &original).ok();
        if let Some(ref v) = visual {
            if (v.0.len() as f64) <= (data.len() as f64) * ZERO_SKIP_LOSSLESS_RATIO {
                return Ok(v.clone());
            }
        }
        let lossless = jpeg_lossless_candidates(data, intensity, &original)
            .into_iter()
            .min_by(|a, b| a.0.len().cmp(&b.0.len()));
        return pick_zero_jpeg(data, &original, lossless, visual);
    }

    if fidelity_first {
        let q_lossy = quality.max(JPEG_FIDELITY_Q_FLOOR).min(96);
        let q2 = q_lossy.saturating_sub(8).max(JPEG_FIDELITY_Q_FLOOR);
        let (lossless_best, perceptual) = std::thread::scope(|s| {
            let l = s.spawn(|| {
                jpeg_lossless_candidates(data, intensity, &original)
                    .into_iter()
                    .min_by(|a, b| a.0.len().cmp(&b.0.len()))
            });
            let a1 = s.spawn(|| attempt(q_lossy, false, "perceptual"));
            let a2 = s.spawn(|| attempt(q2, false, "perceptual"));
            (
                l.join().unwrap_or(None),
                (
                    a1.join().unwrap_or_else(|_| Err("并行试路失败".into())),
                    a2.join().unwrap_or_else(|_| Err("并行试路失败".into())),
                ),
            )
        });
        let mut best = lossless_best;
        let mut last_err = format!("力度 {intensity} 保真路径无可用结果");
        for attempt_res in [perceptual.0, perceptual.1] {
            match attempt_res {
                Ok(v) => {
                    if v.0.len() >= data.len() {
                        continue;
                    }
                    match &best {
                        None => best = Some(v),
                        Some(cur) if v.0.len() < cur.0.len() => best = Some(v),
                        _ => {}
                    }
                }
                Err(e) => last_err = e,
            }
        }
        return best.ok_or(last_err);
    }

    let primary_420 = jpeg_use_420(intensity);
    let q_ref = quality.saturating_sub(12).max(28);
    let do_refine = intensity >= JPEG_SIZE_REFINE_THRESHOLD;

    let (primary_res, refine_res) = std::thread::scope(|s| {
        let p = s.spawn(|| attempt(quality, primary_420, "detail"));
        let r = if do_refine {
            Some(s.spawn(|| attempt(q_ref, true, "sizeRefine")))
        } else {
            None
        };
        let pr = p.join().unwrap_or_else(|_| Err("并行试路失败".into()));
        let rr = r.map(|h| h.join().unwrap_or_else(|_| Err("并行试路失败".into())));
        (pr, rr)
    });

    let mut best = match primary_res {
        Ok(v) => v,
        Err(first) => {
            let mut recovered = Err(first.clone());
            for drop in [8u8, 16, 24] {
                let q2 = quality.saturating_sub(drop).max(28);
                if q2 >= quality {
                    continue;
                }
                if let Ok(v) = attempt(q2, true, "recoverSize") {
                    recovered = Ok(v);
                    break;
                }
            }
            if recovered.is_err() {
                for bump in [6u8, 12] {
                    let q_up = (quality + bump).min(92);
                    if q_up <= quality {
                        continue;
                    }
                    if let Ok(v) = attempt(q_up, true, "recoverQ") {
                        recovered = Ok(v);
                        break;
                    }
                }
            }
            recovered.map_err(|_| first)?
        }
    };

    if let Some(Ok(cand)) = refine_res {
        if cand.0.len() < best.0.len() {
            best = cand;
        }
    }

    Ok(best)
}

fn zopfli_iterations(input_len: usize) -> NonZeroU8 {
    // T022：大幅降低 Zopfli 轮次（旧 2～5 是 PNG 慢的真根因之一）
    let n = if input_len > 1_500_000 { 2 } else { 1 };
    NonZeroU8::new(n).unwrap()
}

fn oxipng_extreme(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut opts = oxipng::Options::max_compression();
    opts.strip = oxipng::StripChunks::All;
    opts.optimize_alpha = true;
    opts.deflate = oxipng::Deflaters::Zopfli {
        iterations: zopfli_iterations(data.len()),
    };
    oxipng::optimize_from_memory(data, &opts).map_err(|e| format!("oxipng 失败: {e}"))
}

/// 高强度路径对比用：preset 级压缩（无 Zopfli），避免「pngquant + 双次 zopfli」卡死 UI。
fn oxipng_compact(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut opts = oxipng::Options::from_preset(4);
    opts.strip = oxipng::StripChunks::All;
    opts.optimize_alpha = true;
    oxipng::optimize_from_memory(data, &opts).map_err(|e| format!("oxipng 失败: {e}"))
}

/// 胜者再跑一遍极限；若无收益则保留 compact 结果。
fn oxipng_finalize(data: &[u8]) -> Vec<u8> {
    match oxipng_extreme(data) {
        Ok(v) if v.len() < data.len() => v,
        _ => data.to_vec(),
    }
}

/// 无损主路径。`light=true`：preset + 轻量 Zopfli（2 轮），兼顾速度与体积。
fn oxipng_best(data: &[u8], light: bool) -> Result<Vec<u8>, String> {
    let compact = oxipng_compact(data)?;
    if light {
        let mut opts = oxipng::Options::from_preset(4);
        opts.strip = oxipng::StripChunks::All;
        opts.optimize_alpha = true;
        opts.deflate = oxipng::Deflaters::Zopfli {
            iterations: NonZeroU8::new(1).unwrap(),
        };
        return match oxipng::optimize_from_memory(&compact, &opts) {
            Ok(v) if v.len() < data.len() => Ok(v),
            Ok(v) if compact.len() < data.len() => Ok(if v.len() < compact.len() {
                v
            } else {
                compact
            }),
            Ok(_) if compact.len() < data.len() => Ok(compact),
            _ => {
                if compact.len() < data.len() {
                    Ok(compact)
                } else {
                    Err("oxipng 未能缩小".into())
                }
            }
        };
    }
    Ok(oxipng_finalize(&compact))
}

/// 直接写索引色 PNG（避免 RGBA 再编码导致体积膨胀）。
fn encode_indexed_png(
    width: u32,
    height: u32,
    palette: &[imagequant::RGBA],
    indices: &[u8],
) -> Result<Vec<u8>, String> {
    let expected = (width as usize).saturating_mul(height as usize);
    if indices.len() != expected {
        return Err(format!(
            "索引长度不符: got {} expect {expected}",
            indices.len()
        ));
    }

    let mut plte = Vec::with_capacity(palette.len() * 3);
    let mut trns = Vec::with_capacity(palette.len());
    let mut has_alpha = false;
    for c in palette {
        plte.extend_from_slice(&[c.r, c.g, c.b]);
        trns.push(c.a);
        if c.a != 255 {
            has_alpha = true;
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Best);
        encoder.set_filter(png::FilterType::Paeth);
        encoder.set_palette(plte);
        if has_alpha {
            encoder.set_trns(trns);
        }
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG header: {e}"))?;
        writer
            .write_image_data(indices)
            .map_err(|e| format!("PNG data: {e}"))?;
    }
    Ok(out)
}

/// 单次 pngquant → 索引色 PNG → oxipng 极限容器优化。
fn quantize_png_once(
    data: &[u8],
    quality_min: u8,
    max_colors: u32,
    dither: f32,
) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(data).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let colors = max_colors.max(2);

    let pixels: Vec<imagequant::RGBA> = rgba
        .pixels()
        .map(|p| imagequant::RGBA {
            r: p[0],
            g: p[1],
            b: p[2],
            a: p[3],
        })
        .collect();

    let run = |qmin: u8, max_cols: u32| -> Result<(Vec<imagequant::RGBA>, Vec<u8>), String> {
        let mut liq = imagequant::new();
        liq.set_speed(4)
            .map_err(|e| format!("imagequant speed: {e}"))?;
        liq.set_quality(qmin, 100)
            .map_err(|e| format!("imagequant quality: {e}"))?;
        liq.set_max_colors(max_cols.max(2))
            .map_err(|e| format!("imagequant colors: {e}"))?;
        let mut img_liq = liq
            .new_image(&pixels[..], w as usize, h as usize, 0.0)
            .map_err(|e| format!("imagequant image: {e}"))?;
        let mut res = liq
            .quantize(&mut img_liq)
            .map_err(|e| format!("imagequant quantize: {e}"))?;
        let _ = res.set_dithering_level(dither.clamp(0.0, 1.0));
        res.remapped(&mut img_liq)
            .map_err(|e| format!("imagequant remap: {e}"))
    };

    // 主尝试 → 256 色 → 阶梯降 qmin（编码器失败恢复；最终由 SSIM 门禁裁决写回）
    let (palette, idxs) = match run(quality_min, colors) {
        Ok(v) => v,
        Err(e) if e.contains("QUALITY_TOO_LOW") => match run(quality_min, 256) {
            Ok(v) => v,
            Err(_) => match run(quality_min.saturating_sub(25).max(20), 256) {
                Ok(v) => v,
                Err(_) => run(0, 256).map_err(|e2| {
                    format!("pngquant 无法在保真下量化（qmin={quality_min}）: {e2}")
                })?,
            },
        },
        Err(e) => return Err(e),
    };

    let encoded = encode_indexed_png(w, h, &palette, &idxs)?;
    oxipng_compact(&encoded).or(Ok(encoded))
}

/// PNG：0 档 = oxipng + 高质 pngquant（过锐度才用）；≥门槛智能取小。
pub fn compress_png(data: &[u8], intensity: u8) -> Result<(Vec<u8>, f64, String), String> {
    let intensity = clamp_intensity(intensity);
    let original = decode_to_rgb(data)?;
    let gate = ssim_min_for_intensity(intensity);

    if intensity == 0 {
        let mut best: Option<(Vec<u8>, f64, String)> = None;
        if let Ok(lossless) = oxipng_best(data, true) {
            if lossless.len() < data.len() {
                best = Some((
                    lossless,
                    1.0,
                    format!("oxipng/zopfli2+strip/i0"),
                ));
            }
        }
        // 仅当无损仍偏大时才量化；量化后用轻量 compact，禁 extreme Zopfli
        let need_quant = best
            .as_ref()
            .map_or(true, |b| (b.0.len() as f64) > (data.len() as f64) * 0.85);
        if need_quant {
            if let Ok(raw_q) = quantize_png_once(data, 95, 256, 0.05) {
                let lossy = oxipng_compact(&raw_q).unwrap_or(raw_q);
                if lossy.len() < data.len() {
                    if let Ok(decoded) = decode_to_rgb(&lossy) {
                        let ssim = ssim_rgb(&original, &decoded).unwrap_or(0.0);
                        let edge = edge_retain_ratio(&original, &decoded);
                        let hf = high_freq_retain_ratio(&original, &decoded);
                        let ok = ssim >= ZERO_SSIM_MIN && edge >= ZERO_EDGE_MIN && hf >= ZERO_HF_MIN;
                        let much_smaller = best.as_ref().map_or(true, |b| lossy.len() < b.0.len())
                            && (lossy.len() as f64) <= (data.len() as f64) * 0.75;
                        if ok && much_smaller {
                            best = Some((
                                lossy,
                                ssim,
                                format!("pngquant/qmin95/c256+oxipng/i0+visual"),
                            ));
                        }
                    }
                }
            }
        }
        return best.ok_or_else(|| {
            "力度 0：未能在保持画质的前提下缩小文件".into()
        });
    }

    if intensity < PNG_LOSSY_THRESHOLD {
        let lossless = oxipng_best(data, true)?;
        if lossless.len() >= data.len() {
            return Err(format!(
                "力度 {intensity}（oxipng 无损）未能缩小——请提高力度启用近无损有损"
            ));
        }
        return Ok((
            lossless,
            1.0,
            format!("oxipng/zopfli2+strip/i{intensity}"),
        ));
    }

    let qmin = pngquant_quality_min(intensity);
    let colors = png_max_colors(intensity);
    let dither = png_dither_level(intensity);

    let (lossless, lossy_raw) = std::thread::scope(|s| {
        let lh = s.spawn(|| oxipng_best(data, false).ok());
        let rh = s.spawn(|| quantize_png_once(data, qmin, colors, dither).ok());
        (
            lh.join().unwrap_or(None),
            rh.join().unwrap_or(None),
        )
    });
    let lossy = lossy_raw.map(|b| oxipng_finalize(&b));

    let mut best: Option<(Vec<u8>, f64, String)> = None;

    if let Some(lossless) = lossless {
        if lossless.len() < data.len() {
            best = Some((
                lossless,
                1.0,
                format!("oxipng/preset+zopfli/i{intensity}"),
            ));
        }
    }

    if let Some(lossy) = lossy {
        if lossy.len() < data.len() {
            let decoded = decode_to_rgb(&lossy)?;
            let ssim = ssim_rgb(&original, &decoded).unwrap_or(0.0);
            if ssim >= gate {
                let better = match &best {
                    Some((cur, _, _)) => lossy.len() < cur.len(),
                    None => true,
                };
                if better {
                    best = Some((
                        lossy,
                        ssim,
                        format!("pngquant/qmin{qmin}/c{colors}+oxipng/i{intensity}"),
                    ));
                }
            }
        }
    }

    best.ok_or_else(|| {
        format!("力度 {intensity}（近无损 pngquant/oxipng）均未能在保真下缩小体积")
    })
}

fn maybe_write_compressed(
    path: &Path,
    input_size: u64,
    format: String,
    out_bytes: Vec<u8>,
    ssim: f64,
    method: String,
    intensity: u8,
) -> Result<CompressResult, String> {
    let gate = ssim_min_for_intensity(intensity);
    if (out_bytes.len() as u64) >= input_size {
        return Ok(skipped_result(
            path,
            input_size,
            format,
            format!("{method}(无变小)"),
            Some(ssim),
            intensity,
        ));
    }
    if ssim < gate {
        return Ok(skipped_result(
            path,
            input_size,
            format,
            format!("{method}(保真跳过 SSIM={ssim:.4}<{gate:.3})"),
            Some(ssim),
            intensity,
        ));
    }
    fs::write(path, &out_bytes).map_err(|e| format!("写入失败: {e}"))?;
    Ok(CompressResult {
        path: path.display().to_string(),
        input_size,
        output_size: out_bytes.len() as u64,
        saved_bytes: input_size as i64 - out_bytes.len() as i64,
        format,
        skipped: false,
        method,
        ssim: Some(ssim),
        intensity,
    })
}

fn skipped_result(
    path: &Path,
    input_size: u64,
    format: String,
    method: String,
    ssim: Option<f64>,
    intensity: u8,
) -> CompressResult {
    CompressResult {
        path: path.display().to_string(),
        input_size,
        output_size: input_size,
        saved_bytes: 0,
        format,
        skipped: true,
        method,
        ssim,
        intensity,
    }
}

/// 压缩单个文件并覆盖原路径（调用方须已确认）。
pub fn compress_file(path: &str, intensity: u8) -> Result<CompressResult, String> {
    let intensity = clamp_intensity(intensity);
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(format!("文件不存在: {}", path.display()));
    }

    let input_size = fs::metadata(&path)
        .map_err(|e| format!("读取元数据失败: {e}"))?
        .len();
    let format = ext_of(&path);
    let data = fs::read(&path).map_err(|e| format!("读取文件失败: {e}"))?;

    match format.as_str() {
        "jpg" | "jpeg" => match compress_jpeg(&data, intensity) {
            Ok((out_bytes, ssim, method)) => {
                maybe_write_compressed(&path, input_size, format, out_bytes, ssim, method, intensity)
            }
            Err(e) => Ok(skipped_result(&path, input_size, format, e, None, intensity)),
        },
        "png" => match compress_png(&data, intensity) {
            Ok((out_bytes, ssim, method)) => {
                maybe_write_compressed(&path, input_size, format, out_bytes, ssim, method, intensity)
            }
            Err(e) => Ok(skipped_result(&path, input_size, format, e, None, intensity)),
        },
        "webp" => match crate::formats_extra::compress_webp(&data, intensity) {
            Ok((out_bytes, ssim, method)) => {
                maybe_write_compressed(&path, input_size, format, out_bytes, ssim, method, intensity)
            }
            Err(e) => Ok(skipped_result(&path, input_size, format, e, None, intensity)),
        },
        "gif" => match crate::formats_extra::compress_gif(&data, intensity) {
            Ok((out_bytes, ssim, method)) => {
                maybe_write_compressed(&path, input_size, format, out_bytes, ssim, method, intensity)
            }
            Err(e) => Ok(skipped_result(&path, input_size, format, e, None, intensity)),
        },
        "bmp" => match crate::formats_extra::compress_raster_same_ext(&data, intensity, "bmp") {
            Ok((out_bytes, ssim, method)) => {
                maybe_write_compressed(&path, input_size, format, out_bytes, ssim, method, intensity)
            }
            Err(e) => Ok(skipped_result(&path, input_size, format, e, None, intensity)),
        },
        "tif" | "tiff" => {
            match crate::formats_extra::compress_raster_same_ext(&data, intensity, "tiff") {
                Ok((out_bytes, ssim, method)) => maybe_write_compressed(
                    &path, input_size, format, out_bytes, ssim, method, intensity,
                ),
                Err(e) => Ok(skipped_result(&path, input_size, format, e, None, intensity)),
            }
        }
        "ico" => match crate::formats_extra::compress_raster_same_ext(&data, intensity, "ico") {
            Ok((out_bytes, ssim, method)) => {
                maybe_write_compressed(&path, input_size, format, out_bytes, ssim, method, intensity)
            }
            Err(e) => Ok(skipped_result(&path, input_size, format, e, None, intensity)),
        },
        other => Ok(skipped_result(
            &path,
            input_size,
            format.clone(),
            format!("不支持 .{other}（支持 jpg/jpeg/png/webp/gif/bmp/tif/tiff/ico）"),
            None,
            intensity,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgb, RgbImage};

    fn sample_jpeg_photo() -> Vec<u8> {
        // 照片型内容（非纯噪声），避免 SSIM 被高频噪声无谓打穿
        let img = photo_rgb(160, 120);
        encode_jpeg_fallback(&img, 92).unwrap()
    }

    /// 偏「相机 PNG」：平滑色块+渐变、弱压缩；避免高频噪声导致 pngquant 无法达标。
    fn sample_png_photo() -> Vec<u8> {
        let img: RgbImage = ImageBuffer::from_fn(128, 128, |x, y| {
            let xf = x as f64 / 128.0;
            let yf = y as f64 / 128.0;
            let band = ((y / 16) * 28) as u8;
            Rgb([
                (40.0 + xf * 140.0 + yf * 30.0) as u8,
                (60.0 + (1.0 - xf) * 100.0 + band as f64 * 0.35) as u8,
                (90.0 + yf * 120.0 + ((x / 8) as f64) * 6.0) as u8,
            ])
        });
        let mut buf = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut buf, 128, 128);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            encoder.set_filter(png::FilterType::NoFilter);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(img.as_raw()).unwrap();
        }
        buf
    }

    #[test]
    fn intensity_maps_monotonic() {
        assert_eq!(jpeg_quality_from_intensity(0), 100);
        assert_eq!(jpeg_quality_from_intensity(100), 42);
        assert_eq!(jpeg_quality_from_intensity(34), 75);
        assert_eq!(quality_percent_from_intensity(DEFAULT_INTENSITY), 100);
        assert!(jpeg_quality_from_intensity(20) > jpeg_quality_from_intensity(80));
        assert!(!jpeg_use_420(0), "0档须 4:4:4/无损");
        assert!(!jpeg_use_420(24));
        assert!(jpeg_use_420(25));
        assert_eq!(DEFAULT_INTENSITY, 0);
        assert!(ssim_min_for_intensity(0) >= 0.994);
        assert!((hf_min_for_intensity(0) - 0.95).abs() < 1e-9);
        assert!((hf_min_for_intensity(100) - 0.50).abs() < 1e-9);
        assert!(ssim_min_for_intensity(100) <= 0.91);
    }

    #[test]
    fn jpeg_strip_metadata_removes_exif_without_pixel_change() {
        let img = photo_rgb(64, 48);
        let core = encode_jpeg_fallback(&img, 92).unwrap();
        let mut with_exif = vec![0xFFu8, 0xD8];
        let payload = b"Exif\0\0fake-thumbnail-padding-xxxxxxxxxxxxxxxx";
        let seglen = (payload.len() + 2) as u16;
        with_exif.extend_from_slice(&[0xFF, 0xE1]);
        with_exif.extend_from_slice(&seglen.to_be_bytes());
        with_exif.extend_from_slice(payload);
        with_exif.extend_from_slice(&core[2..]);

        let slim = jpeg_strip_metadata(&with_exif).expect("strip");
        assert!(slim.len() < with_exif.len());
        let a = decode_to_rgb(&core).unwrap();
        let b = decode_to_rgb(&slim).unwrap();
        assert_eq!(a.dimensions(), b.dimensions());
        assert_eq!(a.as_raw(), b.as_raw(), "去元数据不得改像素");
    }

    #[test]
    fn probe_visual_gate_ladder() {
        let img = photo_rgb(960, 540);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let orig = decode_to_rgb(&raw).unwrap();
        println!("src={}", raw.len());
        for q in ZERO_VISUAL_QUALITIES {
            let out = encode_jpeg_jpegli_ex(&orig, q, false, false).unwrap();
            let dec = decode_to_rgb(&out).unwrap();
            let ssim = ssim_rgb(&orig, &dec).unwrap_or(0.0);
            let hf = high_freq_retain_ratio(&orig, &dec);
            let edge = edge_retain_ratio(&orig, &dec);
            let psnr = psnr_rgb(&orig, &dec);
            let ratio = out.len() as f64 / raw.len() as f64;
            println!(
                "q{q} out={} ratio={:.3} ssim={:.4} hf={:.3} edge={:.3} psnr={:.1} size_ok={} gates_ok={}",
                out.len(),
                ratio,
                ssim,
                hf,
                edge,
                psnr,
                ratio <= VISUAL_MAX_SIZE_RATIO,
                ssim >= ZERO_SSIM_MIN
                    && hf >= ZERO_HF_MIN
                    && edge >= ZERO_EDGE_MIN
                    && psnr >= ZERO_PSNR_MIN
            );
        }
    }

    /// 逻辑1：未极压大图 → 高质观感（禁 q≤82 发糊），且明显压小。
    #[test]
    fn t020_logic1_large_photo_highq_visual() {
        let img = photo_rgb(1920, 1080);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let orig = decode_to_rgb(&raw).unwrap();
        let (out, ssim, method) = compress_jpeg(&raw, 0).expect("i0 large");
        let dec = decode_to_rgb(&out).unwrap();
        let ratio = out.len() as f64 / raw.len() as f64;
        let hf = high_freq_retain_ratio(&orig, &dec);
        println!(
            "L1 LARGE out={} ratio={:.3} {method} ssim={ssim:.4} hf={hf:.3}",
            out.len(),
            ratio
        );
        assert!(method.contains("visual"), "须观感: {method}");
        assert!(
            !method.contains("/q74/")
                && !method.contains("/q78/")
                && !method.contains("/q82/"),
            "0档禁止低q发糊: {method}"
        );
        assert!(ratio <= VISUAL_MAX_SIZE_RATIO, "ratio={ratio}");
        assert!(ssim >= ZERO_SSIM_MIN);
        assert!(hf >= ZERO_HF_MIN);
        assert!(edge_retain_ratio(&orig, &dec) >= ZERO_EDGE_MIN);
        assert!(!method.contains("/420/"));
    }

    /// T021 逻辑A：用户样张 i10 体积须明显小于 i0（修保真区锁死无损反胀）。
    #[test]
    fn t021_logic_a_fidelity_zone_beats_zero_on_compact() {
        let user_path = r"C:\Users\ASUS\.cursor\projects\d-AI-C-tinyImage\assets\c__Users_ASUS_AppData_Roaming_Cursor_User_workspaceStorage_88733b4a7d69dcfd459f54f8ded7846c_images_ss_08af4e9398b8e45152bfbedce3bc24d22e2c0990.1920x1080__1_-5f9124da-ce77-4864-9536-fd0e8a2f8648.png";
        let Ok(user) = fs::read(user_path) else {
            return;
        };
        let (a, _, ma) = compress_jpeg(&user, 0).expect("i0");
        let (b, _, mb) = compress_jpeg(&user, 10).expect("i10");
        println!("A i0={} {ma}; i10={} {mb}", a.len(), b.len());
        assert!(
            b.len() < a.len(),
            "i10 不得反胀/锁死无损大于 i0: i10={} i0={} ({mb} vs {ma})",
            b.len(),
            a.len()
        );
        assert!(
            !mb.contains("lossless") || b.len() <= a.len(),
            "保真区不应仅靠无损且更大"
        );
    }

    /// T021 逻辑B：用户样张 i100 必须成功且远小于 i0。
    #[test]
    fn t021_logic_b_max_intensity_succeeds_on_compact() {
        let user_path = r"C:\Users\ASUS\.cursor\projects\d-AI-C-tinyImage\assets\c__Users_ASUS_AppData_Roaming_Cursor_User_workspaceStorage_88733b4a7d69dcfd459f54f8ded7846c_images_ss_08af4e9398b8e45152bfbedce3bc24d22e2c0990.1920x1080__1_-5f9124da-ce77-4864-9536-fd0e8a2f8648.png";
        let Ok(user) = fs::read(user_path) else {
            return;
        };
        let (a, _, _) = compress_jpeg(&user, 0).expect("i0");
        let (b, sb, mb) = compress_jpeg(&user, 100).expect("i100 must work");
        println!("B i0={} i100={} r={:.3} {mb} ssim={sb:.4}", a.len(), b.len(), b.len() as f64 / user.len() as f64);
        assert!(b.len() < a.len());
        assert!(
            (b.len() as f64) < (user.len() as f64) * 0.55,
            "极档须大力度压小: {} {mb}",
            b.len()
        );
        assert!(sb >= ssim_min_for_intensity(100));
    }

    /// T021 逻辑C：大图滑条单调 i0 > i34 > i100。
    #[test]
    fn t021_logic_c_large_slider_monotonic() {
        let img = photo_rgb(1280, 720);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let (a, _, ma) = compress_jpeg(&raw, 0).unwrap();
        let (b, _, mb) = compress_jpeg(&raw, 34).unwrap();
        let (c, _, mc) = compress_jpeg(&raw, 100).unwrap();
        println!("C i0={} {ma}; i34={} {mb}; i100={} {mc}", a.len(), b.len(), c.len());
        assert!(b.len() < a.len(), "i34 < i0");
        assert!(c.len() < b.len(), "i100 < i34");
        assert!((c.len() as f64) < (raw.len() as f64) * 0.35);
    }

    /// T021 逻辑D：0 档仍高质（禁低 q），SSIM 门槛。
    #[test]
    fn t021_logic_d_zero_stays_highq() {
        let img = photo_rgb(960, 540);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let (out, ssim, method) = compress_jpeg(&raw, 0).unwrap();
        assert!(out.len() < raw.len());
        if method.contains("visual") {
            assert!(ssim >= ZERO_SSIM_MIN);
            assert!(!method.contains("/q74/") && !method.contains("/q78/"));
        }
    }

    /// T021 逻辑E：保真区有损可小于无损（不再 0.85 锁）。
    #[test]
    fn t021_logic_e_fidelity_can_pick_lossy() {
        let img = photo_rgb(800, 600);
        let raw = encode_jpeg_fallback(&img, 93).unwrap();
        let (out, _, method) = compress_jpeg(&raw, 15).expect("i15");
        println!("E i15={} {method}", out.len());
        assert!(out.len() < raw.len());
        assert!(
            method.contains("perceptual") || method.contains("lossless"),
            "{method}"
        );
        assert!(
            (out.len() as f64) < (raw.len() as f64) * 0.75 || method.contains("perceptual"),
            "保真区应允许有损压小: r={:.3} {method}",
            out.len() as f64 / raw.len() as f64
        );
    }

    /// T022 逻辑1：0 档大图压缩须明显快于「重路径预算」（单张上限）。
    #[test]
    fn t022_logic1_zero_jpeg_fast_enough() {
        use std::time::Instant;
        let img = photo_rgb(1280, 720);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let t0 = Instant::now();
        let (out, ssim, method) = compress_jpeg(&raw, 0).expect("i0");
        let ms = t0.elapsed().as_millis();
        println!("T022-1 {ms}ms out={} {method} ssim={ssim:.4}", out.len());
        assert!(out.len() < raw.len());
        assert!(ssim >= ZERO_SSIM_MIN || method.contains("lossless"));
        assert!(
            ms < 12_000,
            "0档单张过慢: {ms}ms（加速目标 <12s）{method}"
        );
        assert_eq!(ZERO_VISUAL_QUALITIES.len(), 4, "阶梯须缩短");
    }

    /// T022 逻辑2：中高档不得默认双跑 Moz trellis（方法串以 jpegli 为主）。
    #[test]
    fn t022_logic2_high_intensity_jpegli_first() {
        let img = photo_rgb(640, 480);
        let raw = encode_jpeg_fallback(&img, 90).unwrap();
        let (out, _, method) = compress_jpeg(&raw, 80).expect("i80");
        println!("T022-2 {method} out={}", out.len());
        assert!(out.len() < raw.len());
        assert!(
            method.starts_with("jpegli/") || method.contains("lossless"),
            "加速后应以 Jpegli 为主: {method}"
        );
    }

    /// T022 逻辑3：窗口默认尺寸 = 最小常态尺寸。
    #[test]
    fn t022_logic3_window_default_equals_min() {
        let conf = include_str!("../tauri.conf.json");
        assert!(
            conf.contains("\"width\": 360") && conf.contains("\"height\": 360"),
            "默认窗口须为 360x360"
        );
        assert!(
            conf.contains("\"minWidth\": 360") && conf.contains("\"minHeight\": 360"),
            "最小窗口须为 360x360"
        );
    }

    /// T022 逻辑4：面向用户文案禁止「学坎」等内部黑话。
    #[test]
    fn t022_logic4_user_facing_copy_no_jargon() {
        let html = include_str!("../../index.html");
        assert!(!html.contains("学坎"), "UI 禁止学坎");
        assert!(!html.contains("Jpegli"), "副标题勿暴露编码器名");
        assert!(html.contains("intensity-hint"));
        let hint = html
            .split("intensity-hint")
            .nth(1)
            .unwrap_or("")
            .split('<')
            .next()
            .unwrap_or("");
        assert!(
            hint.contains("画质") || hint.contains("体积"),
            "提示须面向用户: {hint}"
        );
    }

    /// T037：图钉置顶（alwaysOnTop）已接线：右上角 SVG 图标 + 权限 + 前端 API。
    #[test]
    fn t037_pin_always_on_top_wired() {
        let html = include_str!("../../index.html");
        assert!(html.contains("btn-pin"), "须有图钉按钮");
        assert!(html.contains("pin-icon"), "图钉须为右上角图标");
        assert!(html.contains("pin-svg"), "图钉须为 SVG 图标");
        assert!(html.contains("aria-label=\"窗口置顶\""), "须有置顶无障碍标签");
        assert!(
            !html.contains("图钉置顶"),
            "禁止底部文字图钉按钮"
        );
        let main_ts = include_str!("../../src/main.ts");
        assert!(main_ts.contains("setAlwaysOnTop"), "须调用置顶 API");
        assert!(main_ts.contains("alwaysOnTop"), "须持久化置顶状态");
        let cap = include_str!("../capabilities/default.json");
        assert!(
            cap.contains("allow-set-always-on-top"),
            "须授权 setAlwaysOnTop"
        );
    }

    /// T022 逻辑5：加速后滑条体积仍单调，0 档仍保锐。
    #[test]
    fn t022_logic5_speed_keeps_quality_and_slider() {
        let img = photo_rgb(960, 540);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let (a, sa, ma) = compress_jpeg(&raw, 0).unwrap();
        let (b, _, mb) = compress_jpeg(&raw, 50).unwrap();
        let (c, _, mc) = compress_jpeg(&raw, 100).unwrap();
        println!("T022-5 i0={} {ma}; i50={} {mb}; i100={} {mc}", a.len(), b.len(), c.len());
        assert!(b.len() < a.len());
        assert!(c.len() <= b.len() + 256);
        if ma.contains("visual") {
            assert!(sa >= ZERO_SSIM_MIN);
            assert!(!ma.contains("/q74/"));
        }
    }

    /// 逻辑2：用户聊天样张（已较压）→ 高质有损或像素无损；禁止糊门禁下的狠压。
    #[test]
    fn t020_logic2_user_sample_no_mush() {
        let user_path = r"C:\Users\ASUS\.cursor\projects\d-AI-C-tinyImage\assets\c__Users_ASUS_AppData_Roaming_Cursor_User_workspaceStorage_88733b4a7d69dcfd459f54f8ded7846c_images_ss_08af4e9398b8e45152bfbedce3bc24d22e2c0990.1920x1080__1_-5f9124da-ce77-4864-9536-fd0e8a2f8648.png";
        let Ok(user) = fs::read(user_path) else {
            println!("skip L2: no sample");
            return;
        };
        let o = decode_to_rgb(&user).unwrap();
        let (out, ssim, method) = compress_jpeg(&user, 0).expect("user i0");
        let d = decode_to_rgb(&out).unwrap();
        let ratio = out.len() as f64 / user.len() as f64;
        let hf = high_freq_retain_ratio(&o, &d);
        println!("L2 USER ratio={ratio:.3} {method} ssim={ssim:.4} hf={hf:.3}");
        assert!(out.len() < user.len());
        assert!(
            !method.contains("/q74/")
                && !method.contains("/q78/")
                && !method.contains("/q82/"),
            "禁止低q: {method}"
        );
        if method.contains("lossless") {
            assert_eq!(o.as_raw(), d.as_raw());
        } else {
            assert!(method.contains("visual"));
            assert!(ssim >= ZERO_SSIM_MIN);
            assert!(hf >= ZERO_HF_MIN);
            assert!(edge_retain_ratio(&o, &d) >= ZERO_EDGE_MIN);
            assert!(psnr_rgb(&o, &d) >= ZERO_PSNR_MIN);
        }
    }

    /// 逻辑3：图一图二字节全等 → 对比噪声假证据（非压缩引入）。
    #[test]
    fn t020_logic3_chat_before_after_identical() {
        let p1 = r"C:\Users\ASUS\.cursor\projects\d-AI-C-tinyImage\assets\c__Users_ASUS_AppData_Roaming_Cursor_User_workspaceStorage_88733b4a7d69dcfd459f54f8ded7846c_images_ss_08af4e9398b8e45152bfbedce3bc24d22e2c0990.1920x1080__1_-5f9124da-ce77-4864-9536-fd0e8a2f8648.png";
        let p2 = r"C:\Users\ASUS\.cursor\projects\d-AI-C-tinyImage\assets\c__Users_ASUS_AppData_Roaming_Cursor_User_workspaceStorage_88733b4a7d69dcfd459f54f8ded7846c_images_ss_08af4e9398b8e45152bfbedce3bc24d22e2c0990.1920x1080__2__-___-6e7c3900-c5b3-4ce1-8376-199907e12e87.png";
        let Ok(a) = fs::read(p1) else {
            return;
        };
        let Ok(b) = fs::read(p2) else {
            return;
        };
        assert_eq!(a, b, "聊天前后图字节须全等——假对比证据");
        assert_eq!(&a[0..2], &[0xFF, 0xD8], "实为 JPEG");
    }

    /// 逻辑4：0 档 first-fit 不得比「贪最小体积」更糊（同图 q90 锐度 ≥ 旧 q74 路径）。
    #[test]
    fn t020_logic4_first_fit_sharper_than_greedy_min() {
        let img = photo_rgb(1280, 720);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let orig = decode_to_rgb(&raw).unwrap();
        let (out, ssim, method) = compress_jpeg(&raw, 0).expect("i0");
        let _dec = decode_to_rgb(&out).unwrap();
        let mush = encode_jpeg_jpegli_ex(&orig, 74, false, false).unwrap();
        let mush_dec = decode_to_rgb(&mush).unwrap();
        let mush_ssim = ssim_rgb(&orig, &mush_dec).unwrap_or(0.0);
        println!("L4 {method} ssim={ssim:.4} vs q74 ssim={mush_ssim:.4}");
        assert!(
            ssim + 0.001 >= mush_ssim,
            "first-fit 不得糊过贪心 q74: {ssim} vs {mush_ssim}"
        );
        assert!(
            method.contains("lossless") || !method.contains("/q74/"),
            "{method}"
        );
    }

    /// 逻辑5：力度单调——i100 体积 < i0；且 i0 不过度牺牲锐度。
    #[test]
    fn t020_logic5_intensity_monotonic_and_zero_sharp() {
        let raw = sample_jpeg_photo();
        let orig = decode_to_rgb(&raw).unwrap();
        let (a, sa, ma) = compress_jpeg(&raw, 0).expect("i0");
        let (b, _sb, mb) = compress_jpeg(&raw, 100).expect("i100");
        let da = decode_to_rgb(&a).unwrap();
        println!("L5 i0={} {ma} ssim={sa:.4}; i100={} {mb}", a.len(), b.len());
        assert!(b.len() < a.len(), "i100 须更小");
        assert!(a.len() < raw.len());
        if ma.contains("visual") {
            assert!(sa >= ZERO_SSIM_MIN);
            assert!(high_freq_retain_ratio(&orig, &da) >= ZERO_HF_MIN);
        }
    }

    /// 诊断汇总（兼容旧名）。
    #[test]
    fn diagnose_zero_on_photo_and_user_sample() {
        t020_logic1_large_photo_highq_visual();
        t020_logic2_user_sample_no_mush();
    }

    #[test]
    fn jpeg_intensity_zero_sharp_and_meaningful_volume() {
        let img: RgbImage = ImageBuffer::from_fn(192, 192, |x, y| {
            let line = if (x + y * 2) % 7 == 0 { 40u8 } else { 0 };
            let tex = ((x.wrapping_mul(3) ^ y.wrapping_mul(5)) % 18) as u8;
            let base = 90u8.saturating_add((x / 8) as u8);
            Rgb([
                base.saturating_add(tex).saturating_add(line / 2),
                base.wrapping_add(25).saturating_add(tex / 2),
                160u8.saturating_sub(tex).saturating_add(line),
            ])
        });
        let raw = encode_jpeg_fallback(&img, 90).unwrap();
        let original = decode_to_rgb(&raw).unwrap();
        let (out, ssim, method) = compress_jpeg(&raw, 0).expect("jpeg i0");
        let decoded = decode_to_rgb(&out).unwrap();
        let hf = high_freq_retain_ratio(&original, &decoded);
        let edge = edge_retain_ratio(&original, &decoded);
        let psnr = psnr_rgb(&original, &decoded);

        assert!(out.len() < raw.len(), "0档须压小: {method}");
        assert!(!method.contains("/420/"), "0档禁 420: {method}");
        let saved = 1.0 - (out.len() as f64 / raw.len() as f64);
        assert!(
            saved >= 0.12 || method.contains("lossless"),
            "0档体积收益过低 saved={saved:.3} {method}"
        );

        if method.contains("lossless") {
            assert_eq!(original.as_raw(), decoded.as_raw(), "无损须像素全等");
            assert!((ssim - 1.0).abs() < 1e-9);
        } else {
            assert!(
                method.contains("jpegli") && method.contains("visual"),
                "非无损须走观感 Jpegli: {method}"
            );
            assert!(ssim >= ZERO_SSIM_MIN, "ssim={ssim}");
            assert!(hf >= ZERO_HF_MIN, "hf={hf}");
            assert!(edge >= ZERO_EDGE_MIN, "edge={edge}");
            assert!(psnr >= ZERO_PSNR_MIN, "psnr={psnr}");
            assert!(
                (out.len() as f64) <= (raw.len() as f64) * VISUAL_MAX_SIZE_RATIO,
                "观感路径须显著压小"
            );
        }
    }

    #[test]
    fn jpeg_extreme_volume_at_100() {
        let raw = sample_jpeg_photo();
        let (a, _, ma) = compress_jpeg(&raw, 0).expect("i0");
        let (b, sb, mb) = compress_jpeg(&raw, 100).expect("i100");
        assert!(
            b.len() < a.len(),
            "极致档须更小: i100={} i0={} ({mb} vs {ma})",
            b.len(),
            a.len()
        );
        assert!(
            (b.len() as f64) < (raw.len() as f64) * 0.70,
            "极致档须大幅压小: out={} raw={} {mb}",
            b.len(),
            raw.len()
        );
        assert!(sb >= ssim_min_for_intensity(100), "ssim={sb}");
        assert!(mb.contains("/420/") || mb.contains("sizeRefine"), "{mb}");
    }

    #[test]
    fn jpeg_single_pass_shrinks() {
        let raw = sample_jpeg_photo();
        let (out, ssim, method) = compress_jpeg(&raw, DEFAULT_INTENSITY).expect("jpeg");
        assert!(out.len() < raw.len(), "method={method}");
        assert!(
            method.contains("lossless")
                || method.starts_with("jpegli/")
                || method.starts_with("mozjpeg/"),
            "主路径须无损或 Jpegli/Moz，method={method}"
        );
        assert!(
            ssim >= ssim_min_for_intensity(DEFAULT_INTENSITY),
            "默认档须过当档保真门禁 ssim={ssim} method={method}"
        );
    }

    #[test]
    fn png_smart_nearlossless_shrinks_at_zero() {
        let raw = sample_png_photo();
        let (out0, s0, m0) = compress_png(&raw, 0).expect("png 0");
        assert!(out0.len() < raw.len(), "{m0}");
        assert!(m0.starts_with("oxipng"), "0档 PNG 须无损: {m0}");
        assert!((s0 - 1.0).abs() < 1e-6);

        let (out80, s80, m80) = compress_png(&raw, 80).expect("png 80");
        assert!(out80.len() < raw.len(), "{m80}");
        assert!(s80 >= ssim_min_for_intensity(80), "s80={s80}");
        assert!(
            out80.len() <= out0.len() + 64,
            "推高不得明显变大: {} vs {} ({m80})",
            out80.len(),
            out0.len()
        );
    }

    #[test]
    fn jpeg_slider_changes_size() {
        let raw = sample_jpeg_photo();
        let (a, _, _) = compress_jpeg(&raw, 20).expect("i20");
        let (b, _, _) = compress_jpeg(&raw, 85).expect("i85");
        assert!(
            b.len() < a.len(),
            "推高滑条须更小: i85={} i20={}",
            b.len(),
            a.len()
        );
    }

    /// T021：10 维力度阶梯诊断（大图 + 用户样张 + 二次压缩）。
    #[test]
    fn t021_ten_dim_intensity_ladder() {
        let img = photo_rgb(1280, 720);
        let large = encode_jpeg_fallback(&img, 92).unwrap();
        let user_path = r"C:\Users\ASUS\.cursor\projects\d-AI-C-tinyImage\assets\c__Users_ASUS_AppData_Roaming_Cursor_User_workspaceStorage_88733b4a7d69dcfd459f54f8ded7846c_images_ss_08af4e9398b8e45152bfbedce3bc24d22e2c0990.1920x1080__1_-5f9124da-ce77-4864-9536-fd0e8a2f8648.png";
        let user = fs::read(user_path).ok();
        println!(
            "DIM qmap0={} q34={} q50={} q100={} large={}",
            jpeg_quality_from_intensity(0),
            jpeg_quality_from_intensity(34),
            jpeg_quality_from_intensity(50),
            jpeg_quality_from_intensity(100),
            large.len()
        );
        for i in [0u8, 10, 20, 24, 25, 34, 50, 75, 100] {
            let (out, ssim, method) = compress_jpeg(&large, i).expect("large");
            let r = out.len() as f64 / large.len() as f64;
            println!(
                "DIM L i={i:>3} q={} r={r:.3} ssim={ssim:.4} {method}",
                jpeg_quality_from_intensity(i)
            );
        }
        if let Some(user) = user.as_ref() {
            for i in [0u8, 10, 25, 50, 75, 100] {
                match compress_jpeg(user, i) {
                    Ok((out, ssim, method)) => {
                        let r = out.len() as f64 / user.len() as f64;
                        println!(
                            "DIM U i={i:>3} q={} r={r:.3} ssim={ssim:.4} {method}",
                            jpeg_quality_from_intensity(i)
                        );
                    }
                    Err(e) => println!("DIM U i={i} ERR {e}"),
                }
            }
            let (once, _, m0) = compress_jpeg(user, 0).unwrap();
            match compress_jpeg(&once, 50) {
                Ok((twice, _, m)) => println!(
                    "DIM RECOMP i0={} {m0} -> i50 r={:.3} {m}",
                    once.len(),
                    twice.len() as f64 / once.len() as f64
                ),
                Err(e) => println!("DIM RECOMP ERR {e}"),
            }
        }
    }

    #[test]
    fn t005b_webp_can_shrink_same_ext() {
        let rgb = photo_rgb(96, 72);
        let mut raw = Vec::new();
        // 故意用高质有损 WebP 造「可再压」样本：再走无损/近无损应能变小或至少可解码路径通
        {
            let enc = webp::Encoder::from_rgb(rgb.as_raw(), 96, 72);
            let out = enc.encode(92.0);
            raw.extend_from_slice(&out);
        }
        // 若有损本身已很小，补一份未压缩 BMP→再转 webp 较难；改为用更大图+更高q
        if raw.len() < 800 {
            let big = photo_rgb(240, 180);
            let enc = webp::Encoder::from_rgb(big.as_raw(), 240, 180);
            raw = enc.encode(95.0).to_vec();
        }
        let path = std::env::temp_dir().join("tinyimage_t005b.webp");
        fs::write(&path, &raw).unwrap();
        let res = compress_file(path.to_str().unwrap(), 40).unwrap();
        // 允许 skip（已极压），但不得再报「T005b 未支持」
        assert!(!res.method.contains("T005b"));
        assert!(!res.method.contains("暂仅支持"));
        if !res.skipped {
            assert!(res.output_size < res.input_size);
            assert!(res.method.contains("webp"));
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn t005b_bmp_reencode_can_shrink() {
        let rgb = photo_rgb(64, 48);
        let mut buf = Vec::new();
        {
            let enc = image::codecs::bmp::BmpEncoder::new(&mut buf);
            enc.write_image(rgb.as_raw(), 64, 48, ExtendedColorType::Rgb8)
                .unwrap();
        }
        let path = std::env::temp_dir().join("tinyimage_t005b.bmp");
        fs::write(&path, &buf).unwrap();
        let res = compress_file(path.to_str().unwrap(), 40).unwrap();
        assert!(!res.skipped, "BMP uncompressed should shrink: {}", res.method);
        assert!(res.output_size < res.input_size);
        assert!(res.method.contains("bmp"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn t005b_gif_repack_path() {
        let rgb = photo_rgb(48, 36);
        let mut buf = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut buf);
            enc.encode_frame(image::Frame::new(image::RgbaImage::from_fn(48, 36, |x, y| {
                let p = rgb.get_pixel(x, y);
                image::Rgba([p[0], p[1], p[2], 255])
            })))
            .unwrap();
        }
        let path = std::env::temp_dir().join("tinyimage_t005b.gif");
        fs::write(&path, &buf).unwrap();
        let res = compress_file(path.to_str().unwrap(), 50).unwrap();
        assert!(!res.method.contains("暂仅支持"));
        assert!(!res.method.contains("T005b"));
        let _ = fs::remove_file(&path);
    }

    fn photo_rgb(w: u32, h: u32) -> RgbImage {
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

    fn moz_variant(rgb: &RgbImage, quality: u8, progressive: bool, subsample_420: bool) -> usize {
        let (w, h) = rgb.dimensions();
        let raw = rgb.as_raw().to_vec();
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
            comp.set_size(w as usize, h as usize);
            comp.set_scan_optimization_mode(mozjpeg::ScanMode::AllComponentsTogether);
            if progressive {
                comp.set_progressive_mode();
            }
            comp.set_quality(quality as f32);
            comp.set_optimize_coding(true);
            comp.set_optimize_scans(true);
            comp.set_use_scans_in_trellis(true);
            if subsample_420 {
                comp.set_chroma_sampling_pixel_sizes((2, 2), (2, 2));
            } else {
                comp.set_chroma_sampling_pixel_sizes((1, 1), (1, 1));
            }
            let mut comp = comp.start_compress(Vec::new()).unwrap();
            comp.write_scanlines(&raw).unwrap();
            comp.finish().unwrap()
        }))
        .unwrap();
        out.len()
    }

    fn pct(from: usize, to: usize) -> f64 {
        if from == 0 {
            0.0
        } else {
            (1.0 - to as f64 / from as f64) * 100.0
        }
    }

    /// 体积余量实测。运行：
    /// cargo test --manifest-path src-tauri/Cargo.toml size_headroom_benchmark -- --ignored --nocapture
    #[test]
    #[ignore]
    fn size_headroom_benchmark() {
        let rgb = photo_rgb(640, 480);
        let jpeg_src = encode_jpeg_fallback(&rgb, 92).unwrap();
        // 造一张较小的「难压」PNG，避免 bench 在 Zopfli 上挂死
        let mut png_photo = Vec::new();
        {
            let img: image::RgbaImage = ImageBuffer::from_fn(120, 90, |x, y| {
                image::Rgba([
                    ((x * 7 + y) % 256) as u8,
                    ((x + y * 5) % 256) as u8,
                    ((x * 3 + y * 11) % 256) as u8,
                    255,
                ])
            });
            image::codecs::png::PngEncoder::new(&mut png_photo)
                .write_image(img.as_raw(), 120, 90, ExtendedColorType::Rgba8)
                .unwrap();
        }

        println!("\n=== JPEG encoder headroom @ q70 ===");
        let baseline = encode_jpeg_fallback(&rgb, 70).unwrap().len();
        let moz = encode_jpeg_moz(&rgb, 70, true).unwrap().len();
        let li = encode_jpeg_jpegli(&rgb, 70, true).unwrap().len();
        let fixed = moz_variant(&rgb, 70, true, true);
        let no_prog = moz_variant(&rgb, 70, false, true);
        let no_420 = moz_variant(&rgb, 70, true, false);
        println!("fallback          {baseline}");
        println!("mozjpeg           {moz}  ({:.1}% vs fallback)", pct(baseline, moz));
        println!(
            "jpegli            {li}  ({:.1}% vs fallback; vs moz {:+.1}%)",
            pct(baseline, li),
            (li as f64 - moz as f64) / moz as f64 * 100.0
        );
        println!(
            "fixed prog-after  {fixed}  (vs moz {:+.1}%)",
            (fixed as f64 - moz as f64) / moz as f64 * 100.0
        );
        println!(
            "no progressive    {no_prog}  (vs moz {:+.1}%)",
            (no_prog as f64 - moz as f64) / moz as f64 * 100.0
        );
        println!(
            "4:4:4             {no_420}  (vs moz {:+.1}%)",
            (no_420 as f64 - moz as f64) / moz as f64 * 100.0
        );

        println!("\n=== JPEG intensity / aggressive map ===");
        let q30 = jpeg_quality_from_intensity(30);
        let (o30, s30, _) = compress_jpeg(&jpeg_src, 30).unwrap();
        let (o80, s80, _) = compress_jpeg(&jpeg_src, 80).unwrap();
        let q_aggr = (70u32.saturating_sub(30 * 50 / 100)) as u8; // 对照：更狠一档
        let cur_q = encode_jpeg_jpegli(&rgb, q30, jpeg_use_420(30))
            .or_else(|_| encode_jpeg_moz(&rgb, q30, jpeg_use_420(30)))
            .unwrap()
            .len();
        let aggr = encode_jpeg_jpegli(&rgb, q_aggr, true)
            .or_else(|_| encode_jpeg_moz(&rgb, q_aggr, true))
            .unwrap()
            .len();
        let (o0, s0, m0) = compress_jpeg(&jpeg_src, 0).unwrap();
        println!(
            "i0={} save={:.1}% ssim={s0:.4} {m0}",
            o0.len(),
            pct(jpeg_src.len(), o0.len())
        );
        println!(
            "src={} i30(q{q30})={} save={:.1}% ssim={s30:.4}",
            jpeg_src.len(),
            o30.len(),
            pct(jpeg_src.len(), o30.len())
        );
        println!(
            "i80={} save={:.1}% ssim={s80:.4}  vs_i30_extra={:.1}%",
            o80.len(),
            pct(jpeg_src.len(), o80.len()),
            pct(o30.len(), o80.len())
        );
        println!(
            "extra_lower q{q_aggr}={} vs q{q30} delta={:.1}%",
            aggr,
            pct(cur_q, aggr)
        );

        println!("\n=== PNG photo ===");
        println!("src={}", png_photo.len());
        for i in [30u8, 80] {
            match compress_png(&png_photo, i) {
                Ok((out, ssim, m)) => println!(
                    "i={i} qmin={} c={} out={} save={:.1}% ssim={ssim:.4} {m}",
                    pngquant_quality_min(i),
                    png_max_colors(i),
                    out.len(),
                    pct(png_photo.len(), out.len())
                ),
                Err(e) => println!("i={i} ERR {e}"),
            }
        }
    }

    /// T020 探针：对用户样张强制 Jpegli，看体积/锐度是否值得有损。
    #[test]
    fn force_visual_on_user_sample() {
        let user_path = r"C:\Users\ASUS\.cursor\projects\d-AI-C-tinyImage\assets\c__Users_ASUS_AppData_Roaming_Cursor_User_workspaceStorage_88733b4a7d69dcfd459f54f8ded7846c_images_ss_08af4e9398b8e45152bfbedce3bc24d22e2c0990.1920x1080__1_-5f9124da-ce77-4864-9536-fd0e8a2f8648.png";
        let Ok(user) = fs::read(user_path) else {
            println!("skip: no user sample");
            return;
        };
        let orig = decode_to_rgb(&user).unwrap();
        let (w, h) = orig.dimensions();
        println!(
            "src={} {}x{} bpp={:.3}",
            user.len(),
            w,
            h,
            jpeg_bits_per_pixel(user.len(), w, h)
        );
        for q in [96u8, 94, 92, 90, 88, 86, 82, 78, 74] {
            let Ok(out) = encode_jpeg_jpegli_ex(&orig, q, false, false) else {
                println!("q{q} encode fail");
                continue;
            };
            let dec = decode_to_rgb(&out).unwrap();
            println!(
                "FORCE q{q} out={} ratio={:.3} ssim={:.4} hf={:.3} edge={:.3} psnr={:.1}",
                out.len(),
                out.len() as f64 / user.len() as f64,
                ssim_rgb(&orig, &dec).unwrap_or(0.0),
                high_freq_retain_ratio(&orig, &dec),
                edge_retain_ratio(&orig, &dec),
                psnr_rgb(&orig, &dec)
            );
        }
    }

    #[test]
    fn force_visual_on_large_fixture() {
        let img = photo_rgb(1920, 1080);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let orig = decode_to_rgb(&raw).unwrap();
        println!("LARGE src={}", raw.len());
        for q in ZERO_VISUAL_QUALITIES {
            let out = encode_jpeg_jpegli_ex(&orig, q, false, false).unwrap();
            let dec = decode_to_rgb(&out).unwrap();
            let ssim = ssim_rgb(&orig, &dec).unwrap_or(0.0);
            let hf = high_freq_retain_ratio(&orig, &dec);
            let edge = edge_retain_ratio(&orig, &dec);
            let psnr = psnr_rgb(&orig, &dec);
            let ratio = out.len() as f64 / raw.len() as f64;
            let gates = ssim >= ZERO_SSIM_MIN
                && hf >= ZERO_HF_MIN
                && edge >= ZERO_EDGE_MIN
                && psnr >= ZERO_PSNR_MIN
                && ratio <= VISUAL_MAX_SIZE_RATIO;
            println!(
                "LARGE q{q} out={} ratio={:.3} ssim={:.4} hf={:.3} edge={:.3} psnr={:.1} ok={gates}",
                out.len(),
                ratio,
                ssim,
                hf,
                edge,
                psnr
            );
        }
        match jpeg_visual_zero(&raw, &orig) {
            Ok((o, s, m)) => println!("visual_ok {} s={s:.4} {m}", o.len()),
            Err(e) => println!("visual_err {e}"),
        }
    }

    #[test]
    fn t038_speed_volume_tool_measured() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::Instant;

        let enforce_timing = std::env::var("TINYIMAGE_T038_TIMING")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        fn digest(bytes: &[u8]) -> u64 {
            let mut h = DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        }

        let img = photo_rgb(1280, 720);
        let raw = encode_jpeg_fallback(&img, 92).unwrap();
        let png_img = photo_rgb(512, 384);
        let mut png_enc = Vec::new();
        DynamicImage::ImageRgb8(png_img)
            .write_to(&mut std::io::Cursor::new(&mut png_enc), image::ImageFormat::Png)
            .unwrap();

        let cases: Vec<(&str, Vec<u8>, u8, u128, Option<usize>)> = vec![
            ("jpeg_i0", raw.clone(), 0, 4_000, Some(461617)),
            ("jpeg_i34", raw.clone(), 34, 3_200, None),
            ("jpeg_i80", raw.clone(), 80, 3_200, None),
            ("png_i0", png_enc.clone(), 0, 6_500, None),
            ("png_i60", png_enc.clone(), 60, 16_000, None),
        ];

        for (name, data, intensity, budget_ms, golden_len) in cases {
            let t0 = Instant::now();
            let (out1, ssim1, m1) = if name.starts_with("png") {
                compress_png(&data, intensity).expect(name)
            } else {
                compress_jpeg(&data, intensity).expect(name)
            };
            let ms = t0.elapsed().as_millis();
            let (out2, _, m2) = if name.starts_with("png") {
                compress_png(&data, intensity).expect(name)
            } else {
                compress_jpeg(&data, intensity).expect(name)
            };
            let len = out1.len();
            let hash = digest(&out1);
            println!(
                "T038 {name} i{intensity}: {ms}ms len={len} hash={hash} ssim={ssim1:.4} {m1}"
            );
            assert_eq!(out1, out2, "{name} must be deterministic");
            assert_eq!(m1, m2);
            if enforce_timing {
                assert!(
                    ms <= budget_ms,
                    "T038 {name} too slow: {ms}ms > {budget_ms}ms"
                );
            }
            assert!(out1.len() < data.len(), "{name} must shrink");
            if let Some(expect_len) = golden_len {
                assert_eq!(len, expect_len, "{name} volume changed");
            }
            // 体积指纹：同输入同力度须稳定
            assert_eq!(hash, digest(&out2), "{name} hash unstable");
        }
    }
}
