//! VP9 for a desktop picture, as wlshare and the remotex gateway both code it.
//!
//! One libvpx encoder configuration, written once: a quantizer pinned to a 1–100
//! quality dial rather than a bitrate, screen-content tuning, no lag, no dropped
//! frames, no keyframe that was not asked for, and the colour matrix and range
//! declared in the bitstream so a decoder does not guess. In front of it a
//! [`Picture`] holds the planes libvpx reads — 8-bit 4:2:0 or 4:4:4, BT.601 at
//! studio swing — converted from the packed RGB the gateway's mirror holds or the
//! `B, G, R, X` a framebuffer does. Behind it a [`Decoder`] gives the planes
//! back, for the desktop client and for every test here, which reads what the
//! encoder made with the other half of the same archive.
//!
//! What a frame says about itself is read here too: [`frame_header`] for the
//! profile and keyframe bit of a frame this process did not encode, and
//! [`codec_string`] for the WebCodecs string a browser's `VideoDecoder` is
//! configured with, since VP9 carries no parameter sets a client could read one
//! out of.
//!
//! What is not here is everything about *when*: which picture to encode, how a
//! link is measured and the quality walked, how a frame is framed on a wire. Each
//! user keeps its own.
//!
//! Two things about libvpx's shape are worth knowing before reading:
//!
//! - **It returns error codes rather than asserting.** Every call goes through
//!   [`check`], which turns a bad code into an [`Error`] carrying libvpx's own
//!   explanation, so a failure ends one stream instead of the process.
//! - **Its C API is entirely `unsafe` and largely out-parameters.** The
//!   invariants are stated at each call. Two are easy to get wrong and neither
//!   announces itself: the image built by `vpx_img_wrap` borrows the caller's
//!   planes and must never reach `vpx_img_free`, and `vpx_codec_control_` is
//!   variadic, so passing the wrong argument type for a control id compiles and
//!   corrupts the stack.

use std::os::raw::{c_int, c_uint, c_ulong};

use thiserror::Error;
use vpx_sys as vpx;

/// The coarsest end of the quality dial.
pub const QUALITY_MIN: u8 = 1;
/// The finest end of the quality dial.
pub const QUALITY_MAX: u8 = 100;

/// The finest quantizer the dial reaches. VP9's range is 0–63, coarsest last;
/// below about 8 a VP9 desktop is visually lossless and costs several times the
/// bytes to be so, so a dial that mapped past it would have a top third where
/// turning the knob bought nothing but bandwidth. Settled by measurement.
const Q_FINEST: u32 = 8;
/// VP9's coarsest quantizer.
const Q_COARSEST: u32 = 63;

/// libvpx's encoder speed, 0–9, higher being faster and worse. 7 is inside the
/// 5–8 band libvpx's own live-encoding guidance names, and where the one other
/// real-time desktop encoder to consult sits. Measured rather than inherited.
const CPU_USED: c_int = 7;

/// A tile column is about this wide, so libvpx's `tile_columns` is the log2 of
/// how many of them the picture holds: none under 1920 pixels, two at 1080p,
/// four at 4K. The width rather than the thread count decides it because a tile
/// is a cost as well as a split — the columns are coded apart, which costs
/// bytes, and at 1080p four of them coded slower than two whatever the threads —
/// while at 4K four were worth having. Never more than the threads can fill,
/// since a tile no thread is free for is bytes for nothing.
const TILE_WIDTH: usize = 960;

/// The most threads libvpx takes for one encoder.
const MAX_THREADS: usize = 64;

/// The 1–100 quality dial as a VP9 quantizer: [`QUALITY_MIN`] is the coarsest
/// picture and becomes [`Q_COARSEST`], [`QUALITY_MAX`] the finest and becomes
/// [`Q_FINEST`]. Out-of-range input is clamped rather than refused; a
/// configuration is what rejects a bad dial.
fn quality_to_q(quality: u8) -> u32 {
    let quality = u32::from(quality.clamp(QUALITY_MIN, QUALITY_MAX));
    Q_COARSEST - (quality - 1) * (Q_COARSEST - Q_FINEST) / 99
}

/// libvpx's `tile_columns` for a `width`-wide picture coded by `threads`.
fn tile_columns_log2(width: u16, threads: usize) -> u32 {
    (usize::from(width) / TILE_WIDTH).max(1).ilog2().min(threads.max(1).ilog2())
}

/// How much colour a stream carries per pixel: the VP9 profile at eight bits,
/// and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chroma {
    /// 4:2:0 — one colour sample per 2×2 pixels, VP9 profile 0. The one every VP9
    /// decoder takes, hardware ones included.
    Subsampled,
    /// 4:4:4 — a colour sample per pixel, VP9 profile 1. On a desktop this, not
    /// the quantizer, is where the visible loss is: a one-pixel coloured glyph
    /// stem on a dark terminal shares its one 4:2:0 colour sample with three
    /// pixels of background and comes back at a fraction of its saturation, at
    /// any quality. No hardware VP9 decoder takes profile 1, so it always
    /// decodes in software.
    Full,
}

impl Chroma {
    /// The VP9 profile this sampling is at eight bits: 0 or 1.
    pub fn profile(self) -> u8 {
        match self {
            Self::Subsampled => 0,
            Self::Full => 1,
        }
    }

    /// The sampling as a reader spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Subsampled => "4:2:0",
            Self::Full => "4:4:4",
        }
    }

    /// The libvpx image format that holds it.
    fn img_fmt(self) -> vpx::vpx_img_fmt_t {
        match self {
            Self::Subsampled => vpx::vpx_img_fmt_VPX_IMG_FMT_I420,
            Self::Full => vpx::vpx_img_fmt_VPX_IMG_FMT_I444,
        }
    }

    /// The sampling of a decoded image, or `None` for one that is neither
    /// eight-bit 4:2:0 nor 4:4:4.
    fn of_img(fmt: vpx::vpx_img_fmt_t, bit_depth: u32) -> Option<Self> {
        if bit_depth != 8 {
            return None;
        }
        match fmt {
            f if f == vpx::vpx_img_fmt_VPX_IMG_FMT_I420 => Some(Self::Subsampled),
            f if f == vpx::vpx_img_fmt_VPX_IMG_FMT_I444 => Some(Self::Full),
            _ => None,
        }
    }

    /// The size of a chroma plane for a `width`×`height` picture.
    fn plane_size(self, width: usize, height: usize) -> (usize, usize) {
        match self {
            Self::Subsampled => (width.div_ceil(2), height.div_ceil(2)),
            Self::Full => (width, height),
        }
    }
}

/// Why a picture could not be encoded or a frame decoded.
#[derive(Debug, Error)]
pub enum Error {
    /// libvpx refused a call, in its own words.
    #[error("vp9 {call}: {detail}")]
    Codec { call: &'static str, detail: String },
    #[error("a {0}x{1} picture has no pixels")]
    Empty(u16, u16),
    #[error("an encoder for {0} threads: libvpx takes 1 to {MAX_THREADS}")]
    Threads(usize),
    #[error("a crop of {0} bytes is not a {1}x{2} picture")]
    Crop(usize, usize, usize),
    #[error("a {width}x{height} picture at a stride of {stride} does not fit in {bytes} bytes")]
    Buffer { width: usize, height: usize, stride: usize, bytes: usize },
    #[error("a {0}x{1} {2} encoder was handed a {3}x{4} {5} picture")]
    Mismatch(u16, u16, &'static str, u16, u16, &'static str),
    #[error("the frame decoded to no picture")]
    NoPicture,
    #[error("the frame is not 8-bit 4:2:0 or 4:4:4 (format {0}, {1} bits)")]
    Format(u32, u32),
}

/// Turn a libvpx return code into an [`Error`] naming the call.
fn check(err: vpx::vpx_codec_err_t, call: &'static str) -> Result<(), Error> {
    if err == vpx::vpx_codec_err_t_VPX_CODEC_OK {
        return Ok(());
    }
    // SAFETY: `vpx_codec_err_to_string` takes a code, any code, and returns a
    // pointer to a static string compiled into the archive.
    let detail = unsafe { std::ffi::CStr::from_ptr(vpx::vpx_codec_err_to_string(err)) };
    Err(Error::Codec { call, detail: detail.to_string_lossy().into_owned() })
}

/// Whether `bytes` hold a `width`×`height` picture of 4-byte pixels whose rows
/// are `stride` apart, in measures the conversions' `u32` can carry. A last row
/// with nothing after it is a picture that fits.
fn fits(width: usize, height: usize, stride: usize, bytes: usize) -> bool {
    if [width, height, stride].iter().any(|&n| u32::try_from(n).is_err()) {
        return false;
    }
    width == 0 || height == 0 || (stride >= width * 4 && bytes >= (height - 1) * stride + width * 4)
}

/// One picture as the planes libvpx reads: 8-bit Y, U and V, BT.601 at studio
/// swing, the chroma at one sample per pixel or one per 2×2 group averaged, as
/// its [`Chroma`] says. Reused across frames, so a 1080p conversion is not a
/// 3 MB allocation apiece.
pub struct Picture {
    size: (u16, u16),
    chroma: Chroma,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    /// A tight copy of a `B, G, R, X` picture whose buffer ends with its last
    /// row, made only for one: the conversion reads rows as whole strides.
    tight: Vec<u8>,
}

impl Picture {
    /// A buffer for a `width`×`height` picture at `chroma`. An odd side is
    /// carried as it is: the 4:2:0 chroma planes round up.
    pub fn new(width: u16, height: u16, chroma: Chroma) -> Result<Self, Error> {
        if width == 0 || height == 0 {
            return Err(Error::Empty(width, height));
        }
        let (w, h) = (usize::from(width), usize::from(height));
        let (cw, ch) = chroma.plane_size(w, h);
        Ok(Self { size: (width, height), chroma, y: vec![0; w * h], u: vec![0; cw * ch], v: vec![0; cw * ch], tight: Vec::new() })
    }

