//! Insta360 X6 DNG still reader (TIFF/DNG IFD walk, Deflate strips, DNG
//! colour tags, GainMap opcodes, the still-photo Insta360 trailer).
//!
//! The X6 writes its stills as a classic little-endian TIFF / DNG 1.3 with a
//! single IFD0 that *is* the raw image (no thumbnail, no SubIFD):
//!
//! ```text
//!   II*\0 [u32 ifd0]  | strips (Compression 8 = zlib, 31 × 256 rows) |
//!   IFD0 + tag values | ExifIFD (UserComment = "INSTA360" + lens string) |
//!   a 2nd copy of OpcodeList2 | still trailer | 78-byte Insta360 footer
//! ```
//!
//! - 15520 × 7760, one 16-bit sample per pixel (14-bit codes, WhiteLevel
//!   16383, BlackLevel 0), CFA **GBRG** (`CFAPattern 1 2 0 1`), both lens
//!   halves side by side (left half = calibration entry 1, right half =
//!   entry 2).
//! - `OpcodeList2` carries four DNG 1.3 **GainMap** opcodes (one per CFA
//!   site, 13 × 34 grid each) = the per-lens vignetting + colour-shading
//!   correction, to be applied after black subtraction and before demosaic.
//! - The EXIF `UserComment` (and the trailer's protobuf fields 111/112)
//!   hold the same factory lens string as the camera's videos
//!   ([`parse_calib_string`]), so stills share the video lens geometry.
//! - The file ends with the Insta360 trailer in the **still** layout: a
//!   backward chain `[data][u16 id, big-endian][u32 size]` whose last link is
//!   the footer's own first 6 bytes (the `ExtraMetadata` protobuf), then
//!   IMU samples (0x0003, ~0.6 s around the capture), the AE log (0x0009)
//!   and two ISP blobs (0x001b / 0x001c). `trailer_len` in the footer counts
//!   from the first record to EOF, so the chain stops at `EOF − trailer_len`.
//!
//! Everything here is platform-neutral `std` + `flate2`; strips are inflated
//! on a pool of scoped threads while the main thread streams the compressed
//! bytes from disk, so peak memory is the output mosaic (240 MB for the X6)
//! plus a handful of compressed strips in flight.

use crate::insta360::{
    has_insv_trailer, parse_calib_string, InsvImuSample, InsvLensCalib, InsvWindowCrop,
    INSV_ACCEL_LSB_PER_G, INSV_GYRO_LSB_PER_DPS, INSV_TRAILER_MAGIC,
};
use crate::{Error, Result};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

// ── Public types ───────────────────────────────────────────────────

/// One DNG 1.3 `GainMap` opcode (OpcodeList2 id 9), as stored in the file.
///
/// The map is a `points_v × points_h` grid of `map_planes` gains placed in
/// normalised coordinates — grid node `(i, j)` sits at
/// `v = origin_v + i·spacing_v`, `u = origin_h + j·spacing_h` where `(0, 0)`
/// is the top-left and `(1, 1)` the bottom-right of the opcode's area
/// (`top/left/bottom/right`, which on the X6 is the whole frame). It applies
/// to the pixels of that area stepping by `row_pitch / col_pitch` — the X6
/// writes four opcodes with pitch 2 × 2 and `top/left ∈ {0, 1}`, i.e. one per
/// CFA site ([`GainMap::cfa_site`]). Gains multiply the linear,
/// black-subtracted value before demosaic (`pixel *= gain`); the DNG SDK
/// clamps the product to 1.0, the X6's maps reach 2.65 at the corners.
#[derive(Debug, Clone)]
pub struct GainMap {
    pub top: u32,
    pub left: u32,
    pub bottom: u32,
    pub right: u32,
    pub plane: u32,
    pub planes: u32,
    pub row_pitch: u32,
    pub col_pitch: u32,
    pub points_v: u32,
    pub points_h: u32,
    pub spacing_v: f64,
    pub spacing_h: f64,
    pub origin_v: f64,
    pub origin_h: f64,
    pub map_planes: u32,
    /// `points_v × points_h × map_planes` gains, row-major (plane fastest).
    pub gains: Vec<f32>,
}

impl GainMap {
    /// The CFA site this map applies to: `(top % 2, left % 2)` — meaningful
    /// for the X6's pitch-2 opcodes (GBRG: `(0,0)` G, `(0,1)` B, `(1,0)` R,
    /// `(1,1)` G).
    pub fn cfa_site(&self) -> (u32, u32) {
        (self.top % 2, self.left % 2)
    }

    /// Bilinear gain for the pixel at `(row, col)` (image coordinates) in
    /// map plane `plane`, exactly as the DNG SDK interpolates: normalised
    /// position within the opcode area, clamped to the grid edges.
    pub fn gain_at(&self, row: u32, col: u32, plane: u32) -> f32 {
        let h = self.bottom.saturating_sub(self.top).max(1) as f64;
        let w = self.right.saturating_sub(self.left).max(1) as f64;
        let v = (row as f64 - self.top as f64) / h;
        let u = (col as f64 - self.left as f64) / w;
        let (i0, ti) = grid_index(v, self.origin_v, self.spacing_v, self.points_v);
        let (j0, tj) = grid_index(u, self.origin_h, self.spacing_h, self.points_h);
        let planes = self.map_planes.max(1) as usize;
        let p = (plane as usize).min(planes - 1);
        let ph = self.points_h.max(1) as usize;
        let i1 = (i0 + 1).min(self.points_v.max(1) as usize - 1);
        let j1 = (j0 + 1).min(ph - 1);
        let at = |i: usize, j: usize| -> f64 {
            self.gains.get((i * ph + j) * planes + p).copied().unwrap_or(1.0) as f64
        };
        let top = (1.0 - tj) * at(i0, j0) + tj * at(i0, j1);
        let bot = (1.0 - tj) * at(i1, j0) + tj * at(i1, j1);
        ((1.0 - ti) * top + ti * bot) as f32
    }
}

/// Grid cell + fraction for a normalised coordinate (clamped to the grid).
fn grid_index(x: f64, origin: f64, spacing: f64, points: u32) -> (usize, f64) {
    if points <= 1 || spacing.is_nan() || spacing <= 0.0 || !x.is_finite() {
        return (0, 0.0);
    }
    let f = ((x - origin) / spacing).clamp(0.0, (points - 1) as f64);
    let i0 = (f.floor() as usize).min(points as usize - 2);
    (i0, f - i0 as f64)
}

/// DNG colour / calibration tags of the still.
#[derive(Debug, Clone)]
pub struct DngColor {
    /// `ColorMatrix1`: XYZ → camera, row-major (identity when the tag is absent).
    pub color_matrix1: [f32; 9],
    /// `CalibrationIlluminant1` (21 = D65 on the X6; 0 when absent).
    pub calibration_illuminant1: u16,
    /// `AsShotNeutral` in camera space (`[1,1,1]` when absent).
    pub as_shot_neutral: [f32; 3],
    /// `BaselineExposure` in EV (+2.60 on the X6).
    pub baseline_exposure: f32,
    /// `WhiteLevel` (16383 on the X6; `2^bits − 1` when absent).
    pub white_level: u32,
    /// `BlackLevel` per CFA site, row-major 2 × 2; 0 when absent.
    pub black_level: [f32; 4],
    /// `ProfileHueSatMapDims` (tag 50937): hue, sat, val divisions.
    pub profile_hue_sat_map_dims: Option<[u32; 3]>,
    /// `ProfileHueSatMapData1` (tag 50938): `hue·sat·val` triples.
    pub profile_hue_sat_map_data1: Option<Vec<f32>>,
    /// `ProfileHueSatMapEncoding` (tag 51107).
    pub profile_hue_sat_map_encoding: Option<u32>,
    pub baseline_noise: Option<f32>,
    pub baseline_sharpness: Option<f32>,
}

/// Camera / exposure tags (IFD0 strings + the EXIF IFD).
#[derive(Debug, Clone, Default)]
pub struct DngExif {
    pub make: String,
    pub model: String,
    pub unique_camera_model: String,
    pub software: String,
    pub exposure_s: Option<f64>,
    pub f_number: Option<f64>,
    pub iso: Option<u32>,
    pub focal_mm: Option<f64>,
    pub focal_35mm: Option<u32>,
    pub datetime_original: Option<String>,
    /// `UserComment` text after its 8-byte character-code prefix (the X6
    /// writes `"INSTA360"` there, then the lens string).
    pub user_comment: Option<String>,
}

/// A fully decoded Insta360 X6 DNG still.
#[derive(Debug, Clone)]
pub struct X6Still {
    pub width: u32,
    pub height: u32,
    /// Row-major raw codes, `width × height` samples (14-bit in 16).
    pub bayer: Vec<u16>,
    /// CFA pattern, 2 × 2 row-major, 0 = R, 1 = G, 2 = B (`[1, 2, 0, 1]` = GBRG).
    pub cfa: [u8; 4],
    pub color: DngColor,
    pub exif: DngExif,
    /// GainMap opcodes from `OpcodeList2`, in file order.
    pub gain_maps: Vec<GainMap>,
    /// Factory lens entries: `[0]` = left half, `[1]` = right half.
    pub lenses: Vec<InsvLensCalib>,
    /// Calibration frame size from the lens string (tokens 25/26), e.g.
    /// `(15488, 7744)`; `(0, 0)` when no lens string was found.
    pub calib_frame: (u32, u32),
    /// Sensor window → recorded frame (trailer field 27; the still pads
    /// 7744² → 7760², offset 0).
    pub crop: Option<InsvWindowCrop>,
    /// Sensor readout time in ms (trailer field 25; 37.5 for stills).
    pub rolling_shutter_ms: Option<f64>,
    /// Capture instant on the IMU's boot clock, µs (trailer field 24).
    pub capture_time_us: Option<u64>,
    /// Trailer field 210: nine floats in file order (an orthonormal 3 × 3,
    /// heading-like about its z).
    pub orientation: Option<[f32; 9]>,
    /// IMU samples from record 0x0003, physical units, same decoder rule as
    /// the videos.
    pub imu: Vec<InsvImuSample>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    /// Recorded frame size from trailer field 19 (should equal `(width, height)`).
    pub dimension: Option<(u32, u32)>,
    /// Every record id found in the still trailer chain, footer first.
    pub record_ids: Vec<u16>,
}

// ── Public entry points ────────────────────────────────────────────

/// Cheap classification: the file starts with a little-endian TIFF header
/// and ends with the Insta360 trailer magic.
pub fn is_insta360_dng(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let mut hdr = [0u8; 4];
    if f.read_exact(&mut hdr).is_err() || &hdr != TIFF_LE_MAGIC {
        return false;
    }
    drop(f);
    has_insv_trailer(path)
}

/// Image dimensions `(width, height)` from the main image IFD; no pixel decode.
pub fn probe(path: &Path) -> Result<(u32, u32)> {
    let mut t = TiffFile::open(path)?;
    let (_, main) = t.main_ifd()?;
    let w = t.tag_u32(&main, TAG_IMAGE_WIDTH)?.ok_or_else(|| bad("no ImageWidth"))?;
    let h = t.tag_u32(&main, TAG_IMAGE_LENGTH)?.ok_or_else(|| bad("no ImageLength"))?;
    Ok((w, h))
}

