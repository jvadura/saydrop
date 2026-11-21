//! Delivering the text: onto the clipboard, and from there into the focused
//! window by sending the paste keys.
//!
//! Pasting through the clipboard is the one method that works everywhere,
//! from browsers to terminals, and it is instant for long text. It also means
//! the text is still on the clipboard if the paste did not land.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HWND};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, Sleep,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
};

use crate::config::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT};
use crate::util::{from_wide, wide};

const CF_UNICODETEXT: u32 = 13;

const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;

/// Replace the clipboard contents with `text`.
pub fn set_text(owner: HWND, text: &str) -> Result<(), String> {
    let data = wide(text);

    // Another program may hold the clipboard for a moment (clipboard
    // managers do this constantly), so opening it is retried.
    let mut opened = false;
    for _ in 0..25 {
        // SAFETY: plain Win32 call; `owner` is our own window.
        if unsafe { OpenClipboard(owner) } != 0 {
            opened = true;
            break;
        }
        // SAFETY: Sleep has no preconditions.
        unsafe { Sleep(20) };
    }
    if !opened {
        return Err("The clipboard is locked by another program".to_string());
    }

    // SAFETY: the clipboard is open and ours until CloseClipboard. The block
    // is allocated with the exact size, filled and unlocked; ownership passes
    // to the system only if SetClipboardData succeeds.
    let stored = unsafe {
        EmptyClipboard();
        let bytes = data.len() * 2;
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
        let mut stored = false;
        if !handle.is_null() {
            let target = GlobalLock(handle) as *mut u8;
            if target.is_null() {
                GlobalFree(handle);
            } else {
                std::ptr::copy_nonoverlapping(data.as_ptr().cast::<u8>(), target, bytes);
                GlobalUnlock(handle);
                if SetClipboardData(CF_UNICODETEXT, handle).is_null() {
                    GlobalFree(handle);
                } else {
                    stored = true;
                }
            }
        }
        CloseClipboard();
        stored
    };

    if stored {
        Ok(())
    } else {
        Err("Could not put the text on the clipboard".to_string())
    }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: no preconditions.
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

/// Is the user still holding a modifier key? Sending Ctrl+V while Win or
/// Alt is down from the hotkey would trigger a different shortcut.
pub fn modifiers_held() -> bool {
    [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
        .into_iter()
        .any(key_down)
}

/// Keys from the navigation cluster need the "extended" flag, or Windows
/// treats them as their numeric-keypad twins.
fn is_extended(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2D | 0x2E)
}

fn key_event(vk: u16, up: bool) -> INPUT {
    let mut flags = if up { KEYEVENTF_KEYUP } else { 0 };
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                // SAFETY: no preconditions.
                wScan: unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// The key presses and releases that type `combo`.
fn combo_events(combo: Hotkey) -> Vec<INPUT> {
    let mut modifiers = Vec::new();
    if combo.mods & MOD_CONTROL != 0 {
        modifiers.push(VK_CONTROL);
    }
    if combo.mods & MOD_SHIFT != 0 {
        modifiers.push(VK_SHIFT);
    }
    if combo.mods & MOD_ALT != 0 {
        modifiers.push(VK_MENU);
    }
    let key = combo.vk as u16;

    let mut events = Vec::new();
    events.extend(modifiers.iter().map(|&vk| key_event(vk, false)));
    events.push(key_event(key, false));
    events.push(key_event(key, true));
    events.extend(modifiers.iter().rev().map(|&vk| key_event(vk, true)));
    events
}

/// Press `combo` (for example Ctrl+V) in the focused window.
pub fn send_combo(combo: Hotkey) -> bool {
    let events = combo_events(combo);
    // SAFETY: `events` is a valid array of INPUT of the stated length.
    let sent = unsafe {
        SendInput(
            events.len() as u32,
            events.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    sent as usize == events.len()
}

/// Can the focused window be pasted into?
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Ready,
    /// The taskbar, the desktop or nothing at all has focus.
    NoWindow,
    /// The window belongs to a program running as administrator, which
    /// Windows shields from input sent by normal programs.
    Elevated,
}

pub fn paste_target() -> Target {
    // SAFETY: handle and buffer arguments are valid for each call.
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return Target::NoWindow;
        }
        let mut class = [0u16; 128];
        GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
        if is_shell_class(&from_wide(&class)) {
            return Target::NoWindow;
        }
        let mut process_id = 0u32;
        GetWindowThreadProcessId(window, &mut process_id);
        if process_id == GetCurrentProcessId() {
            return Target::NoWindow;
        }
        if process_elevated(process_id) && !token_elevated(GetCurrentProcess()) {
            return Target::Elevated;
        }
        Target::Ready
    }
}

/// Windows of the shell that take focus but have nowhere to paste.
fn is_shell_class(class: &str) -> bool {
    matches!(
        class,
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "NotifyIconOverflowWindow"
            | "TopLevelWindowForOverflowXamlIsland"
            | "Progman"
            | "WorkerW"
    )
}

/// Does process `id` run elevated? A process that cannot even be opened for
/// a query is protected at least as strongly, so it counts.
unsafe fn process_elevated(id: u32) -> bool {
    // SAFETY: the handle is checked and closed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, id);
        if process.is_null() {
            return true;
        }
        let elevated = token_elevated(process);
        CloseHandle(process);
        elevated
    }
}

unsafe fn token_elevated(process: HANDLE) -> bool {
    // SAFETY: the token handle is checked and closed; the out-structure has
    // the size passed.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return true;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(combo: Hotkey) -> Vec<(u16, bool)> {
        combo_events(combo)
            .iter()
            // SAFETY: every event is built as a keyboard event.
            .map(|e| unsafe {
                (
                    e.Anonymous.ki.wVk,
                    e.Anonymous.ki.dwFlags & KEYEVENTF_KEYUP != 0,
                )
            })
            .collect()
    }

    #[test]
    fn ctrl_v_presses_and_releases_in_order() {
        let combo = Hotkey {
            mods: MOD_CONTROL,
            vk: 0x56,
        };
        assert_eq!(
            summary(combo),
            [
                (VK_CONTROL, false),
                (0x56, false),
                (0x56, true),
                (VK_CONTROL, true)
            ]
        );
    }

    #[test]
    fn shift_insert_marks_insert_as_extended() {
        let combo = Hotkey {
            mods: MOD_SHIFT,
            vk: 0x2D,
        };
        let events = combo_events(combo);
        assert_eq!(events.len(), 4);
        // SAFETY: every event is built as a keyboard event.
        unsafe {
            assert_eq!(events[0].Anonymous.ki.dwFlags & KEYEVENTF_EXTENDEDKEY, 0);
            assert_ne!(events[1].Anonymous.ki.dwFlags & KEYEVENTF_EXTENDEDKEY, 0);
        }
    }

    #[test]
    fn shell_windows_are_not_paste_targets() {
        assert!(is_shell_class("Shell_TrayWnd"));
        assert!(!is_shell_class("Notepad"));
    }
}
