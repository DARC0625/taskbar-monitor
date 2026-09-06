//! Read-only discovery and client-coordinate layout for the primary taskbar.
//!
//! Visibility is intentionally not part of host validity. A child follows its
//! parent's clipping and visibility, including taskbar auto-hide animations.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, GetDpiForWindow, GetWindowDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClassNameW, GetClientRect, GetWindowThreadProcessId, IsWindow,
};
use windows::core::{PCWSTR, w};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskbarHost {
    pub hwnd: HWND,
    pub process_id: u32,
}

/// Finds the primary Shell_TrayWnd even when the taskbar is currently hidden.
pub unsafe fn find() -> Option<TaskbarHost> {
    unsafe {
        let hwnd = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).ok()?;
        if !IsWindow(Some(hwnd)).as_bool() || !is_taskbar_class(hwnd) {
            return None;
        }
        let process_id = process_id(hwnd);
        (process_id != 0).then_some(TaskbarHost { hwnd, process_id })
    }
}

impl TaskbarHost {
    /// Also checks the process ID: Windows may reuse an HWND after Explorer exits.
    pub unsafe fn is_current(&self) -> bool {
        unsafe {
            self.process_id != 0
                && IsWindow(Some(self.hwnd)).as_bool()
                && process_id(self.hwnd) == self.process_id
                && is_taskbar_class(self.hwnd)
                && FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null())
                    .is_ok_and(|hwnd| hwnd == self.hwnd)
        }
    }

    /// Computes a child rectangle in this taskbar's client coordinate system.
    /// The taskbar need not currently be visible or within the monitor bounds.
    pub unsafe fn bounds(&self, width_dip: i32, height_dip: i32, offset_dip: i32) -> Option<RECT> {
        unsafe {
            if !self.is_current() {
                return None;
            }
            let mut client = RECT::default();
            GetClientRect(self.hwnd, &mut client).ok()?;
            layout_rect(
                client.right.checked_sub(client.left)?,
                client.bottom.checked_sub(client.top)?,
                GetDpiForWindow(self.hwnd),
                width_dip,
                height_dip,
                offset_dip,
            )
        }
    }

    /// Returns a null context if the saved host is no longer current.
    pub unsafe fn dpi_context(&self) -> DPI_AWARENESS_CONTEXT {
        unsafe {
            if self.is_current() {
                GetWindowDpiAwarenessContext(self.hwnd)
            } else {
                DPI_AWARENESS_CONTEXT::default()
            }
        }
    }
}

/// Returns zero for a missing or invalid HWND.
pub unsafe fn process_id(hwnd: HWND) -> u32 {
    unsafe {
        let mut id = 0;
        if GetWindowThreadProcessId(hwnd, Some(&mut id)) == 0 {
            0
        } else {
            id
        }
    }
}

unsafe fn is_taskbar_class(hwnd: HWND) -> bool {
    unsafe {
        let mut name = [0u16; 64];
        let length = GetClassNameW(hwnd, &mut name);
        length > 0
            && name[..length as usize]
                .iter()
                .copied()
                .eq("Shell_TrayWnd".encode_utf16())
    }
}

