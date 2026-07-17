use std::cell::RefCell;
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

/// Simplified OCR state — now only tracks status messages since the overlay is self-contained.
pub struct OcrState {
    pub status_message: Option<(String, Instant)>,
}

impl Default for OcrState {
    fn default() -> Self {
        Self {
            status_message: None,
        }
    }
}

/// Internal data for the native Win32 selection overlay, stored in thread-local.
struct OverlayData {
    width: i32,
    height: i32,
    hdc_normal: HDC,
    hbm_normal: HBITMAP,
    hdc_dark: HDC,
    hbm_dark: HBITMAP,
    hdc_back: HDC,
    hbm_back: HBITMAP,
    is_dragging: bool,
    start: (i32, i32),
    end: (i32, i32),
    selection: Option<(i32, i32, i32, i32)>, // (x, y, width, height) in pixels
}

thread_local! {
    static OVERLAY: RefCell<Option<OverlayData>> = RefCell::new(None);
}

// Win32 message constants (defined here to avoid import issues)
const MY_WM_PAINT: u32 = 0x000F;
const MY_WM_ERASEBKGND: u32 = 0x0014;
const MY_WM_SETCURSOR: u32 = 0x0020;
const MY_WM_KEYDOWN: u32 = 0x0100;
const MY_WM_LBUTTONDOWN: u32 = 0x0201;
const MY_WM_LBUTTONUP: u32 = 0x0202;
const MY_WM_MOUSEMOVE: u32 = 0x0200;
const VK_ESCAPE_CODE: usize = 0x1B;

// These functions aren't exported by the windows crate's current feature set
extern "system" {
    fn SetCapture(hwnd: HWND) -> HWND;
    fn ReleaseCapture() -> BOOL;
    fn SetFocus(hwnd: HWND) -> HWND;
}

/// Captures the screen, shows a native Win32 selection overlay, and returns
/// the cropped selection as an image::RgbaImage.
///
/// This function BLOCKS until the user completes or cancels the selection.
/// It runs entirely on the calling thread with its own Win32 message loop.
pub fn run_ocr_capture() -> Option<image::RgbaImage> {
    unsafe {
        // 1. Get virtual screen metrics (covers all monitors)
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if vw <= 0 || vh <= 0 {
            return None;
        }

        // 2. Capture full virtual screen to a GDI bitmap
        let hdc_screen = GetDC(HWND(0));
        if hdc_screen.0 == 0 {
            return None;
        }

        // -- Original screenshot --
        let hdc_normal = CreateCompatibleDC(hdc_screen);
        let hbm_normal = CreateCompatibleBitmap(hdc_screen, vw, vh);
        if hdc_normal.0 == 0 || hbm_normal.0 == 0 {
            ReleaseDC(HWND(0), hdc_screen);
            return None;
        }
        SelectObject(hdc_normal, hbm_normal);
        StretchBlt(hdc_normal, 0, 0, vw, vh, hdc_screen, vx, vy, vw, vh, SRCCOPY);

        // -- Get raw pixel data for creating a darkened version --
        let mut bmi = make_bmi(vw, vh);
        let buf_size = (vw * vh * 4) as usize;
        let mut pixels = vec![0u8; buf_size];
        GetDIBits(
            hdc_normal,
            hbm_normal,
            0,
            vh as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        // Darken pixels (40% brightness)
        let mut dark_pixels = pixels.clone();
        for chunk in dark_pixels.chunks_exact_mut(4) {
            chunk[0] = (chunk[0] as u16 * 2 / 5) as u8;
            chunk[1] = (chunk[1] as u16 * 2 / 5) as u8;
            chunk[2] = (chunk[2] as u16 * 2 / 5) as u8;
        }

        // -- Create darkened bitmap --
        let hdc_dark = CreateCompatibleDC(hdc_screen);
        let hbm_dark = CreateCompatibleBitmap(hdc_screen, vw, vh);
        if hdc_dark.0 == 0 || hbm_dark.0 == 0 {
            cleanup_dc_bm(hdc_normal, hbm_normal);
            ReleaseDC(HWND(0), hdc_screen);
            return None;
        }
        // SetDIBits requires bitmap NOT to be selected into a DC
        let bmi_dark = make_bmi(vw, vh);
        SetDIBits(
            hdc_dark,
            hbm_dark,
            0,
            vh as u32,
            dark_pixels.as_ptr() as *const _,
            &bmi_dark,
            DIB_RGB_COLORS,
        );
        SelectObject(hdc_dark, hbm_dark);

        // -- Back buffer for flicker-free painting --
        let hdc_back = CreateCompatibleDC(hdc_screen);
        let hbm_back = CreateCompatibleBitmap(hdc_screen, vw, vh);
        if hdc_back.0 == 0 || hbm_back.0 == 0 {
            cleanup_dc_bm(hdc_normal, hbm_normal);
            cleanup_dc_bm(hdc_dark, hbm_dark);
            ReleaseDC(HWND(0), hdc_screen);
            return None;
        }
        SelectObject(hdc_back, hbm_back);

        ReleaseDC(HWND(0), hdc_screen);

        // 3. Store state in thread-local for the WndProc callback
        OVERLAY.with(|cell| {
            *cell.borrow_mut() = Some(OverlayData {
                width: vw,
                height: vh,
                hdc_normal,
                hbm_normal,
                hdc_dark,
                hbm_dark,
                hdc_back,
                hbm_back,
                is_dragging: false,
                start: (0, 0),
                end: (0, 0),
                selection: None,
            });
        });

        // 4. Register window class
        let h_module = GetModuleHandleW(None).ok()?;
        let h_instance = HINSTANCE(h_module.0);

        let class_name: Vec<u16> = "VocaOcrOverlay\0".encode_utf16().collect();
        let cursor = LoadCursorW(HINSTANCE(0), IDC_CROSS).unwrap_or_default();

        let wc = WNDCLASSW {
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: h_instance,
            hCursor: cursor,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            cleanup_all_gdi();
            return None;
        }

        // 5. Create fullscreen borderless topmost window
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(std::ptr::null()),
            WS_POPUP | WS_VISIBLE,
            vx,
            vy,
            vw,
            vh,
            HWND(0),
            HMENU(0),
            h_instance,
            None,
        );

        if hwnd.0 == 0 {
            let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), h_instance);
            cleanup_all_gdi();
            return None;
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);

        // 6. Run message loop until WM_QUIT
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, HWND(0), 0, 0);
            if ret.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 7. Destroy window and unregister class
        let _ = DestroyWindow(hwnd);
        let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), h_instance);

        // 8. Extract cropped selection from the original screenshot
        let result = OVERLAY.with(|cell| {
            let data = cell.borrow();
            if let Some(d) = data.as_ref() {
                if let Some((sx, sy, sw, sh)) = d.selection {
                    return extract_selection(d, sx, sy, sw, sh);
                }
            }
            None
        });

        // 9. Cleanup GDI resources
        cleanup_all_gdi();

        result
    }
}

