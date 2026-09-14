//! External-display enumeration for the **3D display** output — a
//! side-by-side 3D monitor or AR glasses that the OS sees as one wide screen
//! (typically 3840×1080: the left 1920×1080 is the left eye, the right half
//! the right eye). The output viewport is placed on such a screen, so we
//! need each display's origin and size in the logical points egui / winit
//! position viewports in, plus its pixel size (to render each preview eye
//! at exactly half the screen width).
//!
//! - macOS: CoreGraphics. `CGDisplayBounds` is already in points, in the
//!   same global space winit uses (origin = top-left of the main display,
//!   y down).
//! - Windows: `EnumDisplayMonitors` reports physical pixels. winit converts
//!   a new viewport's logical position / size with the scale of the monitor
//!   the window is created on — the primary — so everything is expressed in
//!   primary-scale points (exact when both screens run at the same scale,
//!   which a 3840×1080 glasses display at 100 % normally does).
//! - elsewhere: nothing is enumerated; the app falls back to a movable
//!   window the user drags onto the 3D screen and fullscreens with `F`.

#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub name: String,
    /// Top-left origin in logical points (egui / winit viewport space).
    pub origin: egui::Pos2,
    /// Size in logical points.
    pub size: egui::Vec2,
    /// Top-left origin in PHYSICAL virtual-desktop pixels. On Windows with
    /// mixed per-monitor DPI the logical `origin` is ambiguous (winit
    /// resolves it against whichever monitor's scale it happens to use at
    /// creation), so cross-monitor placement corrects itself against this
    /// after the window exists. On macOS it equals `origin` (CG points ARE
    /// the placement space) and is unused.
    pub origin_px: egui::Pos2,
    /// Size in physical pixels.
    pub pixel_w: u32,
    pub pixel_h: u32,
    pub is_primary: bool,
}

impl DisplayInfo {
    /// A side-by-side 3D screen presents as a very wide display (32:9 for
    /// 3840×1080). Prefer those when auto-picking the output screen.
    pub fn is_wide_sbs(&self) -> bool {
        self.pixel_h > 0 && (self.pixel_w as f32 / self.pixel_h as f32) >= 3.0
    }

    pub fn label(&self) -> String {
        format!("{}×{}{}", self.pixel_w, self.pixel_h, if self.is_primary { " (main)" } else { "" })
    }
}

/// Every active display, primary first. Empty where enumeration isn't
/// implemented (the caller then uses the windowed fallback).
pub fn all_displays() -> Vec<DisplayInfo> {
    let mut v = platform::all_displays();
    v.sort_by_key(|d| !d.is_primary);
    v
}

/// The display to put the 3D output on: a connected side-by-side 3D screen
/// (3840×1080 exactly, else any other external screen wider than 3:1 such
/// as 5120×1440 / 7680×2160). `None` when no such screen is connected — a
/// normal external monitor does NOT qualify: the caller then opens the
/// movable fallback window instead of taking over someone's desktop.
/// macOS "Displays have separate Spaces" (System Settings ▸ Desktop & Dock),
/// the default. With it ON every display carries its own menu bar and a
/// plain borderless window gets pushed below it, so the 3D output must use
/// native fullscreen there (its own Space on that display only). With it
/// OFF native fullscreen takes over the whole desktop and flips between
/// Spaces, so the output uses a borderless window covering the screen.
/// `false` on other platforms (no such concept).
pub fn displays_have_separate_spaces() -> bool {
    platform::displays_have_separate_spaces()
}

