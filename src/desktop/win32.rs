//! Windows display enumeration.
//!
//! # This is the only `unsafe` in the crate
//!
//! Measured 2026-09-16: 42,536 lines under `src/` contained no `unsafe` block
//! at all. Reading the physical geometry of a display cannot be done without
//! Win32, so this file introduces it — and keeps it here. The rules it follows,
//! so the next platform backend follows them too:
//!
//! - Every `unsafe` block carries a SAFETY comment naming the invariant that
//!   makes it sound, not a restatement of what the call does.
//! - Nothing `unsafe` escapes. [`monitors`] returns owned, plain Rust values;
//!   no handle, pointer or lifetime crosses back into [`super`].
//! - A failed OS call is reported, never defaulted. See [`collect_one`] for the
//!   one place that is load-bearing.

use std::ptr;

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

use super::{decode_utf16_nul_padded, scale_from_dpi, DesktopError, MonitorInfo, Rect};

/// What the enumeration callback writes into.
///
/// Carries a failure rather than returning one, because the Win32 callback
/// signature has no room for a `Result` — its only channel is a `BOOL` that
/// means "keep going".
struct Collect {
    monitors: Vec<MonitorInfo>,
    failure: Option<String>,
}

/// Every display attached to this session, in OS enumeration order.
///
/// The caller must have established DPI awareness first; [`super::screen_info`]
/// does. Calling this directly on a virtualised process returns geometry that
/// is wrong by the scaling ratio and says nothing about it.
pub(super) fn monitors() -> Result<Vec<MonitorInfo>, DesktopError> {
    let mut collect = Collect {
        monitors: Vec::new(),
        failure: None,
    };

    // SAFETY: `collect_one` matches MONITORENUMPROC exactly, and the pointer
    // handed to it is a live `&mut Collect` that outlives this call — the
    // enumeration is synchronous, so `collect` cannot move or drop while
    // EnumDisplayMonitors is running. A null HDC asks for the whole virtual
    // desktop and a null clip rect asks for no clipping; neither is
    // dereferenced by us.
    let enumerated = unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(collect_one),
            &mut collect as *mut Collect as LPARAM,
        )
    };

    // Checked BEFORE the return value, because a callback that stopped on an
    // error makes EnumDisplayMonitors report FALSE too, and the callback's
    // message is the one that names what actually went wrong.
    if let Some(failure) = collect.failure {
        return Err(DesktopError::Os(failure));
    }

    if enumerated == 0 {
        return Err(DesktopError::Os(
            "EnumDisplayMonitors failed before reporting any display".to_string(),
        ));
    }

    if collect.monitors.is_empty() {
        // Succeeded and found nothing: real on a session with no attached
        // display. Reported as an error rather than an empty list so a caller
        // cannot compute coordinates on a desktop that does not exist.
        return Err(DesktopError::Os(
            "no displays are attached to this session".to_string(),
        ));
    }

    Ok(collect.monitors)
}

