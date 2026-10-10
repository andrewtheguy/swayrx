//! libvpx's VP9 decoder, for the tests alone: the independent decoder the VP9
//! encoding's frames are read back with ([`crate::vp9`]), which is Chrome's
//! too. libvpx is a dev-dependency and this module is not in a build of the
//! crate: the encoder, `screen-vp9-native`, links none of it.
//!
//! Two things about libvpx's shape are worth knowing before reading:
//!
//! - **It returns error codes rather than asserting.** Every fallible call that
//!   returns a status code goes through `check`, which turns a bad code into
//!   libvpx's own explanation.
//! - **Its C API is entirely `unsafe` and largely out-parameters.** The
//!   invariants are stated at each call.

use std::os::raw::{c_int, c_uint};

use vpx_sys as vpx;

/// Turn a libvpx return code into the call and what libvpx says of it.
fn check(err: vpx::vpx_codec_err_t, call: &'static str) -> Result<(), String> {
    if err == vpx::vpx_codec_err_t_VPX_CODEC_OK {
        return Ok(());
    }
    // SAFETY: `vpx_codec_err_to_string` takes a code, any code, and returns a
    // pointer to a static string compiled into the archive.
    let detail = unsafe { std::ffi::CStr::from_ptr(vpx::vpx_codec_err_to_string(err)) };
    Err(format!("vp9 {call}: {}", detail.to_string_lossy()))
}

/// One VP9 stream's decoder: every frame through the same one, in the order
/// they came, since each is coded against the ones before it.
pub(crate) struct Decoder {
    /// Boxed so that its address never changes: libvpx is handed a pointer to
    /// it at init and every call after.
    ctx: Box<vpx::vpx_codec_ctx_t>,
}

impl Decoder {
    /// A decoder on `threads` threads.
    pub(crate) fn new(threads: usize) -> Result<Self, String> {
        // SAFETY: a static interface, a zeroed context written through by
        // `dec_init_ver`, and the ABI version of the linked archive's headers.
        unsafe {
            let iface = vpx::vpx_codec_vp9_dx();
            if iface.is_null() {
                return Err("this libvpx has no VP9 decoder".to_owned());
            }
            let cfg = vpx::vpx_codec_dec_cfg_t { threads: threads as c_uint, w: 0, h: 0 };
            let mut ctx: Box<vpx::vpx_codec_ctx_t> = Box::new(std::mem::zeroed());
            check(vpx::vpx_codec_dec_init_ver(&mut *ctx, iface, &cfg, 0, vpx::VPX_DECODER_ABI_VERSION as c_int), "dec_init_ver")?;
            Ok(Self { ctx })
        }
    }

    /// Decode one `frame` and return the picture it leaves on screen, which
    /// borrows the decoder until the next call. A frame that is not 8-bit
    /// 4:4:4 is not one of this crate's.
    pub(crate) fn decode(&mut self, frame: &[u8]) -> Result<Decoded<'_>, String> {
        // SAFETY: `frame` outlives the call. The image libvpx hands back belongs
        // to the decoder and is valid until it is called again, which the
        // returned borrow forbids.
        unsafe {
            check(vpx::vpx_codec_decode(&mut *self.ctx, frame.as_ptr(), frame.len() as c_uint, std::ptr::null_mut(), 0), "decode")?;
            let mut iter: vpx::vpx_codec_iter_t = std::ptr::null();
            let img = vpx::vpx_codec_get_frame(&mut *self.ctx, &mut iter);
            if img.is_null() {
                return Err("the frame decoded to no picture".to_owned());
            }
            let img = &*img;
            if img.fmt != vpx::vpx_img_fmt_VPX_IMG_FMT_I444 || img.bit_depth != 8 {
                return Err(format!("the frame is not 8-bit 4:4:4 (format {}, {} bits)", img.fmt, img.bit_depth));
            }
            Ok(Decoded { img })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the only handle, destroyed once.
        unsafe {
            vpx::vpx_codec_destroy(&mut *self.ctx);
        }
    }
}

/// The picture a decoded frame leaves on screen: libvpx's own image, borrowed
/// from the decoder.
pub(crate) struct Decoded<'a> {
    img: &'a vpx::vpx_image_t,
}

impl Decoded<'_> {
    /// The picture's size.
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.img.d_w, self.img.d_h)
    }

    /// Y, U and V, each at its own stride from [`Self::strides`].
    fn planes(&self) -> [&[u8]; 3] {
        let (w, h) = (self.img.d_w as usize, self.img.d_h as usize);
        let plane = |i: usize| {
            let stride = self.img.stride[i] as usize;
            // SAFETY: the plane is the decoder's, laid out at the stride it
            // reports, and read only inside the size it reports.
            unsafe { std::slice::from_raw_parts(self.img.planes[i], (h - 1) * stride + w) }
        };
        [plane(0), plane(1), plane(2)]
    }

    /// The width of a row of each plane, in bytes.
    fn strides(&self) -> [usize; 3] {
        [self.img.stride[0] as usize, self.img.stride[1] as usize, self.img.stride[2] as usize]
    }

    /// Write the picture into `out` as `B, G, R, X`, rows `stride` bytes apart,
    /// the X byte zero, and nothing between a row's last pixel and the next
    /// row. BT.601 at studio swing, as the encoder declares. A buffer the
    /// picture does not fit in is refused.
    pub(crate) fn write_bgrx(&self, out: &mut [u8], stride: usize) -> Result<(), String> {
        use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix};
        let (w, h) = (self.img.d_w as usize, self.img.d_h as usize);
        if stride < w * 4 || out.len() < (h - 1) * stride + w * 4 {
            return Err(format!("a {w}x{h} picture at a stride of {stride} does not fit in {} bytes", out.len()));
        }
        let [y_plane, u_plane, v_plane] = self.planes();
        let [ys, us, vs] = self.strides();
        let image = YuvPlanarImage {
            y_plane,
            y_stride: ys as u32,
            u_plane,
            u_stride: us as u32,
            v_plane,
            v_stride: vs as u32,
            width: w as u32,
            height: h as u32,
        };
        yuv::yuv444_to_bgra(&image, out, stride as u32, YuvRange::Limited, YuvStandardMatrix::Bt601).map_err(|e| e.to_string())?;
        // It writes an opaque alpha where the X byte goes.
        for row in 0..h {
            for pixel in out[row * stride..row * stride + w * 4].as_chunks_mut::<4>().0 {
                pixel[3] = 0;
            }
        }
        Ok(())
    }
}
