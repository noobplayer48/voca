use eframe::egui;
use windows::core::{GUID, IUnknown};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};
use crate::{indicator_hwnd, INDICATOR_WIDTH, INDICATOR_TOP_MARGIN};

pub fn top_center_position(ctx: &egui::Context) -> egui::Pos2 {
    ctx.input(|input| {
        if let Some(monitor_size) = input.viewport().monitor_size {
            let x = ((monitor_size.x - INDICATOR_WIDTH) * 0.5).max(0.0);
            return egui::pos2(x, INDICATOR_TOP_MARGIN);
        }

        if let Some(outer_rect) = input.viewport().outer_rect {
            let x = (outer_rect.center().x - INDICATOR_WIDTH * 0.5).max(0.0);
            return egui::pos2(x, INDICATOR_TOP_MARGIN);
        }

        egui::pos2(500.0, INDICATOR_TOP_MARGIN)
    })
}

pub fn offscreen_position() -> egui::Pos2 {
    egui::pos2(-10_000.0, -10_000.0)
}

pub fn current_virtual_desktop_guid() -> Option<GUID> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\VirtualDesktops")
        .ok()?;
    let desktop_bytes = key.get_raw_value("CurrentVirtualDesktop").ok()?.bytes;
    if desktop_bytes.len() != 16 {
        return None;
    }

    let data1 = u32::from_le_bytes([
        desktop_bytes[0],
        desktop_bytes[1],
        desktop_bytes[2],
        desktop_bytes[3],
    ]);
    let data2 = u16::from_le_bytes([desktop_bytes[4], desktop_bytes[5]]);
    let data3 = u16::from_le_bytes([desktop_bytes[6], desktop_bytes[7]]);
    let data4 = [
        desktop_bytes[8],
        desktop_bytes[9],
        desktop_bytes[10],
        desktop_bytes[11],
        desktop_bytes[12],
        desktop_bytes[13],
        desktop_bytes[14],
        desktop_bytes[15],
    ];

    Some(GUID::from_values(data1, data2, data3, data4))
}

pub fn move_indicator_to_current_virtual_desktop() {
    let Some(hwnd) = indicator_hwnd() else {
        return;
    };
    let Some(current_desktop) = current_virtual_desktop_guid() else {
        return;
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if let Ok(manager) = CoCreateInstance::<_, IVirtualDesktopManager>(
            &VirtualDesktopManager,
            None::<&IUnknown>,
            CLSCTX_ALL,
        ) {
            let _ = manager.MoveWindowToDesktop(hwnd, &current_desktop);
        }
    }
}
