//! Still-photo export: one Insta360 X6 `.dng` → a VR180 side-by-side still.
//!
//! The still is one 15520×7760 Bayer frame holding both fisheye circles
//! (left half = calibration entry 1 = LEFT eye post-mod, right half = entry 2
//! = RIGHT eye). Each half is developed on the CPU
//! ([`raw_develop::develop_linear`]: gain map, demosaic, camera-linear
//! RGBA64), projected through the same UCM fisheye kernels the video path
//! uses (16-bit, [`Device::project_fisheye_to_equirect_texture_16`]), read
//! back, and composed side by side on the CPU (a native 15520-wide SBS
//! texture would exceed wgpu's default 8192 limit).
//!
//! Two kinds of output:
//! - **Developed** (PNG 16-bit / JPEG): [`raw_develop::develop_display`]
//!   runs before projection (white balance, matrix, exposure, tone, OETF).
//! - **Linear DNG**: the camera-native linear data is projected as is and
//!   written by [`dng_write`] with the source's colour tags, so a raw
//!   developer renders the VR180 still with the camera's own profile and
//!   full raw latitude. The gain map is baked in before reprojection and
//!   therefore dropped from the output; the headroom it adds is carried by
//!   `BaselineExposure + log2(l_max)` with the data scaled by `1/l_max`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::dng_write::{self, DngCompression, LinearDngImage, LinearDngMeta};
use crate::gpu::{Device, EquirectRotation, FisheyeCalib};
use crate::insv_imu;
use crate::panomap::{mat3_mul_row_major, ViewAdjust};
use crate::raw_develop::{self, DevelopParams, DngColorLike, GainMapLike, Half, LinearEye, RawParams};
use crate::{Error, Result};
use vr180_fisheye::dng::{self as x6dng, X6Still};
use vr180_fisheye::insta360::InsvWindowCrop;

/// Output container for a still export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StillFormat {
    /// 16-bit RGB PNG, display-referred (Rec.709 / sRGB).
    Png16,
    /// 8-bit JPEG, display-referred. `quality` 1..=100.
    Jpeg { quality: u8 },
    /// Linear DNG (camera-native linear RGB, raw developer renders it).
    LinearDng { compression: DngCompression },
}

impl StillFormat {
    pub fn extension(self) -> &'static str {
        match self {
            StillFormat::Png16 => "png",
            StillFormat::Jpeg { .. } => "jpg",
            StillFormat::LinearDng { .. } => "dng",
        }
    }
    pub fn is_linear_dng(self) -> bool { matches!(self, StillFormat::LinearDng { .. }) }
}

/// Everything a still export needs.
#[derive(Debug, Clone)]
pub struct StillExportConfig {
    pub source_path: PathBuf,
    pub output_path: PathBuf,
    pub format: StillFormat,
    /// Output size per eye (half-equirect, square). The SBS file is
    /// `2·eye_w × eye_h`.
    pub eye_w: u32,
    pub eye_h: u32,
    /// Pano / stereo view adjust (same semantics as the video export).
    pub view_adjust: ViewAdjust,
    /// Swap the two halves (user toggle). The halves are already in L|R
    /// order, so this is a plain swap — unlike the dual-stream video path.
    pub swap_eyes: bool,
    /// Developed-output parameters (ignored for the linear DNG).
    pub develop: DevelopParams,
}

/// Progress callback: `(stage, fraction 0..1)`.
pub type StillProgress<'a> = &'a (dyn Fn(&str, f32) + Sync);

/// The per-eye calibration for the still in its own 7760² half-frame.
///
/// The lens strings describe a 15488×7744 frame; the still is two 7760²
/// halves with the 7744 window placed at offset 0 (trailer field 27:
/// `src 7744² → dst 7760², offset 0`). `stream_lens_calib` applies the same
/// `(src − dst)/2 + offset` rule that was verified against Insta360 Studio
/// for video, at scale 1 (no binning for stills).
pub fn still_eye_calib(still: &X6Still, half: Half) -> Result<FisheyeCalib> {
    let half_w = still.width / 2;
    let half_h = still.height;
    let entry = still
        .lenses
        .get(match half { Half::Left => 0, Half::Right => 1 })
        .ok_or_else(|| Error::Ffmpeg("DNG: fewer than two lens calibrations".into()))?;
    let crop = still.crop.unwrap_or(InsvWindowCrop {
        src_w: still.calib_frame.1, src_h: still.calib_frame.1,
        dst_w: half_w, dst_h: half_h, offset_x: 0, offset_y: 0,
    });
    let lens = insv_imu::stream_lens_calib(entry, Some(&crop), half_w, half_h);
    let omni = lens.omni.ok_or_else(|| Error::Ffmpeg("DNG: lens calibration has no UCM model".into()))?;
    let cx = lens.cx.unwrap_or(half_w as f32 * 0.5);
    let cy = lens.cy.unwrap_or(half_h as f32 * 0.5);
    Ok(FisheyeCalib::new_omni(
        omni.fx, omni.fy, cx, cy, omni.xi, omni.radial, omni.tangential, omni.prism,
        half_w as f32, half_h as f32,
    ))
}