    /// The picture's size.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Which sampling the planes hold.
    pub fn chroma(&self) -> Chroma {
        self.chroma
    }

    /// Y, U and V, each tight at its own stride.
    pub fn planes(&self) -> [&[u8]; 3] {
        [&self.y, &self.u, &self.v]
    }

    /// The width of a row of each plane.
    pub fn strides(&self) -> [usize; 3] {
        let (w, h) = (usize::from(self.size.0), usize::from(self.size.1));
        let (cw, _) = self.chroma.plane_size(w, h);
        [w, cw, cw]
    }

    /// The planes as the `yuv` crate writes them.
    fn planar(&mut self) -> yuv::YuvPlanarImageMut<'_, u8> {
        use yuv::BufferStoreMut;
        let [ys, us, vs] = self.strides();
        yuv::YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(&mut self.y),
            y_stride: ys as u32,
            u_plane: BufferStoreMut::Borrowed(&mut self.u),
            u_stride: us as u32,
            v_plane: BufferStoreMut::Borrowed(&mut self.v),
            v_stride: vs as u32,
            width: u32::from(self.size.0),
            height: u32::from(self.size.1),
        }
    }

    /// Convert `rgb` — packed RGB888, tight, for exactly this picture — in place.
    ///
    /// The length is checked rather than trusted, because everything after the
    /// check indexes by the picture size.
    pub fn read_rgb(&mut self, rgb: &[u8]) -> Result<(), Error> {
        use yuv::{YuvConversionMode, YuvRange, YuvStandardMatrix};
        let (w, h) = (usize::from(self.size.0), usize::from(self.size.1));
        if rgb.len() != w * h * 3 {
            return Err(Error::Crop(rgb.len(), w, h));
        }
        let chroma = self.chroma;
        let mut image = self.planar();
        let (range, matrix, mode) = (YuvRange::Limited, YuvStandardMatrix::Bt601, YuvConversionMode::Balanced);
        // The 4:2:0 chroma sample is the 2×2 group's rounded average, as the
        // crate takes it; `the_conversion_is_bt601_studio_swing` holds it to that.
        match chroma {
            Chroma::Full => yuv::rgb_to_yuv444(&mut image, rgb, (w * 3) as u32, range, matrix, mode),
            Chroma::Subsampled => yuv::rgb_to_yuv420(&mut image, rgb, (w * 3) as u32, range, matrix, mode),
        }
        .expect("the picture is the size its buffers were made for");
        Ok(())
    }

    /// Convert `pixels` — `B, G, R, X`, this picture's size, rows `stride` bytes
    /// apart — in place. The X byte is ignored.
    pub fn read_bgrx(&mut self, pixels: &[u8], stride: usize) -> Result<(), Error> {
        use yuv::{YuvConversionMode, YuvRange, YuvStandardMatrix};
        let (w, h) = (usize::from(self.size.0), usize::from(self.size.1));
        if !fits(w, h, stride, pixels.len()) {
            return Err(Error::Buffer { width: w, height: h, stride, bytes: pixels.len() });
        }
        // The crate reads rows as whole strides, so a last row with nothing
        // after it — which is a picture that fits — is copied tight first.
        let (pixels, stride) = if pixels.len() >= h * stride {
            (pixels, stride)
        } else {
            let row = w * 4;
            self.tight.clear();
            self.tight.extend(pixels.chunks(stride).take(h).flat_map(|r| &r[..row]));
            (self.tight.as_slice(), row)
        };
        // Borrowed apart from `self.tight`, which the conversion reads.
        let (size, chroma) = (self.size, self.chroma);
        let [ys, us, vs] = {
            let (cw, _) = chroma.plane_size(w, h);
            [w, cw, cw]
        };
        let mut image = yuv::YuvPlanarImageMut {
            y_plane: yuv::BufferStoreMut::Borrowed(&mut self.y),
            y_stride: ys as u32,
            u_plane: yuv::BufferStoreMut::Borrowed(&mut self.u),
            u_stride: us as u32,
            v_plane: yuv::BufferStoreMut::Borrowed(&mut self.v),
            v_stride: vs as u32,
            width: u32::from(size.0),
            height: u32::from(size.1),
        };
        let (range, matrix, mode) = (YuvRange::Limited, YuvStandardMatrix::Bt601, YuvConversionMode::Balanced);
        match chroma {
            Chroma::Full => yuv::bgra_to_yuv444(&mut image, pixels, stride as u32, range, matrix, mode),
            Chroma::Subsampled => yuv::bgra_to_yuv420(&mut image, pixels, stride as u32, range, matrix, mode),
        }
        .expect("the picture fits its buffer, which was checked");
        Ok(())
    }
}

/// One VP9 stream at one picture size and chroma: a libvpx encoder whose
/// quantizer is pinned to the dial. A picture of another size or chroma needs
/// another encoder, whose first frame is a keyframe by construction — every
/// frame is expressed as a change from the last one, which is what makes an
/// inter-frame stream mean anything.
pub struct Encoder {
    /// Boxed so that its address never changes: libvpx is handed a pointer to
    /// it at init and every call after.
    ctx: Box<vpx::vpx_codec_ctx_t>,
    /// The configuration libvpx is running on, kept so [`Self::set_quality`]
    /// can hand back the same struct with two fields changed rather than
    /// rebuild one and risk disagreeing with the encoder about a field it never
    /// meant to touch.
    cfg: vpx::vpx_codec_enc_cfg_t,
    /// An image that *borrows* a picture's planes. Built once, its pointers
    /// replaced on every frame. Never freed: `vpx_img_wrap` allocated nothing.
    img: vpx::vpx_image_t,
    size: (u16, u16),
    chroma: Chroma,
    /// The dial in force, which [`Self::set_quality`] moves.
    quality: u8,
    /// Where timestamps are measured from. Real elapsed time rather than a
    /// frame counter, so a pts is a millisecond on the timebase set below: the
    /// desktop decides when a frame happens, and a counter would tell the
    /// encoder they all arrived on schedule.
    started: std::time::Instant,
    /// The last timestamp given, which the next must pass.
    pts: i64,
}

