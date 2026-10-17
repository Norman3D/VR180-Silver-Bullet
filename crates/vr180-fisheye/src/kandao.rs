//! Kandao QooCam 3 Ultra metadata.
//!
//! The camera writes a plain MP4 (one HEVC stream holding both fisheye
//! circles stacked top/bottom, 3840×7680 at 30 fps, HLG BT.2020 when the HDR
//! mode is on, a 4-channel AAC) followed by two proprietary top-level boxes
//! AFTER `moov`:
//!
//! - `kfix` — the fixed per-clip record: product / serial / firmware, the
//!   capture time, resolution, bitrate, exposure settings, `HDR`, `STEREO`,
//!   `FOV`.
//! - `kvar` — the variable record: the factory lens calibration (`LENS`, a
//!   hex string), the `INFO` tag (fps, IMU ranges and model, the crop/scale
//!   flags), the per-frame `PTS` table, the raw 1 kHz `IMU` stream, the
//!   per-frame exposure (`EXP`) and ISP records, GPS, and the per-lens lens
//!   shading maps (`GAINMAP0/1`).
//!
//! Both boxes share one key-value layout: a little-endian `u32` entry
//! count, then entries of `name[32]` (NUL padded), `type[8]` (`CHAR`, `U8`,
//! `U32`, `S32`, `U64`, `DOUBLE`, … NUL padded), `count: u32` and
//! `count × sizeof(type)` bytes of data. [`parse_kv_box`] decodes them.
//!
//! The DNG stills carry the same keys as `KEY=value` tokens in the EXIF
//! `UserComment` (`LENS=`, `GAINMAP0=`, `SHUTTER=`, `ISO=`, …), decoded by
//! [`parse_user_comment`].
//!
//! ## Lens record
//!
//! `LENS` is a hex string of two 134-byte blocks, one per lens:
//! `KaNDaOC@mERa` magic, a flags byte (`0xff` on firmware 1.1.3, `0xfe` on
//! 1.1.6), `"13"`, the lens index byte, then little-endian `u32` canvas
//! size — `(13888, 6944)` on 1.1.3 (the photo-resolution dual-fisheye
//! canvas), `(3840, 1920)` on 1.1.6 — lens 0 in the left half and lens 1
//! in the right; every consumer scales the half to its own frame, so the
//! canvas size is whatever the record says —
//! `f32` focal (px/rad), principal point `(cx, cy)` in canvas pixels, four
//! Kannala-Brandt coefficients, 16 reserved bytes, a row-major 3×3 rotation
//! and a translation (the stock back-to-back extrinsics), a `u32`, 8 bytes
//! and an `0xffffffff` terminator. The lens index byte is printed with one
//! hex digit, so the blocks are not byte aligned in the string:
//! [`parse_lens_hex`] scans for the magic at both nibble alignments.
//!
//! The projection is QooCam Studio's own (its stitching shader is embedded
//! as GLSL text in the application and reads, verbatim,
//! `theta_d = theta·(1 + k1θ² + k2θ⁴ + k3θ⁶ + k4θ⁸)`, `r = focal·theta_d`,
//! `uv = (cx, cy) + r·(x, y)/|xy|`) — exactly the app's Kannala-Brandt
//! path with `k5 = p1 = p2 = 0`. The video frame is each canvas half scaled
//! to the eye size (`V_CROP_SCALE=0,0,0,0,1,1,0`: no crop, unit scale), lens
//! 0 on top.
//!
//! ## IMU
//!
//! `IMU` is a stream of 20-byte samples: `u64` timestamp (µs, the same clock
//! as `PTS`), three `i16` gyro and three `i16` accelerometer codes, full
//! scale = the `INFO` tag's `V_G_RANGE` (°/s) and `V_A_RANGE` (g) over
//! ±32768. 1 kHz, starting ~2 s before the first frame. `EXP` is one
//! `(pts_ms: u32, shutter_us: u32)` per frame.

