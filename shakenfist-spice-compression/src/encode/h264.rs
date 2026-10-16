//! H.264 video encoding, wrapping Cisco's openh264.
//!
//! A SPICE server encodes its video streams with this, and ryll its
//! WebRTC video and its capture files. It takes [`Bgrx`] or [`Rgbx`]
//! images and returns each frame as an Annex B byte stream: NAL
//! units behind start codes, with the SPS and PPS in front of every
//! IDR, which is what both a SPICE `STREAM_DATA` message and an MP4
//! writer want.
//!
//! The settings here follow what openh264 0.9.8 does, rather than
//! what its crate's builder suggests; the comments on each say why.

use anyhow::{anyhow, bail, Result};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, QpRange,
    RateControlMode, UsageType,
};
use openh264::formats::{RGB8Source, RGBSource, YUVBuffer};
use openh264::OpenH264API;
use openh264_sys2::{
    SBitrateInfo, SEncParamExt, ENCODER_OPTION, ENCODER_OPTION_BITRATE, ENCODER_OPTION_FRAME_RATE,
    ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, LAYER_NUM,
    SM_FIXEDSLCNUM_SLICE, SPATIAL_LAYER_0, SPATIAL_LAYER_ALL,
};

use super::{Bgrx, Rgbx};

/// Bits per pixel per frame used to derive a default target bitrate,
/// in thousandths (so 100 means 0.1 bits per pixel per frame).
///
/// openh264's own default is a flat 120 kbit/s whatever the
/// resolution, which is a webcam-era number: a 1024x768 desktop at
/// 30fps through 120 kbit/s is unreadable. 0.1 bits per pixel per
/// frame gives ~2.4 Mbit/s at 1024x768@30 and ~6.2 Mbit/s at
/// 1920x1080@30, inside the 0.2-50 Mbit/s range measured for real
/// SPICE console traffic.
///
/// Held in thousandths, and the derivation done in integers, because
/// a float version rounds unpredictably at the `as u32` truncation and
/// makes the policy hard to state in a test.
const MILLIBITS_PER_PIXEL_PER_FRAME: u64 = 100;

/// Floor for the derived bitrate. Small surfaces would otherwise
/// derive a target so low that even a static desktop cannot be coded
/// cleanly.
const MIN_BITRATE_BPS: u32 = 1_000_000;

/// Ceiling for the derived bitrate, so a 4K surface cannot ask for
/// more bandwidth than a viewer is likely to have.
const MAX_BITRATE_BPS: u32 = 20_000_000;

/// The largest frame openh264 encodes: 3840 on the longer side and
/// 2160 on the shorter. It refuses anything larger at the first
/// encode; [`H264Encoder::new`] refuses it up front.
const MAX_LONG_SIDE: u32 = 3840;
const MAX_SHORT_SIDE: u32 = 2160;

/// The QP floor openh264 holds screen content to: it raises any
/// minimum from 1 up to this.
const MIN_SCREEN_QP: u8 = 12;

/// The default number of slices, when the machine has that many
/// cores. Four slices at 1080p halve the encode time for a few
/// percent more bytes.
const DEFAULT_SLICES: u32 = 4;

/// A default target bitrate for `width` x `height` frames at `fps`
/// frames a second: 0.1 bits per pixel per frame, clamped to
/// 1-20 Mbit/s.
pub fn target_bitrate_bps(width: u32, height: u32, fps: u32) -> u32 {
    let pixels_per_second = (width as u64) * (height as u64) * (fps as u64);
    let bps = pixels_per_second * MILLIBITS_PER_PIXEL_PER_FRAME / 1000;
    bps.clamp(MIN_BITRATE_BPS as u64, MAX_BITRATE_BPS as u64) as u32
}

/// Round a size down to the even dimensions H.264 requires.
///
/// 4:2:0 chroma subsampling halves both axes, so both must be even.
/// [`H264Encoder::encode`] accepts an image of an odd size whose
/// rounded size is the encoder's, and drops its last row or column,
/// so a producer of odd frames rounds with this when it picks the
/// encoder's size, and compares sizes after rounding.
pub fn even_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

/// The number of slices [`H264Config::new`] chooses: four, or fewer
/// on a machine with fewer cores.
fn default_slices() -> u32 {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    DEFAULT_SLICES.min(u32::try_from(cores).unwrap_or(u32::MAX))
}

/// How openh264 spends the target bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264RateControl {
    /// Budget each frame as the target bitrate divided by the frame
    /// rate, ignoring timestamps. A server that knows its frame rate
    /// wants this, and keeps the frame rate current with
    /// [`H264Encoder::set_frame_rate`].
    Bitrate,
    /// openh264's quality mode, which treats the target more loosely.
    Quality,
}

