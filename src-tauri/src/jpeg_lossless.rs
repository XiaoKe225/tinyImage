//! JPEG 无损熵优化（系数级，不改 DCT）。
//! - 默认 **不强制 progressive**（强制渐进在部分查看器会显得发糊）。
//! - 可选 progressive 仅当明显更小且像素校验通过。

use mozjpeg_sys::*;
use std::mem;
use std::os::raw::{c_int, c_ulong};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;

extern "C-unwind" fn silence_msg(_cinfo: &mut jpeg_common_struct, _level: c_int) {}

extern "C-unwind" fn panic_on_jpeg_error(cinfo: &mut jpeg_common_struct) {
    let code = unsafe { (*cinfo.err).msg_code };
    panic!("mozjpeg lossless error code={code}");
}

fn install_err(err: &mut jpeg_error_mgr) {
    unsafe {
        jpeg_std_error(err);
        err.error_exit = Some(panic_on_jpeg_error);
        err.emit_message = Some(silence_msg);
    }
}

/// 不重量化：拷贝 DCT + 优化 Huffman；可选 progressive；去 EXIF，保 ICC。
pub fn jpeg_lossless_optimize(data: &[u8]) -> Option<Vec<u8>> {
    jpeg_lossless_optimize_ex(data, false)
}

/// `force_progressive=true` 时转为 progressive（通常更小，但部分查看器观感更软）。
pub fn jpeg_lossless_optimize_ex(data: &[u8], force_progressive: bool) -> Option<Vec<u8>> {
    if data.len() < 100 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        lossless_inner(data, force_progressive)
    }))
    .ok()
    .flatten()
}

/// 取更小者；若体积差 &lt;3%，优先 **非强制 progressive**（保锐利观感）。
pub fn jpeg_lossless_best(data: &[u8]) -> Option<Vec<u8>> {
    let base = jpeg_lossless_optimize_ex(data, false);
    let prog = jpeg_lossless_optimize_ex(data, true);
    match (base, prog) {
        (Some(a), Some(b)) => {
            if (a.len() as f64) <= (b.len() as f64) * 1.03 {
                Some(a)
            } else {
                Some(b)
            }
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

unsafe fn lossless_inner(data: &[u8], force_progressive: bool) -> Option<Vec<u8>> {
    let mut src_err: jpeg_error_mgr = mem::zeroed();
    let mut srcinfo: jpeg_decompress_struct = mem::zeroed();
    install_err(&mut src_err);
    srcinfo.common.err = &mut src_err;
    jpeg_create_decompress(&mut srcinfo);

    jpeg_mem_src(&mut srcinfo, data.as_ptr(), data.len() as c_ulong);
    jcopy_markers_setup(&mut srcinfo as *mut _, JCOPY_OPTION_JCOPYOPT_ICC);

    if jpeg_read_header(&mut srcinfo, true as boolean) != 1 {
        jpeg_destroy_decompress(&mut srcinfo);
        return None;
    }

    let coef_arrays = jpeg_read_coefficients(&mut srcinfo);
    if coef_arrays.is_null() {
        jpeg_destroy_decompress(&mut srcinfo);
        return None;
    }

    let mut dst_err: jpeg_error_mgr = mem::zeroed();
    let mut dstinfo: jpeg_compress_struct = mem::zeroed();
    install_err(&mut dst_err);
    dstinfo.common.err = &mut dst_err;
    jpeg_create_compress(&mut dstinfo);

    let mut out_buf: *mut u8 = ptr::null_mut();
    let mut out_size: c_ulong = 0;
    jpeg_mem_dest(&mut dstinfo, &mut out_buf, &mut out_size);

    jpeg_copy_critical_parameters(&srcinfo, &mut dstinfo);
    dstinfo.optimize_coding = true as boolean;
    if force_progressive {
        jpeg_simple_progression(&mut dstinfo);
    }

    jpeg_write_coefficients(&mut dstinfo, coef_arrays);
    jcopy_markers_execute(
        &mut srcinfo as *mut _,
        &mut dstinfo as *mut _,
        JCOPY_OPTION_JCOPYOPT_ICC,
    );
    jpeg_finish_compress(&mut dstinfo);

    let out = if !out_buf.is_null() && out_size > 0 {
        let sl = slice::from_raw_parts(out_buf, out_size as usize);
        let v = sl.to_vec();
        libc::free(out_buf as *mut libc::c_void);
        v
    } else {
        Vec::new()
    };

    jpeg_destroy_compress(&mut dstinfo);
    let _ = jpeg_finish_decompress(&mut srcinfo);
    jpeg_destroy_decompress(&mut srcinfo);

    if out.len() >= 64 && out.len() < data.len() {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, RgbImage};

    fn sample_jpeg() -> Vec<u8> {
        let img: RgbImage = ImageBuffer::from_fn(96, 72, |x, y| {
            Rgb([
                ((x * 3 + y) % 256) as u8,
                ((x + y * 5) % 256) as u8,
                ((x * 7 + y * 2) % 256) as u8,
            ])
        });
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        enc.encode(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
        out
    }

    #[test]
    fn lossless_optimize_shrinks_without_pixel_change() {
        let raw = sample_jpeg();
        let opt = jpeg_lossless_best(&raw).expect("optimize");
        assert!(opt.len() < raw.len());
        let a = image::load_from_memory(&raw).unwrap().to_rgb8();
        let b = image::load_from_memory(&opt).unwrap().to_rgb8();
        assert_eq!(a.as_raw(), b.as_raw(), "无损路径像素必须一致");
    }
}