impl Encoder {
    /// An encoder for a `width`×`height` picture at `chroma`, starting at
    /// `quality` (1–100, clamped), coded by `threads` threads. The thread count
    /// is the caller's: how many cores a machine can spare is a question about
    /// what else it runs.
    pub fn new(width: u16, height: u16, chroma: Chroma, quality: u8, threads: usize) -> Result<Self, Error> {
        if width == 0 || height == 0 {
            return Err(Error::Empty(width, height));
        }
        if !(1..=MAX_THREADS).contains(&threads) {
            return Err(Error::Threads(threads));
        }
        let quality = quality.clamp(QUALITY_MIN, QUALITY_MAX);
        let q = quality_to_q(quality);

        // SAFETY: takes nothing and returns a pointer to a static interface
        // descriptor compiled into the archive.
        let iface = unsafe { vpx::vpx_codec_vp9_cx() };
        if iface.is_null() {
            return Err(Error::Codec { call: "vp9_cx", detail: "this libvpx has no VP9 encoder".to_owned() });
        }
        // SAFETY: zeroed before libvpx is given a pointer to it, and
        // `config_default` writes through that pointer rather than reading it. A
        // failure is returned before anything reads a half-initialised config.
        let mut cfg: vpx::vpx_codec_enc_cfg_t = unsafe { std::mem::zeroed() };
        check(unsafe { vpx::vpx_codec_enc_config_default(iface, &mut cfg, 0) }, "config_default")?;

        cfg.g_w = u32::from(width);
        cfg.g_h = u32::from(height);
        // The profile is the chroma sampling and nothing else at eight bits. It
        // has to match the image format wrapped below, and `codec_string` tells
        // a decoder the same number.
        cfg.g_profile = u32::from(chroma.profile());
        cfg.g_threads = threads as u32;
        // Milliseconds, so a pts is elapsed wall-clock rather than a frame index.
        cfg.g_timebase.num = 1;
        cfg.g_timebase.den = 1000;
        // **0, not libvpx's default of 25.** The default holds 25 frames inside
        // the encoder before emitting anything, which on a desktop is most of a
        // second of latency. RustDesk works around the default by flushing after
        // every frame; zero is the same thing without the second code path, and
        // it is what makes one encode mean one packet.
        cfg.g_lag_in_frames = 0;
        cfg.g_pass = vpx::vpx_enc_pass_VPX_RC_ONE_PASS;
        // No error resilience. Nothing here is ever lost in transit — the link
        // is TCP — so it would cost compression to protect against loss that
        // cannot happen. The same argument removes the periodic keyframe below.
        cfg.g_error_resilient = 0;
        // No fixed keyframe interval; every keyframe is one somebody asked for —
        // a repaint, a resize, a client coming back.
        cfg.kf_mode = vpx::vpx_kf_mode_VPX_KF_DISABLED;
        // Disabling them is not enough: libvpx 1.16's one-pass rate control still
        // counts down `kf_max_dist`, 128 by default, and codes a keyframe when it
        // runs out. Measured, at frames 128 and 256 of a changing picture. Out of
        // reach instead.
        cfg.kf_max_dist = i32::MAX as u32;
        // Constant quality with the quantizer pinned top and bottom. `VPX_Q` plus
        // the `CQ_LEVEL` control below is what decides it; min == max is what
        // makes that a guarantee rather than a preference, and it is why nothing
        // here sets a bitrate — the quantizer *is* the dial and the bytes land
        // wherever the picture puts them.
        cfg.rc_end_usage = vpx::vpx_rc_mode_VPX_Q;
        cfg.rc_min_quantizer = q;
        cfg.rc_max_quantizer = q;
        // Zero, explicitly, and load-bearing rather than tidy: both users record
        // source pixels as delivered the moment they are captured, so a frame
        // libvpx decided to drop would be permanently wrong pixels that nothing
        // re-sends. It is also libvpx's default, and a default is a thing that
        // can change.
        cfg.rc_dropframe_thresh = 0;
        // Likewise: an encoder that resized itself would change the picture size
        // mid-stream, while a stream has one picture size for its whole life.
        cfg.rc_resize_allowed = 0;

        // SAFETY: `ctx` is zeroed and written through by `enc_init_ver`, `cfg` is
        // the struct libvpx just filled in and this function then edited by
        // name, and the ABI version is the one the linked archive's headers
        // declared — which is what makes a Rust-side layout disagreement a named
        // error here rather than memory corruption later.
        let mut ctx: Box<vpx::vpx_codec_ctx_t> = Box::new(unsafe { std::mem::zeroed() });
        check(
            unsafe { vpx::vpx_codec_enc_init_ver(&mut *ctx, iface, &cfg, 0, vpx::VPX_ENCODER_ABI_VERSION as c_int) },
            "enc_init_ver",
        )?;

        // Everything from here on has a context to destroy, so the error paths
        // go through a constructed `Self` rather than returning: `Drop` is what
        // releases the encoder, and an early `?` before the struct would leak it.
        let mut encoder = Self {
            ctx,
            cfg,
            // SAFETY: zeroed, then filled in by `vpx_img_wrap` below.
            img: unsafe { std::mem::zeroed() },
            size: (width, height),
            chroma,
            quality,
            started: std::time::Instant::now(),
            pts: -1,
        };

        // SAFETY: the context is live and each control's argument really is an
        // `int` — the one thing `vpx_codec_control_`'s variadic signature cannot
        // check.
        unsafe {
            // In `VPX_Q` mode this is the value that actually decides the
            // quantizer. A *control*, not a config field, which is the easiest
            // thing about libvpx's rate control to get wrong: setting only
            // `rc_min/max_quantizer` leaves `cq_level` at its default and the
            // dial half-connected.
            encoder.control(vpx::vp8e_enc_control_id_VP8E_SET_CQ_LEVEL, q as c_int, "cq_level")?;
            // What this encoder is actually looking at. Screen content is mostly
            // flat colour, hard edges and text, none of which a camera preset
            // expects.
            encoder.control(vpx::vp8e_enc_control_id_VP9E_SET_TUNE_CONTENT, vpx::vp9e_tune_content_VP9E_CONTENT_SCREEN as c_int, "tune_content")?;
            encoder.control(vpx::vp8e_enc_control_id_VP8E_SET_CPUUSED, CPU_USED, "cpuused")?;
            // Say in the bitstream what the conversion did: BT.601 matrix,
            // studio swing. libvpx writes *unknown* unless told, and a decoder
            // given unknown guesses — Chromium picks BT.709 for anything HD — so
            // without these two controls a 1080p desktop is converted with one
            // matrix and displayed with another, and every saturated colour
            // lands a little off. The decoder reads this off the keyframe
            // header; nothing on the wire has to carry it.
            encoder.control(vpx::vp8e_enc_control_id_VP9E_SET_COLOR_SPACE, vpx::vpx_color_space_VPX_CS_BT_601 as c_int, "color_space")?;
            encoder.control(vpx::vp8e_enc_control_id_VP9E_SET_COLOR_RANGE, vpx::vpx_color_range_VPX_CR_STUDIO_RANGE as c_int, "color_range")?;
            // Off: adaptive quantization would move the quantizer off the dial
            // that was just pinned.
            encoder.control(vpx::vp8e_enc_control_id_VP9E_SET_AQ_MODE, 0, "aq_mode")?;
            // Tile columns for the threads to have separate work — as many as
            // the width is wide enough for and the threads can fill. Set whatever
            // the thread count: libvpx's default is 6, every column the width
            // allows, which one thread would code one after another for the
            // bytes and nothing else.
            encoder.control(vpx::vp8e_enc_control_id_VP9E_SET_TILE_COLUMNS, tile_columns_log2(width, threads) as c_int, "tile_columns")?;
            if threads > 1 {
                // What turns `g_threads` into actual parallelism inside a tile:
                // row-based multithreading. Inert at one thread.
                encoder.control(vpx::vp8e_enc_control_id_VP9E_SET_ROW_MT, 1, "row_mt")?;
            }
            // An image that *wraps* a picture's planes rather than owning any. A
            // non-null pointer that is never dereferenced is libvpx's own idiom
            // for "compute the layout, allocate nothing" — ffmpeg passes a
            // literal `1` — and it is why `vpx_img_free` must never be called on
            // this image.
            let wrapped = vpx::vpx_img_wrap(&mut encoder.img, chroma.img_fmt(), cfg.g_w, cfg.g_h, 1, std::ptr::dangling_mut::<u8>());
            if wrapped.is_null() {
                return Err(Error::Codec { call: "img_wrap", detail: format!("refused a {width}x{height} picture") });
            }
        }
        Ok(encoder)
    }

    /// The picture size this encoder codes.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// The sampling this encoder codes.
    pub fn chroma(&self) -> Chroma {
        self.chroma
    }

    /// The dial this encoder is coding at.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Move the dial on the live encoder, without a keyframe: the next frame is
    /// coded at the new quantizer against the frames before it. This is how a
    /// link that is behind gives quality up, and "without a keyframe" is the
    /// whole reason it is written this way rather than by rebuilding the encoder,
    /// which would force a keyframe — the most bytes a frame can be — at the
    /// exact moment the link has run out of room.
    ///
    /// `vpx_codec_enc_config_set` is libvpx's own mechanism for it, and both
    /// halves are needed: the config carries the quantizer bounds and the control
    /// carries the value `VPX_Q` mode actually reads.
    pub fn set_quality(&mut self, quality: u8) -> Result<(), Error> {
        let quality = quality.clamp(QUALITY_MIN, QUALITY_MAX);
        let q = quality_to_q(quality);
        // Against the committed quality rather than `cfg`: a `cq_level` refused
        // after the config was accepted leaves `cfg` already at `q`, and the retry
        // must still send the control.
        if q == quality_to_q(self.quality) {
            self.quality = quality;
            return Ok(());
        }
        // Edited on a copy and kept only once libvpx has accepted it, so a
        // refusal leaves `cfg` describing the encoder as it still is.
        let mut cfg = self.cfg;
        cfg.rc_min_quantizer = q;
        cfg.rc_max_quantizer = q;
        // SAFETY: `cfg` is the struct libvpx validated at init with two fields
        // changed, and the context is live. libvpx re-validates it and returns a
        // code rather than accepting nonsense.
        unsafe {
            check(vpx::vpx_codec_enc_config_set(&mut *self.ctx, &cfg), "enc_config_set")?;
            self.cfg = cfg;
            self.control(vpx::vp8e_enc_control_id_VP8E_SET_CQ_LEVEL, q as c_int, "cq_level")?;
        }
        self.quality = quality;
        Ok(())
    }

