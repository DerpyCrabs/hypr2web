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
        gpu: c_int,
        main_profile: c_int,
        error: *mut c_char,
        len: usize,
    ) -> *mut c_void;
    fn hypr_nvenc_bitrate(enc: *mut c_void, bitrate: i64);
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
        gpu: bool,
        main_profile: bool,
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
                16,
                0,
                i32::from(gpu),
                i32::from(main_profile),
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
    pub fn set_bitrate(&mut self, bitrate: u32) {
        // SAFETY: the encoder is exclusively owned and bitrate is validated by the controller.
        unsafe {
            hypr_nvenc_bitrate(self.inner.as_ptr(), i64::from(bitrate));
        }
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

pub fn fit_size(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let scale = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.0);
    let even = |n: u32| (n / 2 * 2).max(2);
    (
        even((width as f64 * scale) as u32),
        even((height as f64 * scale) as u32),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_aspect_and_even_sizes() {
        assert_eq!(fit_size(3840, 2160, 1768, 2208), (1768, 994));
        assert_eq!(fit_size(3840, 2160, 2208, 800), (1422, 800));
        assert_eq!(fit_size(3840, 2160, 8192, 8192), (3840, 2160));
        assert_eq!(fit_size(3840, 2160, 767, 901), (766, 430));
        assert_eq!(fit_size(3840, 2160, 1920, 8192), (1920, 1080));
        assert_eq!(fit_size(3840, 2160, 2560, 8192), (2560, 1440));
        assert_eq!(fit_size(1280, 720, 1920, 8192), (1280, 720));
    }
    #[test]
    #[ignore = "requires NVIDIA NVENC"]
    fn hardware_produces_annex_b() {
        let mut encoder =
            Encoder::new((640, 480), (320, 240), false, 60, 8_000_000, true, true).unwrap();
        let frame = encoder
            .encode(&vec![64; 640 * 480 * 4], 640 * 4, false, true)
            .unwrap();
        assert!(
            frame
                .windows(4)
                .any(|v| v[..3] == [0, 0, 1] && v[3] & 31 == 5)
        );
    }
    #[test]
    #[ignore = "requires NVIDIA NVENC and ffmpeg"]
    fn gpu_scaling_colors_and_inversion_roundtrip() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for rgba in [false, true] {
            for inverted in [false, true] {
                let mut pixels = vec![0u8; 640 * 480 * 4];
                for y in 0..480 {
                    for x in 0..640 {
                        let red = if inverted { y >= 240 } else { y < 240 };
                        let p = &mut pixels[(y * 640 + x) * 4..][..4];
                        p[if rgba { 0 } else { 2 }] = if red { 255 } else { 0 };
                        p[if rgba { 2 } else { 0 }] = if red { 0 } else { 255 };
                        p[3] = 255;
                    }
                }
                let mut encoder =
                    Encoder::new((640, 480), (320, 240), rgba, 60, 8_000_000, true, true).unwrap();
                let packet = encoder.encode(&pixels, 640 * 4, inverted, true).unwrap();
                let mut child = Command::new("ffmpeg")
                    .args([
                        "-v",
                        "error",
                        "-f",
                        "h264",
                        "-i",
                        "pipe:0",
                        "-frames:v",
                        "1",
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                        "rgb24",
                        "pipe:1",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                child.stdin.take().unwrap().write_all(&packet).unwrap();
                let out = child.wait_with_output().unwrap();
                assert!(out.status.success());
                assert_eq!(out.stdout.len(), 320 * 240 * 3);
                let top = &out.stdout[(60 * 320 + 160) * 3..][..3];
                let bottom = &out.stdout[(180 * 320 + 160) * 3..][..3];
                assert!(top[0] > 230 && top[1] < 20 && top[2] < 20, "top {top:?}");
                assert!(
                    bottom[2] > 230 && bottom[0] < 20 && bottom[1] < 20,
                    "bottom {bottom:?}"
                );
            }
        }
    }
    #[test]
    #[ignore = "requires NVIDIA NVENC; prints 4K benchmark"]
    fn benchmark_4k_cpu_vs_cuda() {
        let pixels = vec![96u8; 3840 * 2160 * 4];
        for gpu in [false, true] {
            let mut encoder =
                Encoder::new((3840, 2160), (3840, 2160), false, 60, 16_000_000, gpu, true).unwrap();
            let mut times = Vec::new();
            for i in 0..65 {
                if i == 30 {
                    encoder.set_bitrate(8_000_000);
                }
                let t = std::time::Instant::now();
                let frame = encoder.encode(&pixels, 3840 * 4, false, i == 0).unwrap();
                assert!(!frame.is_empty());
                if i >= 5 {
                    times.push(t.elapsed().as_secs_f64() * 1000.);
                }
            }
            times.sort_by(f64::total_cmp);
            eprintln!(
                "4K gpu={gpu}: median {:.2} ms, p95 {:.2} ms",
                times[30], times[57]
            );
        }
    }
    #[test]
    #[ignore = "requires NVIDIA NVENC; exercises repeated GPU context teardown"]
    fn repeated_resize_and_reconnect() {
        let pixels = vec![80u8; 3840 * 2160 * 4];
        for i in 0..24 {
            let size = if i % 2 == 0 {
                (3840, 2160)
            } else {
                (2560, 1440)
            };
            let mut encoder =
                Encoder::new((3840, 2160), size, false, 60, 16_000_000, true, true).unwrap();
            for frame in 0..8 {
                encoder
                    .encode(&pixels, 3840 * 4, false, frame == 0)
                    .unwrap();
            }
            drop(encoder);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