use crate::{Error, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Magic at the head of each lens block (`"KaNDaOC@mERa"`).
pub const KANDAO_LENS_MAGIC: &[u8; 12] = b"KaNDaOC@mERa";
/// Bytes per lens block after the 12-byte magic (`0xff`, `"13"`, index,
/// body): the block is 132 bytes in all.
const LENS_BLOCK_LEN: usize = 120;
/// Bytes per raw IMU sample.
const IMU_SAMPLE_LEN: usize = 20;

/// One decoded key-value entry of a `kfix` / `kvar` box.
#[derive(Debug, Clone, PartialEq)]
pub struct KvEntry {
    pub name: String,
    pub value: KvValue,
}

/// The typed payload of a [`KvEntry`].
#[derive(Debug, Clone, PartialEq)]
pub enum KvValue {
    /// `CHAR` — NUL-terminated text (the declared count includes padding).
    Text(String),
    U8(Vec<u8>),
    U32(Vec<u32>),
    S32(Vec<i32>),
    U64(Vec<u64>),
    F64(Vec<f64>),
    /// Any other type tag: the raw bytes, with the tag.
    Raw(String, Vec<u8>),
}

impl KvValue {
    /// The text of a `CHAR` entry.
    pub fn as_text(&self) -> Option<&str> {
        match self { KvValue::Text(s) => Some(s), _ => None }
    }
    /// The first scalar of a numeric entry, widened to `f64`.
    pub fn first_f64(&self) -> Option<f64> {
        match self {
            KvValue::U8(v) => v.first().map(|&x| x as f64),
            KvValue::U32(v) => v.first().map(|&x| x as f64),
            KvValue::S32(v) => v.first().map(|&x| x as f64),
            KvValue::U64(v) => v.first().map(|&x| x as f64),
            KvValue::F64(v) => v.first().copied(),
            _ => None,
        }
    }
    /// The raw bytes of a `U8` entry.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self { KvValue::U8(v) => Some(v), KvValue::Raw(_, v) => Some(v), _ => None }
    }
}

/// Decode the entries of a key-value box. `payload` starts at the entry
/// count (i.e. just after the 8-byte box header).
pub fn parse_kv_box(payload: &[u8]) -> Result<Vec<KvEntry>> {
    let bad = |m: String| Error::Kandao(m);
    if payload.len() < 4 {
        return Err(bad("key-value box shorter than its entry count".into()));
    }
    let n = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    let mut off = 4usize;
    let mut out = Vec::with_capacity(n.min(256));
    for i in 0..n {
        if off + 44 > payload.len() {
            return Err(bad(format!("entry {i}: header runs past the box ({} bytes)", payload.len())));
        }
        let name = cstr(&payload[off..off + 32]);
        let ty = cstr(&payload[off + 32..off + 40]);
        let count = u32::from_le_bytes([payload[off + 40], payload[off + 41], payload[off + 42], payload[off + 43]]) as usize;
        off += 44;
        let esize = match ty.as_str() {
            "CHAR" | "U8" | "S8" => 1,
            "U16" | "S16" => 2,
            "U32" | "S32" | "FLOAT" => 4,
            "U64" | "S64" | "DOUBLE" => 8,
            _ => 1,
        };
        let len = count.checked_mul(esize).ok_or_else(|| bad(format!("entry {name}: count overflow")))?;
        if off + len > payload.len() {
            return Err(bad(format!("entry {name}: {len} data bytes run past the box")));
        }
        let data = &payload[off..off + len];
        off += len;
        let value = match ty.as_str() {
            "CHAR" => KvValue::Text(cstr(data)),
            "U8" => KvValue::U8(data.to_vec()),
            "U32" => KvValue::U32(data.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()),
            "S32" => KvValue::S32(data.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()),
            "U64" => KvValue::U64(data.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()),
            "DOUBLE" => KvValue::F64(data.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()),
            other => KvValue::Raw(other.to_string(), data.to_vec()),
        };
        out.push(KvEntry { name, value });
    }
    Ok(out)
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Look up an entry by name.
pub fn kv_get<'a>(entries: &'a [KvEntry], name: &str) -> Option<&'a KvValue> {
    entries.iter().find(|e| e.name == name).map(|e| &e.value)
}

/// One lens of the factory calibration, in the photo-resolution canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct KandaoLens {
    /// 0 = the lens stored in the left canvas half / top video half, 1 =
    /// right half / bottom.
    pub index: u8,
    /// The dual-fisheye canvas the principal point is expressed in
    /// (`13888 × 6944` on the QooCam 3 Ultra: two `6944²` halves).
    pub canvas_w: u32,
    pub canvas_h: u32,
    /// Focal length, px/rad (one value: the lens is isotropic).
    pub focal: f32,
    /// Principal point in canvas pixels (lens 1's `cx` includes the half
    /// offset — see [`KandaoLens::half_origin_x`]).
    pub cx: f32,
    pub cy: f32,
    /// Kannala-Brandt `k1..k4` (θ³, θ⁵, θ⁷, θ⁹ terms).
    pub k: [f32; 4],
    /// Row-major stock extrinsic rotation (lens 1 ≈ 180° about y).
    pub rotation: [f32; 9],
    pub translation: [f32; 3],
}

