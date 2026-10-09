//! Raw stage for stills: gain map, demosaic, camera-linear RGBA64, develop to display RGBA64.
//!
//! CPU-only and platform-neutral (`std::thread::scope` over row bands; no GPU,
//! no `unsafe`, no target cfgs). The stage consumes plain numbers rather than a
//! decoder's types ([`RawParams`], [`GainMapLike`], [`DngColorLike`]) so it
//! stays decoupled from `vr180_fisheye::dng`.
//!
//! Two stages, mirroring the validated Python prototypes (`gainmap.py`,
//! `develop.py`):
//!
//! 1. [`develop_linear`] — one lens half of the side-by-side Bayer mosaic to
//!    camera-native linear RGBA64 ([`LinearEye`]). Per raw site
//!    `L = gain(row, col) · (raw − black_site) / (white − black_site)`, with the
//!    DNG 1.3 GainMap opcodes (vignetting + colour shading) applied BEFORE
//!    demosaic as the spec orders, in FULL-frame coordinates (the maps cover the
//!    whole frame with two vignetting centres, and the CFA phase is a full-frame
//!    property). Then Malvar-He-Cutler 5×5 linear demosaic, or 2×2 superpixel
//!    binning for a cheap half-size preview. Values are stored as
//!    `L · 65535 / l_max` with `l_max` = the largest gain in the file, so a
//!    gain-boosted value never clips in u16; alpha flags pixels whose demosaic
//!    support touched a raw-clipped site, and such pixels are filled with the
//!    camera-space neutral at the local gain so clipped highlights stay neutral
//!    instead of turning magenta after white balance.
//! 2. [`develop_display`] — [`LinearEye`] to display-referred sRGB RGBA64: white
//!    balance, `ColorMatrix1⁻¹` and a Bradford adaptation of the as-shot
//!    neutral to D65 folded into one 3×3 on the UN-balanced camera values,
//!    `2^(BaselineExposure + EV)` and the storage un-scaling, chroma scale about
//!    luma, tone curve, sRGB OETF.

use std::sync::Mutex;

/// Which lens half of the side-by-side still to develop
/// (`Left`: columns `0..width/2`, `Right`: `width/2..width`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    Left,
    Right,
}

/// The mosaic's geometry and linearisation constants.
#[derive(Clone, Debug)]
pub struct RawParams {
    /// Full-frame width (both halves).
    pub width: u32,
    pub height: u32,
    /// 2×2 CFA pattern, row-major: 0 = R, 1 = G, 2 = B; the site of pixel
    /// `(row, col)` is `cfa[(row & 1) * 2 + (col & 1)]` in full-frame coordinates.
    pub cfa: [u8; 4],
    pub white_level: f32,
    /// Per CFA site, same order as `cfa`.
    pub black_level: [f32; 4],
    /// AsShotNeutral (camera-space colour of a neutral, G = 1). Only used to
    /// fill raw-clipped pixels with a neutral colour (see [`develop_linear`]).
    pub as_shot_neutral: [f32; 3],
}

/// A DNG 1.3 `GainMap` opcode (OpcodeList2), decoder-agnostic copy.
///
/// The map grid is `points_v × points_h` samples with `map_planes` gains each,
/// placed in normalised coordinates: point `(i, j)` sits at
/// `v = origin_v + i·spacing_v`, `u = origin_h + j·spacing_h`, where `(0, 0)` is
/// the top-left and `(1, 1)` the bottom-right of the opcode area
/// `top..bottom × left..right`. The opcode applies to the pixels of that area
/// stepping by `row_pitch` / `col_pitch` (pitch 2 selects one CFA plane).
#[derive(Clone, Debug)]
pub struct GainMapLike {
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
    /// `points_v × points_h × map_planes` gains, row-major, plane fastest.
    pub gains: Vec<f32>,
}

/// The DNG colour tags [`develop_display`] needs.
#[derive(Clone, Debug)]
pub struct DngColorLike {
    /// `ColorMatrix1`, XYZ → camera, row-major.
    pub color_matrix1: [f32; 9],
    /// `AsShotNeutral`, camera space (G = 1).
    pub as_shot_neutral: [f32; 3],
    /// `BaselineExposure` in EV.
    pub baseline_exposure: f32,
}

/// One eye in camera-native linear RGBA64 (NOT white balanced).
///
/// `rgba` is row-major, 4×u16 per pixel. `stored = L · 65535 / l_max`;
/// `alpha = 65535` where the demosaic support touched a raw-clipped site,
/// else 0.
#[derive(Clone, Debug)]
pub struct LinearEye {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u16>,
    /// Headroom factor: the largest gain over the file's gain maps (≥ 1).
    /// Multiply the stored value by `l_max / 65535` to get `L`.
    pub l_max: f32,
}

/// White balance applied in [`develop_display`].
#[derive(Clone, Copy, Debug)]
pub enum WhiteBalance {
    /// Divide by `AsShotNeutral` (G = 1): the file's neutral maps to grey.
    AsShot,
    /// Per-channel multipliers on camera RGB (`AsShot` ≡ `1 / as_shot_neutral`).
    /// The chromatic adaptation stays computed from the as-shot neutral; the
    /// multipliers act as a relative tint on top of it.
    Multipliers([f32; 3]),
}

/// Tone curve applied after exposure, before the sRGB OETF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToneCurve {
    /// Clip to `0..1`.
    Linear,
    /// Narkowicz' ACES fit: `x(2.51x + 0.03) / (x(2.43x + 0.59) + 0.14)`.
    AcesFitted,
}

/// Display development parameters.
#[derive(Clone, Debug)]
pub struct DevelopParams {
    /// Added to the file's `BaselineExposure`.
    pub exposure_ev: f32,
    pub white_balance: WhiteBalance,
    /// Chroma scale about Rec.709 luma; 1.0 = none (the X6 file's
    /// ProfileHueSatMap amounts to 0.93).
    pub saturation: f32,
    pub tone: ToneCurve,
}

// ───────── adapters from the DNG reader's types (vr180_fisheye::dng) ─────────

impl From<&vr180_fisheye::dng::GainMap> for GainMapLike {
    fn from(m: &vr180_fisheye::dng::GainMap) -> Self {
        Self {
            top: m.top,
            left: m.left,
            bottom: m.bottom,
            right: m.right,
            plane: m.plane,
            planes: m.planes,
            row_pitch: m.row_pitch,
            col_pitch: m.col_pitch,
            points_v: m.points_v,
            points_h: m.points_h,
            spacing_v: m.spacing_v,
            spacing_h: m.spacing_h,
            origin_v: m.origin_v,
            origin_h: m.origin_h,
            map_planes: m.map_planes,
            gains: m.gains.clone(),
        }
    }
}

impl From<vr180_fisheye::dng::GainMap> for GainMapLike {
    fn from(m: vr180_fisheye::dng::GainMap) -> Self {
        Self::from(&m)
    }
}

impl From<&vr180_fisheye::dng::DngColor> for DngColorLike {
    fn from(c: &vr180_fisheye::dng::DngColor) -> Self {
        Self {
            color_matrix1: c.color_matrix1,
            as_shot_neutral: c.as_shot_neutral,
            baseline_exposure: c.baseline_exposure,
        }
    }
}

impl From<&vr180_fisheye::dng::X6Still> for RawParams {
    fn from(s: &vr180_fisheye::dng::X6Still) -> Self {
        Self {
            width: s.width,
            height: s.height,
            cfa: s.cfa,
            white_level: s.color.white_level as f32,
            black_level: s.color.black_level,
            as_shot_neutral: s.color.as_shot_neutral,
        }
    }
}

/// Convenience: the reader's gain maps as [`GainMapLike`]s.
pub fn gain_maps_from(maps: &[vr180_fisheye::dng::GainMap]) -> Vec<GainMapLike> {
    maps.iter().map(GainMapLike::from).collect()
}

impl Default for DevelopParams {
    fn default() -> Self {
        Self {
            exposure_ev: 0.0,
            white_balance: WhiteBalance::AsShot,
            saturation: 1.0,
            tone: ToneCurve::AcesFitted,
        }
    }
}

// ───────────────────────────── gain map ─────────────────────────────

#[inline(always)]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + t * (b - a)
}

impl GainMapLike {
    fn is_valid(&self) -> bool {
        let n = self.points_v as usize * self.points_h as usize * self.map_planes as usize;
        self.points_v >= 1
            && self.points_h >= 1
            && self.map_planes >= 1
            && self.gains.len() >= n
            && self.bottom > self.top
            && self.right > self.left
            && self.spacing_v.is_finite()
            && self.spacing_h.is_finite()
            && self.origin_v.is_finite()
            && self.origin_h.is_finite()
            && self.gains.iter().all(|g| g.is_finite())
    }

    /// Plane-0 gain of grid point `(i, j)`; 1.0 when out of range (malformed map).
    #[inline]
    fn gain(&self, i: usize, j: usize) -> f32 {
        let idx = (i * self.points_h as usize + j) * self.map_planes.max(1) as usize;
        self.gains.get(idx).copied().unwrap_or(1.0)
    }

    /// Grid index + fraction along one axis for a normalised coordinate,
    /// clamped to the grid (dng_sdk semantics: outside the grid the edge
    /// value holds).
    #[inline]
    fn axis_index(norm: f64, origin: f64, spacing: f64, points: u32) -> (usize, f32) {
        if points < 2 || !(spacing > 0.0) {
            return (0, 0.0);
        }
        let last = (points - 1) as f64;
        let mut f = ((norm - origin) / spacing).clamp(0.0, last);
        if f.is_nan() {
            f = 0.0;
        }
        let i0 = (f.floor() as usize).min(points as usize - 2);
        (i0, (f - i0 as f64) as f32)
    }