/// Full decode: raw mosaic, colour tags, EXIF, GainMaps, lens strings and
/// the still trailer (IMU, capture time, orientation).
pub fn read(path: &Path) -> Result<X6Still> {
    let mut t = TiffFile::open(path)?;
    let (ifd0, main) = t.main_ifd()?;

    // ── DNG identity ──
    let dng_version = t.tag_bytes(&ifd0, TAG_DNG_VERSION)?;
    match dng_version.as_deref() {
        Some([1, ..]) => {}
        Some(v) => return Err(bad(format!("unsupported DNGVersion {v:?}"))),
        None => return Err(bad("no DNGVersion tag — not a DNG")),
    }

    // ── Image geometry ──
    let layout = strip_layout(&mut t, &main)?;
    let cfa = cfa_pattern(&mut t, &main, &ifd0)?;

    // ── Colour + camera tags (IFD0, falling back to the image IFD) ──
    let color = color_tags(&mut t, &ifd0, &main)?;
    let mut exif = DngExif {
        make: t.tag_ascii(&ifd0, TAG_MAKE)?.unwrap_or_default(),
        model: t.tag_ascii(&ifd0, TAG_MODEL)?.unwrap_or_default(),
        unique_camera_model: t.tag_ascii(&ifd0, TAG_UNIQUE_CAMERA_MODEL)?.unwrap_or_default(),
        software: t.tag_ascii(&ifd0, TAG_SOFTWARE)?.unwrap_or_default(),
        ..Default::default()
    };
    if let Some(off) = t.tag_u32(&ifd0, TAG_EXIF_IFD)? {
        let exif_ifd = t.read_ifd(off as u64)?;
        exif_tags(&mut t, &exif_ifd, &mut exif)?;
    }
    let gain_maps = match t.tag_bytes(&ifd0, TAG_OPCODE_LIST2)?.or(t.tag_bytes(&main, TAG_OPCODE_LIST2)?) {
        Some(b) => parse_opcode_list2(&b)?,
        None => Vec::new(),
    };

    // ── Still trailer (small, at the end of the file) ──
    let trailer = read_still_trailer(&mut t.file, t.len)?;
    let meta = trailer.protobuf.as_deref().map(parse_still_protobuf).unwrap_or_default();
    let imu = trailer
        .record(REC_IMU)
        .map(|b| decode_imu_record(b, meta.acc_range_g, meta.gyro_range_dps))
        .unwrap_or_default();

    // ── Lens strings: EXIF UserComment first, then the trailer's copies ──
    let mut lenses = exif.user_comment.as_deref().and_then(find_lens_string).map(|(_, l)| l);
    if lenses.is_none() {
        lenses = meta.lens_strings.iter().find_map(|s| find_lens_string(s)).map(|(_, l)| l);
    }
    let lenses = lenses.unwrap_or_default();
    let calib_frame = lenses.first().map(|l| (l.sensor_w, l.sensor_h)).unwrap_or((0, 0));

    // ── Pixels (the expensive part; parallel inflate) ──
    let n = (layout.width as usize)
        .checked_mul(layout.height as usize)
        .ok_or_else(|| bad("image too large"))?;
    let mut bayer = vec![0u16; n];
    decode_strips(&mut t.file, &layout, &mut bayer)?;

    Ok(X6Still {
        width: layout.width,
        height: layout.height,
        bayer,
        cfa,
        color,
        exif,
        gain_maps,
        lenses,
        calib_frame,
        crop: meta.crop,
        rolling_shutter_ms: meta.rolling_shutter_ms,
        capture_time_us: meta.capture_time_us,
        orientation: meta.orientation,
        imu,
        serial: meta.serial,
        firmware: meta.firmware,
        dimension: meta.dimension,
        record_ids: trailer.records.iter().map(|(id, _)| *id).collect(),
    })
}

/// Locate the `2_…` factory calibration run inside arbitrary text (the EXIF
/// `UserComment` after its prefix, or a trailer string) and parse it. Same
/// rule as the video trailer scan: the run starts at a `2_` not preceded by a
/// token character and extends over digits, `_`, `.`, `-`, `e`, `E` — so the
/// X6's `"…_193_394240 0.000000_…"` tail (note the space) is cut off.
pub fn find_lens_string(text: &str) -> Option<(&str, Vec<InsvLensCalib>)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    while i + 2 < n {
        let starts_here = bytes[i] == b'2'
            && bytes[i + 1] == b'_'
            && bytes[i + 2].is_ascii_digit()
            && (i == 0 || !is_token_byte(bytes[i - 1]));
        if !starts_here {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && is_token_byte(bytes[j]) {
            j += 1;
        }
        let run = &text[i..j];
        if let Some(lenses) = parse_calib_string(run) {
            return Some((run, lenses));
        }
        i = j.max(i + 1);
    }
    None
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'-' || b == b'e' || b == b'E'
}

/// Decode a 0x0003 IMU record: 20-byte samples `[u64 t_µs][ax ay az gx gy gz]`
/// with the six channels as **u16 offset-binary** (`raw − 32768`), LSB per
/// unit = `32768 / full-scale`. The ranges come from the trailer's
/// `gyro_cfg_info` (X6: ±32 g → 1024 LSB/g, ±2000 °/s → 16.384 LSB/(°/s));
/// without them the X6 constants are used. Stops at the first zero or
/// non-monotonic timestamp (unused tail padding).
pub fn decode_imu_record(bytes: &[u8], acc_range_g: Option<u32>, gyro_range_dps: Option<u32>) -> Vec<InsvImuSample> {
    let acc_lsb = acc_range_g.filter(|&r| r > 0).map(|r| 32768.0 / r as f32).unwrap_or(INSV_ACCEL_LSB_PER_G);
    let gyr_lsb = gyro_range_dps.filter(|&r| r > 0).map(|r| 32768.0 / r as f32).unwrap_or(INSV_GYRO_LSB_PER_DPS);
    let (acc_scale, gyr_scale) = (1.0 / acc_lsb, 1.0 / gyr_lsb);
    let mut out = Vec::with_capacity(bytes.len() / 20);
    let mut last_t = 0u64;
    for rec in bytes.chunks_exact(20) {
        let t_us = u64::from_le_bytes(rec[0..8].try_into().unwrap());
        if t_us == 0 || (last_t != 0 && t_us <= last_t) {
            break;
        }
        last_t = t_us;
        let v = |k: usize| (u16::from_le_bytes([rec[8 + 2 * k], rec[9 + 2 * k]]) as i32 - 32768) as f32;
        out.push(InsvImuSample {
            t_us,
            acc_g: [v(0) * acc_scale, v(1) * acc_scale, v(2) * acc_scale],
            gyr_dps: [v(3) * gyr_scale, v(4) * gyr_scale, v(5) * gyr_scale],
        });
    }
    out
}

/// Parse a DNG `OpcodeList2` blob (big-endian): `[u32 count]` then per opcode
/// `[u32 id][u32 version][u32 flags][u32 size][size bytes]`. Returns the
/// GainMap opcodes (id 9); other ids are skipped (logged at debug level).
pub fn parse_opcode_list2(bytes: &[u8]) -> Result<Vec<GainMap>> {
    let mut maps = Vec::new();
    if bytes.len() < 4 {
        return Ok(maps);
    }
    let count = be_u32(bytes, 0);
    let mut pos = 4usize;
    for k in 0..count {
        let hdr = bytes
            .get(pos..pos + 16)
            .ok_or_else(|| bad(format!("OpcodeList2: opcode {k} header truncated")))?;
        let id = be_u32(hdr, 0);
        let version = be_u32(hdr, 4);
        let flags = be_u32(hdr, 8);
        let size = be_u32(hdr, 12) as usize;
        pos += 16;
        let body = bytes
            .get(pos..pos.saturating_add(size))
            .ok_or_else(|| bad(format!("OpcodeList2: opcode {k} (id {id}) body of {size} bytes truncated")))?;
        pos += size;
        if id == OPCODE_GAIN_MAP {
            maps.push(parse_gain_map(body).map_err(|e| bad(format!("OpcodeList2: opcode {k}: {e}")))?);
        } else {
            tracing::debug!("OpcodeList2: skipping opcode id {id} (version {version:#x}, flags {flags}, {size} bytes)");
        }
    }
    Ok(maps)
}

fn parse_gain_map(body: &[u8]) -> Result<GainMap> {
    if body.len() < 76 {
        return Err(bad(format!("GainMap body of {} bytes is shorter than its 76-byte header", body.len())));
    }
    let u = |i: usize| be_u32(body, i);
    let d = |i: usize| f64::from_be_bytes(body[i..i + 8].try_into().unwrap());
    let map = GainMap {
        top: u(0),
        left: u(4),
        bottom: u(8),
        right: u(12),
        plane: u(16),
        planes: u(20),
        row_pitch: u(24),
        col_pitch: u(28),
        points_v: u(32),
        points_h: u(36),
        spacing_v: d(40),
        spacing_h: d(48),
        origin_v: d(56),
        origin_h: d(64),
        map_planes: u(72),
        gains: Vec::new(),
    };
    if map.points_v == 0 || map.points_h == 0 || map.map_planes == 0 {
        return Err(bad("GainMap with an empty grid"));
    }
    let n = (map.points_v as usize)
        .checked_mul(map.points_h as usize)
        .and_then(|n| n.checked_mul(map.map_planes as usize))
        .filter(|&n| n <= (1 << 24))
        .ok_or_else(|| bad("GainMap grid too large"))?;
    let data = body
        .get(76..76 + 4 * n)
        .ok_or_else(|| bad(format!("GainMap needs {n} gains, body holds {}", (body.len() - 76) / 4)))?;
    let gains = data.chunks_exact(4).map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect();
    Ok(GainMap { gains, ..map })
}

fn be_u32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

// ── TIFF layer ─────────────────────────────────────────────────────

const TIFF_LE_MAGIC: &[u8; 4] = b"II*\0";
/// Bytes read around IFD0 in one go (the X6 keeps IFD0, its values, the
/// EXIF IFD and the trailer in the last ~25 KB of the file).
const WINDOW_LEN: usize = 256 * 1024;

const T_BYTE: u16 = 1;
const T_ASCII: u16 = 2;
const T_SHORT: u16 = 3;
const T_LONG: u16 = 4;
const T_RATIONAL: u16 = 5;
const T_SBYTE: u16 = 6;
const T_UNDEFINED: u16 = 7;
const T_SSHORT: u16 = 8;
const T_SLONG: u16 = 9;
const T_SRATIONAL: u16 = 10;
const T_FLOAT: u16 = 11;
const T_DOUBLE: u16 = 12;
/// TIFF-EP `IFD` pointer type — what the X6 uses for `ExifIFD`.
const T_IFD: u16 = 13;