/// Per-eye output rotations for a still: no stabilization (identity),
/// only the pano / stereo view adjust, exactly as the preview's
/// `render_still_from_pair` composes them.
pub fn still_eye_rotations(view_adjust: &ViewAdjust) -> (EquirectRotation, EquirectRotation) {
    if view_adjust.is_identity() {
        return (EquirectRotation::IDENTITY, EquirectRotation::IDENTITY);
    }
    let (vl, vr) = view_adjust.per_eye_matrices();
    (
        EquirectRotation(mat3_mul_row_major(&EquirectRotation::IDENTITY.0, &vl)),
        EquirectRotation(mat3_mul_row_major(&EquirectRotation::IDENTITY.0, &vr)),
    )
}

/// One projected eye: RGBA64LE bytes, `eye_w × eye_h`.
fn project_eye(
    device: &Device,
    rgba64: &[u16],
    src_w: u32, src_h: u32,
    eye_w: u32, eye_h: u32,
    rot: EquirectRotation,
    calib: FisheyeCalib,
) -> Result<Vec<u8>> {
    let bytes: &[u8] = bytemuck::cast_slice(rgba64);
    let tex = device.project_fisheye_to_equirect_texture_16(bytes, src_w, src_h, eye_w, eye_h, rot, calib)?;
    let out = device.read_texture_rgba64(&tex, eye_w, eye_h)?;
    Ok(out)
}

/// Compose two RGBA64LE eyes side by side into interleaved RGB16 (no alpha).
fn compose_rgb16(left: &[u8], right: &[u8], eye_w: u32, eye_h: u32) -> Vec<u16> {
    let (w, h) = (eye_w as usize, eye_h as usize);
    let mut out = vec![0u16; w * 2 * h * 3];
    let l: &[u16] = bytemuck::cast_slice(left);
    let r: &[u16] = bytemuck::cast_slice(right);
    for y in 0..h {
        let row = &mut out[y * w * 2 * 3..(y + 1) * w * 2 * 3];
        for (dst, src) in [(0usize, l), (w, r)] {
            let srow = &src[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                let o = (dst + x) * 3;
                row[o] = srow[x * 4];
                row[o + 1] = srow[x * 4 + 1];
                row[o + 2] = srow[x * 4 + 2];
            }
        }
    }
    out
}

fn linear_dng_meta(still: &X6Still, l_max: f32) -> LinearDngMeta {
    let c = &still.color;
    let e = &still.exif;
    LinearDngMeta {
        make: if e.make.is_empty() { "Arashi Vision".into() } else { e.make.clone() },
        model: if e.model.is_empty() { "Insta360 X6".into() } else { e.model.clone() },
        unique_camera_model: if e.unique_camera_model.is_empty() { "Insta360 X6".into() } else { e.unique_camera_model.clone() },
        software: format!("VR180 Silver Bullet {}", env!("CARGO_PKG_VERSION")),
        color_matrix1: c.color_matrix1,
        calibration_illuminant1: c.calibration_illuminant1,
        as_shot_neutral: c.as_shot_neutral,
        // The gain map is baked into the data at 1/l_max scale: tell the
        // developer the scene is l_max brighter than the codes say.
        baseline_exposure: c.baseline_exposure + l_max.max(1.0).log2(),
        baseline_noise: c.baseline_noise,
        baseline_sharpness: c.baseline_sharpness,
        profile_hue_sat_map_dims: c.profile_hue_sat_map_dims,
        profile_hue_sat_map_data1: c.profile_hue_sat_map_data1.clone(),
        profile_hue_sat_map_encoding: c.profile_hue_sat_map_encoding,
        exposure_s: e.exposure_s,
        f_number: e.f_number,
        iso: e.iso,
        focal_mm: e.focal_mm,
        focal_35mm: e.focal_35mm,
        datetime_original: e.datetime_original.clone(),
        user_comment: e.user_comment.clone(),
        exposure_program: e.exposure_program,
        metering_mode: e.metering_mode,
        white_balance: e.white_balance,
        exposure_compensation: e.exposure_compensation,
        shutter_speed_value: e.shutter_speed_value,
        image_description: Some("VR180 side-by-side (left | right), half-equirect per eye".into()),
    }
}