impl KandaoLens {
    /// Width of one canvas half (the lens's own frame).
    pub fn half_w(&self) -> u32 { self.canvas_w / 2 }
    /// Canvas x of this lens's half: the half the principal point falls in.
    pub fn half_origin_x(&self) -> f32 {
        if self.cx >= self.half_w() as f32 { self.half_w() as f32 } else { 0.0 }
    }
    /// Principal point relative to its own half.
    pub fn half_principal_point(&self) -> (f32, f32) {
        (self.cx - self.half_origin_x(), self.cy)
    }
    /// Forward radial model `r(θ)` in canvas pixels.
    pub fn project_radius(&self, theta: f64) -> f64 {
        let t2 = theta * theta;
        let k = self.k.map(|v| v as f64);
        self.focal as f64 * theta * (1.0 + t2 * (k[0] + t2 * (k[1] + t2 * (k[2] + t2 * k[3]))))
    }
}

/// Decode the `LENS` hex string into its lens blocks (empty when none).
pub fn parse_lens_hex(hex: &str) -> Vec<KandaoLens> {
    let hex = hex.trim();
    let mut out = Vec::new();
    // The blocks are not byte aligned (a one-digit index precedes each), so
    // decode at both nibble alignments and scan each for the magic.
    for shift in 0..2 {
        let s = if hex.len() > shift { &hex[shift..] } else { "" };
        let bytes = match hex_decode(&s[..s.len() / 2 * 2]) { Some(b) => b, None => continue };
        let mut i = 0;
        while i + KANDAO_LENS_MAGIC.len() + LENS_BLOCK_LEN <= bytes.len() {
            if &bytes[i..i + KANDAO_LENS_MAGIC.len()] == KANDAO_LENS_MAGIC {
                if let Some(l) = parse_lens_block(&bytes[i + KANDAO_LENS_MAGIC.len()..]) {
                    if !out.iter().any(|o: &KandaoLens| o.index == l.index) {
                        out.push(l);
                    }
                }
                i += KANDAO_LENS_MAGIC.len() + LENS_BLOCK_LEN;
            } else {
                i += 1;
            }
        }
    }
    out.sort_by_key(|l| l.index);
    out
}

/// `body` starts right after the magic: `ff`, `'1' '3'`, index, then the
/// little-endian fields.
fn parse_lens_block(body: &[u8]) -> Option<KandaoLens> {
    // body[0] is a flags byte (0xff on firmware 1.1.3, 0xfe on 1.1.6 — the
    // 1.1.6 record also moves to a 3840×1920 canvas), body[1..3] = "13".
    if body.len() < LENS_BLOCK_LEN || &body[1..3] != b"13" {
        return None;
    }
    let index = body[3];
    let u32_at = |o: usize| u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
    let f32_at = |o: usize| f32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
    let canvas_w = u32_at(4);
    let canvas_h = u32_at(8);
    let focal = f32_at(12);
    let cx = f32_at(16);
    let cy = f32_at(20);
    let k = [f32_at(24), f32_at(28), f32_at(32), f32_at(36)];
    let mut rotation = [0f32; 9];
    for (j, r) in rotation.iter_mut().enumerate() { *r = f32_at(56 + 4 * j); }
    let translation = [f32_at(92), f32_at(96), f32_at(100)];
    let sane = canvas_w > 0 && canvas_h > 0 && canvas_w < 65536 && canvas_h < 65536
        && focal.is_finite() && focal > 1.0 && cx.is_finite() && cy.is_finite()
        && k.iter().all(|v| v.is_finite() && v.abs() < 1.0)
        && index < 2;
    if !sane {
        return None;
    }
    Some(KandaoLens { index, canvas_w, canvas_h, focal, cx, cy, k, rotation, translation })
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    let b = s.as_bytes();
    let nib = |c: u8| -> Option<u8> {
        match c { b'0'..=b'9' => Some(c - b'0'), b'a'..=b'f' => Some(c - b'a' + 10), b'A'..=b'F' => Some(c - b'A' + 10), _ => None }
    };
    let mut out = Vec::with_capacity(s.len() / 2);
    for c in b.chunks_exact(2) {
        out.push((nib(c[0])? << 4) | nib(c[1])?);
    }
    Some(out)
}

/// A per-lens lens-shading gain grid (`GAINMAP0/1`): `planes × rows × cols`
/// gains, plane-major, over the lens's canvas half (the 17 × 13 grid spans
/// the 6944² half — 16 × 12 cells of 434 × 579 px; the shading centre, gain
/// 1.0, is the grid centre). Plane 0/1/2 = R/G/B.
#[derive(Debug, Clone, PartialEq)]
pub struct KandaoGainMap {
    pub cols: u32,
    pub rows: u32,
    pub planes: u32,
    pub gains: Vec<f32>,
}

impl KandaoGainMap {
    /// Gain at grid node `(row, col)` of `plane`.
    pub fn at(&self, plane: u32, row: u32, col: u32) -> f32 {
        let p = plane.min(self.planes.saturating_sub(1)) as usize;
        let idx = (p * self.rows as usize + row.min(self.rows - 1) as usize) * self.cols as usize + col.min(self.cols - 1) as usize;
        self.gains.get(idx).copied().unwrap_or(1.0)
    }
    /// The plane's `rows × cols` gains, row-major.
    pub fn plane(&self, plane: u32) -> &[f32] {
        let n = (self.rows * self.cols) as usize;
        let p = plane.min(self.planes.saturating_sub(1)) as usize;
        &self.gains[p * n..(p + 1) * n]
    }
}