pub fn pick_3d_display(displays: &[DisplayInfo]) -> Option<usize> {
    displays.iter().position(|d| !d.is_primary && d.pixel_w == 3840 && d.pixel_h == 1080)
        .or_else(|| displays.iter().position(|d| !d.is_primary && d.is_wide_sbs()))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::DisplayInfo;
    use core_graphics::display::CGDisplay;

    pub fn all_displays() -> Vec<DisplayInfo> {
        let ids = match CGDisplay::active_displays() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("3D display: CGGetActiveDisplayList failed ({e:?})");
                return Vec::new();
            }
        };
        ids.into_iter().map(|id| {
            let d = CGDisplay::new(id);
            let b = d.bounds();
            // `pixels_wide()` reports the mode's POINT size (1800 on a
            // "more space" MacBook screen); the framebuffer pixel size comes
            // from the display mode — that is what "3840×1080" means for
            // the glasses and what the preview eye width should match.
            let (pw, ph) = d.display_mode()
                .map(|m| (m.pixel_width() as u32, m.pixel_height() as u32))
                .unwrap_or((d.pixels_wide() as u32, d.pixels_high() as u32));
            DisplayInfo {
                name: format!("Display {id}"),
                origin: egui::pos2(b.origin.x as f32, b.origin.y as f32),
                size: egui::vec2(b.size.width as f32, b.size.height as f32),
                origin_px: egui::pos2(b.origin.x as f32, b.origin.y as f32),
                pixel_w: pw,
                pixel_h: ph,
                is_primary: d.is_main(),
            }
        }).collect()
    }

    /// `defaults read com.apple.spaces spans-displays` is 1 when the
    /// displays SHARE one Space (the setting off); missing or 0 → separate
    /// Spaces (the default).
    pub fn displays_have_separate_spaces() -> bool {
        let out = std::process::Command::new("defaults")
            .args(["read", "com.apple.spaces", "spans-displays"])
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() != "1",
            _ => true,
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::DisplayInfo;

    pub fn displays_have_separate_spaces() -> bool { false }
    use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    /// windows-sys 0.59 puts this in `Win32::UI::WindowsAndMessaging`, a
    /// feature we don't otherwise need — it's a stable Win32 `#define` (1),
    /// so declare it locally instead of pulling the whole module in.
    const MONITORINFOF_PRIMARY: u32 = 1;

    struct Raw { rect: RECT, primary: bool, dpi: u32, name: String }

    unsafe extern "system" fn enum_cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `*mut Vec<Raw>` we pass to EnumDisplayMonitors,
        // alive for the duration of that call.
        let out = unsafe { &mut *(data as *mut Vec<Raw>) };
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let ok = unsafe { GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
        if ok != 0 {
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            let n = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            out.push(Raw {
                rect: info.monitorInfo.rcMonitor,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                dpi: dx.max(1),
                name: String::from_utf16_lossy(&info.szDevice[..n]),
            });
        }
        1
    }

    pub fn all_displays() -> Vec<DisplayInfo> {
        let mut raw: Vec<Raw> = Vec::new();
        // SAFETY: plain Win32 enumeration; the callback only touches `raw`.
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(enum_cb),
                                &mut raw as *mut Vec<Raw> as LPARAM);
        }
        // winit sizes/positions a new viewport with the scale of the monitor
        // it is created on (the primary) — see the module docs.
        let primary_scale = raw.iter().find(|r| r.primary)
            .map(|r| r.dpi as f32 / 96.0).unwrap_or(1.0).max(0.25);
        raw.into_iter().map(|r| {
            let w = (r.rect.right - r.rect.left).max(0) as u32;
            let h = (r.rect.bottom - r.rect.top).max(0) as u32;
            DisplayInfo {
                name: r.name,
                origin: egui::pos2(r.rect.left as f32 / primary_scale, r.rect.top as f32 / primary_scale),
                size: egui::vec2(w as f32 / primary_scale, h as f32 / primary_scale),
                origin_px: egui::pos2(r.rect.left as f32, r.rect.top as f32),
                pixel_w: w, pixel_h: h,
                is_primary: r.primary,
            }
        }).collect()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::DisplayInfo;
    pub fn all_displays() -> Vec<DisplayInfo> { Vec::new() }
    pub fn displays_have_separate_spaces() -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_and_prefers_wide_external() {
        let ds = all_displays();
        for d in &ds { eprintln!("{d:?}"); }
        // Wherever enumeration exists there is at least the main display,
        // listed first.
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert!(!ds.is_empty());
            assert!(ds[0].is_primary);
        }
        let mk = |w, h, p| DisplayInfo { name: String::new(), origin: egui::Pos2::ZERO,
            size: egui::vec2(w as f32, h as f32), origin_px: egui::Pos2::ZERO,
            pixel_w: w, pixel_h: h, is_primary: p };
        let fake = vec![mk(3024, 1964, true), mk(2560, 1440, false), mk(7680, 2160, false), mk(3840, 1080, false)];
        assert_eq!(pick_3d_display(&fake), Some(3), "exact 3840×1080 wins");
        assert_eq!(pick_3d_display(&fake[..3]), Some(2), "other 32:9 screens qualify");
        assert_eq!(pick_3d_display(&fake[..2]), None, "a normal external monitor does not");
        assert_eq!(pick_3d_display(&fake[..1]), None);
        assert_eq!(pick_3d_display(&[mk(3840, 1080, true)]), None, "never the main screen");
    }
}
