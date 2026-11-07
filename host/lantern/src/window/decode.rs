//! Video decoding: VA-API (`/dev/dri/renderD128`) with a software fallback, to NV12.
//!
//! Packets are complete access units (the helper's `MediaCodec` output). Config packets
//! (VPS/SPS/PPS, flag bit0) are held and fed in front of the next frame. A config that differs
//! from the current one (a resize: new size, new SPS) reopens the decoder so the next keyframe
//! starts clean at the new size.

use std::ffi::CStr;
use std::ptr;

use anyhow::{bail, Result};
use ffmpeg_sys_next as ff;
use lantern_proto::VideoPacket;

pub const VAAPI_DEVICE: &CStr = c"/dev/dri/renderD128";

/// One decoded picture in NV12: `y` is `h` rows of `y_stride` bytes, `uv` is `(h+1)/2` rows of
/// `uv_stride` bytes of interleaved U,V.
#[derive(Debug, Clone)]
pub struct Nv12Frame {
    pub w: u32,
    pub h: u32,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    pub y_stride: u32,
    pub uv_stride: u32,
    pub colour: Colour,
}

/// The YUV→RGB matrix of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// How to turn a frame's YUV into RGB: matrix and quantization range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub matrix: Matrix,
    /// `true` for full (JPEG, 0–255) range, `false` for limited (TV, 16–235/240).
    pub full_range: bool,
}

impl Default for Colour {
    fn default() -> Self {
        Colour {
            matrix: Matrix::Bt709,
            full_range: false,
        }
    }
}

/// Follows the stream's signalled colour. Unspecified matrix: BT.601 below 720 lines, BT.709
/// otherwise. Unspecified range: limited, unless the pixel format is a full-range (`yuvj`) one.
pub fn colour_for(
    space: ff::AVColorSpace,
    range: ff::AVColorRange,
    full_range_format: bool,
    height: u32,
) -> Colour {
    use ff::AVColorRange::*;
    use ff::AVColorSpace::*;
    let matrix = match space {
        AVCOL_SPC_BT709 => Matrix::Bt709,
        AVCOL_SPC_BT470BG | AVCOL_SPC_SMPTE170M | AVCOL_SPC_FCC => Matrix::Bt601,
        AVCOL_SPC_BT2020_NCL | AVCOL_SPC_BT2020_CL => Matrix::Bt2020,
        _ if height < 720 => Matrix::Bt601,
        _ => Matrix::Bt709,
    };
    let full_range = match range {
        AVCOL_RANGE_JPEG => true,
        AVCOL_RANGE_MPEG => false,
        _ => full_range_format,
    };
    Colour { matrix, full_range }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vaapi,
    Software,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::Vaapi => "vaapi",
            Backend::Software => "software",
        }
    }
}

fn av_err(code: i32) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: buf is valid for its length; av_strerror NUL-terminates.
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn codec_id(codec: &str) -> Result<ff::AVCodecID> {
    Ok(match codec {
        "h265" | "hevc" => ff::AVCodecID::AV_CODEC_ID_HEVC,
        "h264" | "avc" => ff::AVCodecID::AV_CODEC_ID_H264,
        other => bail!("unsupported codec {other:?}"),
    })
}

/// Picks VA-API surfaces when ffmpeg offers them, else the first software (non-hwaccel)
/// format; `NONE` if there is none.
unsafe extern "C" fn get_format_vaapi(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    use ff::AVPixelFormat::{AV_PIX_FMT_NONE, AV_PIX_FMT_VAAPI};
    let mut software = AV_PIX_FMT_NONE;
    let mut p = fmts;
    while *p != AV_PIX_FMT_NONE {
        if *p == AV_PIX_FMT_VAAPI {
            return *p;
        }
        let desc = ff::av_pix_fmt_desc_get(*p);
        let hw = desc.is_null() || (*desc).flags & ff::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0;
        if !hw && software == AV_PIX_FMT_NONE {
            software = *p;
        }
        p = p.add(1);
    }
    software
}

/// Unrefs a packet on every exit path.
struct PacketRef(*mut ff::AVPacket);

impl Drop for PacketRef {
    fn drop(&mut self) {
        // SAFETY: the packet is valid for the guard's lifetime (owned by `Ctx`).
        unsafe { ff::av_packet_unref(self.0) }
    }
}

/// An open `AVCodecContext` plus its scratch packet/frames. Freed on drop.
struct Ctx {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    sw: *mut ff::AVFrame,
}