/// How to build an [`H264Encoder`].
///
/// [`H264Config::new`] fills in defaults, which a caller then changes
/// field by field.
#[derive(Debug, Clone, PartialEq)]
pub struct H264Config {
    /// Frame size. Both must be even, at most 3840 on the longer side
    /// and 2160 on the shorter.
    pub width: u32,
    pub height: u32,
    /// The starting target bitrate, in bits a second.
    pub bitrate_bps: u32,
    /// The frame rate the bitrate is budgeted over, in frames a second.
    pub max_frame_rate: f32,
    pub rate_control: H264RateControl,
    /// The quantiser range, from 0 (best) to 51 (smallest).
    ///
    /// Set explicitly because openh264's default range of 0 to 51
    /// does not mean what it says for screen content: it silently
    /// narrows it to 26 to 35, which makes high targets unreachable
    /// (every target above about 2.4 Mbit/s at 640x360 gives the same
    /// output) and low ones too. The default here is 12 to 51; any
    /// minimum from 1 to 12 behaves as 12.
    pub qp_min: u8,
    pub qp_max: u8,
    /// Slices per frame, each encoded on its own thread. 1 is a
    /// single slice; openh264 drops to one for a very small frame.
    pub slices: u32,
    /// Whether openh264 may skip a frame to hold the bitrate. A
    /// skipped frame makes [`H264Encoder::encode`] return `None`.
    pub skip_frames: bool,
    /// Frames between IDRs; 0 sends one only at the start and when
    /// asked with [`H264Encoder::force_intra_frame`].
    pub idr_period: u32,
}

impl H264Config {
    /// Defaults for `width` x `height` frames at up to `fps` frames a
    /// second: a [`target_bitrate_bps`] target, budgeted in
    /// [`H264RateControl::Bitrate`] mode, QP 12 to 51, four slices or
    /// one per core if fewer, frame skipping on, and an IDR only at
    /// the start.
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            bitrate_bps: target_bitrate_bps(width, height, fps),
            max_frame_rate: fps as f32,
            rate_control: H264RateControl::Bitrate,
            qp_min: MIN_SCREEN_QP,
            qp_max: 51,
            slices: default_slices(),
            skip_frames: true,
            idr_period: 0,
        }
    }

    fn validate(&self) -> Result<()> {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            bail!("H.264 needs a non-zero, even frame size, not {w}x{h}");
        }
        if w.max(h) > MAX_LONG_SIDE || w.min(h) > MAX_SHORT_SIDE {
            bail!("openh264 encodes at most {MAX_LONG_SIDE}x{MAX_SHORT_SIDE}, not {w}x{h}");
        }
        check_bitrate(self.bitrate_bps)?;
        check_frame_rate(self.max_frame_rate)?;
        if self.qp_max > 51 || self.qp_min > self.qp_max {
            bail!(
                "QP range {} to {} is not within 0 to 51",
                self.qp_min,
                self.qp_max
            );
        }
        if self.slices == 0 {
            bail!("an H.264 frame needs at least one slice");
        }
        Ok(())
    }
}

fn check_bitrate(bps: u32) -> Result<()> {
    if bps == 0 || i32::try_from(bps).is_err() {
        bail!("H.264 bitrate {bps} is not between 1 and {}", i32::MAX);
    }
    Ok(())
}

fn check_frame_rate(fps: f32) -> Result<()> {
    if !(fps.is_finite() && fps > 0.0) {
        bail!("H.264 frame rate {fps} is not a positive number");
    }
    Ok(())
}

/// One encoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264Frame {
    /// The frame's NAL units in Annex B framing, each behind a three
    /// or four byte start code, as openh264 writes them.
    pub data: Vec<u8>,
    /// Whether this is an IDR, which a decoder can start from.
    pub keyframe: bool,
}

/// Pixels as openh264's converter reads them: rows of four-byte
/// pixels, `padded_width` pixels apart, of which the first `width` of
/// the first `height` rows are the frame.
#[derive(Clone, Copy)]
struct Pixels<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    padded_width: usize,
    /// Byte offsets of red, green and blue within a pixel.
    rgb_offsets: (usize, usize, usize),
}

impl RGBSource for Pixels<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let i = (y * self.padded_width + x) * 4;
        let (r, g, b) = self.rgb_offsets;
        (
            f32::from(self.data[i + r]),
            f32::from(self.data[i + g]),
            f32::from(self.data[i + b]),
        )
    }
}

impl RGB8Source for Pixels<'_> {
    fn dimensions_padded(&self) -> (usize, usize) {
        (self.padded_width, self.height)
    }

    fn rgb8_data(&self) -> &[u8] {
        self.data
    }

    fn pixel_stride(&self) -> usize {
        4
    }

    fn rgb_channel_offsets(&self) -> (usize, usize, usize) {
        self.rgb_offsets
    }
}

/// A stateful H.264 encoder for one frame size.
///
/// A new size needs a new encoder: openh264 can re-initialise for one,
/// but doing so silently restores the bitrate and frame rate it was
/// created with, so this does not offer it.
pub struct H264Encoder {
    encoder: Encoder,
    config: H264Config,
    /// The frame converted to YUV 4:2:0, reused between frames.
    yuv: YUVBuffer,
    /// A tightly packed copy of the frame, for an image whose rows
    /// the converter cannot read in place. Empty until one arrives.
    packed: Vec<u8>,
    bitrate_bps: u32,
    /// The highest bitrate the encoder's spatial layer allows.
    max_bitrate_bps: u32,
    frame_rate: f32,
    state: State,
    /// Fail the slice switch, to test the failure path.
    #[cfg(test)]
    fail_slice_switch: bool,
}