/// Called once per display by `EnumDisplayMonitors`.
///
/// Returns FALSE to stop the enumeration, which is used only to abort on a
/// failed OS call.
///
/// A panic here would abort the process rather than unwind into Win32 (the
/// `extern "system"` ABI aborts on unwind), so the body is kept to an
/// allocation and a few field reads, with no indexing or arithmetic that can
/// panic.
unsafe extern "system" fn collect_one(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Collect` that `monitors` passed in, still
    // live for the duration of the synchronous enumeration, and this callback
    // is the only thing holding it — Win32 calls it on the calling thread, one
    // display at a time, so no aliasing is possible.
    let collect = unsafe { &mut *(data as *mut Collect) };

    let mut info = MONITORINFOEXW::default();
    // Win32 distinguishes MONITORINFO from MONITORINFOEXW by this size alone.
    // Getting it wrong yields the smaller struct with no device name, silently.
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    // SAFETY: `info` is a live, correctly sized MONITORINFOEXW whose cbSize
    // declares the EX layout, and `monitor` is the handle Win32 just gave us.
    // MONITORINFOEXW is `repr(C)` with `monitorInfo` first, so a pointer to
    // that field is a valid `*mut MONITORINFO` with the extra name buffer
    // following it, which is exactly the contract cbSize announces.
    let got_info = unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) };
    if got_info == 0 {
        collect.failure = Some(format!(
            "GetMonitorInfoW failed for display {}",
            collect.monitors.len()
        ));
        return 0;
    }

    let name = decode_utf16_nul_padded(&info.szDevice);

    let mut dpi_x: u32 = 0;
    let mut dpi_y: u32 = 0;
    // SAFETY: both out-pointers address live `u32` locals, and `monitor` is the
    // handle from the enumeration. MDT_EFFECTIVE_DPI is a documented constant
    // of the MONITOR_DPI_TYPE enum.
    let hr = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if hr != 0 {
        // NOT defaulted to 96. A fabricated 100% scale would silently place
        // every later click off by the real scaling ratio, and it would look
        // like the model misreading the screen rather than like this line.
        // Refusing is the only honest option.
        collect.failure = Some(format!(
            "GetDpiForMonitor failed for {name} (HRESULT 0x{hr:08X}); \
             refusing to report a guessed scale factor"
        ));
        return 0;
    }

    // The horizontal DPI is what a scale factor is derived from. They differ
    // only on hardware with non-square pixels, which no desktop display has;
    // noted rather than silently dropped.
    let dpi = dpi_x;
    let _ = dpi_y;

    let rc = info.monitorInfo.rcMonitor;
    let work = info.monitorInfo.rcWork;

    collect.monitors.push(MonitorInfo {
        id: collect.monitors.len() as u32,
        name,
        rect: Rect::from_edges(rc.left, rc.top, rc.right, rc.bottom),
        work_area: Rect::from_edges(work.left, work.top, work.right, work.bottom),
        dpi,
        scale: scale_from_dpi(dpi),
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    });

    1
}

#[cfg(test)]
mod tests {
    use crate::desktop::{screen_info, ScreenInfo};

    fn live() -> ScreenInfo {
        screen_info().expect("a Windows session must be able to describe its own displays")
    }

    /// Runs on any Windows machine. These are the invariants that make the
    /// reading usable at all — if one fails, nothing built on top of it is
    /// worth writing.
    #[test]
    fn the_real_display_is_described_soundly() {
        let screen = live();

        assert!(
            screen.coordinates_are_physical,
            "process is {} — geometry below is virtualised",
            screen.dpi_awareness_summary
        );
        assert!(!screen.monitors.is_empty());
        assert!(
            screen.primary().is_some(),
            "Windows always reports exactly one primary display"
        );
        assert_eq!(
            screen.monitors.iter().filter(|m| m.primary).count(),
            1,
            "more than one primary display is not a state Windows reports"
        );

        for m in &screen.monitors {
            assert!(
                m.rect.width > 0 && m.rect.height > 0,
                "empty display: {m:?}"
            );
            assert!(m.dpi >= 96, "DPI below 96 is not a real setting: {m:?}");
            assert!(m.scale >= 1.0, "scale below 1.0: {m:?}");
            assert!(!m.name.is_empty(), "no device name: {m:?}");
            assert!(
                m.work_area.width <= m.rect.width && m.work_area.height <= m.rect.height,
                "work area cannot exceed the display: {m:?}"
            );
        }

        assert!(screen.virtual_bounds().is_some());
    }

    /// Ids must be usable to index the list they came from, since that is the
    /// only thing they are for.
    #[test]
    fn ids_are_the_enumeration_order() {
        let screen = live();
        for (index, monitor) in screen.monitors.iter().enumerate() {
            assert_eq!(monitor.id as usize, index);
        }
    }

    /// Not an assertion — the operator-facing readout for step 1 of the build
    /// order: "prove the reported geometry matches the real display at the
    /// operator's actual scaling".
    ///
    /// Compare against Settings > System > Display. Run with:
    /// `cargo test --lib desktop::win32::tests::report_real_displays -- --ignored --nocapture`
    #[test]
    #[ignore = "prints the real display table for manual comparison"]
    fn report_real_displays() {
        let screen = live();

        println!("\nDPI awareness : {}", screen.dpi_awareness_summary);
        println!("physical      : {}", screen.coordinates_are_physical);
        println!("mixed scaling : {}", screen.has_mixed_scaling());
        println!("virtual bounds: {:?}\n", screen.virtual_bounds());

        for m in &screen.monitors {
            println!(
                "[{}] {:<16} {:>5}x{:<5} at ({:>6},{:>6})  {:>3} dpi  {:>5.2}x  {}",
                m.id,
                m.name,
                m.rect.width,
                m.rect.height,
                m.rect.x,
                m.rect.y,
                m.dpi,
                m.scale,
                if m.primary { "PRIMARY" } else { "" }
            );
        }
        println!();
    }
}