impl Ctx {
    fn open(id: ff::AVCodecID, backend: Backend) -> Result<Self> {
        // SAFETY: plain FFmpeg allocation/opening; every pointer is checked and owned by `Ctx`,
        // which frees them in Drop (also on the early-return paths, as `c` is built first).
        unsafe {
            let codec = ff::avcodec_find_decoder(id);
            if codec.is_null() {
                bail!("no ffmpeg decoder for {id:?}");
            }
            let c = Ctx {
                ctx: ff::avcodec_alloc_context3(codec),
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                sw: ff::av_frame_alloc(),
            };
            if c.ctx.is_null() || c.pkt.is_null() || c.frame.is_null() || c.sw.is_null() {
                bail!("ffmpeg allocation failed");
            }
            (*c.ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Slice threads only: frame threads would hold frames back (latency).
            (*c.ctx).thread_type = ff::FF_THREAD_SLICE;
            (*c.ctx).thread_count = 0;
            if backend == Backend::Vaapi {
                let mut dev: *mut ff::AVBufferRef = ptr::null_mut();
                let r = ff::av_hwdevice_ctx_create(
                    &mut dev,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    VAAPI_DEVICE.as_ptr(),
                    ptr::null_mut(),
                    0,
                );
                if r < 0 {
                    bail!(
                        "VA-API device {}: {}",
                        VAAPI_DEVICE.to_string_lossy(),
                        av_err(r)
                    );
                }
                (*c.ctx).hw_device_ctx = dev; // the context owns the reference now
                (*c.ctx).get_format = Some(get_format_vaapi);
            }
            let r = ff::avcodec_open2(c.ctx, codec, ptr::null_mut());
            if r < 0 {
                bail!("avcodec_open2: {}", av_err(r));
            }
            Ok(c)
        }
    }

    /// Sends one packet and collects every frame it yields.
    fn decode(
        &mut self,
        data: &[u8],
        pts: i64,
        out: &mut Vec<Nv12Frame>,
    ) -> Result<Option<Backend>> {
        let mut seen = None;
        // SAFETY: self's pointers are valid (see `open`); the packet buffer is sized by
        // av_new_packet before the copy.
        unsafe {
            let r = ff::av_new_packet(self.pkt, data.len() as i32);
            if r < 0 {
                bail!("av_new_packet: {}", av_err(r));
            }
            let _unref = PacketRef(self.pkt);
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            (*self.pkt).pts = pts;
            let mut sent = false;
            while !sent {
                let r = ff::avcodec_send_packet(self.ctx, self.pkt);
                if r == ff::AVERROR(libc::EAGAIN) {
                    // Output is full: drain, then resend.
                } else if r < 0 {
                    bail!("avcodec_send_packet: {}", av_err(r));
                } else {
                    sent = true;
                }
                loop {
                    let r = ff::avcodec_receive_frame(self.ctx, self.frame);
                    if r == ff::AVERROR(libc::EAGAIN) || r == ff::AVERROR_EOF {
                        break;
                    }
                    if r < 0 {
                        bail!("avcodec_receive_frame: {}", av_err(r));
                    }
                    let res = self.convert();
                    ff::av_frame_unref(self.frame);
                    let (frame, backend) = res?;
                    seen = Some(backend);
                    out.push(frame);
                }
            }
        }
        Ok(seen)
    }

    /// Converts `self.frame` (VA-API surface or software YUV) to NV12.
    unsafe fn convert(&mut self) -> Result<(Nv12Frame, Backend)> {
        let mut src = self.frame;
        let mut backend = Backend::Software;
        // From the decoded frame itself: a VA-API transfer copy does not carry it.
        let colour = {
            let d = &*self.frame;
            let yuvj = d.format == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
            colour_for(d.colorspace, d.color_range, yuvj, d.height.max(0) as u32)
        };
        if (*src).format == ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
            ff::av_frame_unref(self.sw);
            let r = ff::av_hwframe_transfer_data(self.sw, src, 0);
            if r < 0 {
                bail!("av_hwframe_transfer_data: {}", av_err(r));
            }
            src = self.sw;
            backend = Backend::Vaapi;
        }
        let f = &*src;
        let (w, h) = (f.width as usize, f.height as usize);
        let ch = h.div_ceil(2);
        let plane = |i: usize, rows: usize| -> Result<&[u8]> {
            let ls = f.linesize[i];
            if ls <= 0 || f.data[i].is_null() {
                bail!("unexpected plane {i} layout (linesize {ls})");
            }
            Ok(std::slice::from_raw_parts(f.data[i], ls as usize * rows))
        };
        use ff::AVPixelFormat::*;
        let fmt = f.format;
        let frame = if fmt == AV_PIX_FMT_NV12 as i32 {
            Nv12Frame {
                w: w as u32,
                h: h as u32,
                y: plane(0, h)?.to_vec(),
                uv: plane(1, ch)?.to_vec(),
                y_stride: f.linesize[0] as u32,
                uv_stride: f.linesize[1] as u32,
                colour,
            }
        } else if fmt == AV_PIX_FMT_YUV420P as i32 || fmt == AV_PIX_FMT_YUVJ420P as i32 {
            let (u, v) = (plane(1, ch)?, plane(2, ch)?);
            let (us, vs) = (f.linesize[1] as usize, f.linesize[2] as usize);
            let cw = w.div_ceil(2);
            let uv_stride = cw * 2;
            let mut uv = vec![0u8; uv_stride * ch];
            for row in 0..ch {
                let (ur, vr) = (&u[row * us..row * us + cw], &v[row * vs..row * vs + cw]);
                let out = &mut uv[row * uv_stride..(row + 1) * uv_stride];
                for i in 0..cw {
                    out[2 * i] = ur[i];
                    out[2 * i + 1] = vr[i];
                }
            }
            Nv12Frame {
                w: w as u32,
                h: h as u32,
                y: plane(0, h)?.to_vec(),
                uv,
                y_stride: f.linesize[0] as u32,
                uv_stride: uv_stride as u32,
                colour,
            }
        } else {
            bail!("unsupported decoded pixel format {fmt}");
        };
        Ok((frame, backend))
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned by us; the free functions accept null
        // and null out their argument.
        unsafe {
            ff::av_frame_free(&mut self.sw);
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

// SAFETY: the FFmpeg context is only touched through `&mut Decoder`; it has no thread affinity.
unsafe impl Send for Ctx {}

pub struct Decoder {
    id: ff::AVCodecID,
    /// What we asked for (`Vaapi` may still produce software frames if ffmpeg falls back).
    want: Backend,
    /// `new` may drop from VA-API to software if VA-API fails before any frame decodes.
    may_fall_back: bool,
    ctx: Ctx,
    config: Vec<u8>,
    prepend_config: bool,
    decoded_any: bool,
    /// The backend that actually produced frames (logged once).
    actual: Option<Backend>,
}

impl Decoder {
    /// Tries VA-API, falls back to software.
    pub fn new(codec: &str) -> Result<Self> {
        let id = codec_id(codec)?;
        match Self::open(id, Backend::Vaapi) {
            Ok(mut d) => {
                d.may_fall_back = true;
                Ok(d)
            }
            Err(error) => {
                tracing::warn!(%error, "VA-API unavailable; using software decode");
                Self::open(id, Backend::Software)
            }
        }
    }

    /// Software only, or VA-API only (no fallback).
    pub fn with_backend(codec: &str, backend: Backend) -> Result<Self> {
        Self::open(codec_id(codec)?, backend)
    }

    fn open(id: ff::AVCodecID, backend: Backend) -> Result<Self> {
        Ok(Self {
            id,
            want: backend,
            may_fall_back: false,
            ctx: Ctx::open(id, backend)?,
            config: Vec::new(),
            prepend_config: false,
            decoded_any: false,
            actual: None,
        })
    }

    /// The backend producing frames (before the first frame: the one requested).
    pub fn backend(&self) -> Backend {
        self.actual.unwrap_or(self.want)
    }

    pub fn push(&mut self, pkt: &VideoPacket) -> Result<Vec<Nv12Frame>> {
        if pkt.config {
            if !self.config.is_empty() && self.config != pkt.data {
                // New stream parameters (a resize): start over at the next keyframe.
                tracing::debug!("new codec config; reopening decoder");
                self.ctx = Ctx::open(self.id, self.want)?;
            }
            self.config = pkt.data.clone();
            self.prepend_config = true;
            return Ok(Vec::new());
        }
        let data = if self.prepend_config {
            let mut d = self.config.clone();
            d.extend_from_slice(&pkt.data);
            d
        } else {
            pkt.data.clone()
        };
        let mut out = Vec::new();
        let res = self.ctx.decode(&data, pkt.pts_us as i64, &mut out);
        let res = match res {
            Err(error)
                if self.want == Backend::Vaapi && self.may_fall_back && !self.decoded_any =>
            {
                tracing::warn!(%error, "VA-API decode failed; falling back to software");
                self.want = Backend::Software;
                self.ctx = Ctx::open(self.id, Backend::Software)?;
                out.clear();
                let mut d = self.config.clone();
                d.extend_from_slice(&pkt.data);
                self.ctx.decode(&d, pkt.pts_us as i64, &mut out)
            }
            other => other,
        };
        self.prepend_config = false;
        if let Some(backend) = res? {
            self.decoded_any = true;
            if self.actual.is_none() {
                tracing::info!("decoder={}", backend.name());
            }
            self.actual = Some(backend);
        }
        Ok(out)
    }
}

const _: () = {
    const fn is_send<T: Send>() {}
    is_send::<Decoder>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use ff::AVColorRange::*;
    use ff::AVColorSpace::*;
    use ff::AVPixelFormat::*;

    fn c(matrix: Matrix, full_range: bool) -> Colour {
        Colour { matrix, full_range }
    }

    #[test]
    fn colour_follows_the_stream() {
        let f = colour_for;
        // The phone's HEVC: smpte170m / bt470bg, tv range.
        let bt601_tv = c(Matrix::Bt601, false);
        assert_eq!(
            f(AVCOL_SPC_SMPTE170M, AVCOL_RANGE_MPEG, false, 1600),
            bt601_tv
        );
        assert_eq!(f(AVCOL_SPC_BT470BG, AVCOL_RANGE_MPEG, false, 800), bt601_tv);
        let bt709_pc = c(Matrix::Bt709, true);
        assert_eq!(f(AVCOL_SPC_BT709, AVCOL_RANGE_JPEG, false, 480), bt709_pc);
        let bt2020 = c(Matrix::Bt2020, false);
        assert_eq!(
            f(AVCOL_SPC_BT2020_NCL, AVCOL_RANGE_MPEG, false, 2160),
            bt2020
        );
        // YUVJ420P is full range when the range is unspecified...
        assert_eq!(
            f(AVCOL_SPC_BT709, AVCOL_RANGE_UNSPECIFIED, true, 1080),
            bt709_pc
        );
        // ...but an explicit range wins over the pixel format.
        let bt709_tv = c(Matrix::Bt709, false);
        assert_eq!(f(AVCOL_SPC_BT709, AVCOL_RANGE_MPEG, true, 1080), bt709_tv);
    }

    #[test]
    fn unspecified_matrix_depends_on_height() {
        let f = |h| colour_for(AVCOL_SPC_UNSPECIFIED, AVCOL_RANGE_UNSPECIFIED, false, h);
        assert_eq!(f(576), c(Matrix::Bt601, false));
        assert_eq!(f(719), c(Matrix::Bt601, false));
        assert_eq!(f(720), c(Matrix::Bt709, false));
        let reserved = colour_for(AVCOL_SPC_RESERVED, AVCOL_RANGE_UNSPECIFIED, false, 1600);
        assert_eq!(reserved, c(Matrix::Bt709, false));
    }

    #[test]
    fn get_format_prefers_vaapi_and_never_picks_another_hw_format() {
        let pick = |fmts: &[ff::AVPixelFormat]| {
            let mut v = fmts.to_vec();
            v.push(AV_PIX_FMT_NONE);
            // SAFETY: a NONE-terminated list, as ffmpeg passes it.
            unsafe { get_format_vaapi(ptr::null_mut(), v.as_ptr()) }
        };
        assert_eq!(
            pick(&[AV_PIX_FMT_VAAPI, AV_PIX_FMT_YUV420P]),
            AV_PIX_FMT_VAAPI
        );
        assert_eq!(
            pick(&[AV_PIX_FMT_YUV420P, AV_PIX_FMT_VAAPI]),
            AV_PIX_FMT_VAAPI
        );
        let other_hw = [AV_PIX_FMT_CUDA, AV_PIX_FMT_VDPAU, AV_PIX_FMT_YUV420P];
        assert_eq!(pick(&other_hw), AV_PIX_FMT_YUV420P);
        assert_eq!(pick(&[AV_PIX_FMT_VULKAN]), AV_PIX_FMT_NONE);
    }
}