/// Where an [`H264Encoder`] is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No frame encoded yet. openh264 initialises at the first encode,
    /// and takes options only after it, so until then settings are
    /// only recorded.
    Uninitialised,
    Running,
    /// Initialisation failed part way. openh264 may then be running
    /// in a state nothing here tracks, so the encoder refuses every
    /// later call rather than retry on top of it.
    Failed,
}

// The encoder task moves it between threads; openh264's `Encoder` is
// Send, and this must stay so.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<H264Encoder>();
};

impl H264Encoder {
    /// Build an encoder. Fails when the configuration is out of range,
    /// including a frame size that is odd or too large.
    pub fn new(config: H264Config) -> Result<Self> {
        config.validate()?;
        let rate_control = match config.rate_control {
            H264RateControl::Bitrate => RateControlMode::Bitrate,
            H264RateControl::Quality => RateControlMode::Quality,
        };
        let openh264_config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(config.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(config.max_frame_rate))
            .rate_control_mode(rate_control)
            // Tuned for the sharp edges and large flat regions of a
            // desktop, not camera noise.
            .usage_type(UsageType::ScreenContentRealTime)
            // WebRTC negotiates a baseline profile, which openh264
            // also picks by default; say so rather than rely on it.
            .profile(Profile::Baseline)
            .qp(QpRange::new(config.qp_min, config.qp_max))
            .skip_frames(config.skip_frames)
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.idr_period))
            // openh264 supports neither for screen content, and turns
            // them off itself with a warning on stderr for each
            // encoder.
            .adaptive_quantization(false)
            .background_detection(false);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), openh264_config)
            .map_err(|e| anyhow!("openh264 encoder creation failed: {e}"))?;
        Ok(Self {
            encoder,
            yuv: YUVBuffer::new(config.width as usize, config.height as usize),
            packed: Vec::new(),
            bitrate_bps: config.bitrate_bps,
            max_bitrate_bps: config.bitrate_bps,
            frame_rate: config.max_frame_rate,
            state: State::Uninitialised,
            #[cfg(test)]
            fail_slice_switch: false,
            config,
        })
    }

    /// The configuration the encoder was built with. The bitrate and
    /// frame rate in it are the starting ones.
    pub fn config(&self) -> &H264Config {
        &self.config
    }

    pub fn width(&self) -> u32 {
        self.config.width
    }

    pub fn height(&self) -> u32 {
        self.config.height
    }

    /// The current target bitrate, in bits a second.
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// The frame rate the bitrate is currently budgeted over.
    pub fn frame_rate(&self) -> f32 {
        self.frame_rate
    }

    /// Change the target bitrate. Output follows within a few frames.
    /// If openh264 refuses, the target stays as it was.
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        check_bitrate(bps)?;
        self.check_usable()?;
        let previous = std::mem::replace(&mut self.bitrate_bps, bps);
        if self.state == State::Running {
            if let Err(e) = self.apply_bitrate() {
                self.bitrate_bps = previous;
                return Err(e);
            }
        }
        Ok(())
    }

    /// Change the frame rate the bitrate is budgeted over. In
    /// [`H264RateControl::Bitrate`] mode each frame gets the bitrate
    /// divided by this, so a server keeps it at the rate it actually
    /// sends. It applies from the next frame. If openh264 refuses, the
    /// frame rate stays as it was.
    pub fn set_frame_rate(&mut self, fps: f32) -> Result<()> {
        check_frame_rate(fps)?;
        self.check_usable()?;
        let previous = std::mem::replace(&mut self.frame_rate, fps);
        if self.state == State::Running {
            if let Err(e) = self.apply_frame_rate() {
                self.frame_rate = previous;
                return Err(e);
            }
        }
        Ok(())
    }

    /// Make the next frame an IDR. The first frame always is one.
    pub fn force_intra_frame(&mut self) {
        if self.state == State::Running {
            self.encoder.force_intra_frame();
        }
    }

    fn check_usable(&self) -> Result<()> {
        if self.state == State::Failed {
            bail!("this H.264 encoder failed to initialise; build a new one");
        }
        Ok(())
    }

    /// Encode a BGRX frame. Returns `None` when openh264 skipped it to
    /// hold the bitrate.
    ///
    /// The image is the encoder's size, or one pixel more on an axis
    /// whose rounding with [`even_dimensions`] gives the encoder's, in
    /// which case its last row or column is dropped. Any other size is
    /// an error.
    pub fn encode(&mut self, image: &Bgrx) -> Result<Option<H264Frame>> {
        self.encode_pixels(image, (2, 1, 0))
    }

    /// As [`encode`](Self::encode), for an RGBX frame.
    pub fn encode_rgbx(&mut self, image: &Rgbx) -> Result<Option<H264Frame>> {
        self.encode_pixels(&image.0, (0, 1, 2))
    }

    fn encode_pixels(
        &mut self,
        image: &Bgrx,
        rgb_offsets: (usize, usize, usize),
    ) -> Result<Option<H264Frame>> {
        self.check_usable()?;
        self.convert(image, rgb_offsets)?;
        if self.state == State::Running {
            return self.encode_yuv();
        }
        let result = self.initialise_and_encode();
        self.state = if result.is_ok() {
            State::Running
        } else {
            State::Failed
        };
        result
    }

    /// Encode the first frame, initialising openh264 on the way, and
    /// apply any bitrate and frame rate set before it.
    fn initialise_and_encode(&mut self) -> Result<Option<H264Frame>> {
        if self.config.slices > 1 {
            self.initialise_with_slices()?;
            self.apply_settings()?;
            return self.encode_yuv();
        }
        let frame = self.encode_yuv()?;
        self.apply_settings()?;
        Ok(frame)
    }

    /// Convert `image` into `self.yuv`, reading it in place when its
    /// rows allow and through `self.packed` when not.
    fn convert(&mut self, image: &Bgrx, rgb_offsets: (usize, usize, usize)) -> Result<()> {
        let (w, h) = (self.config.width, self.config.height);
        if even_dimensions(image.width, image.height) != (w, h) {
            bail!(
                "a {}x{} frame does not round to this encoder's size; this encoder is {w}x{h}",
                image.width,
                image.height
            );
        }
        let (w, h) = (w as usize, h as usize);
        // The converter reads every row at the full padded width, the
        // last one too, so it can read the image in place only when
        // the stride is a whole number of pixels and the last row
        // kept has its padding.
        let in_place = image.stride.is_multiple_of(4)
            && image
                .stride
                .checked_mul(h)
                .is_some_and(|n| n <= image.data.len());
        let pixels = if in_place {
            Pixels {
                data: image.data,
                width: w,
                height: h,
                padded_width: image.stride / 4,
                rgb_offsets,
            }
        } else {
            self.packed.clear();
            for row in image.data.chunks(image.stride).take(h) {
                self.packed.extend_from_slice(&row[..w * 4]);
            }
            Pixels {
                data: &self.packed,
                width: w,
                height: h,
                padded_width: w,
                rgb_offsets,
            }
        };
        self.yuv.read_rgb8(pixels);
        Ok(())
    }

    /// Initialise openh264 by encoding the converted frame and
    /// discarding it, then switch it to fixed slices.
    ///
    /// openh264's crate initialises at the first encode, always with a
    /// single slice, and changing the slice count afterwards resets
    /// the encoder, which makes the next frame an IDR. Doing that
    /// after a frame that was sent would cost a second IDR at the start
    /// of every stream; doing it after a frame that was not costs one
    /// extra encode instead. The reset keeps the bitrate and frame
    /// rate, because the parameters written back are the ones read.
    fn initialise_with_slices(&mut self) -> Result<()> {
        self.encoder
            .encode(&self.yuv)
            .map_err(|e| anyhow!("openh264 encode failed: {e}"))?;
        #[cfg(test)]
        if self.fail_slice_switch {
            bail!("openh264 refused to set SVC_ENCODE_PARAM_EXT (injected for a test)");
        }
        let mut params = SEncParamExt::default();
        // SAFETY: ENCODER_OPTION_SVC_ENCODE_PARAM_EXT reads and writes
        // an SEncParamExt, and the encode above initialised the
        // encoder. Only the slice layout and thread count change.
        unsafe {
            let raw = self.encoder.raw_api();
            check_option(
                "get SVC_ENCODE_PARAM_EXT",
                raw.get_option(
                    ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                    (&raw mut params).cast(),
                ),
            )?;
            // openh264 caps the count itself, and falls back to one
            // slice for a frame too small to split.
            let slices = self.config.slices.min(u32::from(u16::MAX));
            params.iMultipleThreadIdc = slices as u16;
            params.sSpatialLayers[0].sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
            params.sSpatialLayers[0].sSliceArgument.uiSliceNum = slices;
            check_option(
                "set SVC_ENCODE_PARAM_EXT",
                raw.set_option(
                    ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                    (&raw mut params).cast(),
                ),
            )?;
        }
        // The reset already makes the next frame an IDR; openh264
        // skips it when the slice count did not change, as for a
        // frame too small to split.
        self.encoder.force_intra_frame();
        Ok(())
    }

    /// Encode `self.yuv`, returning `None` for a skipped frame.
    fn encode_yuv(&mut self) -> Result<Option<H264Frame>> {
        let bitstream = self
            .encoder
            .encode(&self.yuv)
            .map_err(|e| anyhow!("openh264 encode failed: {e}"))?;
        let frame_type = bitstream.frame_type();
        if matches!(frame_type, FrameType::Skip) {
            return Ok(None);
        }
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(H264Frame {
            data,
            keyframe: matches!(frame_type, FrameType::IDR),
        }))
    }

    /// Apply any bitrate and frame rate set before openh264 was
    /// initialised. openh264 must be initialised.
    fn apply_settings(&mut self) -> Result<()> {
        if self.bitrate_bps != self.config.bitrate_bps {
            self.apply_bitrate()?;
        }
        if self.frame_rate != self.config.max_frame_rate {
            self.apply_frame_rate()?;
        }
        Ok(())
    }

    fn apply_bitrate(&mut self) -> Result<()> {
        // openh264 caps the target at the spatial layer's maximum,
        // which starts at the creation bitrate, so raising above it
        // means raising that first. Only the layer's own maximum
        // works: setting the maximum for all layers reports success
        // and changes nothing, and the target then half-applies.
        if self.bitrate_bps > self.max_bitrate_bps {
            self.set_bitrate_option(
                ENCODER_OPTION_MAX_BITRATE,
                SPATIAL_LAYER_0,
                self.bitrate_bps,
            )?;
            self.max_bitrate_bps = self.bitrate_bps;
        }
        self.set_bitrate_option(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, self.bitrate_bps)
    }

    fn set_bitrate_option(
        &mut self,
        option: ENCODER_OPTION,
        layer: LAYER_NUM,
        bps: u32,
    ) -> Result<()> {
        let mut info = SBitrateInfo {
            iLayer: layer,
            // check_bitrate bounded it to i32.
            iBitrate: bps as i32,
        };
        // SAFETY: ENCODER_OPTION_BITRATE and ENCODER_OPTION_MAX_BITRATE
        // read an SBitrateInfo, and the encoder is initialised.
        let rc = unsafe {
            self.encoder
                .raw_api()
                .set_option(option, (&raw mut info).cast())
        };
        check_option("set bitrate", rc)
    }

    fn apply_frame_rate(&mut self) -> Result<()> {
        let mut fps = self.frame_rate;
        // SAFETY: ENCODER_OPTION_FRAME_RATE reads an f32, and the
        // encoder is initialised.
        let rc = unsafe {
            self.encoder
                .raw_api()
                .set_option(ENCODER_OPTION_FRAME_RATE, (&raw mut fps).cast())
        };
        check_option("set frame rate", rc)
    }
}

