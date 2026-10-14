#!/usr/bin/env python3
"""Generate the built-in HLG (BT.2020, full range) → Rec.709 / BT.1886 LUT the
app auto-applies to a Kandao QooCam 3 Ultra clip recorded in its HDR mode.

Pipeline per cube node (input = the decoded full-range HLG signal R'G'B'):
  1. HLG inverse OETF (BT.2100) → scene-linear, 1.0 = the HLG nominal peak.
  2. HLG OOTF for a 1000 cd/m² display (system gamma 1.2): display-linear.
  3. BT.2408: HDR reference white (75 % HLG = 203 cd/m²) → SDR 100 % white,
     i.e. ×1000/203; the headroom above (up to 4.9×) through a smooth
     shoulder on luminance that reaches 1.0 asymptotically, hue preserved.
  4. BT.2020 → BT.709 primaries (linear), out-of-gamut desaturated toward
     the pixel's luminance, then clipped.
  5. BT.1886 inverse EOTF (γ 2.4) → the same display-referred Rec.709 the
     other built-in camera LUTs produce.
Writes crates/vr180-gui/assets/kandao_hlg_bt2020_to_rec709.cube (33³).
"""
import numpy as np, os
N = 33
out = os.path.join(os.path.dirname(__file__), '..', 'crates', 'vr180-gui', 'assets', 'kandao_hlg_bt2020_to_rec709.cube')
g = np.linspace(0.0, 1.0, N)
# .cube order: red fastest, then green, then blue.
b, gg, r = np.meshgrid(g, g, g, indexing='ij')
rgb = np.stack([r, gg, b], -1).reshape(-1, 3)

a, bb, c = 0.17883277, 0.28466892, 0.55991073
def hlg_inv_oetf(e):
    return np.where(e <= 0.5, e * e / 3.0, (np.exp((e - c) / a) + bb) / 12.0)
scene = hlg_inv_oetf(np.clip(rgb, 0.0, 1.0))
ys = scene @ np.array([0.2627, 0.6780, 0.0593])
disp = scene * (np.maximum(ys, 1e-12) ** 0.2)[:, None]          # OOTF, γ = 1.2
x = disp * (1000.0 / 203.0)                                       # 203 nit → SDR white
yl = x @ np.array([0.2627, 0.6780, 0.0593])
k = 0.75
def shoulder(y):
    return np.where(y <= k, y, k + (1.0 - k) * (1.0 - np.exp(-(y - k) / (1.0 - k))))
yt = shoulder(yl)
x = x * (yt / np.maximum(yl, 1e-12))[:, None]
m2020_709 = np.array([[1.6605, -0.5876, -0.0728],
                      [-0.1246, 1.1329, -0.0083],
                      [-0.0182, -0.1006, 1.1187]])
x709 = x @ m2020_709.T
y709 = x709 @ np.array([0.2126, 0.7152, 0.0722])
mn = x709.min(1)
t = np.where(mn < 0.0, -mn / np.maximum(y709 - mn, 1e-9), 0.0)   # desaturate until non-negative
x709 = x709 + t[:, None] * (y709[:, None] - x709)
x709 = np.clip(x709, 0.0, 1.0)
outv = x709 ** (1.0 / 2.4)
with open(out, 'w') as f:
    f.write('TITLE "Kandao QooCam 3 Ultra HLG BT.2020 to Rec.709 BT.1886 (VR180 Silver Bullet)"\n')
    f.write(f'LUT_3D_SIZE {N}\nDOMAIN_MIN 0.0 0.0 0.0\nDOMAIN_MAX 1.0 1.0 1.0\n')
    for v in outv:
        f.write(f'{v[0]:.6f} {v[1]:.6f} {v[2]:.6f}\n')
print('wrote', out, outv.shape)
# sanity: 75% grey HLG → ~1.0 SDR; 50% → ?
for e in (0.25, 0.5, 0.75, 1.0):
    s = hlg_inv_oetf(np.array([e])); d = s * s ** 0.2; v = shoulder(d * 1000 / 203); print(f'HLG {e:.2f} -> scene {s[0]:.4f} disp {d[0]:.4f} ({d[0]*1000:.0f} nit) sdr lin {v[0]:.3f} code {v[0]**(1/2.4):.3f}')