/// Decode a `GAINMAP` payload: `u32` cols, rows, planes, then the `f32`s.
pub fn parse_gain_map(bytes: &[u8]) -> Option<KandaoGainMap> {
    if bytes.len() < 12 { return None; }
    let u = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let (cols, rows, planes) = (u(0), u(4), u(8));
    let n = cols.checked_mul(rows)?.checked_mul(planes)? as usize;
    if n == 0 || n > 1 << 20 || bytes.len() < 12 + n * 4 { return None; }
    let gains: Vec<f32> = bytes[12..12 + n * 4].chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    if gains.iter().any(|g| !g.is_finite() || *g <= 0.0 || *g > 64.0) { return None; }
    Some(KandaoGainMap { cols, rows, planes, gains })
}

/// One raw IMU sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KandaoImuSample {
    /// Camera clock, µs (the `PTS` clock).
    pub t_us: u64,
    /// Gyro, °/s.
    pub gyr_dps: [f32; 3],
    /// Accelerometer, g.
    pub acc_g: [f32; 3],
}

/// Per-frame exposure record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KandaoExposure {
    /// Frame timestamp, ms (`PTS / 1000`, truncated).
    pub pts_ms: u32,
    /// Shutter, µs.
    pub shutter_us: u32,
}

/// Decode the `IMU` payload with the given full-scale ranges.
pub fn parse_imu(bytes: &[u8], gyro_range_dps: f32, acc_range_g: f32) -> Vec<KandaoImuSample> {
    let gs = gyro_range_dps / 32768.0;
    let as_ = acc_range_g / 32768.0;
    bytes.chunks_exact(IMU_SAMPLE_LEN).map(|c| {
        let t_us = u64::from_le_bytes(c[0..8].try_into().unwrap());
        let i16_at = |o: usize| i16::from_le_bytes([c[o], c[o + 1]]) as f32;
        KandaoImuSample {
            t_us,
            gyr_dps: [i16_at(8) * gs, i16_at(10) * gs, i16_at(12) * gs],
            acc_g: [i16_at(14) * as_, i16_at(16) * as_, i16_at(18) * as_],
        }
    }).collect()
}

/// Decode the `EXP` payload.
pub fn parse_exposures(bytes: &[u8]) -> Vec<KandaoExposure> {
    bytes.chunks_exact(8).map(|c| KandaoExposure {
        pts_ms: u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
        shutter_us: u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
    }).collect()
}

/// `KEY=value KEY=value …` (the `INFO` tag and the DNG `UserComment`).
/// Values run to the next space; a leading free-text prefix (the DNG's
/// `"QooCam 3 Ultra "`) is skipped.
pub fn parse_user_comment(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for tok in text.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            if !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}

/// Everything the video boxes hold that the pipeline uses.
#[derive(Debug, Clone, Default)]
pub struct KandaoMeta {
    pub product: String,
    pub serial: String,
    pub firmware: String,
    /// Factory lens calibration, sorted by lens index.
    pub lenses: Vec<KandaoLens>,
    /// Lens shading grids, indexed by lens.
    pub gain_maps: Vec<KandaoGainMap>,
    /// `"3840x7680@30.0"`.
    pub video_resolution: String,
    /// `V_FPS` from the `INFO` tag.
    pub fps: f32,
    /// `HDR` flag (1 = HLG).
    pub hdr: bool,
    /// `STEREO` flag.
    pub stereo: bool,
    /// Per-frame presentation times, µs (the IMU clock).
    pub frame_pts_us: Vec<u64>,
    pub imu: Vec<KandaoImuSample>,
    pub exposures: Vec<KandaoExposure>,
    pub gyro_range_dps: f32,
    pub acc_range_g: f32,
    pub imu_model: String,
    /// The `INFO` tag, split into keys.
    pub info: BTreeMap<String, String>,
}

impl KandaoMeta {
    /// Mean IMU period, s (0 when unknown).
    pub fn imu_period_s(&self) -> f64 {
        if self.imu.len() < 2 { return 0.0; }
        let span = (self.imu[self.imu.len() - 1].t_us - self.imu[0].t_us) as f64 * 1e-6;
        if span <= 0.0 { 0.0 } else { span / (self.imu.len() - 1) as f64 }
    }
    /// Shutter of frame `i`, seconds (the record is per frame; a missing
    /// entry reads the last one).
    pub fn shutter_s(&self, i: usize) -> Option<f64> {
        let e = self.exposures.get(i).or_else(|| self.exposures.last())?;
        Some(e.shutter_us as f64 * 1e-6)
    }
}

