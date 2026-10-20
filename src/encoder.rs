use anyhow::{Context, Result, ensure};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::NonNull;

unsafe extern "C" {
    fn hypr_nvenc_new(
        sw: c_int,
        sh: c_int,
        rgba: c_int,
        w: c_int,
        h: c_int,
        bitrate: i64,
        fps: c_int,
        quality: c_int,
        cqp: c_int,
        error: *mut c_char,
        len: usize,
    ) -> *mut c_void;
    fn hypr_nvenc_free(enc: *mut c_void);
    fn hypr_nvenc_encode(
        enc: *mut c_void,
        bgra: *const u8,
        stride: c_int,
        idr: c_int,
        data: *mut *const u8,
        size: *mut c_int,
        error: *mut c_char,
        len: usize,
    ) -> c_int;
}

pub struct Encoder {
    inner: NonNull<c_void>,
    sw: usize,
    sh: usize,
}
impl Encoder {
    pub fn new(
        source: (u32, u32),
        size: (u32, u32),
        rgba: bool,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        ensure!(
            source.0 > 0 && source.1 > 0 && source.0 <= 16384 && source.1 <= 16384,
            "invalid capture dimensions"
        );
        ensure!(
            size.0 >= 200
                && size.1 >= 200
                && size.0 <= 8192
                && size.1 <= 8192
                && size.0.is_multiple_of(2)
                && size.1.is_multiple_of(2),
            "invalid encoder dimensions"
        );
        ensure!(
            (1..=120).contains(&fps) && bitrate > 0,
            "invalid encoder settings"
        );
        let mut err = [0 as c_char; 512];
        // SAFETY: Validated integer dimensions and writable error buffer. C owns its objects.
        let ptr = unsafe {
            hypr_nvenc_new(
                source.0 as i32,
                source.1 as i32,
                i32::from(rgba),
                size.0 as i32,
                size.1 as i32,
                i64::from(bitrate),
                fps as i32,
                23,
                0,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        let inner = NonNull::new(ptr).with_context(|| message(&err))?;
        Ok(Self {
            inner,
            sw: source.0 as usize,
            sh: source.1 as usize,
        })
    }
    pub fn encode(
        &mut self,
        pixels: &[u8],
        stride: usize,
        invert: bool,
        idr: bool,
    ) -> Result<Vec<u8>> {
        ensure!(
            stride >= self.sw * 4 && stride <= i32::MAX as usize,
            "invalid capture stride"
        );
        let required = stride
            .checked_mul(self.sh)
            .context("capture size overflow")?;
        ensure!(pixels.len() >= required, "short capture buffer");
        let mut err = [0 as c_char; 512];
        let mut data = std::ptr::null();
        let mut size = 0;
        let offset = if invert { (self.sh - 1) * stride } else { 0 };
        let signed_stride = if invert {
            -(stride as i32)
        } else {
            stride as i32
        };
        // SAFETY: The source footprint is validated, including negative stride. Packet is copied
        // before another call can invalidate it. Context is exclusively owned on this thread.
        let result = unsafe {
            hypr_nvenc_encode(
                self.inner.as_ptr(),
                pixels.as_ptr().add(offset),
                signed_stride,
                i32::from(idr),
                &mut data,
                &mut size,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        ensure!(result >= 0, "{}", message(&err));
        ensure!(!data.is_null() && size > 0, "empty NVENC packet");
        // SAFETY: The C bridge returned a valid AVPacket with size initialized bytes.
        Ok(unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec())
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { hypr_nvenc_free(self.inner.as_ptr()) };
    }
}
fn message(err: &[c_char; 512]) -> String {
    unsafe { CStr::from_ptr(err.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub fn fit_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    let w = width.min(max_width) / 2 * 2;
    let h = ((u64::from(height) * u64::from(w) / u64::from(width)) as u32) / 2 * 2;
    (w, h)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_aspect_and_even_sizes() {
        assert_eq!(fit_size(3840, 2160, 1920), (1920, 1080));
        assert_eq!(fit_size(3840, 2160, 2560), (2560, 1440));
        assert_eq!(fit_size(1280, 720, 1920), (1280, 720));
    }
    #[test]
    #[ignore = "requires NVIDIA NVENC"]
    fn hardware_produces_annex_b() {
        let mut encoder = Encoder::new((640, 480), (320, 240), false, 60, 8_000_000).unwrap();
        let frame = encoder
            .encode(&vec![64; 640 * 480 * 4], 640 * 4, false, true)
            .unwrap();
        assert!(
            frame
                .windows(4)
                .any(|v| v[..3] == [0, 0, 1] && v[3] & 31 == 5)
        );
    }
}