    #[inline]
    fn row_index(&self, row: u32) -> (usize, f32) {
        let h = (self.bottom as f64 - self.top as f64).max(1.0);
        Self::axis_index(
            (row as f64 - self.top as f64) / h,
            self.origin_v,
            self.spacing_v,
            self.points_v,
        )
    }

    #[inline]
    fn col_index(&self, col: u32) -> (usize, f32) {
        let w = (self.right as f64 - self.left as f64).max(1.0);
        Self::axis_index(
            (col as f64 - self.left as f64) / w,
            self.origin_h,
            self.spacing_h,
            self.points_h,
        )
    }

    /// Does the opcode's area + pitch select this pixel?
    #[inline]
    fn covers(&self, row: u32, col: u32) -> bool {
        row >= self.top
            && row < self.bottom
            && col >= self.left
            && col < self.right
            && (row - self.top) % self.row_pitch.max(1) == 0
            && (col - self.left) % self.col_pitch.max(1) == 0
    }

    /// Row-interpolated gain line for `row`: `out[j]` for `j in 0..points_h`,
    /// plus one padded copy of the last entry so `out[j0 + 1]` is always valid.
    fn row_table(&self, row: u32, out: &mut Vec<f32>) {
        let (i0, ti) = self.row_index(row);
        let i1 = (i0 + 1).min(self.points_v.max(1) as usize - 1);
        let ph = self.points_h.max(1) as usize;
        out.clear();
        out.reserve(ph + 1);
        for j in 0..ph {
            out.push(lerp(self.gain(i0, j), self.gain(i1, j), ti));
        }
        out.push(out[ph - 1]);
    }
}

/// DNG 1.3 GainMap bilinear interpolation at full-frame pixel `(row, col)`
/// (plane 0 of the map), in the map's normalised coordinates over the opcode
/// area, clamped at the grid edges — `gainmap.py` / dng_sdk semantics. The
/// interpolation is continuous, so it is meaningful at any pixel, not only the
/// ones the opcode's pitch selects.
pub fn gain_at(map: &GainMapLike, row: u32, col: u32) -> f32 {
    let (i0, ti) = map.row_index(row);
    let (j0, tj) = map.col_index(col);
    let i1 = (i0 + 1).min(map.points_v.max(1) as usize - 1);
    let j1 = (j0 + 1).min(map.points_h.max(1) as usize - 1);
    // Rows first, then columns: bit-identical to the row_table fast path.
    let c0 = lerp(map.gain(i0, j0), map.gain(i1, j0), ti);
    let c1 = lerp(map.gain(i0, j1), map.gain(i1, j1), ti);
    lerp(c0, c1, tj)
}

/// The largest gain over all (valid) maps, floored at 1.0 — the `l_max`
/// [`develop_linear`] reports.
pub fn max_gain(gain_maps: &[GainMapLike]) -> f32 {
    gain_maps
        .iter()
        .filter(|m| m.is_valid())
        .flat_map(|m| m.gains.iter().copied())
        .fold(1.0f32, f32::max)
}

// ───────────────────────────── demosaic context ─────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Role {
    R,
    /// A green site; `red_horizontal`: the row neighbours are red (else blue).
    G {
        red_horizontal: bool,
    },
    B,
}

/// Site roles per CFA parity (`(row & 1) * 2 + (col & 1)`), derived from the
/// pattern generically. Panics on anything but a 2×2 Bayer RGGB-family pattern.
fn cfa_roles(cfa: &[u8; 4]) -> [Role; 4] {
    let count = |c: u8| cfa.iter().filter(|&&v| v == c).count();
    let diag = (cfa[0] == 1 && cfa[3] == 1) || (cfa[1] == 1 && cfa[2] == 1);
    assert!(
        count(0) == 1 && count(1) == 2 && count(2) == 1 && diag,
        "raw_develop: unsupported CFA pattern {cfa:?} (need a 2x2 Bayer pattern: one R, one B, G on a diagonal)"
    );
    let mut roles = [Role::R; 4];
    for rp in 0..2 {
        for cp in 0..2 {
            let s = rp * 2 + cp;
            roles[s] = match cfa[s] {
                0 => Role::R,
                2 => Role::B,
                _ => Role::G {
                    red_horizontal: cfa[rp * 2 + (cp ^ 1)] == 0,
                },
            };
        }
    }
    roles
}

/// Per-map column lookup for the half's columns: `j0` grid column + fraction.
struct ColTab {
    j0: Vec<u32>,
    tj: Vec<f32>,
}

#[derive(Clone, Copy)]
struct MapRef {
    idx: usize,
    /// The opcode's area/pitch does not cover the whole parity class, so each
    /// pixel must be tested with `covers`.
    needs_check: bool,
}

/// Everything the band workers need, shared read-only.
struct Ctx<'a> {
    bayer: &'a [u16],
    width: usize,
    height: usize,
    /// First full-frame column of the half and the half's width.
    x0: usize,
    hw: usize,
    roles: [Role; 4],
    cfa: [u8; 4],
    black: [f32; 4],
    inv_range: [f32; 4],
    white: f32,
    maps: Vec<&'a GainMapLike>,
    col_tabs: Vec<ColTab>,
    /// Maps applying to each CFA parity class.
    site_maps: [Vec<MapRef>; 4],
    /// Parity class of the first G site: its maps give the local white level
    /// for clipped pixels.
    g_site: usize,
    /// `as_shot_neutral / max(as_shot_neutral)`.
    neutral_n: [f32; 3],
    /// `65535 / l_max`.
    out_scale: f32,
}

/// Border coordinate: reflect about the edge pixel (`-1 → 1`, `n → n-2`),
/// which keeps the CFA parity — and so the neighbour colours — intact at the
/// edges of the half; a plain clamp would read the wrong colour there. Falls
/// back to a clamp when the image is too small to reflect.
#[inline(always)]
fn reflect(i: isize, n: usize) -> usize {
    let last = n as isize - 1;
    let r = if i < 0 {
        -i
    } else if i > last {
        2 * last - i
    } else {
        i
    };
    r.clamp(0, last.max(0)) as usize
}

/// Per-thread scratch buffers (allocated once per worker thread).
#[derive(Default)]
struct Scratch {
    lin: Vec<f32>,
    sat: Vec<u8>,
    hor: Vec<u8>,
    row_any: Vec<bool>,
    row_tabs: Vec<Vec<f32>>,
}

impl<'a> Ctx<'a> {
    fn new(
        bayer: &'a [u16],
        params: &RawParams,
        gain_maps: &'a [GainMapLike],
        half: Half,
        l_max: f32,
    ) -> Self {
        let width = params.width as usize;
        let height = params.height as usize;
        let (x0, hw) = match half {
            Half::Left => (0, width / 2),
            Half::Right => (width / 2, width - width / 2),
        };
        let roles = cfa_roles(&params.cfa);
        let mut inv_range = [0f32; 4];
        for s in 0..4 {
            inv_range[s] = 1.0 / (params.white_level - params.black_level[s]).max(1e-6);
        }

        let mut maps = Vec::new();
        for m in gain_maps {
            if m.is_valid() {
                maps.push(m);
            } else {
                tracing::warn!(
                    "raw_develop: skipping malformed GainMap opcode ({}x{} grid, {} gains)",
                    m.points_v,
                    m.points_h,
                    m.gains.len()
                );
            }
        }
        let col_tabs = maps
            .iter()
            .map(|m| {
                let mut j0 = Vec::with_capacity(hw);
                let mut tj = Vec::with_capacity(hw);
                for x in 0..hw {
                    let (j, t) = m.col_index((x0 + x) as u32);
                    j0.push(j as u32);
                    tj.push(t);
                }
                ColTab { j0, tj }
            })
            .collect();

        let mut site_maps: [Vec<MapRef>; 4] = Default::default();
        for (idx, m) in maps.iter().enumerate() {
            let (rpitch, cpitch) = (m.row_pitch.max(1), m.col_pitch.max(1));
            for rp in 0..2u32 {
                for cp in 0..2u32 {
                    let row_applies = match rpitch {
                        1 => true,
                        2 => (m.top & 1) == rp,
                        _ => true,
                    };
                    let col_applies = match cpitch {
                        1 => true,
                        2 => (m.left & 1) == cp,
                        _ => true,
                    };
                    if !(row_applies && col_applies) {
                        continue;
                    }
                    let row_full = match rpitch {
                        1 => m.top == 0,
                        2 => m.top < 2,
                        _ => false,
                    } && m.bottom as usize >= height;
                    let col_full = match cpitch {
                        1 => m.left == 0,
                        2 => m.left < 2,
                        _ => false,
                    } && m.right as usize >= width;
                    site_maps[(rp * 2 + cp) as usize].push(MapRef {
                        idx,
                        needs_check: !(row_full && col_full),
                    });
                }
            }
        }

        let g_site = params.cfa.iter().position(|&c| c == 1).unwrap_or(0);
        let n = params.as_shot_neutral;
        let nmax = n.iter().copied().fold(0.0f32, f32::max).max(1e-6);
        let neutral_n = [n[0] / nmax, n[1] / nmax, n[2] / nmax];

        Self {
            bayer,
            width,
            height,
            x0,
            hw,
            roles,
            cfa: params.cfa,
            black: params.black_level,
            inv_range,
            white: params.white_level,
            maps,
            col_tabs,
            site_maps,
            g_site,
            neutral_n,
            out_scale: 65535.0 / l_max.max(1.0),
        }
    }