/// openh264 options return 0 on success.
fn check_option(what: &str, rc: i32) -> Result<()> {
    if rc != 0 {
        bail!("openh264 refused to {what} (error {rc})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator for frame noise.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    /// A BGRX frame of `w` x `h`, tightly packed: gradients that move
    /// with `frame`, plus noise of up to `noise` either way, which is
    /// what keeps the encoder's output sized by its bitrate.
    fn bgrx_frame(w: u32, h: u32, frame: u32, noise: u8, rng: &mut XorShift) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let base = [
                    ((x * 3 + frame * 5) % 256) as u8,
                    ((y * 2 + frame * 3) % 256) as u8,
                    ((x + y + frame * 7) % 256) as u8,
                ];
                for (c, b) in base.into_iter().enumerate() {
                    let n = if noise == 0 {
                        0
                    } else {
                        (rng.next() % (2 * u64::from(noise) + 1)) as i32 - i32::from(noise)
                    };
                    v[i + c] = (i32::from(b) + n).clamp(0, 255) as u8;
                }
                v[i + 3] = 0xAA;
            }
        }
        v
    }

    fn bgrx(data: &[u8], w: u32, h: u32) -> Bgrx<'_> {
        Bgrx::new(data, w, h, (w * 4) as usize).expect("valid image")
    }

    // The rate tests below (`bitrate_changes_follow_in_both_directions`,
    // `a_bitrate_set_before_the_first_frame_applies` and
    // `frame_rate_changes_the_per_frame_budget`) assert ratios of mean
    // frame sizes. Their thresholds and bitrates were tuned against
    // openh264's rate controller at the `=0.9.8` pin in Cargo.toml:
    // whoever bumps that pin should expect to retune them, rather than
    // read a failure as a broken encoder.

    /// Config for the rate tests: one slice, in Bitrate mode, with
    /// frame skipping, which is what holds openh264 to its budget (it
    /// warns that it cannot without). For these noisy 320x240 frames
    /// the mean size, a skipped frame counting 0, then tracks the
    /// budget up to about 4 Mbit/s, where the QP floor of 12 binds.
    fn rate_config(w: u32, h: u32, bps: u32) -> H264Config {
        H264Config {
            bitrate_bps: bps,
            slices: 1,
            ..H264Config::new(w, h, 30)
        }
    }

    /// Encode `n` noisy frames from frame `start`, returning each
    /// one's size, 0 for a skipped one.
    fn encode_run(enc: &mut H264Encoder, start: u32, n: u32, rng: &mut XorShift) -> Vec<usize> {
        let (w, h) = (enc.width(), enc.height());
        (start..start + n)
            .map(|f| {
                let data = bgrx_frame(w, h, f, 12, rng);
                enc.encode(&bgrx(&data, w, h))
                    .expect("encode")
                    .map_or(0, |frame| frame.data.len())
            })
            .collect()
    }

    fn mean(sizes: &[usize]) -> usize {
        sizes.iter().sum::<usize>() / sizes.len()
    }

    #[test]
    fn bitrate_scales_with_pixels_and_clamps_at_both_ends() {
        // A desktop-sized surface has to land far above openh264's own
        // 120 kbit/s default; that default is the bug this policy
        // exists to fix.
        assert_eq!(target_bitrate_bps(1024, 768, 30), 2_359_296);
        assert!(target_bitrate_bps(1920, 1080, 30) > target_bitrate_bps(1024, 768, 30));

        // Tiny surfaces hit the floor, 4K hits the ceiling.
        assert_eq!(target_bitrate_bps(64, 64, 30), MIN_BITRATE_BPS);
        assert_eq!(target_bitrate_bps(3840, 2160, 30), MAX_BITRATE_BPS);
    }

    #[test]
    fn bitrate_scales_with_frame_rate_too() {
        // The policy is per *pixel per frame*, so halving the cadence
        // halves the budget.
        assert_eq!(
            target_bitrate_bps(1024, 768, 60),
            2 * target_bitrate_bps(1024, 768, 30)
        );
        assert_eq!(target_bitrate_bps(1024, 768, 15), 1_179_648);
        // Still clamped at both ends whatever the cadence.
        assert_eq!(target_bitrate_bps(1024, 768, 1), MIN_BITRATE_BPS);
        assert_eq!(target_bitrate_bps(1920, 1080, 240), MAX_BITRATE_BPS);
    }

    #[test]
    fn even_dimensions_rounds_each_axis_down_independently() {
        assert_eq!(even_dimensions(64, 64), (64, 64));
        assert_eq!(even_dimensions(63, 45), (62, 44));
        assert_eq!(even_dimensions(65, 64), (64, 64));
        assert_eq!(even_dimensions(64, 65), (64, 64));
        assert_eq!(even_dimensions(1, 1), (0, 0));
    }

    #[test]
    fn odd_and_out_of_range_configs_are_errors_not_panics() {
        // openh264's YUV buffer panics on an odd size.
        for (w, h) in [(65, 64), (64, 63), (0, 64), (3842, 2160), (2162, 2162)] {
            assert!(
                H264Encoder::new(H264Config::new(w, h, 30)).is_err(),
                "{w}x{h}"
            );
        }
        let base = H264Config::new(64, 64, 30);
        for bad in [
            H264Config {
                bitrate_bps: 0,
                ..base.clone()
            },
            H264Config {
                max_frame_rate: 0.0,
                ..base.clone()
            },
            H264Config {
                max_frame_rate: f32::NAN,
                ..base.clone()
            },
            H264Config {
                qp_min: 40,
                qp_max: 30,
                ..base.clone()
            },
            H264Config {
                qp_max: 52,
                ..base.clone()
            },
            H264Config {
                slices: 0,
                ..base.clone()
            },
        ] {
            assert!(H264Encoder::new(bad.clone()).is_err(), "{bad:?}");
        }
        let mut enc = H264Encoder::new(base).expect("init");
        assert!(enc.set_bitrate(0).is_err());
        assert!(enc.set_frame_rate(-1.0).is_err());
    }

    #[test]
    fn a_frame_of_another_size_is_an_error() {
        let mut enc = H264Encoder::new(H264Config::new(64, 64, 30)).expect("init");
        let data = vec![0u8; 32 * 32 * 4];
        let err = enc
            .encode(&bgrx(&data, 32, 32))
            .expect_err("a different size must not be encoded");
        assert!(
            err.to_string().contains("this encoder is 64x64"),
            "unhelpful error: {err}"
        );
    }

    /// Strided, unaligned and odd-sized images are read without their
    /// padding or their extra row and column: each encodes exactly as
    /// the same pixels tightly packed do. A misread row would shear
    /// the picture and still encode, so compare the bitstreams.
    #[test]
    fn padded_and_odd_images_encode_as_their_tight_crop() {
        let (w, h) = (64u32, 48u32);
        let mut rng = XorShift(7);
        let tight = bgrx_frame(w, h, 0, 0, &mut rng);
        let encode = |image: &Bgrx| {
            let config = H264Config {
                slices: 1,
                ..H264Config::new(w, h, 30)
            };
            let mut enc = H264Encoder::new(config).expect("init");
            enc.encode(image).expect("encode").expect("a frame").data
        };
        let expected = encode(&bgrx(&tight, w, h));

        // Embed the frame in a larger buffer of junk, at a given stride
        // and with an extra row and column when `odd`.
        let embed = |stride: usize, odd: bool| {
            let rows = h as usize + usize::from(odd);
            let mut v = vec![0x5Au8; stride * rows];
            for y in 0..h as usize {
                let row = &tight[y * w as usize * 4..(y + 1) * w as usize * 4];
                v[y * stride..y * stride + row.len()].copy_from_slice(row);
            }
            v
        };
        for (stride, odd) in [(64 * 4 + 16, false), (64 * 4 + 3, false), (65 * 4, true)] {
            let data = embed(stride, odd);
            let (iw, ih) = if odd { (w + 1, h + 1) } else { (w, h) };
            let image = Bgrx::new(&data, iw, ih, stride).expect("valid image");
            assert_eq!(
                encode(&image),
                expected,
                "stride {stride}, odd {odd} encoded differently"
            );
        }
    }

    /// RGBX and BGRX of the same picture encode identically.
    #[test]
    fn rgbx_is_bgrx_with_red_and_blue_swapped() {
        let (w, h) = (64u32, 48u32);
        let bgrx_data = bgrx_frame(w, h, 3, 0, &mut XorShift(9));
        let rgbx_data: Vec<u8> = bgrx_data
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let config = H264Config {
            slices: 1,
            ..H264Config::new(w, h, 30)
        };
        let mut a = H264Encoder::new(config.clone()).expect("init");
        let mut b = H264Encoder::new(config).expect("init");
        let from_bgrx = a.encode(&bgrx(&bgrx_data, w, h)).expect("encode");
        let rgbx = Rgbx::new(&rgbx_data, w, h, (w * 4) as usize).expect("valid image");
        let from_rgbx = b.encode_rgbx(&rgbx).expect("encode");
        assert_eq!(from_bgrx, from_rgbx);
    }

    /// The NAL unit types in an Annex B stream, each read from the
    /// byte after a start code's final 01.
    fn nal_types(data: &[u8]) -> Vec<u8> {
        data.windows(4)
            .filter(|w| w[..3] == [0, 0, 1])
            .map(|w| w[3] & 0x1F)
            .collect()
    }

    /// The first frame is an IDR, and only the first: switching to
    /// several slices resets openh264, which must not cost a second
    /// IDR. A 64x64 frame is too small to split, and 320x240 is not.
    #[test]
    fn first_frame_is_an_idr_with_parameter_sets_and_later_ones_are_not() {
        for (slices, w, h) in [(1, 64, 64), (4, 64, 64), (4, 320, 240)] {
            let config = H264Config {
                slices,
                ..H264Config::new(w, h, 30)
            };
            let mut enc = H264Encoder::new(config).expect("init");
            let frame = vec![0u8; (w * h * 4) as usize];
            let first = enc
                .encode(&bgrx(&frame, w, h))
                .expect("encode")
                .expect("a frame");
            assert!(first.keyframe, "{slices} slices: first frame");
            let types = nal_types(&first.data);
            for t in [7, 8, 5] {
                assert!(types.contains(&t), "{slices} slices: NAL types {types:?}");
            }
            let second = enc.encode(&bgrx(&frame, w, h)).expect("encode");
            assert!(
                second.is_none_or(|f| !f.keyframe),
                "{slices} slices: the second frame is an IDR too"
            );
            enc.force_intra_frame();
            let forced = enc
                .encode(&bgrx(&frame, w, h))
                .expect("encode")
                .expect("a forced IDR is not skipped");
            assert!(forced.keyframe, "{slices} slices: forced IDR");
        }
    }

    #[test]
    fn a_starved_encoder_skips_frames_and_says_so() {
        let config = H264Config {
            bitrate_bps: 64_000,
            slices: 1,
            ..H264Config::new(320, 240, 30)
        };
        let mut enc = H264Encoder::new(config).expect("init");
        let mut rng = XorShift(11);
        let mut skipped = 0;
        for f in 0..30 {
            let data = bgrx_frame(320, 240, f, 12, &mut rng);
            match enc.encode(&bgrx(&data, 320, 240)).expect("encode") {
                None => skipped += 1,
                Some(frame) => assert!(!frame.data.is_empty(), "frame {f} is empty, not None"),
            }
        }
        assert!(skipped > 0, "64 kbit/s of noise never skipped a frame");
    }

    /// Lowering the bitrate shrinks the frames within ~15 of them, and
    /// raising it above the rate the encoder was created with grows
    /// them, which needs the layer's maximum raised first.
    #[test]
    fn bitrate_changes_follow_in_both_directions() {
        let (w, h) = (320, 240);
        let mut rng = XorShift(1);
        let mut enc = H264Encoder::new(rate_config(w, h, 1_000_000)).expect("init");
        let before = mean(&encode_run(&mut enc, 0, 30, &mut rng));

        enc.set_bitrate(250_000).expect("lower the bitrate");
        assert_eq!(enc.bitrate_bps(), 250_000);
        let settling = encode_run(&mut enc, 30, 15, &mut rng);
        let lowered = mean(&encode_run(&mut enc, 45, 30, &mut rng));
        assert!(
            lowered * 2 < before,
            "250 kbit/s frames ({lowered} B) are not much smaller than 1 Mbit/s ones \
             ({before} B); settling {settling:?}"
        );

        // Every raw call has to return 0 for this to be Ok.
        enc.set_bitrate(3_000_000)
            .expect("raise above the creation bitrate");
        assert_eq!(enc.bitrate_bps(), 3_000_000);
        encode_run(&mut enc, 75, 15, &mut rng);
        let raised = mean(&encode_run(&mut enc, 90, 30, &mut rng));
        assert!(
            raised > before * 2,
            "3 Mbit/s frames ({raised} B) are not much larger than the 1 Mbit/s ones \
             ({before} B) the encoder was created at"
        );
    }

    /// If initialisation fails part way, the encoder refuses every later
    /// call, rather than retry the slice switch on an openh264 that is
    /// already running, or record settings it will never apply.
    #[test]
    fn a_failed_initialisation_poisons_the_encoder() {
        let config = H264Config {
            slices: 4,
            ..H264Config::new(320, 240, 30)
        };
        let mut enc = H264Encoder::new(config).expect("init");
        enc.fail_slice_switch = true;
        let data = vec![0u8; 320 * 240 * 4];
        let err = enc
            .encode(&bgrx(&data, 320, 240))
            .expect_err("the injected slice failure");
        assert!(err.to_string().contains("injected"), "{err}");

        // Even with the fault gone, the encoder stays refused.
        enc.fail_slice_switch = false;
        for err in [
            enc.encode(&bgrx(&data, 320, 240)).map(|_| ()),
            enc.set_bitrate(500_000),
            enc.set_frame_rate(10.0),
        ] {
            let err = err.expect_err("a poisoned encoder refuses");
            assert!(err.to_string().contains("failed to initialise"), "{err}");
        }
        assert_eq!(enc.bitrate_bps(), enc.config().bitrate_bps);
    }

    /// A bitrate set before the first frame applies from the start.
    #[test]
    fn a_bitrate_set_before_the_first_frame_applies() {
        let (w, h) = (320, 240);
        let mut base = H264Encoder::new(rate_config(w, h, 2_000_000)).expect("init");
        let mut lowered = H264Encoder::new(rate_config(w, h, 2_000_000)).expect("init");
        lowered
            .set_bitrate(250_000)
            .expect("set before the first frame");
        let base_sizes = encode_run(&mut base, 0, 45, &mut XorShift(3));
        let lowered_sizes = encode_run(&mut lowered, 0, 45, &mut XorShift(3));
        assert!(
            mean(&lowered_sizes[15..]) * 3 < mean(&base_sizes[15..]),
            "{lowered_sizes:?} vs {base_sizes:?}"
        );
    }

    /// In Bitrate mode each frame gets the bitrate over the frame rate,
    /// so halving the frame rate about doubles each frame.
    #[test]
    fn frame_rate_changes_the_per_frame_budget() {
        let (w, h) = (320, 240);
        let mut rng = XorShift(5);
        let mut enc = H264Encoder::new(rate_config(w, h, 500_000)).expect("init");
        encode_run(&mut enc, 0, 15, &mut rng);
        let at_30 = mean(&encode_run(&mut enc, 15, 30, &mut rng));
        enc.set_frame_rate(10.0).expect("set the frame rate");
        assert_eq!(enc.frame_rate(), 10.0);
        encode_run(&mut enc, 45, 10, &mut rng);
        let at_10 = mean(&encode_run(&mut enc, 55, 30, &mut rng));
        assert!(
            at_10 > at_30 * 2,
            "frames budgeted at 10 fps ({at_10} B) are not much larger than at 30 fps ({at_30} B)"
        );
    }

    #[cfg(feature = "jpeg")]
    mod round_trip {
        use super::*;
        use crate::video::for_stream;
        use crate::{JpegDecoderRsDecoder, SPICE_VIDEO_CODEC_TYPE_H264};
        use std::sync::Arc;

        /// Encode frames, decode them with the client's stream decoder,
        /// and return the mean absolute error of the last one, per
        /// channel, against the source, and the most slices a frame
        /// had.
        fn round_trip(slices: u32) -> (f64, usize) {
            let (w, h) = (320u32, 240u32);
            let config = H264Config {
                slices,
                ..H264Config::new(w, h, 30)
            };
            let mut enc = H264Encoder::new(config).expect("init");
            let mut dec = for_stream(
                SPICE_VIDEO_CODEC_TYPE_H264,
                Arc::new(JpegDecoderRsDecoder::new()),
            )
            .expect("an H.264 decoder");
            let mut rng = XorShift(13);
            let mut last_error = None;
            let mut most_slices = 0;
            for f in 0..10 {
                let source = bgrx_frame(w, h, f, 0, &mut rng);
                let Some(frame) = enc.encode(&bgrx(&source, w, h)).expect("encode") else {
                    continue;
                };
                let frame_slices = nal_types(&frame.data)
                    .into_iter()
                    .filter(|t| *t == 1 || *t == 5)
                    .count();
                most_slices = most_slices.max(frame_slices);
                let Some(decoded) = dec.decode(&frame.data).expect("decode") else {
                    continue;
                };
                assert_eq!((decoded.width, decoded.height), (w, h));
                // BGRX in, RGBA out.
                let total: u64 = source
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(decoded.rgba.as_chunks::<4>().0)
                    .map(|(s, d)| {
                        u64::from(s[2].abs_diff(d[0]))
                            + u64::from(s[1].abs_diff(d[1]))
                            + u64::from(s[0].abs_diff(d[2]))
                    })
                    .sum();
                last_error = Some(total as f64 / f64::from(w * h * 3));
            }
            (last_error.expect("some frame decoded"), most_slices)
        }

        #[test]
        fn frames_decode_close_to_their_source() {
            let (error, slices) = round_trip(1);
            assert!(error < 6.0, "mean error {error:.2} per channel");
            assert_eq!(slices, 1);
        }

        #[test]
        fn sliced_frames_still_decode() {
            let (error, slices) = round_trip(4);
            assert!(error < 6.0, "mean error {error:.2} per channel");
            assert_eq!(slices, 4, "the frames were not split into four slices");
        }
    }
}