const TAG_NEW_SUBFILE_TYPE: u16 = 254;
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_MAKE: u16 = 271;
const TAG_MODEL: u16 = 272;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_PLANAR_CONFIG: u16 = 284;
const TAG_SOFTWARE: u16 = 305;
const TAG_PREDICTOR: u16 = 317;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_SUB_IFDS: u16 = 330;
const TAG_CFA_REPEAT_PATTERN_DIM: u16 = 33421;
const TAG_CFA_PATTERN: u16 = 33422;
const TAG_EXIF_IFD: u16 = 34665;
const TAG_DNG_VERSION: u16 = 50706;
const TAG_UNIQUE_CAMERA_MODEL: u16 = 50708;
const TAG_CFA_PLANE_COLOR: u16 = 50710;
const TAG_CFA_LAYOUT: u16 = 50711;
const TAG_LINEARIZATION_TABLE: u16 = 50712;
const TAG_BLACK_LEVEL_REPEAT_DIM: u16 = 50713;
const TAG_BLACK_LEVEL: u16 = 50714;
const TAG_WHITE_LEVEL: u16 = 50717;
const TAG_COLOR_MATRIX1: u16 = 50721;
const TAG_AS_SHOT_NEUTRAL: u16 = 50728;
const TAG_BASELINE_EXPOSURE: u16 = 50730;
const TAG_BASELINE_NOISE: u16 = 50731;
const TAG_BASELINE_SHARPNESS: u16 = 50732;
const TAG_CALIBRATION_ILLUMINANT1: u16 = 50778;
/// DNG 1.2 profile tags — the spec numbers are 50937 (Dims) / 50938 (Data1)
/// / 50939 (Data2); the X6 writes 50937 + 50938 + 51107.
const TAG_PROFILE_HUE_SAT_MAP_DIMS: u16 = 50937;
const TAG_PROFILE_HUE_SAT_MAP_DATA1: u16 = 50938;
const TAG_OPCODE_LIST2: u16 = 51009;
const TAG_PROFILE_HUE_SAT_MAP_ENCODING: u16 = 51107;

const EXIF_EXPOSURE_TIME: u16 = 33434;
const EXIF_F_NUMBER: u16 = 33437;
const EXIF_ISO: u16 = 34855;
const EXIF_DATETIME_ORIGINAL: u16 = 36867;
const EXIF_FOCAL_LENGTH: u16 = 37386;
const EXIF_USER_COMMENT: u16 = 37510;
const EXIF_FOCAL_35MM: u16 = 41989;

const PHOTOMETRIC_CFA: u32 = 32803;
const COMPRESSION_NONE: u32 = 1;
const COMPRESSION_DEFLATE: u32 = 8;
const OPCODE_GAIN_MAP: u32 = 9;

fn bad(msg: impl Into<String>) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidData, format!("dng: {}", msg.into())))
}

fn type_size(typ: u16) -> Option<usize> {
    match typ {
        T_BYTE | T_ASCII | T_SBYTE | T_UNDEFINED => Some(1),
        T_SHORT | T_SSHORT => Some(2),
        T_LONG | T_SLONG | T_FLOAT | T_IFD => Some(4),
        T_RATIONAL | T_SRATIONAL | T_DOUBLE => Some(8),
        _ => None,
    }
}

/// One IFD entry; `raw` is the 4-byte value-or-offset field.
#[derive(Debug, Clone, Copy)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    raw: [u8; 4],
}

impl Entry {
    fn byte_len(&self) -> Option<usize> {
        type_size(self.typ)?.checked_mul(self.count as usize)
    }
}

#[derive(Debug, Clone)]
struct Ifd {
    entries: Vec<Entry>,
}

impl Ifd {
    fn get(&self, tag: u16) -> Option<Entry> {
        self.entries.iter().copied().find(|e| e.tag == tag)
    }
}

/// A TIFF file opened for IFD walking: seekable, with one cached window so
/// the ~40 small tag-value reads don't each hit the disk (or the SMB share).
struct TiffFile {
    file: File,
    len: u64,
    ifd0: u64,
    win_start: u64,
    win: Vec<u8>,
}

impl TiffFile {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut hdr = [0u8; 8];
        file.read_exact(&mut hdr).map_err(|_| bad("file shorter than a TIFF header"))?;
        if &hdr[0..4] != TIFF_LE_MAGIC {
            return Err(bad("not a little-endian classic TIFF ('II*\\0' header expected)"));
        }
        let ifd0 = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as u64;
        let mut t = TiffFile { file, len, ifd0, win_start: 0, win: Vec::new() };
        t.prime(ifd0, WINDOW_LEN)?;
        Ok(t)
    }

    /// Load a read-through window starting at `off`.
    fn prime(&mut self, off: u64, want: usize) -> Result<()> {
        if off >= self.len {
            return Err(bad(format!("IFD offset {off} beyond end of file ({} bytes)", self.len)));
        }
        let n = want.min((self.len - off) as usize);
        self.win.clear();
        self.win.resize(n, 0);
        self.file.seek(SeekFrom::Start(off))?;
        self.file.read_exact(&mut self.win)?;
        self.win_start = off;
        Ok(())
    }

    fn read_at(&mut self, off: u64, n: usize) -> Result<Vec<u8>> {
        let end = off.checked_add(n as u64).ok_or_else(|| bad("offset overflow"))?;
        if end > self.len {
            return Err(bad(format!("read of {n} bytes at {off} runs past end of file ({} bytes)", self.len)));
        }
        if off >= self.win_start && end <= self.win_start + self.win.len() as u64 {
            let s = (off - self.win_start) as usize;
            return Ok(self.win[s..s + n].to_vec());
        }
        let mut buf = vec![0u8; n];
        self.file.seek(SeekFrom::Start(off))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn read_ifd(&mut self, off: u64) -> Result<Ifd> {
        let head = self.read_at(off, 2)?;
        let n = u16::from_le_bytes([head[0], head[1]]) as usize;
        let table = self.read_at(off + 2, 12 * n)?;
        let entries = table
            .chunks_exact(12)
            .map(|e| Entry {
                tag: u16::from_le_bytes([e[0], e[1]]),
                typ: u16::from_le_bytes([e[2], e[3]]),
                count: u32::from_le_bytes(e[4..8].try_into().unwrap()),
                raw: e[8..12].try_into().unwrap(),
            })
            .collect();
        Ok(Ifd { entries })
    }

    /// IFD0 and the IFD holding the full-resolution image (IFD0 itself on
    /// the X6; the first full-res `SubIFD` when IFD0 is a thumbnail).
    fn main_ifd(&mut self) -> Result<(Ifd, Ifd)> {
        let ifd0 = self.read_ifd(self.ifd0)?;
        let subtype = self.tag_u32(&ifd0, TAG_NEW_SUBFILE_TYPE)?.unwrap_or(0);
        if subtype & 1 == 1 {
            if let Some(subs) = self.tag_u32s(&ifd0, TAG_SUB_IFDS)? {
                for off in subs {
                    let sub = self.read_ifd(off as u64)?;
                    if self.tag_u32(&sub, TAG_NEW_SUBFILE_TYPE)?.unwrap_or(0) & 1 == 0 {
                        return Ok((ifd0, sub));
                    }
                }
            }
        }
        Ok((ifd0.clone(), ifd0))
    }

    fn value(&mut self, e: &Entry) -> Result<Vec<u8>> {
        let n = e
            .byte_len()
            .ok_or_else(|| bad(format!("tag {}: unknown TIFF type {}", e.tag, e.typ)))?;
        if n <= 4 {
            Ok(e.raw[..n].to_vec())
        } else {
            let off = u32::from_le_bytes(e.raw) as u64;
            self.read_at(off, n)
                .map_err(|err| bad(format!("tag {}: {err}", e.tag)))
        }
    }

    fn tag_bytes(&mut self, ifd: &Ifd, tag: u16) -> Result<Option<Vec<u8>>> {
        match ifd.get(tag) {
            Some(e) => self.value(&e).map(Some),
            None => Ok(None),
        }
    }

    fn tag_u32s(&mut self, ifd: &Ifd, tag: u16) -> Result<Option<Vec<u32>>> {
        let Some(e) = ifd.get(tag) else { return Ok(None) };
        let b = self.value(&e)?;
        to_u32s(e.typ, &b)
            .map(Some)
            .ok_or_else(|| bad(format!("tag {tag}: type {} is not an integer type", e.typ)))
    }

    fn tag_u32(&mut self, ifd: &Ifd, tag: u16) -> Result<Option<u32>> {
        Ok(self.tag_u32s(ifd, tag)?.and_then(|v| v.first().copied()))
    }

    fn tag_f64s(&mut self, ifd: &Ifd, tag: u16) -> Result<Option<Vec<f64>>> {
        let Some(e) = ifd.get(tag) else { return Ok(None) };
        let b = self.value(&e)?;
        to_f64s(e.typ, &b)
            .map(Some)
            .ok_or_else(|| bad(format!("tag {tag}: type {} is not numeric", e.typ)))
    }

    fn tag_f64(&mut self, ifd: &Ifd, tag: u16) -> Result<Option<f64>> {
        Ok(self.tag_f64s(ifd, tag)?.and_then(|v| v.first().copied()))
    }

    fn tag_ascii(&mut self, ifd: &Ifd, tag: u16) -> Result<Option<String>> {
        Ok(self.tag_bytes(ifd, tag)?.map(|b| ascii_string(&b)))
    }
}

fn ascii_string(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

fn to_u32s(typ: u16, b: &[u8]) -> Option<Vec<u32>> {
    Some(match typ {
        T_BYTE | T_UNDEFINED | T_ASCII => b.iter().map(|&v| v as u32).collect(),
        T_SBYTE => b.iter().map(|&v| v as i8 as i32 as u32).collect(),
        T_SHORT => b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32).collect(),
        T_SSHORT => b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as i32 as u32).collect(),
        T_LONG | T_SLONG | T_IFD => b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect(),
        _ => return None,
    })
}

fn to_f64s(typ: u16, b: &[u8]) -> Option<Vec<f64>> {
    let ratio = |num: f64, den: f64| if den == 0.0 { 0.0 } else { num / den };
    Some(match typ {
        T_RATIONAL => b
            .chunks_exact(8)
            .map(|c| ratio(u32::from_le_bytes(c[0..4].try_into().unwrap()) as f64, u32::from_le_bytes(c[4..8].try_into().unwrap()) as f64))
            .collect(),
        T_SRATIONAL => b
            .chunks_exact(8)
            .map(|c| ratio(i32::from_le_bytes(c[0..4].try_into().unwrap()) as f64, i32::from_le_bytes(c[4..8].try_into().unwrap()) as f64))
            .collect(),
        T_FLOAT => b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).collect(),
        T_DOUBLE => b.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect(),
        T_SBYTE | T_SSHORT | T_SLONG => to_u32s(typ, b)?.into_iter().map(|v| v as i32 as f64).collect(),
        _ => to_u32s(typ, b)?.into_iter().map(|v| v as f64).collect(),
    })
}

// ── Tag groups ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct StripLayout {
    width: u32,
    height: u32,
    compression: u32,
    predictor: u32,
    rows_per_strip: u32,
    offsets: Vec<u64>,
    counts: Vec<u64>,
}