    /// Encode `picture` and append the frame to `out`. `keyframe` makes it one a
    /// decoder can start from; the first frame is one either way. Returns whether
    /// the frame is a keyframe, or `None` when the encoder produced no bitstream
    /// and nothing was appended: with no lag and no dropped frames that should be
    /// unreachable, but it is a return value rather than an assertion because a
    /// caller has to be ready to carry its pixels over to the next frame anyway.
    pub fn encode(&mut self, picture: &Picture, keyframe: bool, out: &mut Vec<u8>) -> Result<Option<bool>, Error> {
        if picture.size() != self.size || picture.chroma() != self.chroma {
            let (w, h) = self.size;
            let (pw, ph) = picture.size();
            return Err(Error::Mismatch(w, h, self.chroma.name(), pw, ph, picture.chroma().name()));
        }
        // Strictly increasing, which two encodes inside one millisecond would
        // otherwise not be.
        self.pts = (self.started.elapsed().as_millis() as i64).max(self.pts + 1);
        // `vpx_enc_frame_flags_t` is a C `long`: 64 bits on Linux and macOS, 32 on
        // Windows. A cast rather than `From`, because no `From<u32>` exists for
        // the 32-bit case.
        let flags: vpx::vpx_enc_frame_flags_t = if keyframe { vpx::VPX_EFLAG_FORCE_KF as vpx::vpx_enc_frame_flags_t } else { 0 };
        let planes = picture.planes();
        let strides = picture.strides();

        // SAFETY: the three planes outlive this call — they belong to `picture`,
        // which is borrowed for it — and the strides are the ones the picture
        // reports for exactly this size. Casting away const is what the C API
        // requires; libvpx does not write to an input image. The packets drained
        // below point into the encoder and are copied out before it is called
        // again, which is the lifetime libvpx documents for them.
        unsafe {
            for i in 0..3 {
                self.img.planes[i] = planes[i].as_ptr().cast_mut();
                self.img.stride[i] = strides[i] as c_int;
            }
            check(
                vpx::vpx_codec_encode(
                    &mut *self.ctx,
                    &self.img,
                    self.pts,
                    // Duration: one tick of the timebase, deliberately a constant
                    // rather than the gap since the last frame. libvpx reads it
                    // for rate control, and there is no rate control here to read
                    // it: the quantizer is pinned, no bitrate is set, and no frame
                    // may be dropped. What must stay true is `pts`, which is real
                    // elapsed time — a decoder's timestamps read that, not this.
                    1,
                    flags,
                    vpx::VPX_DL_REALTIME as c_ulong,
                ),
                "encode",
            )?;
            let from = out.len();
            let mut keyframe = false;
            let mut packets = 0usize;
            let mut iter: vpx::vpx_codec_iter_t = std::ptr::null();
            loop {
                let packet = vpx::vpx_codec_get_cx_data(&mut *self.ctx, &mut iter);
                if packet.is_null() {
                    break;
                }
                if (*packet).kind != vpx::vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let frame = &(*packet).data.frame;
                out.extend_from_slice(std::slice::from_raw_parts(frame.buf.cast::<u8>(), frame.sz));
                keyframe |= frame.flags & vpx::VPX_FRAME_IS_KEY != 0;
                packets += 1;
            }
            // More than one packet is a superframe, which is a legal thing to
            // concatenate and hand to a decoder as one frame — but it also means
            // no lag is not doing what this module assumes, so it is said rather
            // than silently absorbed.
            if packets > 1 {
                log::warn!("vp9: one encode produced {packets} packets; expected one");
            }
            Ok((out.len() > from).then_some(keyframe))
        }
    }

    /// One `vpx_codec_control_` call with an `int` argument, checked.
    ///
    /// # Safety
    ///
    /// `id` must be a control whose argument really is an `int`. The call is
    /// variadic, so passing the wrong type compiles and corrupts the stack.
    unsafe fn control(&mut self, id: vpx::vp8e_enc_control_id, value: c_int, call: &'static str) -> Result<(), Error> {
        check(unsafe { vpx::vpx_codec_control_(&mut *self.ctx, id as c_int, value) }, call)
    }
}

// SAFETY: an `Encoder` owns its context exclusively — it is not `Clone`, and
// `encode` and `set_quality` take `&mut self` — and libvpx keeps no thread-local
// state for an encoder instance, so moving one between threads is sound. The
// gateway carries one onto a blocking worker for the encode and back.
//
// Deliberately **not** `Sync`. Two threads calling `vpx_codec_encode` on one
// context concurrently is undefined behaviour, and nothing needs to.
//
// `img`'s plane pointers borrow whatever picture was last encoded; they are
// overwritten at the top of every `encode` before libvpx reads them, so a stale
// pointer is never dereferenced.
unsafe impl Send for Encoder {}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: destroyed exactly once — `Encoder` is not `Clone` and holds the
        // only handle. The wrapped image is deliberately *not* freed:
        // `vpx_img_wrap` allocated nothing.
        unsafe {
            vpx::vpx_codec_destroy(&mut *self.ctx);
        }
    }
}

/// One VP9 stream's decoder: every frame through the same one, in the order
/// they came, since each is coded against the ones before it.
pub struct Decoder {
    /// Boxed for the same reason as the encoder's.
    ctx: Box<vpx::vpx_codec_ctx_t>,
}

impl Decoder {
    /// A decoder on `threads` threads. A decode gains nothing past the stream's
    /// tile columns, and the caller knows what else its machine is doing.
    pub fn new(threads: usize) -> Result<Self, Error> {
        if !(1..=MAX_THREADS).contains(&threads) {
            return Err(Error::Threads(threads));
        }
        // SAFETY: a static interface, a zeroed context written through by
        // `dec_init_ver`, and the ABI version of the linked archive's headers.
        unsafe {
            let iface = vpx::vpx_codec_vp9_dx();
            if iface.is_null() {
                return Err(Error::Codec { call: "vp9_dx", detail: "this libvpx has no VP9 decoder".to_owned() });
            }
            let cfg = vpx::vpx_codec_dec_cfg_t { threads: threads as c_uint, w: 0, h: 0 };
            let mut ctx: Box<vpx::vpx_codec_ctx_t> = Box::new(std::mem::zeroed());
            check(vpx::vpx_codec_dec_init_ver(&mut *ctx, iface, &cfg, 0, vpx::VPX_DECODER_ABI_VERSION as c_int), "dec_init_ver")?;
            Ok(Self { ctx })
        }
    }

    /// Decode one `frame` and return the picture it leaves on screen, which
    /// borrows the decoder until the next call.
    pub fn decode(&mut self, frame: &[u8]) -> Result<Decoded<'_>, Error> {
        // SAFETY: `frame` outlives the call. The image libvpx hands back belongs
        // to the decoder and is valid until it is called again, which the
        // returned borrow forbids.
        unsafe {
            check(vpx::vpx_codec_decode(&mut *self.ctx, frame.as_ptr(), frame.len() as c_uint, std::ptr::null_mut(), 0), "decode")?;
            let mut iter: vpx::vpx_codec_iter_t = std::ptr::null();
            let img = vpx::vpx_codec_get_frame(&mut *self.ctx, &mut iter);
            if img.is_null() {
                return Err(Error::NoPicture);
            }
            let img = &*img;
            let chroma = Chroma::of_img(img.fmt, img.bit_depth).ok_or(Error::Format(img.fmt as u32, img.bit_depth))?;
            Ok(Decoded { img, chroma })
        }
    }
}

// SAFETY: as the encoder's — one owner, `&mut self` for every call.
unsafe impl Send for Decoder {}

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
pub struct Decoded<'a> {
    img: &'a vpx::vpx_image_t,
    chroma: Chroma,
}

impl Decoded<'_> {
    /// The picture's size.
    pub fn size(&self) -> (u32, u32) {
        (self.img.d_w, self.img.d_h)
    }

    /// Which sampling the frame was coded at.
    pub fn chroma(&self) -> Chroma {
        self.chroma
    }

    /// Whether the stream declares its pixels BT.601 at studio swing, which is
    /// what every encoder here writes and what [`Self::write_bgrx`] assumes.
    pub fn declares_bt601_studio_swing(&self) -> bool {
        self.img.cs == vpx::vpx_color_space_VPX_CS_BT_601 && self.img.range == vpx::vpx_color_range_VPX_CR_STUDIO_RANGE
    }

    /// Y, U and V, each at its own stride from [`Self::strides`].
    pub fn planes(&self) -> [&[u8]; 3] {
        let (w, h) = (self.img.d_w as usize, self.img.d_h as usize);
        let (cw, ch) = self.chroma.plane_size(w, h);
        let plane = |i: usize, w: usize, h: usize| {
            let stride = self.img.stride[i] as usize;
            // SAFETY: the plane is the decoder's, laid out at the stride it
            // reports, and read only inside the size it reports.
            unsafe { std::slice::from_raw_parts(self.img.planes[i], (h - 1) * stride + w) }
        };
        [plane(0, w, h), plane(1, cw, ch), plane(2, cw, ch)]
    }

    /// The width of a row of each plane, in bytes.
    pub fn strides(&self) -> [usize; 3] {
        [self.img.stride[0] as usize, self.img.stride[1] as usize, self.img.stride[2] as usize]
    }

    /// Write the picture into `out` as `B, G, R, X`, rows `stride` bytes apart,
    /// the X byte zero. BT.601 at studio swing, as the encoders here declare.
    pub fn write_bgrx(&self, out: &mut [u8], stride: usize) -> Result<(), Error> {
        use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix};
        let (w, h) = (self.img.d_w as usize, self.img.d_h as usize);
        if !fits(w, h, stride, out.len()) {
            return Err(Error::Buffer { width: w, height: h, stride, bytes: out.len() });
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
        match self.chroma {
            Chroma::Full => yuv::yuv444_to_bgra(&image, out, stride as u32, YuvRange::Limited, YuvStandardMatrix::Bt601),
            Chroma::Subsampled => yuv::yuv420_to_bgra(&image, out, stride as u32, YuvRange::Limited, YuvStandardMatrix::Bt601),
        }
        .expect("the picture fits its buffer, which was checked");
        // It writes an opaque alpha where the X byte goes.
        for row in 0..h {
            for pixel in out[row * stride..row * stride + w * 4].as_chunks_mut::<4>().0 {
                pixel[3] = 0;
            }
        }
        Ok(())
    }
}