/// Window procedure for the overlay. Handles painting, mouse input, and keyboard.
unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        MY_WM_ERASEBKGND => {
            // Prevent default background erase (we paint everything ourselves)
            LRESULT(1)
        }

        MY_WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            OVERLAY.with(|cell| {
                let data = cell.borrow();
                if let Some(d) = data.as_ref() {
                    // Draw darkened screenshot to back buffer
                    let _ = BitBlt(d.hdc_back, 0, 0, d.width, d.height, d.hdc_dark, 0, 0, SRCCOPY);

                    // If dragging, show original screenshot in selection area + border
                    if d.is_dragging {
                        let left = d.start.0.min(d.end.0);
                        let top = d.start.1.min(d.end.1);
                        let right = d.start.0.max(d.end.0);
                        let bottom = d.start.1.max(d.end.1);
                        let w = right - left;
                        let h = bottom - top;

                        if w > 0 && h > 0 {
                            // Bright original in selection area
                            let _ = BitBlt(
                                d.hdc_back, left, top, w, h, d.hdc_normal, left, top, SRCCOPY,
                            );

                            // Selection border (cyan-ish color: R=56, G=189, B=248)
                            let pen = CreatePen(PS_SOLID, 2, COLORREF(56 | (189 << 8) | (248 << 16)));
                            let old_pen = SelectObject(d.hdc_back, pen);
                            let null_brush = GetStockObject(NULL_BRUSH);
                            let old_brush = SelectObject(d.hdc_back, null_brush);
                            Rectangle(d.hdc_back, left, top, right, bottom);
                            SelectObject(d.hdc_back, old_pen);
                            SelectObject(d.hdc_back, old_brush);
                            let _ = DeleteObject(pen);
                        }
                    }

                    // Copy back buffer to screen (single blit = no flicker)
                    let _ = BitBlt(hdc, 0, 0, d.width, d.height, d.hdc_back, 0, 0, SRCCOPY);
                }
            });

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        MY_WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            OVERLAY.with(|cell| {
                if let Some(d) = cell.borrow_mut().as_mut() {
                    d.is_dragging = true;
                    d.start = (x, y);
                    d.end = (x, y);
                }
            });
            SetCapture(hwnd);
            LRESULT(0)
        }

        MY_WM_MOUSEMOVE => {
            let should_update = OVERLAY.with(|cell| {
                cell.borrow().as_ref().map_or(false, |d| d.is_dragging)
            });
            if should_update {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                OVERLAY.with(|cell| {
                    if let Some(d) = cell.borrow_mut().as_mut() {
                        d.end = (x, y);
                    }
                });
                let _ = InvalidateRect(hwnd, None, BOOL(0));
            }
            LRESULT(0)
        }

        MY_WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            OVERLAY.with(|cell| {
                if let Some(d) = cell.borrow_mut().as_mut() {
                    d.end = (x, y);
                    d.is_dragging = false;
                    let left = d.start.0.min(d.end.0);
                    let top = d.start.1.min(d.end.1);
                    let w = (d.start.0 - d.end.0).abs();
                    let h = (d.start.1 - d.end.1).abs();
                    if w > 5 && h > 5 {
                        d.selection = Some((left, top, w, h));
                    }
                }
            });
            PostQuitMessage(0);
            LRESULT(0)
        }

        MY_WM_KEYDOWN => {
            if wparam.0 == VK_ESCAPE_CODE {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }

        MY_WM_SETCURSOR => {
            if let Ok(cursor) = LoadCursorW(HINSTANCE(0), IDC_CROSS) {
                SetCursor(cursor);
            }
            LRESULT(1)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Extract the selected region from the original screenshot as an RGBA image.
unsafe fn extract_selection(
    data: &OverlayData,
    sx: i32,
    sy: i32,
    sw: i32,
    sh: i32,
) -> Option<image::RgbaImage> {
    if sw <= 0 || sh <= 0 {
        return None;
    }

    let hdc_screen = GetDC(HWND(0));
    let hdc_crop = CreateCompatibleDC(hdc_screen);
    let hbm_crop = CreateCompatibleBitmap(hdc_screen, sw, sh);
    if hdc_crop.0 == 0 || hbm_crop.0 == 0 {
        ReleaseDC(HWND(0), hdc_screen);
        return None;
    }
    let old_crop = SelectObject(hdc_crop, hbm_crop);

    // Copy the selection area from the original screenshot
    let _ = BitBlt(hdc_crop, 0, 0, sw, sh, data.hdc_normal, sx, sy, SRCCOPY);

    // Extract pixel data
    let mut bmi = make_bmi(sw, sh);
    let buf_size = (sw * sh * 4) as usize;
    let mut pixels = vec![0u8; buf_size];
    GetDIBits(
        hdc_crop,
        hbm_crop,
        0,
        sh as u32,
        Some(pixels.as_mut_ptr() as *mut _),
        &mut bmi,
        DIB_RGB_COLORS,
    );

    // Cleanup crop resources
    SelectObject(hdc_crop, old_crop);
    let _ = DeleteObject(hbm_crop);
    let _ = DeleteDC(hdc_crop);
    ReleaseDC(HWND(0), hdc_screen);

    // Convert BGRA -> RGBA
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
        chunk[3] = 255;
    }

    image::RgbaImage::from_raw(sw as u32, sh as u32, pixels)
}

/// Helper: create a BITMAPINFO for a top-down 32bpp bitmap.
fn make_bmi(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // Negative = top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: Default::default(),
    }
}

/// Helper: delete a DC and its associated bitmap.
unsafe fn cleanup_dc_bm(hdc: HDC, hbm: HBITMAP) {
    let _ = DeleteDC(hdc);
    let _ = DeleteObject(hbm);
}

/// Cleanup all GDI resources from the thread-local overlay state.
fn cleanup_all_gdi() {
    OVERLAY.with(|cell| {
        if let Some(data) = cell.borrow_mut().take() {
            unsafe {
                let _ = DeleteDC(data.hdc_back);
                let _ = DeleteObject(data.hbm_back);
                let _ = DeleteDC(data.hdc_dark);
                let _ = DeleteObject(data.hbm_dark);
                let _ = DeleteDC(data.hdc_normal);
                let _ = DeleteObject(data.hbm_normal);
            }
        }
    });
}