    fn scratch(&self) -> Scratch {
        Scratch {
            row_tabs: vec![Vec::new(); self.maps.len()],
            ..Default::default()
        }
    }

    /// Local white level for a clipped pixel: the G-site gain(s) evaluated at
    /// `(row, col)` (continuous bilinear, so no CFA checkerboard).
    fn white_gain(&self, row: usize, col: usize) -> f32 {
        let mut g = 1.0f32;
        for m in &self.site_maps[self.g_site] {
            let map = self.maps[m.idx];
            if m.needs_check && !map.covers(row as u32, col as u32) {
                continue;
            }
            g *= gain_at(map, row as u32, col as u32);
        }
        g
    }

    /// Linearise + gain-correct one full-frame row over the half's columns.
    /// `lin[x] = L`, `sat[x] = raw >= white`.
    fn linearise_row(
        &self,
        row: usize,
        row_tabs: &mut [Vec<f32>],
        lin: &mut [f32],
        sat: &mut [u8],
    ) {
        let rp = row & 1;
        for cp in 0..2 {
            for m in &self.site_maps[rp * 2 + cp] {
                self.maps[m.idx].row_table(row as u32, &mut row_tabs[m.idx]);
            }
        }
        let base = row * self.width + self.x0;
        let src = &self.bayer[base..base + self.hw];
        for cp in 0..2 {
            let site = rp * 2 + cp;
            let black = self.black[site];
            let inv = self.inv_range[site];
            let white = self.white;
            let maps = &self.site_maps[site];
            let mut x = (cp + self.x0) & 1;
            while x < self.hw {
                let raw = src[x];
                let rawf = raw as f32;
                let mut v = (rawf - black) * inv;
                for m in maps {
                    if m.needs_check && !self.maps[m.idx].covers(row as u32, (self.x0 + x) as u32) {
                        continue;
                    }
                    let t = &self.col_tabs[m.idx];
                    let rt = &row_tabs[m.idx];
                    let j0 = t.j0[x] as usize;
                    v *= lerp(rt[j0], rt[j0 + 1], t.tj[x]);
                }
                lin[x] = v;
                sat[x] = (rawf >= white) as u8;
                x += 2;
            }
        }
    }

    #[inline(always)]
    fn quantise(&self, v: f32) -> u16 {
        // `as u16` saturates (65535 cap) and maps NaN to 0.
        ((v * self.out_scale).max(0.0) + 0.5) as u16
    }

    /// Full-resolution MHC demosaic of output rows `y0..y1` into `out`
    /// (`(y1 - y0) × hw × 4` u16).
    fn band_mhc(&self, y0: usize, y1: usize, out: &mut [u16], s: &mut Scratch) {
        let hw = self.hw;
        let nrows = y1 - y0 + 4;
        let Scratch {
            lin,
            sat,
            hor,
            row_any,
            row_tabs,
        } = s;
        lin.clear();
        lin.resize(nrows * hw, 0.0);
        sat.clear();
        sat.resize(nrows * hw, 0);
        hor.clear();
        hor.resize(nrows * hw, 0);
        row_any.clear();
        row_any.resize(nrows, false);

        // Linearised rows y0-2 .. y1+2, reflected at the frame edges.
        for k in 0..nrows {
            let src_row = reflect(y0 as isize - 2 + k as isize, self.height);
            let lrow = &mut lin[k * hw..(k + 1) * hw];
            let srow = &mut sat[k * hw..(k + 1) * hw];
            self.linearise_row(src_row, row_tabs, lrow, srow);
            let any = srow.iter().any(|&v| v != 0);
            row_any[k] = any;
            let hrow = &mut hor[k * hw..(k + 1) * hw];
            if any {
                // 5-wide horizontal OR over the same (reflected) support.
                for x in 0..hw {
                    let xi = x as isize;
                    hrow[x] = (-2..=2).fold(0u8, |a, d| a | srow[reflect(xi + d, hw)]);
                }
            } else {
                hrow.fill(0);
            }
        }

        let n = self.neutral_n;
        for y in y0..y1 {
            let k = y - y0 + 2;
            let rows: [&[f32]; 5] = [
                &lin[(k - 2) * hw..(k - 1) * hw],
                &lin[(k - 1) * hw..k * hw],
                &lin[k * hw..(k + 1) * hw],
                &lin[(k + 1) * hw..(k + 2) * hw],
                &lin[(k + 2) * hw..(k + 3) * hw],
            ];
            let hors: [&[u8]; 5] = [
                &hor[(k - 2) * hw..(k - 1) * hw],
                &hor[(k - 1) * hw..k * hw],
                &hor[k * hw..(k + 1) * hw],
                &hor[(k + 1) * hw..(k + 2) * hw],
                &hor[(k + 2) * hw..(k + 3) * hw],
            ];
            let any_sat = row_any[k - 2..=k + 2].iter().any(|&a| a);
            let orow = &mut out[(y - y0) * hw * 4..(y - y0 + 1) * hw * 4];
            let rp = y & 1;
            for cp in 0..2 {
                let role = self.roles[rp * 2 + cp];
                let mut x = (cp + self.x0) & 1;
                while x < hw {
                    let cols: [usize; 5] = if x >= 2 && x + 2 < hw {
                        [x - 2, x - 1, x, x + 1, x + 2]
                    } else {
                        let xi = x as isize;
                        [
                            reflect(xi - 2, hw),
                            reflect(xi - 1, hw),
                            x,
                            reflect(xi + 1, hw),
                            reflect(xi + 2, hw),
                        ]
                    };
                    let (mut r, mut g, mut b) = mhc(&rows, &cols, role);
                    let mut a = 0u16;
                    if any_sat
                        && (hors[0][x] | hors[1][x] | hors[2][x] | hors[3][x] | hors[4][x]) != 0
                    {
                        let wg = self.white_gain(y, self.x0 + x);
                        r = wg * n[0];
                        g = wg * n[1];
                        b = wg * n[2];
                        a = 65535;
                    }
                    let o = &mut orow[x * 4..x * 4 + 4];
                    o[0] = self.quantise(r);
                    o[1] = self.quantise(g);
                    o[2] = self.quantise(b);
                    o[3] = a;
                    x += 2;
                }
            }
        }
    }

    /// 2×2 superpixel binning of output rows `by0..by1` (input rows
    /// `2·by0..2·by1`) into `out` (`(by1 - by0) × (hw/2) × 4` u16).
    fn band_bin2(&self, by0: usize, by1: usize, out: &mut [u16], s: &mut Scratch) {
        let hw = self.hw;
        let ow = hw / 2;
        let Scratch {
            lin, sat, row_tabs, ..
        } = s;
        lin.clear();
        lin.resize(2 * hw, 0.0);
        sat.clear();
        sat.resize(2 * hw, 0);
        let n = self.neutral_n;
        for by in by0..by1 {
            {
                let (l0, l1) = lin.split_at_mut(hw);
                let (s0, s1) = sat.split_at_mut(hw);
                self.linearise_row(2 * by, row_tabs, l0, s0);
                self.linearise_row(2 * by + 1, row_tabs, l1, s1);
            }
            let orow = &mut out[(by - by0) * ow * 4..(by - by0 + 1) * ow * 4];
            for bx in 0..ow {
                let x = 2 * bx;
                let (mut r, mut g, mut b) = (0f32, 0f32, 0f32);
                let mut gn = 0u32;
                let mut g_first: Option<(usize, usize)> = None;
                let mut a = 0u8;
                for q in 0..4usize {
                    let (dy, dx) = (q / 2, q % 2);
                    let row = 2 * by + dy;
                    let col = self.x0 + x + dx;
                    let site = ((row & 1) << 1) | (col & 1);
                    let v = lin[dy * hw + x + dx];
                    a |= sat[dy * hw + x + dx];
                    match self.cfa[site] {
                        0 => r = v,
                        2 => b = v,
                        _ => {
                            g += v;
                            gn += 1;
                            if g_first.is_none() {
                                g_first = Some((row, col));
                            }
                        }
                    }
                }
                if gn > 0 {
                    g /= gn as f32;
                }
                let mut alpha = 0u16;
                if a != 0 {
                    let (row, col) = g_first.unwrap_or((2 * by, self.x0 + x));
                    let wg = self.white_gain(row, col);
                    r = wg * n[0];
                    g = wg * n[1];
                    b = wg * n[2];
                    alpha = 65535;
                }
                let o = &mut orow[bx * 4..bx * 4 + 4];
                o[0] = self.quantise(r);
                o[1] = self.quantise(g);
                o[2] = self.quantise(b);
                o[3] = alpha;
            }
        }
    }
}