/// Converts DIPs to client pixels and clamps a horizontally arranged child to
/// the available client area. Negative offsets snap to the left edge; invalid
/// dimensions, unknown DPI, and vertical/square hosts are rejected.
pub fn layout_rect(
    client_width: i32,
    client_height: i32,
    dpi: u32,
    width_dip: i32,
    height_dip: i32,
    offset_dip: i32,
) -> Option<RECT> {
    if client_width <= 0
        || client_height <= 0
        || client_width <= client_height
        || dpi == 0
        || width_dip <= 0
        || height_dip <= 0
    {
        return None;
    }
    // i32-positive DIPs times u32 DPI fit in u64, including the rounding term.
    // Clamp before narrowing so malformed large settings cannot overflow RECT.
    let pixels = |dip: i32| (dip.max(0) as u64 * dpi as u64 + 48) / 96;
    let width = pixels(width_dip).max(1).min(client_width as u64) as i32;
    let height = pixels(height_dip).max(1).min(client_height as u64) as i32;
    let left = pixels(offset_dip).min((client_width - width) as u64) as i32;
    let top = (client_height - height) / 2;
    Some(RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const TEST_DPIS: [u32; 7] = [96, 120, 144, 168, 192, 240, 288];

    #[test]
    fn double_dpi_matches_parent_client_example() {
        assert_eq!(
            layout_rect(3840, 96, 192, 584, 40, 12),
            Some(RECT {
                left: 24,
                top: 8,
                right: 1192,
                bottom: 88
            }),
        );
    }

    #[test]
    fn non_integer_scale_rounds_before_centering() {
        assert_eq!(
            layout_rect(2000, 100, 200, 584, 40, 12),
            Some(RECT {
                left: 25,
                top: 8,
                right: 1242,
                bottom: 91
            }),
        );
    }

    #[test]
    fn offset_and_size_stay_inside_client_area() {
        assert_eq!(
            layout_rect(800, 48, 96, 300, 40, -100),
            Some(RECT {
                left: 0,
                top: 4,
                right: 300,
                bottom: 44
            }),
        );
        assert_eq!(
            layout_rect(800, 48, 96, 300, 40, 900),
            Some(RECT {
                left: 500,
                top: 4,
                right: 800,
                bottom: 44
            }),
        );
        assert_eq!(
            layout_rect(800, 48, 192, 900, 60, 12),
            Some(RECT {
                left: 0,
                top: 0,
                right: 800,
                bottom: 48
            }),
        );
    }

    #[test]
    fn malformed_dimensions_and_vertical_hosts_are_rejected() {
        for (width, height) in [
            (0, 48),
            (-100, 48),
            (800, 0),
            (800, -48),
            (48, 800),
            (48, 48),
        ] {
            assert!(layout_rect(width, height, 96, 300, 40, 12).is_none());
        }
        for (width, height) in [(0, 40), (-10, 40), (300, 0), (300, -10)] {
            assert!(layout_rect(800, 48, 96, width, height, 12).is_none());
        }
        assert!(layout_rect(800, 48, 0, 300, 40, 12).is_none());
    }

    #[test]
    fn extreme_inputs_cannot_overflow_coordinates() {
        assert_eq!(
            layout_rect(i32::MAX, 100, u32::MAX, i32::MAX, i32::MAX, i32::MAX),
            Some(RECT {
                left: 0,
                top: 0,
                right: i32::MAX,
                bottom: 100
            }),
        );
    }

    #[test]
    fn all_configured_columns_fit_parent_across_supported_scale_choices() {
        let offsets = [
            i32::MIN,
            -16001,
            -1,
            0,
            1,
            2,
            3,
            11,
            12,
            13,
            15999,
            16000,
            16001,
            i32::MAX,
        ];
        for dpi in TEST_DPIS {
            let scale = f64::from(dpi) / 96.0;
            let wanted_height = (40.0 * scale).round() as i32;
            for columns in 1..=6 {
                for column_width in 88..=116 {
                    let mut config = Config {
                        column_dip: column_width,
                        visible: [false; 6],
                        ..Config::default()
                    };
                    config.visible[..columns].fill(true);
                    let width_dip = config.width();
                    let wanted_width = (f64::from(width_dip) * scale).round() as i32;
                    for client_height in [1, wanted_height - 1, (48.0 * scale).round() as i32] {
                        for client_width in [
                            client_height + 1,
                            wanted_width - 1,
                            wanted_width,
                            wanted_width + 1,
                            3840,
                            i32::MAX,
                        ] {
                            if client_width <= client_height {
                                continue;
                            }
                            let mut previous_left = 0;
                            for offset in offsets {
                                let rect = layout_rect(
                                    client_width,
                                    client_height,
                                    dpi,
                                    width_dip,
                                    40,
                                    offset,
                                )
                                .unwrap();
                                assert!(
                                    0 <= rect.left
                                        && rect.left < rect.right
                                        && rect.right <= client_width,
                                    "horizontal clipping: dpi={dpi}, columns={columns}, column={column_width}, client={client_width}x{client_height}, offset={offset}, rect={rect:?}"
                                );
                                assert!(
                                    0 <= rect.top
                                        && rect.top < rect.bottom
                                        && rect.bottom <= client_height
                                );
                                assert_eq!(rect.right - rect.left, wanted_width.min(client_width));
                                assert_eq!(
                                    rect.bottom - rect.top,
                                    wanted_height.min(client_height)
                                );
                                // Integer centering can leave the extra pixel below, never above.
                                let lower_margin = client_height - rect.bottom;
                                assert!((0..=1).contains(&(lower_margin - rect.top)));
                                assert!(
                                    rect.left >= previous_left,
                                    "increasing offset moved the widget left"
                                );
                                previous_left = rect.left;
                                let available = client_width - (rect.right - rect.left);
                                let rounded_offset = (f64::from(offset).max(0.0) * scale).round();
                                assert_eq!(
                                    f64::from(rect.left),
                                    rounded_offset.min(f64::from(available))
                                );
                            }
                            // Oversized offsets pin the right edge; they never resize the widget.
                            assert_eq!(
                                previous_left + wanted_width.min(client_width),
                                client_width
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fractional_scaling_has_at_most_half_a_pixel_error_without_accumulation() {
        for dpi in TEST_DPIS {
            let scale = f64::from(dpi) / 96.0;
            for dip in 1..=704 {
                let rect = layout_rect(10000, 2000, dpi, 704, dip, dip).unwrap();
                let exact = f64::from(dip) * scale;
                assert!((f64::from(rect.left) - exact).abs() <= 0.5);
                // Height has enough room here except at the largest 300% sizes.
                if exact <= 2000.0 {
                    assert!((f64::from(rect.bottom - rect.top) - exact).abs() <= 0.5);
                }
                if dip + 96 <= 704 {
                    let next = layout_rect(10000, 2000, dpi, 704, dip + 96, dip + 96).unwrap();
                    assert_eq!(
                        next.left - rect.left,
                        dpi as i32,
                        "one logical inch must add exactly one DPI unit"
                    );
                }
            }
        }
        // Exact half-pixel ties round upward. These cases also catch truncation.
        for (dpi, dip, expected) in [(120, 2, 3), (144, 1, 2), (168, 2, 4), (240, 1, 3)] {
            let rect = layout_rect(10000, 300, dpi, 100, dip, dip).unwrap();
            assert_eq!(rect.left, expected);
            assert_eq!(rect.bottom - rect.top, expected);
        }
    }

    #[test]
    fn extreme_valid_inputs_keep_positive_size_and_stay_inside_client() {
        for dpi in [1, 96, 288, u32::MAX] {
            for (client_width, client_height) in [(2, 1), (i32::MAX, 1), (i32::MAX, i32::MAX - 1)] {
                for width in [1, 704, i32::MAX] {
                    for height in [1, 40, i32::MAX] {
                        let mut previous_left = 0;
                        for offset in [i32::MIN, -1, 0, 1, 16000, i32::MAX] {
                            let rect = layout_rect(
                                client_width,
                                client_height,
                                dpi,
                                width,
                                height,
                                offset,
                            )
                            .unwrap();
                            assert!(rect.left >= previous_left);
                            assert!(
                                0 <= rect.left
                                    && rect.left < rect.right
                                    && rect.right <= client_width
                            );
                            assert!(
                                0 <= rect.top
                                    && rect.top < rect.bottom
                                    && rect.bottom <= client_height
                            );
                            assert!((0..=1).contains(&(client_height - rect.bottom - rect.top)));
                            previous_left = rect.left;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn empty_vertical_and_square_clients_are_rejected_at_every_scale() {
        for dpi in TEST_DPIS {
            for (width, height) in [
                (i32::MIN, 1),
                (0, 48),
                (1920, 0),
                (1920, i32::MIN),
                (1, 1),
                (48, 1920),
                (i32::MAX, i32::MAX),
            ] {
                assert!(layout_rect(width, height, dpi, 584, 40, 12).is_none());
            }
            for (width, height) in [(i32::MIN, 40), (0, 40), (584, i32::MIN), (584, 0)] {
                assert!(layout_rect(3840, 144, dpi, width, height, 12).is_none());
            }
        }
    }
}