fn strip_layout(t: &mut TiffFile, main: &Ifd) -> Result<StripLayout> {
    let width = t.tag_u32(main, TAG_IMAGE_WIDTH)?.ok_or_else(|| bad("no ImageWidth"))?;
    let height = t.tag_u32(main, TAG_IMAGE_LENGTH)?.ok_or_else(|| bad("no ImageLength"))?;
    if width == 0 || height == 0 {
        return Err(bad(format!("empty image {width}×{height}")));
    }
    let bits = t.tag_u32(main, TAG_BITS_PER_SAMPLE)?.unwrap_or(1);
    if bits != 16 {
        return Err(bad(format!("BitsPerSample {bits} (only 16-bit raw is supported)")));
    }
    let spp = t.tag_u32(main, TAG_SAMPLES_PER_PIXEL)?.unwrap_or(1);
    if spp != 1 {
        return Err(bad(format!("SamplesPerPixel {spp} (expected 1 — a CFA mosaic)")));
    }
    let photometric = t.tag_u32(main, TAG_PHOTOMETRIC)?.unwrap_or(0);
    if photometric != PHOTOMETRIC_CFA {
        return Err(bad(format!("PhotometricInterpretation {photometric} (expected 32803 CFA)")));
    }
    let planar = t.tag_u32(main, TAG_PLANAR_CONFIG)?.unwrap_or(1);
    if planar != 1 {
        return Err(bad(format!("PlanarConfiguration {planar} (expected 1)")));
    }
    let compression = t.tag_u32(main, TAG_COMPRESSION)?.unwrap_or(COMPRESSION_NONE);
    if compression != COMPRESSION_NONE && compression != COMPRESSION_DEFLATE {
        return Err(bad(format!(
            "Compression {compression} is not supported (1 = none and 8 = Deflate only; lossless-JPEG DNGs are not X6 stills)"
        )));
    }
    let predictor = t.tag_u32(main, TAG_PREDICTOR)?.unwrap_or(1);
    if predictor != 1 && predictor != 2 {
        return Err(bad(format!("Predictor {predictor} is not supported (1 or 2 = horizontal differencing)")));
    }
    if main.get(TAG_TILE_OFFSETS).is_some() || main.get(TAG_TILE_WIDTH).is_some() {
        return Err(bad("tiled layout is not supported (X6 stills use strips)"));
    }
    if main.get(TAG_LINEARIZATION_TABLE).is_some() {
        tracing::warn!("dng: LinearizationTable present but not applied (the X6 writes none)");
    }
    let offsets: Vec<u64> = t
        .tag_u32s(main, TAG_STRIP_OFFSETS)?
        .ok_or_else(|| bad("no StripOffsets"))?
        .into_iter()
        .map(u64::from)
        .collect();
    let counts: Vec<u64> = t
        .tag_u32s(main, TAG_STRIP_BYTE_COUNTS)?
        .ok_or_else(|| bad("no StripByteCounts"))?
        .into_iter()
        .map(u64::from)
        .collect();
    let rows_per_strip = t.tag_u32(main, TAG_ROWS_PER_STRIP)?.unwrap_or(height).min(height);
    if rows_per_strip == 0 {
        return Err(bad("RowsPerStrip 0"));
    }
    let nstrips = ((height - 1) / rows_per_strip + 1) as usize;
    if offsets.len() != nstrips || counts.len() != nstrips {
        return Err(bad(format!(
            "{} StripOffsets / {} StripByteCounts for {nstrips} strips ({height} rows / {rows_per_strip} per strip)",
            offsets.len(),
            counts.len()
        )));
    }
    for (i, (&off, &cnt)) in offsets.iter().zip(&counts).enumerate() {
        if off.checked_add(cnt).map_or(true, |end| end > t.len) {
            return Err(bad(format!("strip {i}: {cnt} bytes at {off} run past end of file ({} bytes)", t.len)));
        }
    }
    Ok(StripLayout { width, height, compression, predictor, rows_per_strip, offsets, counts })
}

fn cfa_pattern(t: &mut TiffFile, main: &Ifd, ifd0: &Ifd) -> Result<[u8; 4]> {
    let dim = t.tag_u32s(main, TAG_CFA_REPEAT_PATTERN_DIM)?.unwrap_or_else(|| vec![2, 2]);
    if dim != [2, 2] {
        return Err(bad(format!("CFARepeatPatternDim {dim:?} (only 2×2 is supported)")));
    }
    let pattern = t.tag_u32s(main, TAG_CFA_PATTERN)?.ok_or_else(|| bad("no CFAPattern"))?;
    if pattern.len() < 4 {
        return Err(bad(format!("CFAPattern has {} entries, need 4", pattern.len())));
    }
    let plane_color = t
        .tag_u32s(main, TAG_CFA_PLANE_COLOR)?
        .or(t.tag_u32s(ifd0, TAG_CFA_PLANE_COLOR)?)
        .unwrap_or_else(|| vec![0, 1, 2]);
    let layout = t.tag_u32(main, TAG_CFA_LAYOUT)?.unwrap_or(1);
    if layout != 1 {
        return Err(bad(format!("CFALayout {layout} (only 1 = rectangular is supported)")));
    }
    let mut cfa = [0u8; 4];
    for (k, &p) in pattern.iter().take(4).enumerate() {
        let color = plane_color.get(p as usize).copied().unwrap_or(p);
        if color > 2 {
            return Err(bad(format!("CFA colour {color} is not R/G/B")));
        }
        cfa[k] = color as u8;
    }
    Ok(cfa)
}

fn color_tags(t: &mut TiffFile, ifd0: &Ifd, main: &Ifd) -> Result<DngColor> {
    // Colour tags live in IFD0 by spec; look in the image IFD as a fallback.
    macro_rules! either {
        ($m:ident, $tag:expr) => {
            match t.$m(ifd0, $tag)? {
                Some(v) => Some(v),
                None => t.$m(main, $tag)?,
            }
        };
    }
    let mut color_matrix1 = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0f32];
    match either!(tag_f64s, TAG_COLOR_MATRIX1) {
        Some(v) if v.len() >= 9 => {
            for (dst, src) in color_matrix1.iter_mut().zip(v) {
                *dst = src as f32;
            }
        }
        Some(v) => return Err(bad(format!("ColorMatrix1 has {} entries, need 9", v.len()))),
        None => tracing::warn!("dng: no ColorMatrix1 — using identity"),
    }
    let mut as_shot_neutral = [1.0f32; 3];
    if let Some(v) = either!(tag_f64s, TAG_AS_SHOT_NEUTRAL) {
        for (dst, src) in as_shot_neutral.iter_mut().zip(v) {
            *dst = src as f32;
        }
    }
    let bits = either!(tag_u32, TAG_BITS_PER_SAMPLE).unwrap_or(16).min(16);
    let white_level = either!(tag_u32, TAG_WHITE_LEVEL).unwrap_or((1u32 << bits) - 1);

    let mut black_level = [0f32; 4];
    if let Some(v) = either!(tag_f64s, TAG_BLACK_LEVEL) {
        let dim = either!(tag_u32s, TAG_BLACK_LEVEL_REPEAT_DIM).unwrap_or_else(|| vec![1, 1]);
        let (rows, cols) = (dim.first().copied().unwrap_or(1).max(1) as usize, dim.get(1).copied().unwrap_or(1).max(1) as usize);
        for r in 0..2 {
            for c in 0..2 {
                let idx = (r % rows) * cols + (c % cols);
                black_level[r * 2 + c] = v.get(idx).or(v.first()).copied().unwrap_or(0.0) as f32;
            }
        }
    }
    let profile_hue_sat_map_dims = either!(tag_u32s, TAG_PROFILE_HUE_SAT_MAP_DIMS)
        .filter(|v| v.len() >= 3)
        .map(|v| [v[0], v[1], v[2]]);
    let profile_hue_sat_map_data1 =
        either!(tag_f64s, TAG_PROFILE_HUE_SAT_MAP_DATA1).map(|v| v.into_iter().map(|x| x as f32).collect());
    Ok(DngColor {
        color_matrix1,
        calibration_illuminant1: either!(tag_u32, TAG_CALIBRATION_ILLUMINANT1).unwrap_or(0) as u16,
        as_shot_neutral,
        baseline_exposure: either!(tag_f64, TAG_BASELINE_EXPOSURE).unwrap_or(0.0) as f32,
        white_level,
        black_level,
        profile_hue_sat_map_dims,
        profile_hue_sat_map_data1,
        profile_hue_sat_map_encoding: either!(tag_u32, TAG_PROFILE_HUE_SAT_MAP_ENCODING),
        baseline_noise: either!(tag_f64, TAG_BASELINE_NOISE).map(|v| v as f32),
        baseline_sharpness: either!(tag_f64, TAG_BASELINE_SHARPNESS).map(|v| v as f32),
    })
}

fn exif_tags(t: &mut TiffFile, exif: &Ifd, out: &mut DngExif) -> Result<()> {
    out.exposure_s = t.tag_f64(exif, EXIF_EXPOSURE_TIME)?;
    out.f_number = t.tag_f64(exif, EXIF_F_NUMBER)?;
    out.iso = t.tag_u32(exif, EXIF_ISO)?;
    out.focal_mm = t.tag_f64(exif, EXIF_FOCAL_LENGTH)?;
    out.focal_35mm = t.tag_u32(exif, EXIF_FOCAL_35MM)?;
    out.datetime_original = t.tag_ascii(exif, EXIF_DATETIME_ORIGINAL)?;
    if let Some(b) = t.tag_bytes(exif, EXIF_USER_COMMENT)? {
        // 8-byte character-code prefix ("ASCII\0\0\0", "UNICODE\0", … — the
        // X6 writes "INSTA360"), then the text.
        let text = if b.len() > 8 { &b[8..] } else { &[][..] };
        let s = ascii_string(text);
        if !s.is_empty() {
            out.user_comment = Some(s);
        }
    }
    Ok(())
}

// ── Strip decode ───────────────────────────────────────────────────

/// Stream the strips from `file` in order and inflate them on scoped worker
/// threads, each writing straight into its slice of `out` (`width × height`
/// samples). Strips go round-robin to the workers over bounded channels so
/// only a few compressed strips are in memory at once.
fn decode_strips(file: &mut File, lay: &StripLayout, out: &mut [u16]) -> Result<()> {
    let width = lay.width as usize;
    let nstrips = lay.offsets.len();
    let strip_samples = lay.rows_per_strip as usize * width;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 32)
        .min(nstrips.max(1));

    struct Job<'a> {
        index: usize,
        data: Vec<u8>,
        out: &'a mut [u16],
    }
    let first_err: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);

    std::thread::scope(|s| -> Result<()> {
        let mut senders = Vec::with_capacity(threads);
        for _ in 0..threads {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Job<'_>>(1);
            senders.push(tx);
            let first_err = &first_err;
            s.spawn(move || {
                while let Ok(job) = rx.recv() {
                    let rows = job.out.len() / width;
                    if let Err(e) = decode_strip(lay.compression, lay.predictor, &job.data, width, rows, job.out) {
                        let mut slot = first_err.lock().unwrap_or_else(|p| p.into_inner());
                        if slot.is_none() {
                            *slot = Some(bad(format!("strip {}: {e}", job.index)));
                        }
                        return; // dropping `rx` makes the producer's send fail → it stops
                    }
                }
            });
        }
        let mut chunks = out.chunks_mut(strip_samples);
        for i in 0..nstrips {
            let Some(chunk) = chunks.next() else { break };
            let n = lay.counts[i] as usize;
            let mut data = vec![0u8; n];
            file.seek(SeekFrom::Start(lay.offsets[i]))?;
            file.read_exact(&mut data)
                .map_err(|e| bad(format!("strip {i}: reading {n} bytes at {}: {e}", lay.offsets[i])))?;
            if senders[i % threads].send(Job { index: i, data, out: chunk }).is_err() {
                break;
            }
        }
        drop(senders);
        Ok(())
    })?;

    match first_err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Decode one strip of `rows × width` 16-bit little-endian samples into