/// What the opening bits of a VP9 frame say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// 0 for 4:2:0, 1 for 4:4:4 at the eight bits every stream here has.
    pub profile: u8,
    /// Whether a decoder that has seen nothing before this frame can start here.
    pub keyframe: bool,
}

/// Read the first fields of a frame's uncompressed header (VP9 bitstream §6.2):
/// `frame_marker` (two bits, always 2), `profile_low_bit`, `profile_high_bit`, a
/// reserved zero bit on profile 3, `show_existing_frame`, and `frame_type`,
/// where 0 is a keyframe. A frame that only shows an earlier one is not a
/// keyframe.
///
/// For a stream this process did not encode, whose keyframe bit is therefore
/// not the encoder's to report. `None` for bytes that do not start a VP9 frame.
pub fn frame_header(frame: &[u8]) -> Option<FrameHeader> {
    let mut bits = frame.iter().take(2).flat_map(|byte| (0..8).rev().map(move |i| byte >> i & 1));
    let mut bit = || bits.next();
    if (bit()? << 1 | bit()?) != 2 {
        return None;
    }
    let low = bit()?;
    let profile = bit()? << 1 | low;
    if profile == 3 && bit()? != 0 {
        return None;
    }
    let show_existing = bit()? == 1;
    let keyframe = !show_existing && bit()? == 0;
    Some(FrameHeader { profile, keyframe })
}

/// VP9's levels, as `(level, max_luma_sample_rate, max_luma_picture_size,
/// max_luma_breadth)`.
///
/// Transcribed from `vp9_level_defs[]` in libvpx's own
/// `vp9/encoder/vp9_encoder.c` at the commit `libvpx-prebuilt` pins, and not
/// from memory: the table is the normative one, the rows are not evenly spaced,
/// and 4K30 lands on level 5.0 rather than the 5.1 a plausible guess gives. Only
/// the three fields a picture size can violate are kept — the rest of each row
/// is about bitrate, buffer size and tiling, none of which decides the level of
/// a stream whose quantizer is pinned.
///
/// The lowest row a picture fits is the level announced, which matters because
/// a level is a ceiling: a decoder that accepts 5.0 accepts every stream below
/// it, so announcing the smallest true level is the widest claim that is honest.
const LEVELS: [(u8, u64, u32, u16); 14] = [
    (10, 829_440, 36_864, 512),
    (11, 2_764_800, 73_728, 768),
    (20, 4_608_000, 122_880, 960),
    (21, 9_216_000, 245_760, 1_344),
    (30, 20_736_000, 552_960, 2_048),
    (31, 36_864_000, 983_040, 2_752),
    (40, 83_558_400, 2_228_224, 4_160),
    (41, 160_432_128, 2_228_224, 4_160),
    (50, 311_951_360, 8_912_896, 8_384),
    (51, 588_251_136, 8_912_896, 8_384),
    (52, 1_176_502_272, 8_912_896, 8_384),
    (60, 1_176_502_272, 35_651_584, 16_832),
    (61, 2_353_004_544, 35_651_584, 16_832),
    (62, 4_706_009_088, 35_651_584, 16_832),
];