/// Malvar-He-Cutler 5×5 linear demosaic at one site. `rows[dy]` are the five
/// linearised rows (centre = 2) and `c[dx]` the five (border-reflected) column indices.
/// Kernels (÷8):
/// - G at R/B: `[0 0 -1 0 0; 0 0 2 0 0; -1 2 4 2 -1; 0 0 2 0 0; 0 0 -1 0 0]`
/// - R/B at G with the wanted colour as ROW neighbours:
///   `[0 0 .5 0 0; 0 -1 0 -1 0; -1 4 5 4 -1; 0 -1 0 -1 0; 0 0 .5 0 0]`, transposed
///   when the wanted colour is above/below
/// - R at B and B at R: `[0 0 -1.5 0 0; 0 2 0 2 0; -1.5 0 6 0 -1.5; 0 2 0 2 0; 0 0 -1.5 0 0]`
#[inline(always)]
fn mhc(rows: &[&[f32]; 5], c: &[usize; 5], role: Role) -> (f32, f32, f32) {
    let p = |dy: usize, dx: usize| rows[dy][c[dx]];
    let ctr = p(2, 2);
    let (n, s, w, e) = (p(1, 2), p(3, 2), p(2, 1), p(2, 3));
    let (nn, ss, ww, ee) = (p(0, 2), p(4, 2), p(2, 0), p(2, 4));
    let diag = p(1, 1) + p(1, 3) + p(3, 1) + p(3, 3);
    let far = nn + ss + ww + ee;
    match role {
        Role::R | Role::B => {
            let g = (4.0 * ctr + 2.0 * (n + s + w + e) - far) * 0.125;
            let x = (6.0 * ctr + 2.0 * diag - 1.5 * far) * 0.125;
            if matches!(role, Role::R) {
                (ctr, g, x)
            } else {
                (x, g, ctr)
            }
        }
        Role::G { red_horizontal } => {
            let hz = (5.0 * ctr + 4.0 * (w + e) - (ww + ee) - diag + 0.5 * (nn + ss)) * 0.125;
            let vt = (5.0 * ctr + 4.0 * (n + s) - (nn + ss) - diag + 0.5 * (ww + ee)) * 0.125;
            if red_horizontal {
                (hz, ctr, vt)
            } else {
                (vt, ctr, hz)
            }
        }
    }
}

// ───────────────────────────── threading ─────────────────────────────

fn worker_threads(bands: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(bands)
        .max(1)
}

/// Split `out` into bands of `band_len` elements and process them on a thread
/// pool with work stealing; each thread gets its own scratch from `new_scratch`.
fn run_bands<T: Send, S>(
    out: &mut [T],
    band_len: usize,
    new_scratch: impl Fn() -> S + Sync,
    work: impl Fn(usize, &mut [T], &mut S) + Sync,
) {
    if out.is_empty() || band_len == 0 {
        return;
    }
    let n_bands = out.len().div_ceil(band_len);
    let threads = worker_threads(n_bands);
    let bands = Mutex::new(out.chunks_mut(band_len).enumerate());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut scratch = new_scratch();
                loop {
                    let next = bands.lock().unwrap_or_else(|e| e.into_inner()).next();
                    match next {
                        Some((i, slice)) => work(i, slice, &mut scratch),
                        None => break,
                    }
                }
            });
        }
    });
}

// ───────────────────────────── stage 1 ─────────────────────────────

const MHC_BAND_ROWS: usize = 128;
const BIN2_BAND_ROWS: usize = 64;

/// Develop one lens half of the mosaic to camera-native linear RGBA64.
///
/// `bayer` is the full frame (`width × height` u16, row-major). Gain maps and
/// the CFA phase are evaluated in full-frame coordinates; the demosaic never
/// reads across the seam into the other half (a different lens) — border taps
/// are reflected about the edge pixel, which keeps the CFA parity intact.
///
/// - `bin2 = false`: Malvar-He-Cutler 5×5 at full resolution (`width/2 × height`).
/// - `bin2 = true`: 2×2 superpixels — R, (G1 + G2)/2, B per block — at half
///   size (`width/4 × height/2`), the cheap preview path.
///
/// Saturation: a raw site with `raw >= white_level` is clipped. A pixel whose
/// demosaic support (5×5, or the 2×2 block) touches a clipped site gets
/// `alpha = 65535` and `rgb = l_local · as_shot_neutral / max(as_shot_neutral)`
/// with `l_local` the G-site gain at that pixel, i.e. the camera-space neutral
/// at the local white level, so clipped highlights stay neutral after white
/// balance instead of turning magenta.
///
/// Storage: `stored = clamp(L · 65535 / l_max)` with `l_max = max_gain(gain_maps)`
/// (≥ 1), so no gain-boosted value clips; `l_max` is returned in the eye.
///
/// Panics if `bayer` is shorter than `width × height` or the CFA is not a 2×2
/// Bayer pattern.
pub fn develop_linear(
    bayer: &[u16],
    params: &RawParams,
    gain_maps: &[GainMapLike],
    half: Half,
    bin2: bool,
) -> LinearEye {
    let width = params.width as usize;
    let height = params.height as usize;
    assert!(
        bayer.len() >= width * height,
        "raw_develop: bayer buffer too short ({} < {}×{})",
        bayer.len(),
        width,
        height
    );
    let l_max = max_gain(gain_maps);
    let ctx = Ctx::new(bayer, params, gain_maps, half, l_max);

    if bin2 {
        let (ow, oh) = (ctx.hw / 2, height / 2);
        let mut out = vec![0u16; ow * oh * 4];
        run_bands(
            &mut out,
            BIN2_BAND_ROWS * ow * 4,
            || ctx.scratch(),
            |band, slice, s| {
                let by0 = band * BIN2_BAND_ROWS;
                let by1 = by0 + slice.len() / (ow * 4);
                ctx.band_bin2(by0, by1, slice, s);
            },
        );
        LinearEye {
            width: ow as u32,
            height: oh as u32,
            rgba: out,
            l_max,
        }
    } else {
        let (ow, oh) = (ctx.hw, height);
        let mut out = vec![0u16; ow * oh * 4];
        run_bands(
            &mut out,
            MHC_BAND_ROWS * ow * 4,
            || ctx.scratch(),
            |band, slice, s| {
                let y0 = band * MHC_BAND_ROWS;
                let y1 = y0 + slice.len() / (ow * 4);
                ctx.band_mhc(y0, y1, slice, s);
            },
        );
        LinearEye {
            width: ow as u32,
            height: oh as u32,
            rgba: out,
            l_max,
        }
    }
}

// ───────────────────────────── stage 2 ─────────────────────────────

type M3 = [[f64; 3]; 3];

const BRADFORD: M3 = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];
const D65_XYZ: [f64; 3] = [0.95047, 1.0, 1.08883];
const XYZ_TO_SRGB: M3 = [
    [3.2406, -1.5372, -0.4986],
    [-0.9689, 1.8758, 0.0415],
    [0.0557, -0.2040, 1.0570],
];

fn mat_mul(a: &M3, b: &M3) -> M3 {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    r
}

fn mat_vec(a: &M3, v: &[f64; 3]) -> [f64; 3] {
    let mut r = [0.0; 3];
    for i in 0..3 {
        r[i] = a[i][0] * v[0] + a[i][1] * v[1] + a[i][2] * v[2];
    }
    r
}

fn mat_inv(a: &M3) -> Option<M3> {
    let c = [
        [
            a[1][1] * a[2][2] - a[1][2] * a[2][1],
            a[0][2] * a[2][1] - a[0][1] * a[2][2],
            a[0][1] * a[1][2] - a[0][2] * a[1][1],
        ],
        [
            a[1][2] * a[2][0] - a[1][0] * a[2][2],
            a[0][0] * a[2][2] - a[0][2] * a[2][0],
            a[0][2] * a[1][0] - a[0][0] * a[1][2],
        ],
        [
            a[1][0] * a[2][1] - a[1][1] * a[2][0],
            a[0][1] * a[2][0] - a[0][0] * a[2][1],
            a[0][0] * a[1][1] - a[0][1] * a[1][0],
        ],
    ];
    let det = a[0][0] * c[0][0] + a[0][1] * c[1][0] + a[0][2] * c[2][0];
    if !(det.abs() > 1e-12) || !det.is_finite() {
        return None;
    }
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = c[i][j] / det;
        }
    }
    Some(r)
}

fn mat_diag(d: &[f64; 3]) -> M3 {
    [[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]]
}

/// Bradford chromatic adaptation `src` white → `dst` white (XYZ, Y = 1).
fn bradford(src: &[f64; 3], dst: &[f64; 3]) -> M3 {
    let cs = mat_vec(&BRADFORD, src);
    let cd = mat_vec(&BRADFORD, dst);
    let scale = [cd[0] / cs[0], cd[1] / cs[1], cd[2] / cs[2]];
    let inv = mat_inv(&BRADFORD).expect("Bradford matrix is invertible");
    mat_mul(&inv, &mat_mul(&mat_diag(&scale), &BRADFORD))
}

fn cm1(color: &DngColorLike) -> M3 {
    let m = &color.color_matrix1;
    [
        [m[0] as f64, m[1] as f64, m[2] as f64],
        [m[3] as f64, m[4] as f64, m[5] as f64],
        [m[6] as f64, m[7] as f64, m[8] as f64],
    ]
}

fn neutral64(color: &DngColorLike) -> [f64; 3] {
    let n = color.as_shot_neutral;
    [n[0] as f64, n[1] as f64, n[2] as f64]
}

/// XYZ of the as-shot neutral through `ColorMatrix1⁻¹` (unnormalised).
fn neutral_xyz(color: &DngColorLike) -> Option<[f64; 3]> {
    let cam2xyz = mat_inv(&cm1(color))?;
    let xyz = mat_vec(&cam2xyz, &neutral64(color));
    (xyz[1] > 0.0 && xyz.iter().all(|v| v.is_finite())).then_some(xyz)
}

/// Chromaticity `(x, y)` of the as-shot neutral under `ColorMatrix1`'s
/// illuminant (sample X6 file: `(0.3435, 0.3695)`). `None` if the matrix is singular.
pub fn as_shot_neutral_xy(color: &DngColorLike) -> Option<[f32; 2]> {
    let xyz = neutral_xyz(color)?;
    let s = xyz[0] + xyz[1] + xyz[2];
    (s > 0.0).then(|| [(xyz[0] / s) as f32, (xyz[1] / s) as f32])
}