/// `out`, undoing TIFF Predictor 2 (horizontal differencing) when asked.
fn decode_strip(compression: u32, predictor: u32, data: &[u8], width: usize, rows: usize, out: &mut [u16]) -> Result<()> {
    let need = rows * width;
    let out = out.get_mut(..need).ok_or_else(|| bad("output slice shorter than the strip"))?;
    match compression {
        COMPRESSION_NONE => {
            if data.len() < need * 2 {
                return Err(bad(format!("uncompressed strip holds {} bytes, need {}", data.len(), need * 2)));
            }
            for (o, b) in out.iter_mut().zip(data.chunks_exact(2)) {
                *o = u16::from_le_bytes([b[0], b[1]]);
            }
        }
        COMPRESSION_DEFLATE => {
            let mut z = flate2::read::ZlibDecoder::new(data);
            read_le_u16(&mut z, out).map_err(|e| bad(format!("deflate: {e}")))?;
        }
        other => return Err(bad(format!("unsupported Compression {other}"))),
    }
    if predictor == 2 {
        for row in out.chunks_exact_mut(width) {
            undo_predictor2(row);
        }
    }
    Ok(())
}

/// Fill `out` with little-endian u16 samples from a byte stream, converting
/// through a small stack buffer (no strip-sized temporary).
fn read_le_u16<R: Read>(r: &mut R, out: &mut [u16]) -> io::Result<()> {
    let mut buf = [0u8; 1 << 15];
    let mut done = 0usize;
    while done < out.len() {
        let n = (out.len() - done).min(buf.len() / 2);
        r.read_exact(&mut buf[..n * 2])?;
        for (o, b) in out[done..done + n].iter_mut().zip(buf[..n * 2].chunks_exact(2)) {
            *o = u16::from_le_bytes([b[0], b[1]]);
        }
        done += n;
    }
    Ok(())
}

/// TIFF Predictor 2 on 16-bit samples: each value is stored as the
/// difference from its left neighbour (mod 2¹⁶).
fn undo_predictor2(row: &mut [u16]) {
    for i in 1..row.len() {
        row[i] = row[i].wrapping_add(row[i - 1]);
    }
}

// ── Still trailer ──────────────────────────────────────────────────

const FOOTER_LEN: u64 = 78;
const REC_PROTOBUF: u16 = 0x0101;
const REC_IMU: u16 = 0x0003;
/// Sanity cap on the trailer we are willing to slurp (the `.insp` cover
/// record is ~5 MB; the DNG trailer is 16 KB).
const MAX_TRAILER_LEN: u64 = 256 << 20;
const MAX_RECORDS: usize = 64;

#[derive(Debug, Default)]
struct StillTrailer {
    /// Records in walk order (footer first): `(id, payload)`.
    records: Vec<(u16, Vec<u8>)>,
    /// The `ExtraMetadata` protobuf (record 0x0101) when present.
    protobuf: Option<Vec<u8>>,
    #[allow(dead_code)]
    version: u32,
}

impl StillTrailer {
    fn record(&self, id: u16) -> Option<&[u8]> {
        self.records.iter().find(|(i, _)| *i == id).map(|(_, b)| b.as_slice())
    }
}

/// Read the still-layout trailer: footer at EOF−78 (`[u16 id][u32 size]` =
/// the chain header of the protobuf, `trailer_len` at [38..42], version at
/// [42..46], magic at [46..78]); records walk backward from the footer as
/// `[data][u16 id BE][u32 size LE]` down to `EOF − trailer_len`.
fn read_still_trailer(file: &mut File, file_len: u64) -> Result<StillTrailer> {
    let terr = |m: String| Error::Insv(m);
    if file_len < FOOTER_LEN {
        return Err(terr("file too small for an Insta360 footer".into()));
    }
    file.seek(SeekFrom::Start(file_len - FOOTER_LEN))?;
    let mut foot = [0u8; FOOTER_LEN as usize];
    file.read_exact(&mut foot)?;
    if &foot[46..78] != INSV_TRAILER_MAGIC {
        return Err(terr("Insta360 trailer magic not found at EOF".into()));
    }
    let trailer_len = u32::from_le_bytes(foot[38..42].try_into().unwrap()) as u64;
    let version = u32::from_le_bytes(foot[42..46].try_into().unwrap());
    if trailer_len < FOOTER_LEN || trailer_len > file_len || trailer_len > MAX_TRAILER_LEN {
        return Err(terr(format!("bad still trailer_len {trailer_len} for a {file_len}-byte file")));
    }
    let base = file_len - trailer_len;
    let mut buf = vec![0u8; trailer_len as usize];
    file.seek(SeekFrom::Start(base))?;
    file.read_exact(&mut buf)?;

    let mut out = StillTrailer { version, ..Default::default() };
    // End of the last record's data = start of the footer + 6 (the footer's
    // first 6 bytes are that record's chain header).
    let mut pos = buf.len() - FOOTER_LEN as usize + 6;
    while pos >= 6 && out.records.len() < MAX_RECORDS {
        let hdr = &buf[pos - 6..pos];
        let id = u16::from_be_bytes([hdr[0], hdr[1]]);
        let size = u32::from_le_bytes(hdr[2..6].try_into().unwrap()) as usize;
        let Some(start) = (pos - 6).checked_sub(size) else { break };
        let data = buf[start..pos - 6].to_vec();
        if id == REC_PROTOBUF && out.protobuf.is_none() {
            out.protobuf = Some(data.clone());
        }
        out.records.push((id, data));
        pos = start;
        if size == 0 {
            break;
        }
    }
    if out.records.is_empty() {
        return Err(terr("empty still trailer chain".into()));
    }
    Ok(out)
}

/// Fields of the still's `ExtraMetadata` protobuf that we consume.
#[derive(Debug, Default)]
struct StillMeta {
    serial: Option<String>,
    firmware: Option<String>,
    dimension: Option<(u32, u32)>,
    capture_time_us: Option<u64>,
    rolling_shutter_ms: Option<f64>,
    crop: Option<InsvWindowCrop>,
    acc_range_g: Option<u32>,
    gyro_range_dps: Option<u32>,
    /// Fields 111 / 112 (and 5 / 17 on videos): copies of the lens string.
    lens_strings: Vec<String>,
    orientation: Option<[f32; 9]>,
}

/// Protobuf fields: 1 serial, 3 firmware, 19 `{1 w, 2 h}`, 24 capture time
/// (µs, varint), 25 rolling-shutter ms (double), 27 window crop
/// `{src_w, src_h, dst_w, dst_h, off_x, off_y}`, 65 `{acc_range, gyro_range}`,
/// 111 / 112 lens strings, 210 nine floats (tagged or packed).
fn parse_still_protobuf(bytes: &[u8]) -> StillMeta {
    let mut m = StillMeta::default();
    let mut f210: Vec<f32> = Vec::new();
    for (field, wire) in protobuf_fields(bytes) {
        match (field, wire) {
            (1 | 3 | 111 | 112, Wire::Len(p)) => {
                if let Ok(s) = std::str::from_utf8(p) {
                    let s = s.trim_end_matches('\0');
                    if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                        match field {
                            1 => m.serial = Some(s.to_string()),
                            3 => m.firmware = Some(s.to_string()),
                            _ => m.lens_strings.push(s.to_string()),
                        }
                    }
                }
            }
            (19, Wire::Len(p)) => {
                let v = varint_fields(p);
                if let (Some(&w), Some(&h)) = (v.get(&1), v.get(&2)) {
                    m.dimension = Some((w as u32, h as u32));
                }
            }
            (24, Wire::Varint(v)) => m.capture_time_us = Some(v),
            (25, Wire::F64(v)) => m.rolling_shutter_ms = Some(v).filter(|v| v.is_finite() && *v > 0.0),
            (27, Wire::Len(p)) => {
                let v = varint_fields(p);
                let g = |k: u64| v.get(&k).copied().unwrap_or(0);
                if g(1) > 0 && g(3) > 0 {
                    m.crop = Some(InsvWindowCrop {
                        src_w: g(1) as u32,
                        src_h: g(2) as u32,
                        dst_w: g(3) as u32,
                        dst_h: g(4) as u32,
                        offset_x: g(5) as i64 as i32,
                        offset_y: g(6) as i64 as i32,
                    });
                }
            }
            (65, Wire::Len(p)) => {
                let v = varint_fields(p);
                m.acc_range_g = v.get(&1).map(|&x| x as u32).filter(|&x| x > 0);
                m.gyro_range_dps = v.get(&2).map(|&x| x as u32).filter(|&x| x > 0);
            }
            (210, Wire::F32(v)) => f210.push(v),
            (210, Wire::Len(p)) if p.len() % 4 == 0 => {
                f210.extend(p.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())));
            }
            _ => {}
        }
    }
    if f210.len() == 9 {
        m.orientation = Some(f210[..9].try_into().unwrap());
    }
    m
}

enum Wire<'a> {
    Varint(u64),
    F64(f64),
    F32(f32),
    Len(&'a [u8]),
}

/// Walk a protobuf message's top-level fields; stops at the first malformed
/// tag or unknown wire type.
fn protobuf_fields(bytes: &[u8]) -> Vec<(u32, Wire<'_>)> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some((tag, next)) = read_varint(bytes, cursor) else { break };
        cursor = next;
        let field = (tag >> 3) as u32;
        match tag & 7 {
            0 => {
                let Some((v, next)) = read_varint(bytes, cursor) else { break };
                cursor = next;
                out.push((field, Wire::Varint(v)));
            }
            1 => {
                let Some(b) = bytes.get(cursor..cursor + 8) else { break };
                cursor += 8;
                out.push((field, Wire::F64(f64::from_le_bytes(b.try_into().unwrap()))));
            }
            2 => {
                let Some((len, next)) = read_varint(bytes, cursor) else { break };
                cursor = next;
                let end = cursor.saturating_add(len as usize);
                if end > bytes.len() {
                    break;
                }
                out.push((field, Wire::Len(&bytes[cursor..end])));
                cursor = end;
            }
            5 => {
                let Some(b) = bytes.get(cursor..cursor + 4) else { break };
                cursor += 4;
                out.push((field, Wire::F32(f32::from_le_bytes(b.try_into().unwrap()))));
            }
            _ => break,
        }
    }
    out
}