/// The WebCodecs codec string for a `w`×`h` VP9 stream at `chroma`, every field
/// of it: `vp09.<profile>.<level>.<depth>.<chroma>.<primaries>.<transfer>.<matrix>.<range>`.
///
/// Eight-bit, because that is what the encoder produces, and the profile is the
/// chroma's. The optional fields are spelled out rather than left to their
/// defaults because the defaults are wrong for this stream on both counts: an
/// omitted chroma field means 4:2:0, which Chromium reads as 4:2:2 on a profile
/// 1 string since 4:2:0 is not a profile 1 picture, and omitted colour fields
/// mean BT.709 where the keyframe header says BT.601 (SMPTE 170M primaries,
/// transfer and matrix — code 6 each, what Chromium's own VP9 parser maps that
/// header flag to) at studio swing. The level comes from [`LEVELS`] at `fps`
/// frames a second. `None` for a picture no VP9 level covers.
pub fn codec_string(w: u16, h: u16, chroma: Chroma, fps: u64) -> Option<String> {
    let (profile, sampling) = match chroma {
        Chroma::Subsampled => ("00", "01"),
        Chroma::Full => ("01", "03"),
    };
    let size = u32::from(w) * u32::from(h);
    let rate = u64::from(size) * fps;
    let breadth = w.max(h);
    let (level, ..) = LEVELS.iter().find(|(_, max_rate, max_size, max_breadth)| rate <= *max_rate && size <= *max_size && breadth <= *max_breadth).copied()?;
    Some(format!("vp09.{profile}.{level:02}.08.{sampling}.06.06.06.00"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `w`×`h` of one colour, packed RGB.
    fn flat(w: u16, h: u16, colour: [u8; 3]) -> Vec<u8> {
        colour.iter().copied().cycle().take(usize::from(w) * usize::from(h) * 3).collect()
    }

    /// A picture read from `rgb`.
    fn picture(w: u16, h: u16, chroma: Chroma, rgb: &[u8]) -> Picture {
        let mut picture = Picture::new(w, h, chroma).expect("a picture");
        picture.read_rgb(rgb).expect("its own picture");
        picture
    }

    /// Encode `picture` and return the frame with its keyframe bit.
    fn encode(encoder: &mut Encoder, picture: &Picture, keyframe: bool) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let key = encoder.encode(picture, keyframe, &mut out).expect("an encode").expect("a frame");
        (out, key)
    }

    /// A chain of frames through one decoder, keyframe first, and the picture
    /// the last one leaves as `B, G, R, X` rows of `w` pixels.
    fn decode_chain(frames: &[Vec<u8>], w: usize, h: usize) -> Vec<u8> {
        let mut decoder = Decoder::new(1).expect("a decoder");
        let mut out = vec![0; w * h * 4];
        for frame in frames {
            let decoded = decoder.decode(frame).expect("a decode");
            assert_eq!(decoded.size(), (w as u32, h as u32));
            decoded.write_bgrx(&mut out, w * 4).expect("a picture that fits");
        }
        out
    }

    /// The pixel at `(x, y)` of a decoded picture, as RGB.
    fn rgb_at(bgrx: &[u8], w: usize, x: usize, y: usize) -> [u8; 3] {
        let p = &bgrx[(y * w + x) * 4..][..3];
        [p[2], p[1], p[0]]
    }

    /// A dark terminal with one-pixel coloured glyph stems: the picture 4:2:0
    /// cannot carry. Every stem is at an odd column and shares its 2×2 chroma
    /// group with three pixels of background.
    fn stems(w: u16, h: u16) -> (Vec<u8>, Vec<(usize, usize)>) {
        let (w, h) = (usize::from(w), usize::from(h));
        let mut rgb = flat(w as u16, h as u16, [30, 30, 30]);
        let mut at = Vec::new();
        for y in (0..h).filter(|y| y % 2 == 0) {
            for x in (1..w).step_by(4) {
                rgb[(y * w + x) * 3..][..3].copy_from_slice(&[255, 121, 198]);
                at.push((x, y));
            }
        }
        (rgb, at)
    }

    /// A 320×240 picture with a block moved by `step`, so there is something for
    /// the quantizer to be coarse about — a stream of identical frames costs
    /// nearly nothing at any quality and would compare two zeroes.
    fn moving(step: u16) -> Vec<u8> {
        let mut rgb = flat(320, 240, [30, 60, 90]);
        let stride = 320 * 3;
        for row in 0..80 {
            let at = (usize::from(step) * 3 + row) * stride + usize::from(step) * 9;
            rgb[at..at + 300].fill(230);
        }
        rgb
    }

    #[test]
    fn the_quality_dial_maps_onto_the_encoders_whole_range() {
        assert_eq!(quality_to_q(QUALITY_MIN), Q_COARSEST);
        assert_eq!(quality_to_q(QUALITY_MAX), Q_FINEST);
        assert_eq!(quality_to_q(0), Q_COARSEST, "clamped, not wrapped");
        assert_eq!(quality_to_q(255), Q_FINEST);
        for quality in 0..=u8::MAX {
            let q = quality_to_q(quality);
            assert!((Q_FINEST..=Q_COARSEST).contains(&q), "quality {quality} gave q {q}, outside VP9's range");
        }
        // Monotone: a higher dial is never a coarser picture.
        for quality in 1..100u8 {
            assert!(quality_to_q(quality) >= quality_to_q(quality + 1));
        }
    }

    /// Tile columns follow the width — one under 1920, two at 1080p, four at 4K
    /// and 5K — and never outnumber the threads.
    #[test]
    fn tile_columns_follow_the_width_and_never_outnumber_the_threads() {
        assert_eq!(tile_columns_log2(1280, 8), 0);
        assert_eq!(tile_columns_log2(1920, 8), 1);
        assert_eq!(tile_columns_log2(2560, 8), 1);
        assert_eq!(tile_columns_log2(3840, 8), 2);
        assert_eq!(tile_columns_log2(5120, 8), 2);
        assert_eq!(tile_columns_log2(3840, 2), 1);
        assert_eq!(tile_columns_log2(3840, 1), 0);
        assert_eq!(tile_columns_log2(3840, 0), 0);
    }

    #[test]
    fn a_picture_refuses_a_crop_that_is_not_its_picture() {
        let mut i420 = Picture::new(64, 32, Chroma::Subsampled).expect("a picture");
        i420.read_rgb(&flat(64, 32, [10, 20, 30])).expect("its own picture");
        assert!(i420.read_rgb(&flat(64, 31, [10, 20, 30])).is_err(), "a mis-sized crop would have indexed out of the planes");
        // I420: full-size luma, quarter-size chroma, both tight.
        let [y, u, v] = i420.planes();
        assert_eq!((y.len(), u.len(), v.len()), (64 * 32, 32 * 16, 32 * 16));
        assert_eq!(i420.strides(), [64, 32, 32]);
        // I444: three full-size planes.
        let mut i444 = Picture::new(64, 32, Chroma::Full).expect("a picture");
        i444.read_rgb(&flat(64, 32, [10, 20, 30])).expect("its own picture");
        assert!(i444.read_rgb(&flat(64, 31, [10, 20, 30])).is_err());
        let [y, u, v] = i444.planes();
        assert_eq!((y.len(), u.len(), v.len()), (64 * 32, 64 * 32, 64 * 32));
        assert_eq!(i444.strides(), [64, 64, 64]);
        // An odd side rounds its 4:2:0 chroma up rather than dropping a column.
        let odd = Picture::new(33, 17, Chroma::Subsampled).expect("a picture");
        assert_eq!(odd.strides(), [33, 17, 17]);
        assert_eq!(odd.planes()[1].len(), 17 * 9);
        assert!(matches!(Picture::new(0, 16, Chroma::Full), Err(Error::Empty(0, 16))));
    }

    /// The conversion's arithmetic, at the points BT.601 studio swing pins
    /// exactly: black and white land on 16 and 235, every grey is chroma-neutral
    /// at 128, and a saturated red is the strongest V a swing this size has. The
    /// tolerance is one code value, which is the rounding the integer
    /// coefficients are allowed.
    #[test]
    fn the_conversion_is_bt601_studio_swing() {
        let mut i420 = Picture::new(2, 2, Chroma::Subsampled).expect("a picture");
        let mut i444 = Picture::new(2, 2, Chroma::Full).expect("a picture");
        let close = |got: u8, want: u8, what: &str| {
            assert!(got.abs_diff(want) <= 1, "{what}: got {got}, wanted {want}");
        };
        for (colour, y_want, u_want, v_want, name) in [
            ([0u8, 0, 0], 16u8, 128u8, 128u8, "black"),
            ([255, 255, 255], 235, 128, 128, "white"),
            ([128, 128, 128], 126, 128, 128, "mid grey"),
            ([255, 0, 0], 81, 90, 240, "red"),
            ([0, 0, 255], 41, 240, 110, "blue"),
        ] {
            for yuv in [&mut i420, &mut i444] {
                yuv.read_rgb(&flat(2, 2, colour)).expect("a 2x2 picture");
                let [y, u, v] = yuv.planes();
                close(y[0], y_want, name);
                close(u[0], u_want, name);
                close(v[0], v_want, name);
                // The same pixels as `B, G, R, X` land on the same planes.
                let bgrx: Vec<u8> = (0..4).flat_map(|_| [colour[2], colour[1], colour[0], 0x55]).collect();
                yuv.read_bgrx(&bgrx, 8).expect("a 2x2 picture");
                let [y, u, v] = yuv.planes();
                close(y[0], y_want, name);
                close(u[0], u_want, name);
                close(v[0], v_want, name);
            }
        }
        // At 4:2:0 the chroma sample is the 2×2 average, not the top-left pixel:
        // a checkerboard of full red and full blue meets in the middle. At 4:4:4
        // each pixel keeps its own.
        let mut quad = Vec::new();
        quad.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
        quad.extend_from_slice(&[0, 0, 255, 255, 0, 0]);
        i420.read_rgb(&quad).expect("a 2x2 picture");
        let [_, u, v] = i420.planes();
        close(u[0], 165, "checkerboard U");
        close(v[0], 175, "checkerboard V");
        i444.read_rgb(&quad).expect("a 2x2 picture");
        let [_, u, v] = i444.planes();
        close(u[0], 90, "red pixel U");
        close(v[0], 240, "red pixel V");
        close(u[1], 240, "blue pixel U");
        close(v[1], 110, "blue pixel V");
    }

    /// The two conversions are each other's inverse to within the rounding a
    /// studio-swing round trip costs, on every channel's full range, through a
    /// real encode at the finest quantizer.
    #[test]
    fn the_colour_conversion_round_trips_through_the_codec() {
        let colours: Vec<[u8; 4]> = (0..=255u8)
            .step_by(15)
            .flat_map(|r| (0..=255u8).step_by(51).flat_map(move |g| (0..=255u8).step_by(85).map(move |b| [b, g, r, 0])))
            .collect();
        let width = colours.len();
        let pixels: Vec<u8> = colours.iter().flatten().copied().collect();
        let mut picture = Picture::new(width as u16, 1, Chroma::Full).expect("a picture");
        picture.read_bgrx(&pixels, width * 4).expect("a row");
        assert!(picture.planes()[0].iter().all(|&y| (16..=235).contains(&y)), "luma stays in studio range");

        let mut encoder = Encoder::new(width as u16, 1, Chroma::Full, QUALITY_MAX, 1).expect("an encoder");
        let (frame, _) = encode(&mut encoder, &picture, false);
        let back = decode_chain(&[frame], width, 1);
        for (i, (want, got)) in pixels.as_chunks::<4>().0.iter().zip(back.as_chunks::<4>().0).enumerate() {
            // Studio swing folds 256 levels into 219 and the quantizer costs a
            // little more; a wrong matrix would be off by tens.
            for channel in 0..3 {
                assert!(want[channel].abs_diff(got[channel]) <= 6, "pixel {i}: {want:?} came back {got:?}");
            }
            assert_eq!(got[3], 0, "X is written as zero");
        }
    }

    /// A last row with nothing after it is converted like the rows before it,
    /// which have a stride's padding after them, and written back the same way.
    #[test]
    fn the_last_row_needs_no_padding_after_it() {
        let (width, height, stride) = (3usize, 3usize, 3 * 4 + 8);
        let colour = |row: usize| [40 * row as u8, 200 - 50 * row as u8, 90, 0];
        let mut pixels = vec![0xAA; (height - 1) * stride + width * 4];
        for row in 0..height {
            for x in 0..width {
                pixels[row * stride + x * 4..][..4].copy_from_slice(&colour(row));
            }
        }
        for chroma in [Chroma::Full, Chroma::Subsampled] {
            let mut picture = Picture::new(width as u16, height as u16, chroma).expect("a picture");
            picture.read_bgrx(&pixels, stride).expect("a picture that fits");
            let mut encoder = Encoder::new(width as u16, height as u16, chroma, QUALITY_MAX, 1).expect("an encoder");
            let (frame, _) = encode(&mut encoder, &picture, false);
            let mut decoder = Decoder::new(1).expect("a decoder");
            let mut back = vec![0xAA; (height - 1) * stride + width * 4];
            decoder.decode(&frame).expect("a decode").write_bgrx(&mut back, stride).expect("a picture that fits");
            for row in 0..height {
                for x in 0..width {
                    let got = &back[row * stride + x * 4..][..4];
                    let want = colour(row);
                    // What is tested is the stride, not the quantizer: the
                    // finest one still costs a few code values on a 3×3
                    // picture, and 4:2:0 averages this picture's edge pixels
                    // with their neighbours' besides.
                    let within = if chroma == Chroma::Full { 8 } else { 40 };
                    assert!((0..3).all(|c| want[c].abs_diff(got[c]) <= within) && got[3] == 0, "{chroma:?} row {row}: {want:?} came back {got:?}");
                }
            }
            assert!(back[width * 4..stride].iter().all(|&b| b == 0xAA), "the padding is not written");
        }
    }

    #[test]
    fn a_picture_that_does_not_fit_its_buffer_is_refused() {
        assert!(fits(4, 2, 16, 32));
        assert!(fits(4, 2, 20, 36), "the last row needs no padding after it");
        assert!(!fits(4, 2, 12, 32), "a stride shorter than a row");
        assert!(!fits(4, 2, 16, 31));
        assert!(fits(0, 0, 0, 0));
        assert!(!fits(4, 1, usize::MAX, 16), "a stride the conversion cannot carry, however the bytes add up");
        let mut picture = Picture::new(4, 2, Chroma::Full).expect("a picture");
        assert!(matches!(picture.read_bgrx(&[0; 12], 16), Err(Error::Buffer { .. })));
    }

    #[test]
    fn a_lower_quality_makes_a_smaller_stream() {
        let bytes = |quality| {
            let mut encoder = Encoder::new(320, 240, Chroma::Subsampled, quality, 2).expect("an encoder");
            (0..5u16).map(|step| encode(&mut encoder, &picture(320, 240, Chroma::Subsampled, &moving(step)), false).0.len()).sum::<usize>()
        };
        let (coarse, fine) = (bytes(5), bytes(90));
        assert!(coarse < fine, "quality 5 encoded {coarse} bytes and quality 90 encoded {fine}; the dial is not reaching the encoder");
    }

    #[test]
    fn the_first_frame_is_a_keyframe_and_another_can_be_asked_for() {
        let mut encoder = Encoder::new(320, 240, Chroma::Subsampled, 60, 2).expect("an encoder");
        let mut frame = |step, keyframe| encode(&mut encoder, &picture(320, 240, Chroma::Subsampled, &moving(step)), keyframe);
        let first = frame(0, false);
        assert!(first.1, "a decoder has to be able to start somewhere");
        assert_eq!(frame_header(&first.0), Some(FrameHeader { profile: 0, keyframe: true }));
        assert!(!frame(1, false).1, "an unasked-for keyframe is bytes for nothing");
        assert!(!frame(2, false).1);
        let asked = frame(3, true);
        assert!(asked.1, "the keyframe flag did not reach the encoder");
        assert_eq!(frame_header(&asked.0), Some(FrameHeader { profile: 0, keyframe: true }));
        // And it is not sticky: the frame after a forced keyframe is an ordinary one.
        let after = frame(4, false);
        assert!(!after.1);
        assert_eq!(frame_header(&after.0), Some(FrameHeader { profile: 0, keyframe: false }));
    }

    /// However long a stream runs, no keyframe comes that was not asked for, at
    /// either chroma.
    #[test]
    fn no_keyframe_comes_unasked() {
        let (w, h) = (64u16, 48u16);
        for chroma in [Chroma::Subsampled, Chroma::Full] {
            let mut encoder = Encoder::new(w, h, chroma, 60, 1).expect("an encoder");
            let mut rgb = flat(w, h, [80, 40, 20]);
            for step in 0..300u32 {
                let x = (step * 3) as usize % 48;
                for y in 8..24 {
                    rgb[(y * usize::from(w) + x) * 3..][..3].copy_from_slice(&[step as u8, (step * 7) as u8, (step * 13) as u8]);
                }
                let (frame, keyframe) = encode(&mut encoder, &picture(w, h, chroma, &rgb), false);
                assert_eq!(keyframe, step == 0, "{chroma:?} frame {step}");
                assert_eq!(frame_header(&frame).map(|header| header.profile), Some(chroma.profile()), "{chroma:?} frame {step}");
            }
        }
    }

    #[test]
    fn a_frame_header_is_read_from_the_bitstream_alone() {
        assert_eq!(frame_header(&[]), None);
        assert_eq!(frame_header(&[0x00, 0x00]), None, "no frame marker");
    }

    /// What a settle rests on: re-encoding a picture that has not changed, at a
    /// finer quantizer, sharpens the *unchanged* blocks — as an ordinary inter
    /// frame, with no keyframe. Were libvpx to skip blocks whose source had not
    /// moved, a desktop sent coarse on a link that was behind would stay coarse
    /// until it next changed, and the settle would have to spend a keyframe.
    #[test]
    fn a_finer_quantizer_sharpens_an_unchanged_picture_without_a_keyframe() {
        let (w, h) = (320u16, 240u16);
        // Speckle, so a coarse quantizer has detail to lose.
        let mut rgb = flat(w, h, [240, 240, 240]);
        let mut seed = 12_345u32;
        for px in rgb.chunks_mut(3) {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if (seed >> 16).is_multiple_of(5) {
                px.copy_from_slice(&[20, 20, 20]);
            }
        }
        let source = picture(w, h, Chroma::Subsampled, &rgb);
        let (wu, hu) = (usize::from(w), usize::from(h));
        let error = |bgrx: &[u8]| {
            let sum: u64 = (0..hu).flat_map(|y| (0..wu).map(move |x| (x, y))).map(|(x, y)| u64::from(rgb_at(bgrx, wu, x, y)[1].abs_diff(rgb[(y * wu + x) * 3 + 1]))).sum();
            sum as f64 / (wu * hu) as f64
        };

        let mut encoder = Encoder::new(w, h, Chroma::Subsampled, QUALITY_MIN, 2).expect("an encoder");
        let mut frames = vec![encode(&mut encoder, &source, false).0];
        let coarse = error(&decode_chain(&frames, wu, hu));

        encoder.set_quality(90).expect("the encoder to accept a new quantizer");
        let (settle, keyframe) = encode(&mut encoder, &source, false);
        assert!(!keyframe, "the settle frame cost a keyframe");
        frames.push(settle);
        let settled = error(&decode_chain(&frames, wu, hu));

        let mut fresh = Encoder::new(w, h, Chroma::Subsampled, 90, 2).expect("an encoder");
        let fine = error(&decode_chain(&[encode(&mut fresh, &source, false).0], wu, hu));
        assert!(
            settled < coarse / 4.0 && settled < fine * 2.0,
            "one frame at quality 90 left the unchanged picture at error {settled:.2} (coarse {coarse:.2}, a quality-90 keyframe {fine:.2}): the encoder skipped blocks that did not move"
        );
    }

    /// The mechanism a quality walk rests on: quality can be given up
    /// mid-stream without spending a keyframe to do it.
    #[test]
    fn the_quality_moves_on_a_live_encoder_without_a_keyframe() {
        let mut encoder = Encoder::new(320, 240, Chroma::Subsampled, 90, 2).expect("an encoder");
        assert_eq!(encoder.quality(), 90);
        let frame = |encoder: &mut Encoder, step| encode(encoder, &picture(320, 240, Chroma::Subsampled, &moving(step)), false);
        frame(&mut encoder, 0);
        let fine: usize = (1..5).map(|step| frame(&mut encoder, step).0.len()).sum();

        encoder.set_quality(QUALITY_MIN).expect("the encoder to accept a new quantizer");
        assert_eq!(encoder.quality(), QUALITY_MIN);
        let coarse: Vec<_> = (5..9).map(|step| frame(&mut encoder, step)).collect();
        assert!(coarse.iter().map(|(data, _)| data.len()).sum::<usize>() < fine, "the quantizer did not reach the running encoder");
        assert!(!coarse.iter().any(|(_, keyframe)| *keyframe), "moving the quantizer cost a keyframe, which is what this avoids");

        encoder.set_quality(0).expect("a clamp");
        assert_eq!(encoder.quality(), QUALITY_MIN, "clamped, not wrapped");
    }

    /// An odd side is carried as it is, at both chromas, and decodes at its own
    /// size.
    #[test]
    fn an_odd_sized_picture_encodes_and_decodes_at_its_own_size() {
        let (w, h) = (33u16, 17u16);
        for chroma in [Chroma::Full, Chroma::Subsampled] {
            let source = picture(w, h, chroma, &flat(w, h, [40, 180, 90]));
            let mut encoder = Encoder::new(w, h, chroma, QUALITY_MAX, 1).expect("an encoder");
            let (frame, _) = encode(&mut encoder, &source, false);
            let mut decoder = Decoder::new(1).expect("a decoder");
            let decoded = decoder.decode(&frame).expect("a decode");
            assert_eq!(decoded.size(), (33, 17), "{chroma:?}");
            assert_eq!(decoded.chroma(), chroma);
            let mut out = vec![0; 33 * 17 * 4];
            decoded.write_bgrx(&mut out, 33 * 4).expect("a picture that fits");
            for pixel in out.as_chunks::<4>().0 {
                assert!(pixel[0].abs_diff(90) <= 3 && pixel[1].abs_diff(180) <= 3 && pixel[2].abs_diff(40) <= 3, "{chroma:?}: {pixel:?}");
            }
        }
        // 1919×1079 as a desktop is, at the size the gateway once padded.
        let source = picture(1919, 1079, Chroma::Subsampled, &flat(1919, 1079, [90, 90, 90]));
        let mut encoder = Encoder::new(1919, 1079, Chroma::Subsampled, 60, 2).expect("an encoder");
        assert!(encoder.encode(&source, false, &mut Vec::new()).expect("an encode").is_some());
    }

    /// Only the frames that are asked to be keyframes are, and the ones between
    /// decode against them.
    #[test]
    fn keyframes_come_when_asked_and_the_frames_between_decode() {
        let (w, h) = (64u16, 32u16);
        let mut encoder = Encoder::new(w, h, Chroma::Full, 60, 1).expect("an encoder");
        let mut decoder = Decoder::new(1).expect("a decoder");
        let mut out = vec![0; 64 * 32 * 4];
        for step in 0..5usize {
            let mut rgb = flat(w, h, [80, 40, 20]);
            for x in step * 4..step * 4 + 8 {
                for y in 0..32 {
                    rgb[(y * 64 + x) * 3..][..3].copy_from_slice(&[240, 240, 240]);
                }
            }
            let (frame, keyframe) = encode(&mut encoder, &picture(w, h, Chroma::Full, &rgb), step == 3);
            assert_eq!(keyframe, step == 0 || step == 3, "frame {step}");
            decoder.decode(&frame).expect("a decode").write_bgrx(&mut out, 64 * 4).expect("a picture that fits");
            let lit = &out[(step * 4 + 4) * 4..][..3];
            assert!(lit.iter().all(|&c| c > 200), "frame {step} did not decode to its own picture: {lit:?}");
        }
    }

    /// The level table, at the rows a desktop actually lands on. The numbers are
    /// libvpx's own; what is tested is that the *lowest* fitting row is chosen,
    /// since a level is a ceiling and announcing a higher one narrows the set of
    /// decoders that will accept the stream.
    #[test]
    fn the_codec_string_names_the_lowest_level_that_fits() {
        let codec_string = |w, h| super::codec_string(w, h, Chroma::Subsampled, 30);
        // 1280x800 at 30: 1_024_000 samples. Level 3.1 allows only 983_040 of
        // them, so this is level 4 — the *picture size* binds here, not the
        // sample rate, which at 30_720_000 is well inside 3.1's 36_864_000.
        assert_eq!(codec_string(1280, 800).as_deref(), Some("vp09.00.40.08.01.06.06.06.00"));
        // 1920x1080: 2_073_600 samples, inside level 4's picture size of
        // 2_228_224, and 62_208_000 per second inside its 83_558_400.
        assert_eq!(codec_string(1920, 1080).as_deref(), Some("vp09.00.40.08.01.06.06.06.00"));
        // 3840x2160: 8_294_400 samples — past 4.1's picture size, inside 5.0's
        // 8_912_896 — and 248_832_000 per second, inside 5.0's 311_951_360.
        assert_eq!(codec_string(3840, 2160).as_deref(), Some("vp09.00.50.08.01.06.06.06.00"));
        // 3840x2400: 9_216_000 samples, past every level 5's 8_912_896 picture
        // size by the 16:10 panel's extra 240 rows — level 6.0, the first that
        // holds it.
        assert_eq!(codec_string(3840, 2400).as_deref(), Some("vp09.00.60.08.01.06.06.06.00"));
        // Small, but not level 1: 76_800 samples is already past level 1.1's
        // 73_728. Level 1 is 256x144, which no desktop is.
        assert_eq!(codec_string(320, 240).as_deref(), Some("vp09.00.20.08.01.06.06.06.00"));
        // The bit depth is fixed — eight, whatever the size — and the profile and
        // chroma field are the chroma's: 4:4:4 is profile 1 with sampling 03 at
        // the same level, since a level is about luma samples. The colour fields
        // never move: BT.601 at studio swing is what every keyframe header
        // declares, and a string that left them out would be read as BT.709.
        for (w, h) in [(320u16, 240u16), (1920, 1080), (3840, 2160), (3840, 2400)] {
            let string = codec_string(w, h).expect("a level for a real desktop");
            assert!(string.starts_with("vp09.00."), "not profile 0: {string}");
            assert!(string.ends_with(".08.01.06.06.06.00"), "not 8-bit 4:2:0 BT.601: {string}");
            let full = super::codec_string(w, h, Chroma::Full, 30).expect("a level for a real desktop");
            assert!(full.ends_with(".08.03.06.06.06.00"), "not 8-bit 4:4:4 BT.601: {full}");
            assert_eq!(full, string.replacen("vp09.00.", "vp09.01.", 1).replacen(".08.01.", ".08.03.", 1), "{w}x{h}");
        }
        // At 60 frames a second the sample rate binds where the picture size did
        // not: 1080p60 is 124_416_000 samples a second, past 4.0's 83_558_400,
        // and 4K60 497_664_000, past 5.0's 311_951_360.
        let at_60 = |w, h| super::codec_string(w, h, Chroma::Full, 60);
        assert_eq!(at_60(1920, 1080).as_deref(), Some("vp09.01.41.08.03.06.06.06.00"));
        assert_eq!(at_60(3840, 2160).as_deref(), Some("vp09.01.51.08.03.06.06.06.00"));
        assert_eq!(super::codec_string(20_000, 20_000, Chroma::Full, 60), None, "no level covers it");
    }

    /// **Where the picture loss on a desktop stream is.** At the dial's finest
    /// quantizer a 4:2:0 stream returns a one-pixel coloured glyph stem at a
    /// fraction of its colour, because the stem's one chroma sample is an
    /// average with three background pixels — and a 4:4:4 stream returns it as
    /// it was. The quantizer is the same in both, so the difference is the
    /// sampling and nothing else.
    #[test]
    fn a_444_stream_keeps_the_colour_420_averages_away() {
        let (rgb, at) = stems(64, 64);
        let worst = |chroma: Chroma| -> u8 {
            let mut encoder = Encoder::new(64, 64, chroma, QUALITY_MAX, 1).expect("an encoder");
            let (frame, _) = encode(&mut encoder, &picture(64, 64, chroma, &rgb), false);
            let decoded = decode_chain(&[frame], 64, 64);
            at.iter()
                .map(|&(x, y)| rgb_at(&decoded, 64, x, y).iter().zip([255u8, 121, 198]).map(|(a, b)| a.abs_diff(b)).max().unwrap())
                .max()
                .unwrap()
        };
        let (subsampled, full) = (worst(Chroma::Subsampled), worst(Chroma::Full));
        assert!(subsampled >= 40, "4:2:0 returned the stems within {subsampled} code values — the picture is not the one this test is about");
        assert!(full <= 24, "4:4:4 returned a stem pixel {full} code values off at the finest quantizer");
        assert!(full * 2 < subsampled, "4:4:4 ({full}) is not clearly better than 4:2:0 ({subsampled})");
    }

    /// The keyframe header says which matrix and range the pixels were converted
    /// with, so a decoder does not guess — and guesses BT.709 for an HD picture,
    /// which is not what the conversion did. The decoder reads the profile off
    /// the same header.
    #[test]
    fn the_bitstream_declares_bt601_studio_swing_at_its_chroma() {
        for chroma in [Chroma::Subsampled, Chroma::Full] {
            let mut encoder = Encoder::new(64, 64, chroma, 60, 1).expect("an encoder");
            let (frame, _) = encode(&mut encoder, &picture(64, 64, chroma, &flat(64, 64, [200, 30, 30])), false);
            let mut decoder = Decoder::new(1).expect("a decoder");
            let decoded = decoder.decode(&frame).expect("a decode");
            assert!(decoded.declares_bt601_studio_swing(), "{chroma:?}");
            assert_eq!(decoded.chroma(), chroma, "the profile did not reach the bitstream");
            let (cw, ch) = chroma.plane_size(64, 64);
            assert_eq!(decoded.planes()[1].len(), (ch - 1) * decoded.strides()[1] + cw, "{chroma:?}");
        }
    }

    #[test]
    fn a_wrong_picture_or_frame_is_an_error_rather_than_a_read_past_the_end() {
        let mut encoder = Encoder::new(32, 16, Chroma::Full, 60, 1).expect("an encoder");
        let other = picture(16, 16, Chroma::Full, &flat(16, 16, [0, 0, 0]));
        assert!(matches!(encoder.encode(&other, false, &mut Vec::new()), Err(Error::Mismatch(32, 16, _, 16, 16, _))));
        let subsampled = picture(32, 16, Chroma::Subsampled, &flat(32, 16, [0, 0, 0]));
        assert!(matches!(encoder.encode(&subsampled, false, &mut Vec::new()), Err(Error::Mismatch(..))));
        assert!(matches!(Encoder::new(0, 16, Chroma::Full, 60, 1), Err(Error::Empty(0, 16))));
        assert!(matches!(Encoder::new(16, 16, Chroma::Full, 60, 0), Err(Error::Threads(0))));
        assert!(matches!(Decoder::new(0), Err(Error::Threads(0))));

        let (frame, _) = encode(&mut encoder, &picture(32, 16, Chroma::Full, &flat(32, 16, [0, 0, 0])), false);
        let mut decoder = Decoder::new(1).expect("a decoder");
        assert!(decoder.decode(&[0xFF, 0x00, 0x12]).is_err());
        let decoded = decoder.decode(&frame).expect("a decode");
        let mut out = [0; 10];
        assert!(matches!(decoded.write_bgrx(&mut out, 128), Err(Error::Buffer { .. })));
    }
}