/// Camera-native (UN-balanced) linear RGB → linear sRGB (D65), row-major.
///
/// `M = XYZ→sRGB · Bradford(neutral_XYZ → D65) · ColorMatrix1⁻¹ / Y_neutral`,
/// which maps `AsShotNeutral` to `(1, 1, 1)` (`develop.py`'s `m_spec`). With
/// [`WhiteBalance::Multipliers`] `m`, the result is `M · diag(neutral ∘ m)`,
/// i.e. the camera values are multiplied by `m` before the as-shot colour
/// transform (`m = 1 / neutral` reproduces `AsShot`). Falls back to a plain
/// per-channel white balance when `ColorMatrix1` is singular.
pub fn camera_to_srgb_matrix(color: &DngColorLike, wb: WhiteBalance) -> [[f32; 3]; 3] {
    let n = neutral64(color);
    let m_spec = match (mat_inv(&cm1(color)), neutral_xyz(color)) {
        (Some(cam2xyz), Some(xyz_n)) => {
            let xyz_n1 = [xyz_n[0] / xyz_n[1], 1.0, xyz_n[2] / xyz_n[1]];
            let ca = bradford(&xyz_n1, &D65_XYZ);
            let mut m = mat_mul(&XYZ_TO_SRGB, &mat_mul(&ca, &cam2xyz));
            for row in m.iter_mut() {
                for v in row.iter_mut() {
                    *v /= xyz_n[1];
                }
            }
            m
        }
        _ => {
            tracing::warn!("raw_develop: ColorMatrix1 is singular; using a plain white balance");
            mat_diag(&[
                1.0 / n[0].max(1e-6),
                1.0 / n[1].max(1e-6),
                1.0 / n[2].max(1e-6),
            ])
        }
    };
    let m = match wb {
        WhiteBalance::AsShot => m_spec,
        WhiteBalance::Multipliers(mult) => mat_mul(
            &m_spec,
            &mat_diag(&[
                n[0] * mult[0] as f64,
                n[1] * mult[1] as f64,
                n[2] * mult[2] as f64,
            ]),
        ),
    };
    let mut out = [[0f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = m[i][j] as f32;
        }
    }
    out
}

/// Narkowicz' ACES fit, clamped to `0..1` (0.18 → 0.267, 1.0 → 0.804).
#[inline]
pub fn tone_aces(x: f32) -> f32 {
    let x = x.max(0.0);
    ((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)).clamp(0.0, 1.0)
}