fn varint_fields(bytes: &[u8]) -> std::collections::HashMap<u64, u64> {
    protobuf_fields(bytes)
        .into_iter()
        .filter_map(|(f, w)| match w {
            Wire::Varint(v) => Some((f as u64, v)),
            _ => None,
        })
        .collect()
}

fn read_varint(buf: &[u8], mut cursor: usize) -> Option<(u64, usize)> {
    let mut shift = 0u32;
    let mut value = 0u64;
    loop {
        let b = *buf.get(cursor)?;
        cursor += 1;
        value |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((value, cursor));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The real X6 UserComment text (after the "INSTA360" prefix) of
    /// IMG_20261007_133105_00_050.dng, including its odd `" 0.000000_…"` tail.
    const X6_USER_COMMENT: &str = "2_2.455430_7214.730_7214.730_3853.990_3884.350_0.206_0.393_89.440_0.000000_0.000000_0.000000_1.30036104_-1.16655576_3.39640808_7.49219084_0.00000000_-0.00007278_0.00000862_0.00514644_0.01285061_-0.00059777_-0.00101431_0.04616402_-0.02407980_15488_7744_193_2.455430_7223.550_7223.550_11612.800_3879.390_-0.213_-0.057_89.549_-0.000132_-0.000100_-0.032797_1.30198634_-1.23367751_3.01822877_9.18691730_0.00000000_0.00004737_-0.00016086_0.00663970_0.01089690_0.00038586_-0.00151548_-0.03414438_-0.02008163_15488_7744_193_394240 0.000000_0.000000_0.000000_0.000000_0.000000_0.000000";

    const REAL_SAMPLE: &str = "/Volumes/Database/261007 3 test/x6/IMG_20261007_133105_00_050.dng";
    const REAL_SAMPLE_RAW: &str = "/private/tmp/claude-501/-Users-siyangqi-Downloads-vr180-processor-memory/dbf707b1-3979-452f-966e-74fe9e218aeb/scratchpad/dng/050_bayer.raw";

    // ── tiny TIFF / protobuf / trailer writers ──

    struct Tag {
        tag: u16,
        typ: u16,
        count: u32,
        data: Vec<u8>,
    }

    fn t_short(tag: u16, v: &[u16]) -> Tag {
        Tag { tag, typ: T_SHORT, count: v.len() as u32, data: v.iter().flat_map(|x| x.to_le_bytes()).collect() }
    }
    fn t_long(tag: u16, v: &[u32]) -> Tag {
        Tag { tag, typ: T_LONG, count: v.len() as u32, data: v.iter().flat_map(|x| x.to_le_bytes()).collect() }
    }
    fn t_byte(tag: u16, v: &[u8]) -> Tag {
        Tag { tag, typ: T_BYTE, count: v.len() as u32, data: v.to_vec() }
    }
    fn t_undef(tag: u16, v: &[u8]) -> Tag {
        Tag { tag, typ: T_UNDEFINED, count: v.len() as u32, data: v.to_vec() }
    }
    fn t_ascii(tag: u16, s: &str) -> Tag {
        let mut d = s.as_bytes().to_vec();
        d.push(0);
        Tag { tag, typ: T_ASCII, count: d.len() as u32, data: d }
    }
    fn t_rational(tag: u16, v: &[(u32, u32)]) -> Tag {
        Tag { tag, typ: T_RATIONAL, count: v.len() as u32, data: v.iter().flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat()).collect() }
    }
    fn t_srational(tag: u16, v: &[(i32, i32)]) -> Tag {
        Tag { tag, typ: T_SRATIONAL, count: v.len() as u32, data: v.iter().flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat()).collect() }
    }
    fn t_float(tag: u16, v: &[f32]) -> Tag {
        Tag { tag, typ: T_FLOAT, count: v.len() as u32, data: v.iter().flat_map(|x| x.to_le_bytes()).collect() }
    }
    fn t_ifd(tag: u16, off: u32) -> Tag {
        Tag { tag, typ: T_IFD, count: 1, data: off.to_le_bytes().to_vec() }
    }

    /// Serialize an IFD at absolute offset `base`: sorted entries, values ≤ 4
    /// bytes inline, larger ones 4-byte aligned after the table.
    fn serialize_ifd(mut tags: Vec<Tag>, base: u32) -> Vec<u8> {
        tags.sort_by_key(|t| t.tag);
        let n = tags.len() as u32;
        let head = base + 2 + 12 * n + 4;
        let mut ent = Vec::new();
        let mut data: Vec<u8> = Vec::new();
        for t in &tags {
            ent.extend_from_slice(&t.tag.to_le_bytes());
            ent.extend_from_slice(&t.typ.to_le_bytes());
            ent.extend_from_slice(&t.count.to_le_bytes());
            if t.data.len() <= 4 {
                let mut v = t.data.clone();
                v.resize(4, 0);
                ent.extend_from_slice(&v);
            } else {
                while (head + data.len() as u32) % 4 != 0 {
                    data.push(0);
                }
                ent.extend_from_slice(&(head + data.len() as u32).to_le_bytes());
                data.extend_from_slice(&t.data);
            }
        }
        let mut out = (n as u16).to_le_bytes().to_vec();
        out.extend_from_slice(&ent);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    fn varint_bytes(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }
    fn pb_varint(field: u32, v: u64) -> Vec<u8> {
        let mut out = varint_bytes((field as u64) << 3);
        out.extend(varint_bytes(v));
        out
    }
    fn pb_len(field: u32, b: &[u8]) -> Vec<u8> {
        let mut out = varint_bytes(((field as u64) << 3) | 2);
        out.extend(varint_bytes(b.len() as u64));
        out.extend_from_slice(b);
        out
    }
    fn pb_f64(field: u32, v: f64) -> Vec<u8> {
        let mut out = varint_bytes(((field as u64) << 3) | 1);
        out.extend_from_slice(&v.to_le_bytes());
        out
    }
    fn pb_f32(field: u32, v: f32) -> Vec<u8> {
        let mut out = varint_bytes(((field as u64) << 3) | 5);
        out.extend_from_slice(&v.to_le_bytes());
        out
    }

    const ORIENT: [f32; 9] = [-0.62727, -0.77816, 0.03154, 0.77873, -0.62726, 0.01151, 0.01083, 0.03178, 0.99944];

    fn fixture_protobuf() -> Vec<u8> {
        let mut pb = Vec::new();
        pb.extend(pb_len(1, b"BXEA3ABFS89CAV"));
        pb.extend(pb_len(2, b"Insta360 X6"));
        pb.extend(pb_len(3, b"v1.2.35"));
        pb.extend(pb_len(19, &[pb_varint(1, 64), pb_varint(2, 32)].concat()));
        pb.extend(pb_varint(24, 10417891503));
        pb.extend(pb_f64(25, 37.506667));
        pb.extend(pb_len(
            27,
            &[pb_varint(1, 7744), pb_varint(2, 7744), pb_varint(3, 7760), pb_varint(4, 7760), pb_varint(5, 0), pb_varint(6, 0)].concat(),
        ));
        pb.extend(pb_len(65, &[pb_varint(1, 32), pb_varint(2, 2000)].concat()));
        pb.extend(pb_len(111, X6_USER_COMMENT.split(' ').next().unwrap().as_bytes()));
        for v in ORIENT {
            pb.extend(pb_f32(210, v));
        }
        pb
    }

    /// Three IMU samples: raw offset-binary codes with known physical values.
    fn fixture_imu() -> Vec<u8> {
        let mut rec = Vec::new();
        let samples: [(u64, [i32; 6]); 3] = [
            (1_000, [1024, 0, -512, 16384, 0, -1638]),
            (2_000, [0, 1024, 0, 0, 8192, 0]),
            (3_003, [-1024, 0, 2048, -16384, 0, 164]),
        ];
        for (t, v) in samples {
            rec.extend_from_slice(&t.to_le_bytes());
            for x in v {
                rec.extend_from_slice(&((x + 32768) as u16).to_le_bytes());
            }
        }
        rec
    }

    /// Still trailer: `[0x0009 48 B][hdr][0x0003 IMU][hdr][protobuf][footer]`.
    fn fixture_trailer() -> Vec<u8> {
        let pb = fixture_protobuf();
        let imu = fixture_imu();
        let ae = vec![0xAAu8; 48];
        let hdr = |id: u16, size: usize| {
            let mut h = id.to_be_bytes().to_vec();
            h.extend_from_slice(&(size as u32).to_le_bytes());
            h
        };
        let mut t = Vec::new();
        t.extend_from_slice(&ae);
        t.extend(hdr(0x0009, ae.len()));
        t.extend_from_slice(&imu);
        t.extend(hdr(REC_IMU, imu.len()));
        t.extend_from_slice(&pb);
        let trailer_len = t.len() + FOOTER_LEN as usize;
        let mut foot = hdr(REC_PROTOBUF, pb.len());
        foot.resize(38, 0);
        foot.extend_from_slice(&(trailer_len as u32).to_le_bytes());
        foot.extend_from_slice(&3u32.to_le_bytes());
        foot.extend_from_slice(INSV_TRAILER_MAGIC);
        assert_eq!(foot.len(), FOOTER_LEN as usize);
        t.extend_from_slice(&foot);
        t
    }

    /// Hand-built OpcodeList2: one unknown opcode (id 4) then one GainMap
    /// 2 × 3 × 1 over the full 64 × 32 frame applied to CFA site (1, 0).
    fn fixture_opcodes() -> Vec<u8> {
        let mut b = 2u32.to_be_bytes().to_vec();
        // opcode 0: id 4, version 1.3.0.0, flags 1, 5 junk bytes
        for v in [4u32, 0x0103_0000, 1, 5] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&[9, 9, 9, 9, 9]);
        let mut body = Vec::new();
        for v in [1u32, 0, 32, 64, 0, 1, 2, 2, 2, 3] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        for v in [1.0f64, 0.5, 0.0, 0.0] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        body.extend_from_slice(&1u32.to_be_bytes());
        for g in [1.0f32, 1.5, 2.0, 1.2, 1.7, 2.2] {
            body.extend_from_slice(&g.to_be_bytes());
        }
        for v in [OPCODE_GAIN_MAP, 0x0103_0000, 0, body.len() as u32] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&body);
        b
    }

    const FW: u32 = 64;
    const FH: u32 = 32;
    const RPS: u32 = 10; // 4 strips: 10, 10, 10, 2 rows

    fn fixture_pixels() -> Vec<u16> {
        (0..FH * FW).map(|i| (((i % FW) * 7 + (i / FW) * 131 + 5) & 0x3FFF) as u16).collect()
    }

    fn encode_strips(px: &[u16], compression: u32, predictor: u32) -> Vec<Vec<u8>> {
        px.chunks((RPS * FW) as usize)
            .map(|strip| {
                let mut raw = Vec::with_capacity(strip.len() * 2);
                for row in strip.chunks(FW as usize) {
                    let mut prev = 0u16;
                    for (x, &v) in row.iter().enumerate() {
                        let s = if predictor == 2 && x > 0 { v.wrapping_sub(prev) } else { v };
                        prev = v;
                        raw.extend_from_slice(&s.to_le_bytes());
                    }
                }
                if compression == COMPRESSION_DEFLATE {
                    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                    z.write_all(&raw).unwrap();
                    z.finish().unwrap()
                } else {
                    raw
                }
            })
            .collect()
    }

    /// Build a complete synthetic X6-style DNG; returns the bytes and the
    /// expected mosaic.
    fn fixture_dng(compression: u32, predictor: u32) -> (Vec<u8>, Vec<u16>) {
        let px = fixture_pixels();
        let strips = encode_strips(&px, compression, predictor);
        let mut out = b"II*\0".to_vec();
        out.extend_from_slice(&[0, 0, 0, 0]); // IFD0 pointer, patched below
        let mut offs = Vec::new();
        for s in &strips {
            while out.len() % 4 != 0 {
                out.push(0);
            }
            offs.push(out.len() as u32);
            out.extend_from_slice(s);
        }
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let mut uc = b"INSTA360".to_vec();
        uc.extend_from_slice(X6_USER_COMMENT.as_bytes());
        let exif_off = out.len() as u32;
        let exif = serialize_ifd(
            vec![
                t_rational(EXIF_EXPOSURE_TIME, &[(1, 5072)]),
                t_rational(EXIF_F_NUMBER, &[(2, 1)]),
                t_short(EXIF_ISO, &[120]),
                t_ascii(EXIF_DATETIME_ORIGINAL, "2026:10:07 13:31:05"),
                t_rational(EXIF_FOCAL_LENGTH, &[(254, 100)]),
                t_undef(37500, b"makernote-ignored"),
                t_undef(EXIF_USER_COMMENT, &uc),
                t_short(EXIF_FOCAL_35MM, &[8]),
            ],
            exif_off,
        );
        out.extend_from_slice(&exif);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let ifd0_off = out.len() as u32;
        let mut tags = vec![
            t_long(TAG_NEW_SUBFILE_TYPE, &[0]),
            t_short(TAG_IMAGE_WIDTH, &[FW as u16]),
            t_short(TAG_IMAGE_LENGTH, &[FH as u16]),
            t_short(TAG_BITS_PER_SAMPLE, &[16]),
            t_short(TAG_COMPRESSION, &[compression as u16]),
            t_short(TAG_PHOTOMETRIC, &[PHOTOMETRIC_CFA as u16]),
            t_ascii(TAG_MAKE, "Arashi Vision"),
            t_ascii(TAG_MODEL, "Insta360 X6"),
            t_long(TAG_STRIP_OFFSETS, &offs),
            t_short(TAG_SAMPLES_PER_PIXEL, &[1]),
            t_short(TAG_ROWS_PER_STRIP, &[RPS as u16]),
            t_long(TAG_STRIP_BYTE_COUNTS, &strips.iter().map(|s| s.len() as u32).collect::<Vec<_>>()),
            t_short(TAG_PLANAR_CONFIG, &[1]),
            t_ascii(TAG_SOFTWARE, "v1.2.35"),
            t_short(TAG_CFA_REPEAT_PATTERN_DIM, &[2, 2]),
            t_byte(TAG_CFA_PATTERN, &[1, 2, 0, 1]),
            t_ifd(TAG_EXIF_IFD, exif_off),
            t_byte(TAG_DNG_VERSION, &[1, 3, 0, 0]),
            t_ascii(TAG_UNIQUE_CAMERA_MODEL, "Insta360 X6"),
            t_byte(TAG_CFA_PLANE_COLOR, &[0, 1, 2]),
            t_short(TAG_CFA_LAYOUT, &[1]),
            t_short(TAG_BLACK_LEVEL_REPEAT_DIM, &[2, 2]),
            t_rational(TAG_BLACK_LEVEL, &[(0, 1), (3, 2), (6, 1), (0, 1)]),
            t_long(TAG_WHITE_LEVEL, &[16383]),
            t_srational(TAG_COLOR_MATRIX1, &[(23, 32), (-19, 128), (-3, 32), (-39, 64), (93, 64), (7, 64), (-5, 32), (19, 64), (61, 128)]),
            t_rational(TAG_AS_SHOT_NEUTRAL, &[(471, 1024), (1, 1), (275, 512)]),
            t_srational(TAG_BASELINE_EXPOSURE, &[(5453351, 2097152)]), // 2.600360394, as the camera writes it
            t_rational(TAG_BASELINE_NOISE, &[(1, 1)]),
            t_rational(TAG_BASELINE_SHARPNESS, &[(1, 1)]),
            t_short(TAG_CALIBRATION_ILLUMINANT1, &[21]),
            t_long(TAG_PROFILE_HUE_SAT_MAP_DIMS, &[1, 2, 1]),
            t_float(TAG_PROFILE_HUE_SAT_MAP_DATA1, &[1.0, 0.93, 1.0, 1.0, 0.93, 1.0]),
            t_undef(TAG_OPCODE_LIST2, &fixture_opcodes()),
            t_long(TAG_PROFILE_HUE_SAT_MAP_ENCODING, &[1]),
        ];
        if predictor == 2 {
            tags.push(t_short(TAG_PREDICTOR, &[2]));
        }
        out.extend_from_slice(&serialize_ifd(tags, ifd0_off));
        out[4..8].copy_from_slice(&ifd0_off.to_le_bytes());
        out.extend_from_slice(&fixture_trailer());
        (out, px)
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("vr180_dng_test_{}_{name}", std::process::id()));
        p
    }

    fn check_fixture(still: &X6Still, px: &[u16]) {
        assert_eq!((still.width, still.height), (FW, FH));
        assert_eq!(still.bayer.len(), px.len());
        assert!(still.bayer == px, "mosaic mismatch");
        assert_eq!(still.cfa, [1, 2, 0, 1]);

        let c = &still.color;
        assert_eq!(c.color_matrix1, [23.0 / 32.0, -19.0 / 128.0, -3.0 / 32.0, -39.0 / 64.0, 93.0 / 64.0, 7.0 / 64.0, -5.0 / 32.0, 19.0 / 64.0, 61.0 / 128.0]);
        assert_eq!(c.calibration_illuminant1, 21);
        assert_eq!(c.as_shot_neutral, [471.0 / 1024.0, 1.0, 275.0 / 512.0]);
        assert!((c.baseline_exposure - 2.600_360_4).abs() < 1e-6, "{}", c.baseline_exposure);
        assert_eq!(c.white_level, 16383);
        assert_eq!(c.black_level, [0.0, 1.5, 6.0, 0.0]);
        assert_eq!(c.profile_hue_sat_map_dims, Some([1, 2, 1]));
        assert_eq!(c.profile_hue_sat_map_data1.as_deref(), Some(&[1.0, 0.93, 1.0, 1.0, 0.93, 1.0][..]));
        assert_eq!(c.profile_hue_sat_map_encoding, Some(1));
        assert_eq!(c.baseline_noise, Some(1.0));
        assert_eq!(c.baseline_sharpness, Some(1.0));

        let e = &still.exif;
        assert_eq!(e.make, "Arashi Vision");
        assert_eq!(e.model, "Insta360 X6");
        assert_eq!(e.unique_camera_model, "Insta360 X6");
        assert_eq!(e.software, "v1.2.35");
        assert!((e.exposure_s.unwrap() - 1.0 / 5072.0).abs() < 1e-12);
        assert_eq!(e.f_number, Some(2.0));
        assert_eq!(e.iso, Some(120));
        assert_eq!(e.focal_mm, Some(2.54));
        assert_eq!(e.focal_35mm, Some(8));
        assert_eq!(e.datetime_original.as_deref(), Some("2026:10:07 13:31:05"));
        assert_eq!(e.user_comment.as_deref(), Some(X6_USER_COMMENT));

        assert_eq!(still.gain_maps.len(), 1);
        let g = &still.gain_maps[0];
        assert_eq!((g.top, g.left, g.bottom, g.right), (1, 0, 32, 64));
        assert_eq!((g.points_v, g.points_h, g.map_planes), (2, 3, 1));
        assert_eq!(g.gains, [1.0, 1.5, 2.0, 1.2, 1.7, 2.2]);

        assert_eq!(still.lenses.len(), 2);
        assert!((still.lenses[0].fx - 7214.73).abs() < 1e-2);
        assert!((still.lenses[1].cx - 11612.8).abs() < 1e-2);
        assert_eq!(still.calib_frame, (15488, 7744));
        assert_eq!(still.crop, Some(InsvWindowCrop { src_w: 7744, src_h: 7744, dst_w: 7760, dst_h: 7760, offset_x: 0, offset_y: 0 }));
        assert!((still.rolling_shutter_ms.unwrap() - 37.506667).abs() < 1e-12);
        assert_eq!(still.capture_time_us, Some(10417891503));
        assert_eq!(still.orientation, Some(ORIENT));
        assert_eq!(still.serial.as_deref(), Some("BXEA3ABFS89CAV"));
        assert_eq!(still.firmware.as_deref(), Some("v1.2.35"));
        assert_eq!(still.dimension, Some((64, 32)));
        assert_eq!(still.record_ids, vec![REC_PROTOBUF, REC_IMU, 0x0009]);

        assert_eq!(still.imu.len(), 3);
        let s0 = &still.imu[0];
        assert_eq!(s0.t_us, 1000);
        assert_eq!(s0.acc_g, [1.0, 0.0, -0.5]);
        assert!((s0.gyr_dps[0] - 1000.0).abs() < 1e-3 && s0.gyr_dps[1] == 0.0 && (s0.gyr_dps[2] + 99.975586).abs() < 1e-3, "{:?}", s0.gyr_dps);
        assert_eq!(still.imu[2].t_us, 3003);
        assert_eq!(still.imu[2].acc_g, [-1.0, 0.0, 2.0]);
        assert!((still.imu[2].gyr_dps[2] - 164.0 / 16.384).abs() < 1e-4);
    }

    #[test]
    fn synthetic_dng_uncompressed_round_trips() {
        let (bytes, px) = fixture_dng(COMPRESSION_NONE, 1);
        let p = temp_path("c1.dng");
        std::fs::write(&p, &bytes).unwrap();
        assert!(is_insta360_dng(&p));
        assert_eq!(probe(&p).unwrap(), (FW, FH));
        let still = read(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        check_fixture(&still, &px);
    }

    #[test]
    fn synthetic_dng_deflate_round_trips() {
        let (bytes, px) = fixture_dng(COMPRESSION_DEFLATE, 1);
        let p = temp_path("c8.dng");
        std::fs::write(&p, &bytes).unwrap();
        let still = read(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        check_fixture(&still, &px);
    }

    #[test]
    fn synthetic_dng_deflate_predictor2_round_trips() {
        let (bytes, px) = fixture_dng(COMPRESSION_DEFLATE, 2);
        let p = temp_path("c8p2.dng");
        std::fs::write(&p, &bytes).unwrap();
        let still = read(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        check_fixture(&still, &px);
    }

    #[test]
    fn non_dng_files_are_rejected() {
        let p = temp_path("not.dng");
        std::fs::write(&p, b"MM\0*not a dng at all").unwrap();
        assert!(!is_insta360_dng(&p));
        assert!(probe(&p).is_err());
        assert!(read(&p).is_err());
        let _ = std::fs::remove_file(&p);
        // A valid TIFF header without the Insta360 footer is not an X6 still.
        let (bytes, _) = fixture_dng(COMPRESSION_NONE, 1);
        let cut = &bytes[..bytes.len() - 40];
        std::fs::write(&p, cut).unwrap();
        assert!(!is_insta360_dng(&p));
        assert_eq!(probe(&p).unwrap(), (FW, FH));
        assert!(read(&p).is_err(), "a truncated trailer must be an error");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unsupported_compression_is_a_clear_error() {
        let (mut bytes, _) = fixture_dng(COMPRESSION_NONE, 1);
        // Patch Compression (tag 259) to 7 (lossless JPEG).
        let ifd0 = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let n = u16::from_le_bytes([bytes[ifd0], bytes[ifd0 + 1]]) as usize;
        for k in 0..n {
            let e = ifd0 + 2 + 12 * k;
            if u16::from_le_bytes([bytes[e], bytes[e + 1]]) == TAG_COMPRESSION {
                bytes[e + 8..e + 10].copy_from_slice(&7u16.to_le_bytes());
            }
        }
        let p = temp_path("c7.dng");
        std::fs::write(&p, &bytes).unwrap();
        let err = read(&p).unwrap_err().to_string();
        let _ = std::fs::remove_file(&p);
        assert!(err.contains("Compression 7"), "{err}");
    }

    #[test]
    fn gain_map_opcode_parse_and_bilinear() {
        let maps = parse_opcode_list2(&fixture_opcodes()).unwrap();
        assert_eq!(maps.len(), 1, "the unknown opcode id 4 is skipped");
        let g = &maps[0];
        assert_eq!((g.top, g.left, g.bottom, g.right), (1, 0, 32, 64));
        assert_eq!((g.plane, g.planes, g.row_pitch, g.col_pitch), (0, 1, 2, 2));
        assert_eq!((g.points_v, g.points_h, g.map_planes), (2, 3, 1));
        assert_eq!((g.spacing_v, g.spacing_h, g.origin_v, g.origin_h), (1.0, 0.5, 0.0, 0.0));
        assert_eq!(g.cfa_site(), (1, 0));
        // Grid nodes: (row 1, col 0) → v=0,u=0 → 1.0; col 32 → u=0.5 → 1.5; col 64 → 2.0.
        assert_eq!(g.gain_at(1, 0, 0), 1.0);
        assert_eq!(g.gain_at(1, 32, 0), 1.5);
        assert_eq!(g.gain_at(1, 64, 0), 2.0);
        // Bottom row v=1 → second grid row.
        assert!((g.gain_at(32, 0, 0) - 1.2).abs() < 1e-6);
        // Midpoints interpolate; the clamp holds past the grid edges.
        assert!((g.gain_at(1, 16, 0) - 1.25).abs() < 1e-6);
        let mid = g.gain_at(1 + 31 / 2 + 1, 16, 0); // v = 16/31
        let want = (1.0 - 16.0 / 31.0) * 1.25 + (16.0 / 31.0) * 1.45;
        assert!((mid as f64 - want).abs() < 1e-5, "{mid} vs {want}");
        assert_eq!(g.gain_at(0, 200, 0), 1.0 * 0.0 + 2.0, "clamped to the last column");
        assert_eq!(g.gain_at(0, 200, 7), 2.0, "plane index clamps too");

        // Truncated blob → error, empty → no maps.
        assert!(parse_opcode_list2(&fixture_opcodes()[..60]).is_err());
        assert!(parse_opcode_list2(&0u32.to_be_bytes()).unwrap().is_empty());
        assert!(parse_opcode_list2(&[]).unwrap().is_empty());
    }

    #[test]
    fn predictor2_strip_decode() {
        let width = 5;
        let rows = [[100u16, 105, 103, 65535, 0], [7, 7, 7, 7, 7]];
        let mut data = Vec::new();
        for r in rows {
            let mut prev = 0u16;
            for (x, &v) in r.iter().enumerate() {
                data.extend_from_slice(&(if x == 0 { v } else { v.wrapping_sub(prev) }).to_le_bytes());
                prev = v;
            }
        }
        let mut out = vec![0u16; 10];
        decode_strip(COMPRESSION_NONE, 2, &data, width, 2, &mut out).unwrap();
        assert_eq!(out, [100, 105, 103, 65535, 0, 7, 7, 7, 7, 7]);
        // Same bytes through zlib.
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        z.write_all(&data).unwrap();
        let zd = z.finish().unwrap();
        let mut out2 = vec![0u16; 10];
        decode_strip(COMPRESSION_DEFLATE, 2, &zd, width, 2, &mut out2).unwrap();
        assert_eq!(out2, out);
        // Short streams are errors, not silent zeros.
        assert!(decode_strip(COMPRESSION_NONE, 1, &data[..9], width, 2, &mut out).is_err());
        assert!(decode_strip(COMPRESSION_DEFLATE, 1, &zd[..zd.len() / 2], width, 2, &mut out).is_err());
    }

    #[test]
    fn lens_string_scan_handles_the_x6_tail() {
        let (run, lenses) = find_lens_string(X6_USER_COMMENT).expect("found");
        assert!(run.ends_with("_193_394240"), "{run}");
        assert_eq!(lenses.len(), 2);
        assert_eq!((lenses[0].sensor_w, lenses[0].sensor_h), (15488, 7744));
        // The prefix-stripped text is what the reader sees; a raw "INSTA360"
        // prefix glues a digit to the "2_" and must not match there.
        let glued = format!("INSTA360{X6_USER_COMMENT}");
        assert!(find_lens_string(&glued).is_none());
        assert!(find_lens_string("nothing here 2_x").is_none());
        let padded = format!("junk, {X6_USER_COMMENT}");
        assert_eq!(find_lens_string(&padded).unwrap().1.len(), 2);
    }

    #[test]
    fn still_trailer_walk() {
        let p = temp_path("trailer.bin");
        let mut bytes = vec![0x55u8; 1000]; // "pixel data"
        bytes.extend_from_slice(&fixture_trailer());
        std::fs::write(&p, &bytes).unwrap();
        let mut f = File::open(&p).unwrap();
        let len = f.metadata().unwrap().len();
        let t = read_still_trailer(&mut f, len).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(t.version, 3);
        assert_eq!(t.records.iter().map(|(id, b)| (*id, b.len())).collect::<Vec<_>>(), vec![(REC_PROTOBUF, fixture_protobuf().len()), (REC_IMU, 60), (0x0009, 48)]);
        assert_eq!(t.record(0x0009).unwrap(), &[0xAAu8; 48][..]);
        let m = parse_still_protobuf(t.protobuf.as_deref().unwrap());
        assert_eq!(m.lens_strings.len(), 1);
        assert_eq!((m.acc_range_g, m.gyro_range_dps), (Some(32), Some(2000)));
        // Packed field 210 is accepted too.
        let mut packed = Vec::new();
        for v in ORIENT {
            packed.extend_from_slice(&v.to_le_bytes());
        }
        let m2 = parse_still_protobuf(&pb_len(210, &packed));
        assert_eq!(m2.orientation, Some(ORIENT));
        // Default scaling without field 65 = the X6 constants.
        let s = decode_imu_record(&fixture_imu(), None, None);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].acc_g, [1.0, 0.0, -0.5]);
    }

    /// Real-file test — `cargo test -p vr180-fisheye --lib dng -- --ignored --nocapture`.
    /// Skips when the SMB share isn't mounted.
    #[test]
    #[ignore]
    fn real_x6_sample() {
        let p = Path::new(REAL_SAMPLE);
        if !p.exists() {
            eprintln!("skip: {REAL_SAMPLE} not mounted");
            return;
        }
        assert!(is_insta360_dng(p));
        assert_eq!(probe(p).unwrap(), (15520, 7760));
        let t0 = std::time::Instant::now();
        let s = read(p).unwrap();
        let dt = t0.elapsed();
        eprintln!("read() wall time: {:.3} s ({} threads)", dt.as_secs_f64(), std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0));
        assert_eq!((s.width, s.height), (15520, 7760));
        assert_eq!(s.bayer.len(), 15520 * 7760);
        assert_eq!(s.cfa, [1, 2, 0, 1]);
        assert_eq!(s.gain_maps.len(), 4);
        for g in &s.gain_maps {
            assert_eq!((g.points_v, g.points_h, g.map_planes), (13, 34, 1));
            assert_eq!((g.bottom, g.right, g.row_pitch, g.col_pitch), (7760, 15520, 2, 2));
        }
        let sites: Vec<_> = s.gain_maps.iter().map(|g| g.cfa_site()).collect();
        assert_eq!(sites, [(1, 0), (0, 0), (1, 1), (0, 1)]);
        assert_eq!(s.lenses.len(), 2);
        assert_eq!(s.calib_frame, (15488, 7744));
        assert!(!s.imu.is_empty());
        assert!(s.orientation.is_some());
        eprintln!("colour: {:#?}", s.color);
        eprintln!("exif: {:#?}", s.exif);
        eprintln!(
            "trailer: serial {:?} fw {:?} dim {:?} crop {:?} rs {:?} ms capture {:?} us orient {:?} imu {} samples ({}..{} us) records {:#06x?}",
            s.serial, s.firmware, s.dimension, s.crop, s.rolling_shutter_ms, s.capture_time_us, s.orientation, s.imu.len(),
            s.imu.first().map(|x| x.t_us).unwrap_or(0), s.imu.last().map(|x| x.t_us).unwrap_or(0), s.record_ids
        );
        eprintln!("gain map R plane row 6: {:?}", &s.gain_maps[0].gains[6 * 34..6 * 34 + 34]);
        assert_eq!(s.imu.len(), 594);
        assert_eq!(s.capture_time_us, Some(10417891503));
        assert_eq!(s.crop, Some(InsvWindowCrop { src_w: 7744, src_h: 7744, dst_w: 7760, dst_h: 7760, offset_x: 0, offset_y: 0 }));
        assert!((s.rolling_shutter_ms.unwrap() - 37.506667).abs() < 1e-6);
        assert_eq!(s.color.white_level, 16383);
        assert_eq!(s.color.calibration_illuminant1, 21);
        assert!((s.color.baseline_exposure - 2.6003604).abs() < 1e-5);
        assert_eq!(s.exif.iso, Some(120));
        // Independent decode (Python zlib, scratchpad) of the same file.
        let raw = Path::new(REAL_SAMPLE_RAW);
        if raw.exists() {
            let r = std::fs::read(raw).unwrap();
            assert_eq!(r.len(), s.bayer.len() * 2);
            let mismatch = s.bayer.iter().zip(r.chunks_exact(2)).position(|(&a, b)| a != u16::from_le_bytes([b[0], b[1]]));
            assert_eq!(mismatch, None, "mosaic differs from the reference decode");
            eprintln!("mosaic identical to {REAL_SAMPLE_RAW}");
        } else {
            eprintln!("reference raw not present; skipped the pixel comparison");
        }
    }
}