/// Export one still. Blocking; call from a worker thread.
pub fn export_still(device: Arc<Device>, cfg: &StillExportConfig, progress: StillProgress) -> Result<()> {
    progress("Reading DNG", 0.0);
    let still = x6dng::read(&cfg.source_path).map_err(|e| Error::Ffmpeg(format!("DNG read: {e}")))?;
    if still.lenses.len() < 2 {
        return Err(Error::Ffmpeg("DNG: no Insta360 lens calibration found".into()));
    }
    let half_w = still.width / 2;
    let half_h = still.height;
    let params = RawParams::from(&still);
    let gain_maps: Vec<GainMapLike> = raw_develop::gain_maps_from(&still.gain_maps);
    let color_like = DngColorLike::from(&still.color);
    let (rot_l, rot_r) = still_eye_rotations(&cfg.view_adjust);
    let linear = cfg.format.is_linear_dng();

    // Eye order: left half → left eye unless swapped.
    let order = if cfg.swap_eyes { [Half::Right, Half::Left] } else { [Half::Left, Half::Right] };
    let mut eyes: Vec<Vec<u8>> = Vec::with_capacity(2);
    let mut l_max = 1.0_f32;
    for (i, half) in order.iter().enumerate() {
        let label = if i == 0 { "left" } else { "right" };
        progress(&format!("Developing {label} eye"), 0.1 + 0.35 * i as f32);
        let eye: LinearEye = raw_develop::develop_linear(&still.bayer, &params, &gain_maps, *half, false);
        l_max = l_max.max(eye.l_max);
        let rgba: Vec<u16> = if linear {
            eye.rgba.clone()
        } else {
            raw_develop::develop_display(&eye, &color_like, &cfg.develop)
        };
        drop(eye);
        progress(&format!("Projecting {label} eye"), 0.25 + 0.35 * i as f32);
        let calib = still_eye_calib(&still, *half)?;
        let rot = if i == 0 { rot_l } else { rot_r };
        let out = project_eye(&device, &rgba, half_w, half_h, cfg.eye_w, cfg.eye_h, rot, calib)?;
        eyes.push(out);
    }
    progress("Writing", 0.85);
    let rgb16 = compose_rgb16(&eyes[0], &eyes[1], cfg.eye_w, cfg.eye_h);
    drop(eyes);
    let (w, h) = (cfg.eye_w * 2, cfg.eye_h);
    write_output(&cfg.output_path, cfg.format, w, h, &rgb16, &still, l_max)?;
    progress("Done", 1.0);
    Ok(())
}

fn write_output(path: &Path, format: StillFormat, w: u32, h: u32, rgb16: &[u16], still: &X6Still, l_max: f32) -> Result<()> {
    match format {
        StillFormat::Png16 => {
            let bytes: Vec<u8> = rgb16.iter().flat_map(|v| v.to_be_bytes()).collect();
            image::save_buffer(path, &bytes, w, h, image::ExtendedColorType::Rgb16)
                .map_err(|e| Error::Ffmpeg(format!("PNG write: {e}")))?;
        }
        StillFormat::Jpeg { quality } => {
            let rgb8: Vec<u8> = rgb16.iter().map(|v| (v >> 8) as u8).collect();
            let file = std::fs::File::create(path)?;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), quality.clamp(1, 100));
            use image::ImageEncoder;
            enc.write_image(&rgb8, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| Error::Ffmpeg(format!("JPEG write: {e}")))?;
        }
        StillFormat::LinearDng { compression } => {
            let meta = linear_dng_meta(still, l_max);
            let img = LinearDngImage { width: w, height: h, rgb16, white_level: 65535 };
            dng_write::write_linear_dng(path, &img, &meta, compression, None)?;
        }
    }
    Ok(())
}
