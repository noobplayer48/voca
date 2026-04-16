use std::sync::mpsc::Sender;
use std::sync::Mutex;
use lazy_static::lazy_static;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use crate::types::TriggerEvent;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F11, VK_F12};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

lazy_static! {
    static ref EVENT_TX: Mutex<Option<Sender<TriggerEvent>>> = Mutex::new(None);
}

static mut GLOBAL_HOOK: Option<HHOOK> = None;

pub fn set_trigger_sender(tx: Sender<TriggerEvent>) {
    if let Ok(mut guard) = EVENT_TX.lock() {
        *guard = Some(tx);
    }
}

unsafe extern "system" fn hook_callback(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode == HC_ACTION as i32 {
        let kb_struct = *(lparam.0 as *const KBDLLHOOKSTRUCT);
        let msg = wparam.0 as u32;

        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            let event = if kb_struct.vkCode == VK_F11.0 as u32 {
                Some(TriggerEvent::Transcribe)
            } else if kb_struct.vkCode == VK_F12.0 as u32 {
                Some(TriggerEvent::Translate)
            } else {
                None
            };

            if let Some(trigger) = event {
                if let Ok(guard) = EVENT_TX.lock() {
                    if let Some(tx) = guard.as_ref() {
                        let _ = tx.send(trigger);
                    }
                }
                return LRESULT(1); // Swallow hotkeys
            }
        }
    }
    
    // Continue down the chain if not intercepted
    let current_hook = GLOBAL_HOOK.unwrap_or(HHOOK(0 as _));
    CallNextHookEx(current_hook, ncode, wparam, lparam)
}

pub fn start_hook_loop() -> windows::core::Result<()> {
    unsafe {
        let h_instance = GetModuleHandleW(None)?;
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_callback), h_instance, 0)?;

        GLOBAL_HOOK = Some(hook);

        let mut msg = MSG::default();
        // Event loop ensures hook isn't torn down immediately
        while GetMessageW(&mut msg, None, 0, 0).into() {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        UnhookWindowsHookEx(hook)?;
        GLOBAL_HOOK = None;
    }
    Ok(())
}