/// Top-level MP4 boxes: `(type, payload offset, payload length)`.
fn top_level_boxes(f: &mut File) -> Result<Vec<([u8; 4], u64, u64)>> {
    let len = f.metadata()?.len();
    let mut out = Vec::new();
    let mut off = 0u64;
    let mut hdr = [0u8; 16];
    while off + 8 <= len {
        f.seek(SeekFrom::Start(off))?;
        let got = f.read(&mut hdr)?;
        if got < 8 { break; }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (size, hlen) = if size32 == 1 {
            if got < 16 { break; }
            (u64::from_be_bytes(hdr[8..16].try_into().unwrap()), 16u64)
        } else if size32 == 0 {
            (len - off, 8)
        } else {
            (size32, 8)
        };
        if size < hlen { break; }
        out.push((typ, off + hlen, size - hlen));
        off = off.saturating_add(size);
        if out.len() > 4096 { break; }
    }
    Ok(out)
}

/// True when the file has a Kandao `kfix` box (a QooCam MP4), or a `.kdat`
/// sidecar next to it does (see [`kdat_sidecar`]).
pub fn has_kandao_boxes(path: &Path) -> bool {
    let has = |p: &Path| -> bool {
        let Ok(mut f) = File::open(p) else { return false };
        top_level_boxes(&mut f).map(|b| b.iter().any(|(t, _, _)| t == b"kfix")).unwrap_or(false)
    };
    has(path) || kdat_sidecar(path).map_or(false, |k| has(&k))
}

/// The camera also writes the two boxes as a `<name>.kdat` sidecar (the
/// same `kfix` + `kvar` byte for byte, QooCam Studio reads it too) — the
/// copy that survives when a recording is cut short before `moov`. The
/// path when such a file exists.
pub fn kdat_sidecar(path: &Path) -> Option<std::path::PathBuf> {
    let k = path.with_extension("kdat");
    if k.is_file() { return Some(k); }
    let k2 = path.with_extension("KDAT");
    if k2.is_file() { Some(k2) } else { None }
}

