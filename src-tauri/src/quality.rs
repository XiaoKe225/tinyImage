//! 结构相似度（SSIM）：近无损 / 写回保真门禁。
//! 对 RGB 转灰度后按 Wang 经典公式计算，范围约 [-1, 1]，1 为完全相同。

use image::{RgbImage, RgbaImage};

/// 写回保真参考阈值（细部档）；实际写回见 `ssim_min_for_intensity`。
#[allow(dead_code)]
pub const SSIM_MIN: f64 = 0.985;

pub fn rgb_to_gray(img: &RgbImage) -> Vec<f64> {
    img.pixels()
        .map(|p| 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64)
        .collect()
}

/// 简易高频能量（3×3 Laplacian 绝对值均值）：细颗粒/纹理敏感，SSIM 对此不敏感。
pub fn high_freq_energy(img: &RgbImage) -> f64 {
    let (w, h) = img.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let g = rgb_to_gray(img);
    let ww = w as usize;
    let hh = h as usize;
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in 1..hh - 1 {
        for x in 1..ww - 1 {
            let c = g[y * ww + x];
            let lap = -4.0 * c
                + g[y * ww + x - 1]
                + g[y * ww + x + 1]
                + g[(y - 1) * ww + x]
                + g[(y + 1) * ww + x];
            sum += lap.abs();
            n += 1.0;
        }
    }
    if n < 1.0 {
        0.0
    } else {
        sum / n
    }
}

/// 解码图相对原图的高频保留比（1=完全保留；细颗粒糊时会明显下降）。
pub fn high_freq_retain_ratio(original: &RgbImage, decoded: &RgbImage) -> f64 {
    let a = high_freq_energy(original);
    let b = high_freq_energy(decoded);
    if a < 1e-9 {
        1.0
    } else {
        (b / a).clamp(0.0, 2.0)
    }
}

/// Sobel 梯度能量比：专治「SSIM 还行但边缘发糊」。
pub fn edge_retain_ratio(original: &RgbImage, decoded: &RgbImage) -> f64 {
    let a = sobel_energy(original);
    let b = sobel_energy(decoded);
    if a < 1e-9 {
        1.0
    } else {
        (b / a).clamp(0.0, 2.0)
    }
}

fn sobel_energy(img: &RgbImage) -> f64 {
    let (w, h) = img.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let g = rgb_to_gray(img);
    let ww = w as usize;
    let hh = h as usize;
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in 1..hh - 1 {
        for x in 1..ww - 1 {
            let at = |xx: usize, yy: usize| g[yy * ww + xx];
            let gx = -at(x - 1, y - 1) + at(x + 1, y - 1)
                - 2.0 * at(x - 1, y)
                + 2.0 * at(x + 1, y)
                - at(x - 1, y + 1)
                + at(x + 1, y + 1);
            let gy = -at(x - 1, y - 1) - 2.0 * at(x, y - 1) - at(x + 1, y - 1)
                + at(x - 1, y + 1)
                + 2.0 * at(x, y + 1)
                + at(x + 1, y + 1);
            sum += (gx * gx + gy * gy).sqrt();
            n += 1.0;
        }
    }
    if n < 1.0 {
        0.0
    } else {
        sum / n
    }
}

/// PSNR（dB）；像素全等 → +∞，用 99 封顶。
pub fn psnr_rgb(a: &RgbImage, b: &RgbImage) -> f64 {
    if a.dimensions() != b.dimensions() {
        return 0.0;
    }
    let n = a.as_raw().len() as f64;
    if n < 1.0 {
        return 0.0;
    }
    let mut mse = 0.0;
    for (pa, pb) in a.as_raw().iter().zip(b.as_raw().iter()) {
        let d = *pa as f64 - *pb as f64;
        mse += d * d;
    }
    mse /= n;
    if mse < 1e-12 {
        99.0
    } else {
        (10.0 * (255.0_f64 * 255.0 / mse).log10()).min(99.0)
    }
}

pub fn rgba_to_rgb(img: &RgbaImage) -> RgbImage {
    let (w, h) = img.dimensions();
    RgbImage::from_fn(w, h, |x, y| {
        let p = img.get_pixel(x, y);
        image::Rgb([p[0], p[1], p[2]])
    })
}

/// 窗口 8×8 的均值 SSIM（简化但足够作压缩门禁）。
pub fn ssim_rgb(a: &RgbImage, b: &RgbImage) -> Result<f64, String> {
    if a.dimensions() != b.dimensions() {
        return Err("SSIM：尺寸不一致".into());
    }
    let (w, h) = a.dimensions();
    if w < 8 || h < 8 {
        // 小图：全图像素相关近似
        return Ok(ssim_full(&rgb_to_gray(a), &rgb_to_gray(b)));
    }
    let ga = rgb_to_gray(a);
    let gb = rgb_to_gray(b);
    let ww = w as usize;
    let mut sum = 0.0;
    let mut count = 0.0;
    let step = 4usize;
    let mut y = 0usize;
    while y + 8 <= h as usize {
        let mut x = 0usize;
        while x + 8 <= ww {
            sum += ssim_window(&ga, &gb, ww, x, y, 8);
            count += 1.0;
            x += step;
        }
        y += step;
    }
    if count < 1.0 {
        return Ok(ssim_full(&ga, &gb));
    }
    Ok(sum / count)
}

fn ssim_full(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len()) as f64;
    if n < 1.0 {
        return 0.0;
    }
    let (mut ma, mut mb) = (0.0, 0.0);
    for i in 0..a.len().min(b.len()) {
        ma += a[i];
        mb += b[i];
    }
    ma /= n;
    mb /= n;
    let (mut va, mut vb, mut cov) = (0.0, 0.0, 0.0);
    for i in 0..a.len().min(b.len()) {
        let da = a[i] - ma;
        let db = b[i] - mb;
        va += da * da;
        vb += db * db;
        cov += da * db;
    }
    va /= n;
    vb /= n;
    cov /= n;
    let c1 = (0.01f64 * 255.0).powi(2);
    let c2 = (0.03f64 * 255.0).powi(2);
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

fn ssim_window(a: &[f64], b: &[f64], stride: usize, x0: usize, y0: usize, win: usize) -> f64 {
    let n = (win * win) as f64;
    let mut ma = 0.0;
    let mut mb = 0.0;
    for dy in 0..win {
        for dx in 0..win {
            let i = (y0 + dy) * stride + (x0 + dx);
            ma += a[i];
            mb += b[i];
        }
    }
    ma /= n;
    mb /= n;
    let mut va = 0.0;
    let mut vb = 0.0;
    let mut cov = 0.0;
    for dy in 0..win {
        for dx in 0..win {
            let i = (y0 + dy) * stride + (x0 + dx);
            let da = a[i] - ma;
            let db = b[i] - mb;
            va += da * da;
            vb += db * db;
            cov += da * db;
        }
    }
    va /= n;
    vb /= n;
    cov /= n;
    let c1 = (0.01f64 * 255.0).powi(2);
    let c2 = (0.03f64 * 255.0).powi(2);
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    #[test]
    fn identical_images_ssim_near_one() {
        let img = RgbImage::from_fn(32, 32, |x, y| Rgb([(x as u8).wrapping_mul(3), y as u8, 100]));
        let s = ssim_rgb(&img, &img).unwrap();
        assert!(s > 0.999, "ssim={s}");
    }
}