/// sRGB OETF on `0..1` (input clamped).
#[inline]
pub fn srgb_oetf(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

const DISPLAY_BAND_ROWS: usize = 64;

/// Develop a [`LinearEye`] to display-referred sRGB RGBA64 (OETF applied,
/// alpha 65535 everywhere).
///
/// Per pixel: camera RGB → (white balance + `ColorMatrix1⁻¹` + Bradford to D65)
/// → linear sRGB → `× 2^(baseline_exposure + exposure_ev) · l_max / 65535`
/// (undoes the storage scaling) → chroma scale about Rec.709 luma → tone curve
/// → sRGB OETF → u16. Pixels flagged saturated (`alpha ≥ 32768`) come out white.
///
/// Panics if `eye.rgba` is shorter than `width × height × 4`.
pub fn develop_display(eye: &LinearEye, color: &DngColorLike, p: &DevelopParams) -> Vec<u16> {
    let w = eye.width as usize;
    let h = eye.height as usize;
    let n = w * h;
    assert!(
        eye.rgba.len() >= n * 4,
        "raw_develop: eye buffer too short ({} < {}×{}×4)",
        eye.rgba.len(),
        w,
        h
    );
    let mut out = vec![0u16; n * 4];
    if n == 0 {
        return out;
    }
    let scale = 2f32.powf(color.baseline_exposure + p.exposure_ev) * eye.l_max.max(1.0) / 65535.0;
    let mut m = camera_to_srgb_matrix(color, p.white_balance);
    for row in m.iter_mut() {
        for v in row.iter_mut() {
            *v *= scale;
        }
    }
    let sat = p.saturation;
    let tone = p.tone;
    let band = DISPLAY_BAND_ROWS * w * 4;
    run_bands(
        &mut out,
        band,
        || (),
        |i, slice, _| {
            let off = i * band;
            let src = &eye.rgba[off..off + slice.len()];
            for (o, px) in slice.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                if px[3] >= 32768 {
                    o.copy_from_slice(&[65535, 65535, 65535, 65535]);
                    continue;
                }
                let (cr, cg, cb) = (px[0] as f32, px[1] as f32, px[2] as f32);
                let mut r = m[0][0] * cr + m[0][1] * cg + m[0][2] * cb;
                let mut g = m[1][0] * cr + m[1][1] * cg + m[1][2] * cb;
                let mut b = m[2][0] * cr + m[2][1] * cg + m[2][2] * cb;
                if sat != 1.0 {
                    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                    r = y + sat * (r - y);
                    g = y + sat * (g - y);
                    b = y + sat * (b - y);
                }
                let curve = |v: f32| match tone {
                    ToneCurve::Linear => v.clamp(0.0, 1.0),
                    ToneCurve::AcesFitted => tone_aces(v),
                };
                o[0] = (srgb_oetf(curve(r)) * 65535.0 + 0.5) as u16;
                o[1] = (srgb_oetf(curve(g)) * 65535.0 + 0.5) as u16;
                o[2] = (srgb_oetf(curve(b)) * 65535.0 + 0.5) as u16;
                o[3] = 65535;
            }
        },
    );
    out
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const GBRG: [u8; 4] = [1, 2, 0, 1];
    const RGGB: [u8; 4] = [0, 1, 1, 2];

    // Sample X6 file (IMG_20261007_133105_00_050.dng) colour tags.
    const SAMPLE_CM1: [f32; 9] = [
        0.71875, -0.1484375, -0.09375, -0.609375, 1.453125, 0.109375, -0.15625, 0.296875, 0.4765625,
    ];
    const SAMPLE_NEUTRAL: [f32; 3] = [0.4599609375, 1.0, 0.537109375];
    const SAMPLE_BASELINE_EV: f32 = 2.600360394;

    const SCRATCH_DNG: &str =
        "/private/tmp/claude-501/-Users-siyangqi-Downloads-vr180-processor-memory/dbf707b1-3979-452f-966e-74fe9e218aeb/scratchpad/dng";
    const SAMPLE_DNG: &str = "/Volumes/Database/261007 3 test/x6/IMG_20261007_133105_00_050.dng";

    fn sample_color() -> DngColorLike {
        DngColorLike {
            color_matrix1: SAMPLE_CM1,
            as_shot_neutral: SAMPLE_NEUTRAL,
            baseline_exposure: SAMPLE_BASELINE_EV,
        }
    }

    fn params(width: u32, height: u32, cfa: [u8; 4], white: f32) -> RawParams {
        RawParams {
            width,
            height,
            cfa,
            white_level: white,
            black_level: [0.0; 4],
            as_shot_neutral: SAMPLE_NEUTRAL,
        }
    }

    /// Mosaic a per-colour constant.
    fn constant_mosaic(w: usize, h: usize, cfa: &[u8; 4], rgb: [u16; 3]) -> Vec<u16> {
        let mut v = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                v[y * w + x] = rgb[cfa[(y & 1) * 2 + (x & 1)] as usize];
            }
        }
        v
    }

    fn uniform_map(
        top: u32,
        left: u32,
        bottom: u32,
        right: u32,
        pitch: u32,
        gain: f32,
    ) -> GainMapLike {
        GainMapLike {
            top,
            left,
            bottom,
            right,
            plane: 0,
            planes: 1,
            row_pitch: pitch,
            col_pitch: pitch,
            points_v: 3,
            points_h: 4,
            spacing_v: 0.5,
            spacing_h: 1.0 / 3.0,
            origin_v: 0.0,
            origin_h: 0.0,
            map_planes: 1,
            gains: vec![gain; 12],
        }
    }

    fn q(l: f32, l_max: f32) -> u16 {
        ((l * 65535.0 / l_max).max(0.0) + 0.5) as u16
    }

    fn check_constant(eye: &LinearEye, expect: [u16; 3], alpha: u16) {
        let first = &eye.rgba[..4];
        for (i, px) in eye.rgba.chunks_exact(4).enumerate() {
            assert_eq!(px, first, "pixel {i} differs from pixel 0 (not constant)");
        }
        for c in 0..3 {
            assert!(
                (first[c] as i32 - expect[c] as i32).abs() <= 1,
                "channel {c}: {} vs expected {}",
                first[c],
                expect[c]
            );
        }
        assert_eq!(first[3], alpha);
    }

    #[test]
    fn constant_mosaic_is_constant_rgb_gbrg_and_rggb() {
        let white = 16383.0;
        let raw = [4000u16, 9000, 2500];
        for cfa in [GBRG, RGGB] {
            // Even half start (Left) and odd half start (Right of an odd-width/2 frame).
            for (w, h, half) in [
                (128usize, 32usize, Half::Left),
                (130, 20, Half::Right),
                (64, 16, Half::Right),
            ] {
                let bayer = constant_mosaic(w, h, &cfa, raw);
                let p = params(w as u32, h as u32, cfa, white);
                let eye = develop_linear(&bayer, &p, &[], half, false);
                let hw = match half {
                    Half::Left => w / 2,
                    Half::Right => w - w / 2,
                };
                assert_eq!((eye.width as usize, eye.height as usize), (hw, h));
                assert_eq!(eye.l_max, 1.0);
                let expect = [
                    q(4000.0 / white, 1.0),
                    q(9000.0 / white, 1.0),
                    q(2500.0 / white, 1.0),
                ];
                check_constant(&eye, expect, 0);
                // bin2 on the same input: identical constants at half size.
                let eye2 = develop_linear(&bayer, &p, &[], half, true);
                assert_eq!((eye2.width as usize, eye2.height as usize), (hw / 2, h / 2));
                check_constant(&eye2, expect, 0);
            }
        }
    }

    #[test]
    fn uniform_gain_map_scales_l_and_l_max_together() {
        // A constant 1.5 gain on every site: L is 1.5× bigger but so is l_max,
        // so the stored u16 is unchanged and l_max reports the headroom.
        let (w, h) = (96usize, 24usize);
        let white = 16383.0;
        let bayer = constant_mosaic(w, h, &GBRG, [4000, 9000, 2500]);
        let p = params(w as u32, h as u32, GBRG, white);
        // Four pitch-2 maps, one per parity (X6 layout).
        let maps: Vec<GainMapLike> = (0..4)
            .map(|s| uniform_map(s / 2, s % 2, h as u32, w as u32, 2, 1.5))
            .collect();
        let eye = develop_linear(&bayer, &p, &maps, Half::Left, false);
        assert!((eye.l_max - 1.5).abs() < 1e-6);
        let expect = [
            q(4000.0 / white, 1.0),
            q(9000.0 / white, 1.0),
            q(2500.0 / white, 1.0),
        ];
        check_constant(&eye, expect, 0);
        // A single pitch-1 map covering everything behaves the same.
        let eye = develop_linear(
            &bayer,
            &p,
            &[uniform_map(0, 0, h as u32, w as u32, 1, 1.5)],
            Half::Right,
            true,
        );
        assert!((eye.l_max - 1.5).abs() < 1e-6);
        check_constant(&eye, expect, 0);
    }

    #[test]
    fn gain_at_matches_hand_bilinear() {
        // Small grid: 2 rows x 3 cols over a 100x300 area, origin 0, spacing 1 / 0.5.
        let map = GainMapLike {
            top: 0,
            left: 0,
            bottom: 100,
            right: 300,
            plane: 0,
            planes: 1,
            row_pitch: 1,
            col_pitch: 1,
            points_v: 2,
            points_h: 3,
            spacing_v: 1.0,
            spacing_h: 0.5,
            origin_v: 0.0,
            origin_h: 0.0,
            map_planes: 1,
            gains: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
        };
        // (row 25, col 75): v = 0.25, u = 0.25 -> fi = 0.25, fj = 0.5
        // rows first: c0 = 1 + 0.25*(4-1) = 1.75, c1 = 2 + 0.25*(5-2) = 2.75 -> 1.75 + 0.5*(1.0) = 2.25
        assert!((gain_at(&map, 25, 75) - 2.25).abs() < 1e-6);
        // Grid corners are exact.
        assert_eq!(gain_at(&map, 0, 0), 1.0);
        assert!((gain_at(&map, 100, 300) - 6.0).abs() < 1e-6);
        // Clamped outside the grid (origin shifted so the first point sits at u = 0.5).
        let shifted = GainMapLike {
            origin_h: 0.5,
            spacing_h: 0.25,
            ..map.clone()
        };
        assert_eq!(gain_at(&shifted, 0, 0), 1.0);
        assert_eq!(gain_at(&shifted, 0, 150), 1.0);
        assert!((gain_at(&shifted, 0, 225) - 2.0).abs() < 1e-6);

        // The X6 layout: 13x34 grid, spacing 1/12, 1/33, R plane (top=1,left=0).
        // Hand-computed from gainmap.py at (1001, 2000): i0=1, j0=4,
        // ti=0.5465910555, tj=0.2525773196, corners 2.2159407, 1.9947460,
        // 1.7767191, 1.6246117 -> 1.9295353.
        let mut gains = vec![1.0f32; 13 * 34];
        gains[1 * 34 + 4] = 2.2159407;
        gains[1 * 34 + 5] = 1.9947460;
        gains[2 * 34 + 4] = 1.7767191;
        gains[2 * 34 + 5] = 1.6246117;
        let x6 = GainMapLike {
            top: 1,
            left: 0,
            bottom: 7760,
            right: 15520,
            plane: 0,
            planes: 1,
            row_pitch: 2,
            col_pitch: 2,
            points_v: 13,
            points_h: 34,
            spacing_v: 1.0 / 12.0,
            spacing_h: 1.0 / 33.0,
            origin_v: 0.0,
            origin_h: 0.0,
            map_planes: 1,
            gains,
        };
        assert!(
            (gain_at(&x6, 1001, 2000) - 1.9295353).abs() < 2e-6,
            "{}",
            gain_at(&x6, 1001, 2000)
        );
    }

    #[test]
    fn gain_map_fast_path_matches_gain_at() {
        // A smoothly varying map applied through develop_linear must equal
        // gain_at() per site, bit for bit (both paths lerp rows then columns).
        let (w, h) = (80usize, 40usize);
        let white = 1000.0;
        let mut gains = Vec::new();
        for i in 0..5 {
            for j in 0..7 {
                gains.push(1.0 + 0.1 * i as f32 + 0.03 * j as f32 + 0.01 * ((i * j) % 3) as f32);
            }
        }
        let maps: Vec<GainMapLike> = (0..4)
            .map(|s| GainMapLike {
                top: s / 2,
                left: s % 2,
                bottom: h as u32,
                right: w as u32,
                plane: 0,
                planes: 1,
                row_pitch: 2,
                col_pitch: 2,
                points_v: 5,
                points_h: 7,
                spacing_v: 0.25,
                spacing_h: 1.0 / 6.0,
                origin_v: 0.0,
                origin_h: 0.0,
                map_planes: 1,
                gains: gains.iter().map(|g| g * (1.0 + 0.05 * s as f32)).collect(),
            })
            .collect();
        // bin2 reads the sites directly (no kernel), so R and B are single sites.
        let bayer: Vec<u16> = (0..w * h).map(|i| 400 + (i % 97) as u16).collect();
        let p = params(w as u32, h as u32, RGGB, white);
        let eye = develop_linear(&bayer, &p, &maps, Half::Right, true);
        let x0 = w / 2;
        for by in 0..eye.height as usize {
            for bx in 0..eye.width as usize {
                let px = &eye.rgba[(by * eye.width as usize + bx) * 4..][..4];
                // RGGB: R at (0,0) of the block, B at (1,1).
                let (rr, rc) = (2 * by, x0 + 2 * bx);
                let (br, bc) = (2 * by + 1, x0 + 2 * bx + 1);
                let r_map = maps
                    .iter()
                    .find(|m| (m.top & 1) as usize == rr & 1 && (m.left & 1) as usize == rc & 1)
                    .unwrap();
                let b_map = maps
                    .iter()
                    .find(|m| (m.top & 1) as usize == br & 1 && (m.left & 1) as usize == bc & 1)
                    .unwrap();
                let lr = bayer[rr * w + rc] as f32 / white * gain_at(r_map, rr as u32, rc as u32);
                let lb = bayer[br * w + bc] as f32 / white * gain_at(b_map, br as u32, bc as u32);
                assert_eq!(px[0], q(lr, eye.l_max), "R at block ({by},{bx})");
                assert_eq!(px[2], q(lb, eye.l_max), "B at block ({by},{bx})");
            }
        }
    }

    /// Bilinear demosaic reference (clamped coordinates), full frame, one half.
    fn bilinear_half(
        bayer: &[u16],
        w: usize,
        h: usize,
        cfa: &[u8; 4],
        x0: usize,
        hw: usize,
    ) -> Vec<[f64; 3]> {
        let at = |y: isize, x: isize| {
            let y = y.clamp(0, h as isize - 1) as usize;
            let x = x.clamp(x0 as isize, (x0 + hw) as isize - 1) as usize;
            (bayer[y * w + x] as f64, cfa[(y & 1) * 2 + (x & 1)])
        };
        let mut out = Vec::with_capacity(hw * h);
        for y in 0..h as isize {
            for x in x0 as isize..(x0 + hw) as isize {
                let (v, c) = at(y, x);
                let mut rgb = [0.0; 3];
                rgb[c as usize] = v;
                let cross = [at(y - 1, x), at(y + 1, x), at(y, x - 1), at(y, x + 1)];
                let diag = [
                    at(y - 1, x - 1),
                    at(y - 1, x + 1),
                    at(y + 1, x - 1),
                    at(y + 1, x + 1),
                ];
                for want in 0..3u8 {
                    if want == c {
                        continue;
                    }
                    let mut sum = 0.0;
                    let mut n = 0;
                    for (pv, pc) in cross.iter().chain(diag.iter()) {
                        if *pc == want {
                            sum += pv;
                            n += 1;
                        }
                    }
                    rgb[want as usize] = sum / n as f64;
                }
                out.push(rgb);
            }
        }
        out
    }

    fn psnr(truth: &[[f64; 3]], test: &[[f64; 3]], hw: usize, h: usize, peak: f64) -> f64 {
        let mut se = 0.0;
        let mut n = 0usize;
        for y in 2..h - 2 {
            for x in 2..hw - 2 {
                let i = y * hw + x;
                for c in 0..3 {
                    let d = truth[i][c] - test[i][c];
                    se += d * d;
                    n += 1;
                }
            }
        }
        10.0 * (peak * peak / (se / n as f64)).log10()
    }

    #[test]
    fn mhc_beats_bilinear_on_gradient_plus_checker() {
        let (w, h) = (256usize, 128usize);
        let hw = w / 2;
        let peak = 60000.0;
        // Ground truth RGB: smooth colour gradients + a luminance checker (period 12).
        let mut truth = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let fx = x as f64 / w as f64;
                let fy = y as f64 / h as f64;
                let checker = if ((x / 6) + (y / 6)) % 2 == 0 {
                    0.22
                } else {
                    -0.22
                };
                let wave = 0.08 * (fx * 40.0).sin() * (fy * 25.0).cos();
                let r = 0.45 + 0.25 * fx + checker + wave;
                let g = 0.5 + 0.2 * fy + checker + wave;
                let b = 0.55 - 0.25 * fx + checker + wave;
                truth.push([r * peak, g * peak, b * peak]);
            }
        }
        let bayer: Vec<u16> = (0..w * h)
            .map(|i| {
                let (y, x) = (i / w, i % w);
                truth[i][GBRG[(y & 1) * 2 + (x & 1)] as usize]
                    .round()
                    .clamp(0.0, 65535.0) as u16
            })
            .collect();
        let p = params(w as u32, h as u32, GBRG, 65535.0);
        let eye = develop_linear(&bayer, &p, &[], Half::Left, false);
        assert_eq!(eye.l_max, 1.0);
        let mhc: Vec<[f64; 3]> = eye
            .rgba
            .chunks_exact(4)
            .map(|p| [p[0] as f64, p[1] as f64, p[2] as f64])
            .collect();
        let bil = bilinear_half(&bayer, w, h, &GBRG, 0, hw);
        let truth_half: Vec<[f64; 3]> = (0..h)
            .flat_map(|y| (0..hw).map(move |x| (y, x)))
            .map(|(y, x)| truth[y * w + x])
            .collect();
        let p_mhc = psnr(&truth_half, &mhc, hw, h, peak);
        let p_bil = psnr(&truth_half, &bil, hw, h, peak);
        eprintln!("PSNR MHC {p_mhc:.2} dB vs bilinear {p_bil:.2} dB");
        assert!(
            p_mhc > p_bil + 1.0,
            "MHC {p_mhc:.2} dB should beat bilinear {p_bil:.2} dB"
        );
    }

    #[test]
    fn bin2_equals_block_means() {
        let (w, h) = (64usize, 32usize);
        let white = 4095.0;
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 20) as u16 // 0..4095
        };
        let bayer: Vec<u16> = (0..w * h).map(|_| rnd().min(4094)).collect();
        for cfa in [GBRG, RGGB] {
            for half in [Half::Left, Half::Right] {
                let p = params(w as u32, h as u32, cfa, white);
                let eye = develop_linear(&bayer, &p, &[], half, true);
                let x0 = match half {
                    Half::Left => 0,
                    Half::Right => w / 2,
                };
                assert_eq!((eye.width, eye.height), (16, 16));
                for by in 0..16usize {
                    for bx in 0..16usize {
                        let (mut r, mut g, mut b) = (0f32, 0f32, 0f32);
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let (row, col) = (2 * by + dy, x0 + 2 * bx + dx);
                                let v = bayer[row * w + col] as f32 / white;
                                match cfa[(row & 1) * 2 + (col & 1)] {
                                    0 => r = v,
                                    2 => b = v,
                                    _ => g += 0.5 * v,
                                }
                            }
                        }
                        let px = &eye.rgba[(by * 16 + bx) * 4..][..4];
                        let expect = [q(r, 1.0), q(g, 1.0), q(b, 1.0)];
                        for c in 0..3 {
                            assert!(
                                (px[c] as i32 - expect[c] as i32).abs() <= 1,
                                "block ({by},{bx}) ch {c}: {} vs {}",
                                px[c],
                                expect[c]
                            );
                        }
                        assert_eq!(px[3], 0);
                    }
                }
            }
        }
    }

    #[test]
    fn saturation_flag_propagates_over_the_support() {
        let (w, h) = (64usize, 32usize);
        let white = 16383.0;
        let mut bayer = constant_mosaic(w, h, &GBRG, [4000, 9000, 2500]);
        // One clipped site in the left half, one at white-1 (not clipped).
        bayer[10 * w + 20] = 16383;
        bayer[20 * w + 5] = 16382;
        let p = params(w as u32, h as u32, GBRG, white);
        let eye = develop_linear(&bayer, &p, &[], Half::Left, false);
        let hw = w / 2;
        let nmax = SAMPLE_NEUTRAL.iter().copied().fold(0.0f32, f32::max);
        for y in 0..h {
            for x in 0..hw {
                let px = &eye.rgba[(y * hw + x) * 4..][..4];
                let inside = (y as i32 - 10).abs() <= 2 && (x as i32 - 20).abs() <= 2;
                assert_eq!(px[3], if inside { 65535 } else { 0 }, "alpha at ({y},{x})");
                if inside {
                    // Neutral white at the local gain (1.0 without maps).
                    for c in 0..3 {
                        assert_eq!(
                            px[c],
                            q(SAMPLE_NEUTRAL[c] / nmax, 1.0),
                            "neutral fill ch {c} at ({y},{x})"
                        );
                    }
                }
            }
        }
        // bin2: only the block containing the clipped site.
        let eye2 = develop_linear(&bayer, &p, &[], Half::Left, true);
        for by in 0..h / 2 {
            for bx in 0..hw / 2 {
                let px = &eye2.rgba[(by * (hw / 2) + bx) * 4..][..4];
                let inside = by == 5 && bx == 10;
                assert_eq!(
                    px[3],
                    if inside { 65535 } else { 0 },
                    "bin2 alpha at block ({by},{bx})"
                );
            }
        }
        // Clipped pixels come out white from develop_display, others do not.
        let disp = develop_display(
            &eye,
            &sample_color(),
            &DevelopParams {
                exposure_ev: -2.0,
                ..Default::default()
            },
        );
        let at = |y: usize, x: usize| &disp[(y * hw + x) * 4..][..4];
        assert_eq!(at(10, 20), &[65535, 65535, 65535, 65535]);
        assert_eq!(at(12, 22), &[65535, 65535, 65535, 65535]);
        assert_ne!(&at(13, 23)[..3], &[65535, 65535, 65535]);
        assert_eq!(at(13, 23)[3], 65535);
    }

    #[test]
    fn colour_matrix_matches_prototype() {
        let color = sample_color();
        let xy = as_shot_neutral_xy(&color).unwrap();
        assert!(
            (xy[0] - 0.3435).abs() < 5e-4 && (xy[1] - 0.3695).abs() < 5e-4,
            "xy = {xy:?}"
        );
        let m = camera_to_srgb_matrix(&color, WhiteBalance::AsShot);
        let expect = [
            [3.711, -0.626, -0.151],
            [-0.276, 1.280, -0.285],
            [0.122, -0.810, 3.265],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (m[i][j] - expect[i][j]).abs() < 2e-3,
                    "m[{i}][{j}] = {} vs {}",
                    m[i][j],
                    expect[i][j]
                );
            }
        }
        // The net matrix maps the as-shot neutral to (1,1,1).
        let n = SAMPLE_NEUTRAL;
        for i in 0..3 {
            let v = m[i][0] * n[0] + m[i][1] * n[1] + m[i][2] * n[2];
            assert!((v - 1.0).abs() < 2e-4, "row {i}: neutral -> {v}");
        }
        // Multipliers(1/neutral) reproduces AsShot.
        let m2 = camera_to_srgb_matrix(
            &color,
            WhiteBalance::Multipliers([1.0 / n[0], 1.0 / n[1], 1.0 / n[2]]),
        );
        for i in 0..3 {
            for j in 0..3 {
                assert!((m[i][j] - m2[i][j]).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn tone_curve_anchors() {
        assert!(
            (tone_aces(0.18) - 0.267).abs() < 1e-3,
            "{}",
            tone_aces(0.18)
        );
        assert!((tone_aces(1.0) - 0.80).abs() < 5e-3, "{}", tone_aces(1.0));
        assert_eq!(tone_aces(0.0), 0.0);
        assert!((srgb_oetf(0.5) - 0.735357).abs() < 1e-5);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-6);
        assert_eq!(srgb_oetf(0.0), 0.0);
    }

    #[test]
    fn develop_display_renders_the_neutral_grey() {
        // Stored camera value = neutral * 2^-baseline * 0.5 (l_max = 1) must land
        // on linear 0.5 grey after the matrix + baseline exposure.
        let color = sample_color();
        let k = 2f32.powf(-SAMPLE_BASELINE_EV) * 0.5 * 65535.0;
        let px = [
            (SAMPLE_NEUTRAL[0] * k + 0.5) as u16,
            (SAMPLE_NEUTRAL[1] * k + 0.5) as u16,
            (SAMPLE_NEUTRAL[2] * k + 0.5) as u16,
            0,
        ];
        let eye = LinearEye {
            width: 3,
            height: 2,
            rgba: px.repeat(6),
            l_max: 1.0,
        };
        let p = DevelopParams {
            exposure_ev: 0.0,
            white_balance: WhiteBalance::AsShot,
            saturation: 0.93,
            tone: ToneCurve::Linear,
        };
        let out = develop_display(&eye, &color, &p);
        let expect = (srgb_oetf(0.5) * 65535.0 + 0.5) as u16;
        for px in out.chunks_exact(4) {
            for c in 0..3 {
                assert!(
                    (px[c] as i32 - expect as i32).abs() <= 8,
                    "ch {c}: {} vs {expect}",
                    px[c]
                );
            }
            // The stored camera values are u16-quantised (R ≈ 2486 LSB here), which
            // spreads the channels by a few LSB after the OETF.
            assert!(
                (px[0] as i32 - px[1] as i32).abs() <= 8
                    && (px[1] as i32 - px[2] as i32).abs() <= 8,
                "not grey: {px:?}"
            );
            assert_eq!(px[3], 65535);
        }
        // +1 EV with the ACES curve: 1.0 -> 0.804 -> OETF.
        let p = DevelopParams {
            exposure_ev: 1.0,
            tone: ToneCurve::AcesFitted,
            ..p
        };
        let out = develop_display(&eye, &color, &p);
        let expect = (srgb_oetf(tone_aces(1.0)) * 65535.0 + 0.5) as u16;
        assert!(
            (out[0] as i32 - expect as i32).abs() <= 8,
            "{} vs {expect}",
            out[0]
        );
        // l_max scales the stored values back up.
        let eye2 = LinearEye {
            l_max: 2.0,
            ..eye.clone()
        };
        let p = DevelopParams {
            exposure_ev: -1.0,
            tone: ToneCurve::Linear,
            ..p
        };
        let out2 = develop_display(&eye2, &color, &p);
        let expect = (srgb_oetf(0.5) * 65535.0 + 0.5) as u16;
        assert!(
            (out2[1] as i32 - expect as i32).abs() <= 8,
            "{} vs {expect}",
            out2[1]
        );
    }

    // ───────── sample-file helpers (skip gracefully when absent) ─────────

    /// Minimal DNG OpcodeList2 reader (big-endian): GainMap (id 9) opcodes only.
    fn parse_opcode_list2(b: &[u8]) -> Vec<GainMapLike> {
        let u32be = |o: usize| u32::from_be_bytes(b[o..o + 4].try_into().unwrap());
        let f64be = |o: usize| f64::from_be_bytes(b[o..o + 8].try_into().unwrap());
        let f32be = |o: usize| f32::from_be_bytes(b[o..o + 4].try_into().unwrap());
        let n = u32be(0) as usize;
        let mut p = 4;
        let mut maps = Vec::new();
        for _ in 0..n {
            let (id, size) = (u32be(p), u32be(p + 12) as usize);
            let body = p + 16;
            p = body + size;
            if id != 9 {
                continue;
            }
            let f = |k: usize| u32be(body + 4 * k);
            let (points_v, points_h, map_planes) = (f(8), f(9), u32be(body + 72));
            let count = (points_v * points_h * map_planes) as usize;
            let gains = (0..count).map(|k| f32be(body + 76 + 4 * k)).collect();
            maps.push(GainMapLike {
                top: f(0),
                left: f(1),
                bottom: f(2),
                right: f(3),
                plane: f(4),
                planes: f(5),
                row_pitch: f(6),
                col_pitch: f(7),
                points_v,
                points_h,
                spacing_v: f64be(body + 40),
                spacing_h: f64be(body + 48),
                origin_v: f64be(body + 56),
                origin_h: f64be(body + 64),
                map_planes,
                gains,
            });
        }
        maps
    }

    fn sample_gain_maps() -> Option<Vec<GainMapLike>> {
        let blob = Path::new(SCRATCH_DNG).join("opcodelist2.bin");
        let bytes = if blob.exists() {
            std::fs::read(&blob).ok()?
        } else if Path::new(SAMPLE_DNG).exists() {
            let out = std::process::Command::new("exiftool")
                .args(["-b", "-OpcodeList2", SAMPLE_DNG])
                .output()
                .ok()?;
            if !out.status.success() || out.stdout.len() < 4 {
                return None;
            }
            out.stdout
        } else {
            return None;
        };
        Some(parse_opcode_list2(&bytes))
    }

    #[test]
    fn sample_opcode_blob_parses_to_four_x6_maps() {
        let Some(maps) = sample_gain_maps() else {
            eprintln!("skipped: no opcodelist2.bin / sample DNG available");
            return;
        };
        assert_eq!(maps.len(), 4);
        let mut parities: Vec<(u32, u32)> = maps.iter().map(|m| (m.top & 1, m.left & 1)).collect();
        parities.sort();
        assert_eq!(parities, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
        for m in &maps {
            assert!(m.is_valid());
            assert_eq!(
                (
                    m.points_v,
                    m.points_h,
                    m.map_planes,
                    m.row_pitch,
                    m.col_pitch
                ),
                (13, 34, 1, 2, 2)
            );
            assert_eq!((m.bottom, m.right), (7760, 15520));
            assert!(
                (m.spacing_v - 1.0 / 12.0).abs() < 1e-12
                    && (m.spacing_h - 1.0 / 33.0).abs() < 1e-12
            );
        }
        assert!(
            (max_gain(&maps) - 2.648239).abs() < 1e-5,
            "{}",
            max_gain(&maps)
        );
        let r = maps
            .iter()
            .find(|m| (m.top & 1, m.left & 1) == (1, 0))
            .unwrap();
        assert!((gain_at(r, 1001, 2000) - 1.9295353).abs() < 2e-6);
        assert!((gain_at(r, 4001, 10002) - 1.2117959).abs() < 2e-6);
        assert!((gain_at(r, 7759, 15518) - 2.647661).abs() < 2e-6);
    }

    /// Full-resolution run on the decoded sample (120 MP): timing + a 1/8-scale
    /// PNG preview. `cargo test -p vr180-pipeline --release --lib raw_develop -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sample_050_left_full_res_preview() {
        let raw_path = Path::new(SCRATCH_DNG).join("050_bayer.raw");
        let (w, h) = (15520usize, 7760usize);
        let Ok(bytes) = std::fs::read(&raw_path) else {
            eprintln!("skipped: {} not available", raw_path.display());
            return;
        };
        if bytes.len() < w * h * 2 {
            eprintln!("skipped: {} is too short", raw_path.display());
            return;
        }
        let Some(maps) = sample_gain_maps() else {
            eprintln!("skipped: no gain-map source available");
            return;
        };
        let bayer: Vec<u16> = bytes
            .chunks_exact(2)
            .take(w * h)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        drop(bytes);
        let p = params(w as u32, h as u32, GBRG, 16383.0);
        let color = sample_color();

        let t0 = std::time::Instant::now();
        let eye = develop_linear(&bayer, &p, &maps, Half::Left, false);
        let t_lin = t0.elapsed();
        let t1 = std::time::Instant::now();
        let disp = develop_display(
            &eye,
            &color,
            &DevelopParams {
                saturation: 1.0,
                ..Default::default()
            },
        );
        let t_disp = t1.elapsed();
        let t2 = std::time::Instant::now();
        let eye_bin = develop_linear(&bayer, &p, &maps, Half::Left, true);
        let t_bin = t2.elapsed();
        assert_eq!((eye_bin.width, eye_bin.height), (eye.width / 2, eye.height / 2));
        assert_eq!(eye_bin.l_max, eye.l_max);
        eprintln!(
            "develop_linear(MHC) {:?}, develop_display {:?}, develop_linear(bin2) {:?} on {} threads; l_max {}",
            t_lin,
            t_disp,
            t_bin,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            eye.l_max
        );
        let clipped = eye.rgba.chunks_exact(4).filter(|p| p[3] != 0).count();
        eprintln!(
            "clipped-support pixels: {clipped} ({:.4}%)",
            100.0 * clipped as f64 / (eye.width as f64 * eye.height as f64)
        );

        // Raw crop of the linear eye (rows 3000..3064, cols 3000..3064, RGBA u16 LE)
        // for an independent numpy check of the gain map + MHC + scaling.
        {
            let ew = eye.width as usize;
            let mut crop = Vec::with_capacity(64 * 64 * 4 * 2);
            for y in 3000..3064 {
                for v in &eye.rgba[(y * ew + 3000) * 4..(y * ew + 3064) * 4] {
                    crop.extend_from_slice(&v.to_le_bytes());
                }
            }
            let crop_path = Path::new(SCRATCH_DNG).join("050_left_rust_crop_3000.bin");
            std::fs::write(&crop_path, &crop).expect("write crop");
            eprintln!(
                "wrote {} (64x64 RGBA u16 LE, l_max {})",
                crop_path.display(),
                eye.l_max
            );
        }

        // 1/8-scale box filter of the display output -> RGB16 PNG.
        let (ew, eh) = (eye.width as usize, eye.height as usize);
        let (pw, ph) = (ew / 8, eh / 8);
        let mut png = vec![0u16; pw * ph * 3];
        for py in 0..ph {
            for px in 0..pw {
                let mut acc = [0u64; 3];
                for dy in 0..8 {
                    for dx in 0..8 {
                        let i = ((py * 8 + dy) * ew + px * 8 + dx) * 4;
                        for c in 0..3 {
                            acc[c] += disp[i + c] as u64;
                        }
                    }
                }
                for c in 0..3 {
                    png[(py * pw + px) * 3 + c] = (acc[c] / 64) as u16;
                }
            }
        }
        let out_path = Path::new(SCRATCH_DNG).join("050_left_rust.png");
        let img = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_raw(
            pw as u32,
            ph as u32,
            png.clone(),
        )
        .unwrap();
        img.save(&out_path).expect("write preview PNG");
        eprintln!("wrote {} ({pw}x{ph})", out_path.display());

        // Compare against the Python prototype's preview when present.
        let py_path = Path::new(SCRATCH_DNG).join("050_left_dev.png");
        if let Ok(py) = image::open(&py_path) {
            let py = py.to_rgb16();
            if (py.width() as usize, py.height() as usize) == (pw, ph) {
                let (mut sum, mut n) = (0f64, 0usize);
                for (a, b) in py.as_raw().iter().zip(png.iter()) {
                    sum += (*a as f64 - *b as f64).abs();
                    n += 1;
                }
                eprintln!(
                    "mean |rust - python| over the preview: {:.1} / 65535",
                    sum / n as f64
                );
            }
        }
        assert!(clipped < (ew * eh) / 10, "implausible clipped fraction");
    }
}