fn read_box(f: &mut File, off: u64, len: u64) -> Result<Vec<u8>> {
    if len > 256 << 20 {
        return Err(Error::Kandao(format!("box of {len} bytes is implausibly large")));
    }
    let mut buf = vec![0u8; len as usize];
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read and decode the `kfix` + `kvar` boxes. `with_motion = false` skips
/// the IMU / PTS / exposure tables (identity + lenses only).
pub fn read_kandao_meta(path: &Path, with_motion: bool) -> Result<KandaoMeta> {
    let mut f = File::open(path)?;
    let mut boxes = top_level_boxes(&mut f)?;
    if !boxes.iter().any(|(t, _, _)| t == b"kfix") {
        // Fall back to the sidecar copy.
        if let Some(k) = kdat_sidecar(path) {
            tracing::info!("kandao: no kfix box in {} — reading the {} sidecar", path.display(), k.display());
            f = File::open(&k)?;
            boxes = top_level_boxes(&mut f)?;
        }
    }
    let find = |name: &[u8; 4]| boxes.iter().find(|(t, _, _)| t == name).copied();
    let (_, fix_off, fix_len) = find(b"kfix").ok_or_else(|| Error::Kandao("no kfix box (nor a .kdat sidecar)".into()))?;
    let fixed = parse_kv_box(&read_box(&mut f, fix_off, fix_len)?)?;
    let var = match find(b"kvar") {
        Some((_, o, l)) => parse_kv_box(&read_box(&mut f, o, l)?)?,
        None => Vec::new(),
    };
    Ok(meta_from_entries(&fixed, &var, with_motion))
}

fn meta_from_entries(fixed: &[KvEntry], var: &[KvEntry], with_motion: bool) -> KandaoMeta {
    let text = |entries: &[KvEntry], k: &str| kv_get(entries, k).and_then(|v| v.as_text()).unwrap_or("").to_string();
    let num = |entries: &[KvEntry], k: &str| kv_get(entries, k).and_then(|v| v.first_f64());
    let info = parse_user_comment(&text(var, "INFO"));
    let info_f = |k: &str, d: f32| info.get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    let gyro_range_dps = info_f("V_G_RANGE", 2000.0);
    let acc_range_g = info_f("V_A_RANGE", 16.0);
    let mut gain_maps = Vec::new();
    for k in ["GAINMAP0", "GAINMAP1"] {
        if let Some(b) = kv_get(var, k).and_then(|v| v.as_bytes()) {
            if let Some(g) = parse_gain_map(b) { gain_maps.push(g); }
        }
    }
    let mut m = KandaoMeta {
        product: text(fixed, "PRODUCT"),
        serial: text(fixed, "SN"),
        firmware: text(fixed, "SW_VER"),
        lenses: parse_lens_hex(&text(var, "LENS")),
        gain_maps,
        video_resolution: text(fixed, "VIDEO_RESOLUTION"),
        fps: info_f("V_FPS", 0.0),
        hdr: num(fixed, "HDR").map(|v| v > 0.5).unwrap_or(false),
        stereo: num(fixed, "STEREO").map(|v| v > 0.5).unwrap_or(false),
        gyro_range_dps,
        acc_range_g,
        imu_model: info.get("V_IMU_MODEL").cloned().unwrap_or_default(),
        info,
        ..Default::default()
    };
    if with_motion {
        if let Some(KvValue::U64(p)) = kv_get(var, "PTS") { m.frame_pts_us = p.clone(); }
        if let Some(b) = kv_get(var, "IMU").and_then(|v| v.as_bytes()) {
            m.imu = parse_imu(b, gyro_range_dps, acc_range_g);
        }
        if let Some(b) = kv_get(var, "EXP").and_then(|v| v.as_bytes()) {
            m.exposures = parse_exposures(b);
        }
    }
    m
}

/// What a QooCam DNG's `UserComment` carries.
#[derive(Debug, Clone, Default)]
pub struct KandaoStillMeta {
    pub product: String,
    pub serial: String,
    pub firmware: String,
    pub lenses: Vec<KandaoLens>,
    pub gain_maps: Vec<KandaoGainMap>,
    pub shutter_us: Option<u64>,
    pub iso: Option<u32>,
    pub hdr: bool,
    /// Every `KEY=value` of the comment.
    pub keys: BTreeMap<String, String>,
}

/// Decode a QooCam DNG `UserComment` (`None` when it carries no lens record).
pub fn still_meta_from_user_comment(text: &str) -> Option<KandaoStillMeta> {
    let keys = parse_user_comment(text);
    let lenses = parse_lens_hex(keys.get("LENS")?);
    if lenses.is_empty() {
        return None;
    }
    let mut gain_maps = Vec::new();
    for k in ["GAINMAP0", "GAINMAP1"] {
        if let Some(g) = keys.get(k).and_then(|h| hex_decode(h)).and_then(|b| parse_gain_map(&b)) {
            gain_maps.push(g);
        }
    }
    let unplus = |s: &String| s.replace('+', " ");
    Some(KandaoStillMeta {
        product: keys.get("PRODUCT").map(unplus).unwrap_or_default(),
        serial: keys.get("SN").cloned().unwrap_or_default(),
        firmware: keys.get("SW_VER").cloned().unwrap_or_default(),
        lenses,
        gain_maps,
        shutter_us: keys.get("SHUTTER").and_then(|v| v.parse().ok()),
        iso: keys.get("ISO").and_then(|v| v.parse().ok()),
        hdr: keys.get("HDR").map(|v| v == "1").unwrap_or(false),
        keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ty: &str, count: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        let mut n = [0u8; 32];
        n[..name.len()].copy_from_slice(name.as_bytes());
        v.extend_from_slice(&n);
        let mut t = [0u8; 8];
        t[..ty.len()].copy_from_slice(ty.as_bytes());
        v.extend_from_slice(&t);
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    fn kv_box(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut v = (entries.len() as u32).to_le_bytes().to_vec();
        for e in entries { v.extend_from_slice(e); }
        v
    }

    /// The sample clip's LENS string (lens 0 + lens 1).
    const LENS_HEX: &str = "14b614e44614f43406d455261ff31330040360000201b00001ce20545cd275845f07359458d6239bcaed7f43aacc868bb40da7f390000000000000000000000000000000003d97f3f6e440d3dd1a6cc226e440dbd03d97f3f5860b925d1a6cc225860b9a50000803f000000000000000000000080f00000000000000000000000ffffffff0f24b614e44614f43406d455261ff31330140360000201b0000126f0545a7c022461c9758458d6239bcaed7f43aacc868bb40da7f3900000000000000000000000000000000fafd7fbf5a7af63b484115bb7603f73b6afc7f3f94beedbb95ab113bb74ceebb1bfe7fbf5a0530402b5552bd95e986c0f00000000000000000000000ffffffff5b";

    #[test]
    fn lens_hex_decodes_both_blocks() {
        let l = parse_lens_hex(LENS_HEX);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].index, l[1].index), (0, 1));
        assert_eq!((l[0].canvas_w, l[0].canvas_h), (13888, 6944));
        assert!((l[0].focal - 2142.13).abs() < 0.01, "{}", l[0].focal);
        assert!((l[0].cx - 3458.49).abs() < 0.01);
        assert!((l[0].cy - 3479.25).abs() < 0.01);
        assert!((l[1].focal - 2134.94).abs() < 0.01);
        assert!((l[1].cx - 10416.16).abs() < 0.02, "{}", l[1].cx);
        assert!((l[1].cy - 3465.44).abs() < 0.01);
        assert!((l[0].k[0] + 0.011315).abs() < 1e-6);
        assert!((l[0].k[3] - 0.000244).abs() < 1e-6);
        assert_eq!(l[0].k, l[1].k);
        assert_eq!(l[0].half_origin_x(), 0.0);
        assert_eq!(l[1].half_origin_x(), 6944.0);
        let (px, py) = l[1].half_principal_point();
        assert!((px - 3472.16).abs() < 0.02 && (py - 3465.44).abs() < 0.01);
        // lens 1's stock extrinsic is ~180° about y: diag(-1, 1, -1).
        assert!(l[1].rotation[0] < -0.999 && l[1].rotation[4] > 0.999 && l[1].rotation[8] < -0.999);
        assert!(l[0].rotation[0] > 0.999);
        // rim: θ = 95° lands inside the 6944 half.
        let r = l[0].project_radius(95f64.to_radians());
        assert!(r > 3200.0 && r < 3472.0, "{r}");
    }

    /// Firmware 1.1.6.6 (a second camera): flags byte 0xfe, 3840×1920 canvas.
    const LENS_HEX_116: &str = "14b614e44614f43406d455261fe313300000f000080070000e000154418e96c4460bb6e448d6239bcaed7f43aacc868bb40da7f3900000000000000000000000000000000e4f57f3fecb0003b47f88e3ce8ab00bbdfff7f3ff4bd0fb859f88ebc0000000005f67f3f000000000000000000000000f00000000000000000000000ffffffff0a24b614e44614f43406d455261fe313301000f000080070000e0001544688f3345e36370448d6239bcaed7f43aacc868bb40da7f390000000000000000000000000000000057eb7f3f4ef5b93c0923303cd1a2babc10ed7f3f8987fa3ba43d2dbc4e3d01bc49fa7f3f1ad6cfc0ecb07e3e611d8b3df00000000000000000000000ffffffff5a";

    #[test]
    fn lens_hex_firmware_116_record_decodes() {
        let l = parse_lens_hex(LENS_HEX_116);
        assert_eq!(l.len(), 2, "{l:?}");
        assert_eq!((l[0].canvas_w, l[0].canvas_h), (3840, 1920));
        assert!((l[0].focal - 596.01).abs() < 0.01, "{}", l[0].focal);
        assert!((l[0].cx - 947.64).abs() < 0.01 && (l[0].cy - 954.93).abs() < 0.01);
        assert!((l[1].cx - 2872.96).abs() < 0.01 && (l[1].cy - 961.56).abs() < 0.01);
        assert_eq!(l[0].half_w(), 1920);
        assert_eq!(l[1].half_origin_x(), 1920.0);
        assert_eq!(l[0].k, l[1].k);
    }

    #[test]
    fn kdat_sidecar_serves_the_boxes() {
        let dir = std::env::temp_dir().join(format!("vr180_kandao_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("clip.MP4");
        std::fs::write(&mp4, b"\0\0\0\x10ftypisom\0\0\0\0").unwrap(); // ftyp only, no boxes
        assert!(!has_kandao_boxes(&mp4));
        let payload = kv_box(&[entry("PRODUCT", "CHAR", 16, b"QooCam 3 Ultra\0\0"), entry("HDR", "U8", 1, &[1])]);
        let mut kfix = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        kfix.extend_from_slice(b"kfix"); kfix.extend_from_slice(&payload);
        std::fs::write(dir.join("clip.kdat"), &kfix).unwrap();
        assert!(has_kandao_boxes(&mp4));
        let m = read_kandao_meta(&mp4, false).unwrap();
        assert_eq!(m.product, "QooCam 3 Ultra");
        assert!(m.hdr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kv_box_round_trip() {
        let b = kv_box(&[
            entry("PRODUCT", "CHAR", 16, b"QooCam 3 Ultra\0\0"),
            entry("HDR", "U8", 1, &[1]),
            entry("VIDEO_BITRATE", "U32", 1, &150_000_000u32.to_le_bytes()),
            entry("EV", "DOUBLE", 1, &(-0.5f64).to_le_bytes()),
            entry("GPS_CAPTIME", "U64", 1, &1747080583535u64.to_le_bytes()),
            entry("AUDIO_GAIN", "S32", 1, &(-1i32).to_le_bytes()),
        ]);
        let e = parse_kv_box(&b).unwrap();
        assert_eq!(e.len(), 6);
        assert_eq!(kv_get(&e, "PRODUCT").unwrap().as_text(), Some("QooCam 3 Ultra"));
        assert_eq!(kv_get(&e, "HDR").unwrap().first_f64(), Some(1.0));
        assert_eq!(kv_get(&e, "VIDEO_BITRATE").unwrap().first_f64(), Some(150_000_000.0));
        assert_eq!(kv_get(&e, "EV").unwrap().first_f64(), Some(-0.5));
        assert_eq!(kv_get(&e, "GPS_CAPTIME").unwrap().first_f64(), Some(1747080583535.0));
        assert_eq!(kv_get(&e, "AUDIO_GAIN").unwrap().first_f64(), Some(-1.0));
        assert!(parse_kv_box(&b[..b.len() - 2]).is_err());
    }

    #[test]
    fn imu_and_exposure_decode() {
        let mut s = Vec::new();
        s.extend_from_slice(&17_760_307_737u64.to_le_bytes());
        for v in [-91i16, -16, 15, 21, -2047, 88] { s.extend_from_slice(&v.to_le_bytes()); }
        let imu = parse_imu(&s, 2000.0, 16.0);
        assert_eq!(imu.len(), 1);
        assert_eq!(imu[0].t_us, 17_760_307_737);
        assert!((imu[0].gyr_dps[0] + 91.0 * 2000.0 / 32768.0).abs() < 1e-4);
        assert!((imu[0].acc_g[1] + 2047.0 / 2048.0).abs() < 1e-5);
        let mut e = Vec::new();
        e.extend_from_slice(&17762176u32.to_le_bytes());
        e.extend_from_slice(&6933u32.to_le_bytes());
        let ex = parse_exposures(&e);
        assert_eq!(ex, vec![KandaoExposure { pts_ms: 17762176, shutter_us: 6933 }]);
    }

    #[test]
    fn gain_map_layout_is_plane_major() {
        let (cols, rows, planes) = (3u32, 2u32, 3u32);
        let mut b = Vec::new();
        for v in [cols, rows, planes] { b.extend_from_slice(&v.to_le_bytes()); }
        for p in 0..planes { for r in 0..rows { for c in 0..cols {
            b.extend_from_slice(&(1.0 + p as f32 * 0.1 + r as f32 * 0.01 + c as f32 * 0.001).to_le_bytes());
        }}}
        let g = parse_gain_map(&b).unwrap();
        assert!((g.at(2, 1, 2) - 1.212).abs() < 1e-6);
        assert!((g.at(0, 0, 0) - 1.0).abs() < 1e-6);
        assert_eq!(g.plane(1).len(), 6);
        assert!(parse_gain_map(&b[..b.len() - 4]).is_none());
    }

    #[test]
    fn user_comment_keys() {
        let text = format!("QooCam 3 Ultra PRODUCT=QooCam+3+Ultra PROJECT=Cherry SN=QU9302313165 SW_VER=V1.1.3.8 LENS={LENS_HEX} HDR=0 DNG=1 ISO=100 SHUTTER=292025 FORMAT=jpg+dng");
        let m = still_meta_from_user_comment(&text).unwrap();
        assert_eq!(m.product, "QooCam 3 Ultra");
        assert_eq!(m.serial, "QU9302313165");
        assert_eq!(m.lenses.len(), 2);
        assert_eq!(m.shutter_us, Some(292025));
        assert_eq!(m.iso, Some(100));
        assert!(!m.hdr);
        assert!(still_meta_from_user_comment("nothing here").is_none());
    }

    #[test]
    fn meta_from_entries_reads_info_and_tables() {
        let info = b"V_S_PTS=1 V_FPS=30.0 V_IMU_F=6 V_G_RANGE=2000 V_A_RANGE=16 V_CROP_SCALE=0,0,0,0,1,1,0 V_IMU_MODEL=sh3001\0";
        let mut pts = Vec::new();
        for v in [100u64, 133_333, 166_667] { pts.extend_from_slice(&v.to_le_bytes()); }
        let fixed = parse_kv_box(&kv_box(&[entry("PRODUCT", "CHAR", 16, b"QooCam 3 Ultra\0\0"), entry("HDR", "U8", 1, &[1])])).unwrap();
        let var = parse_kv_box(&kv_box(&[
            entry("INFO", "CHAR", info.len() as u32, info),
            entry("LENS", "CHAR", LENS_HEX.len() as u32, LENS_HEX.as_bytes()),
            entry("PTS", "U64", 3, &pts),
        ])).unwrap();
        let m = meta_from_entries(&fixed, &var, true);
        assert_eq!(m.product, "QooCam 3 Ultra");
        assert!(m.hdr);
        assert_eq!(m.fps, 30.0);
        assert_eq!(m.imu_model, "sh3001");
        assert_eq!(m.lenses.len(), 2);
        assert_eq!(m.frame_pts_us, vec![100, 133_333, 166_667]);
        let m2 = meta_from_entries(&fixed, &var, false);
        assert!(m2.frame_pts_us.is_empty());
    }
}
