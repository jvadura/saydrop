//! saydrop: press a hotkey, speak, press it again, and the text is pasted
//! into the window you were working in.
//!
//! The speech recognition is done by a server you choose (any that speaks
//! the OpenAI transcription API); an optional second server cleans the text
//! up. This program is the part in between: hotkeys, the microphone, a small
//! on-screen bar, and delivering the text.
//!
//! One UI thread with one message loop owns everything visible. Network
//! calls run on a worker thread per dictation and report back by posting a
//! message to the hidden main window.

#![windows_subsystem = "windows"]

mod audio;
mod cli;
mod clipboard;
mod config;
mod net;
mod overlay;
mod pipeline;
#[cfg(test)]
mod preview;
mod toast;
mod tray;
mod util;

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Media::Audio::WAVEHDR;
use windows_sys::Win32::Media::MM_WIM_DATA;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, Sleep};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowRect,
    HICON, IDI_APPLICATION, KillTimer, LoadIconW, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK,
    MB_SETFOREGROUND, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SW_SHOWNORMAL,
    SetForegroundWindow, SetTimer, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_HOTKEY, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use config::{Binding, Config};
use overlay::Phase;
use pipeline::Transcript;
use toast::Kind;
use tray::Status;
use util::{APP_NAME, UiCell, log, one_line, tick, wide};

const CLASS_NAME: &str = "saydrop.main";
const MUTEX_NAME: &str = "Local\\saydrop.single-instance";

const WM_TRAY: u32 = WM_APP + 1;
/// Posted by a second copy of the program to the one already running.
const WM_ALREADY_RUNNING: u32 = WM_APP + 2;
/// Worker: speech-to-text is done, cleanup has started. wparam = generation.
const WM_STAGE: u32 = WM_APP + 3;
/// Worker: finished. lparam = `Box<Outcome>`.
const WM_OUTCOME: u32 = WM_APP + 4;
/// Worker: a server check finished. wparam = all good, lparam = `Box<String>`.
const WM_CHECKED: u32 = WM_APP + 5;
/// Worker: the speech server did not answer. lparam = `Box<String>`.
const WM_UNREACHABLE: u32 = WM_APP + 6;

const MOD_NOREPEAT: u32 = 0x4000;
/// Hotkey ids: the keys that start a dictation stay registered; the keys
/// that stop or cancel one are registered only while recording.
const STANDING_IDS: i32 = 100;
const RECORDING_IDS: i32 = 200;

/// Fires when the recording reaches `max_seconds`.
const LIMIT_TIMER: usize = 1;
/// Drives the level meter when a WAV file stands in for the microphone.
const FAKE_LEVEL_TIMER: usize = 2;

/// Shorter recordings are an accidental double press, not speech.
const MIN_RECORDING_MS: u64 = 350;
/// Below this peak the microphone delivered silence: muted or blocked.
const SILENCE_PEAK: f32 = 0.0005;
/// How long to wait for the user to let go of the hotkey before pasting.
const MODIFIER_WAIT_MS: u32 = 3000;

const CMD_TOGGLE: usize = 1;
const CMD_CANCEL: usize = 2;
const CMD_LLM: usize = 3;
const CMD_PASTE: usize = 4;
const CMD_COPY_LAST: usize = 5;
const CMD_COPY_RAW: usize = 6;
const CMD_RETRY: usize = 7;
const CMD_TEST: usize = 8;
const CMD_EDIT: usize = 9;
const CMD_PROMPT: usize = 10;
const CMD_RELOAD: usize = 11;
const CMD_LOG: usize = 12;
const CMD_AUTOSTART: usize = 13;
const CMD_EXIT: usize = 14;
const CMD_MIC_DEFAULT: usize = 100;
/// Microphone `n` in the menu has id `CMD_MIC_FIRST + n`.
const CMD_MIC_FIRST: usize = 101;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Toggle,
    Start,
    Stop,
    Cancel,
}

enum State {
    Idle,
    Recording {
        /// `None` when a WAV file stands in for the microphone.
        recorder: Option<audio::Recorder>,
        started: u64,
    },
    /// A worker thread is producing the text.
    Working,
}

/// The last dictation, for the "Copy last ..." menu items.
struct Last {
    text: String,
    raw: String,
}

/// What a worker thread hands back to the UI thread.
struct Outcome {
    generation: u32,
    result: Result<Transcript, String>,
    limit_reached: bool,
}

struct App {
    hwnd: HWND,
    instance: *mut c_void,
    /// Idle, recording, working: see `status_icon`.
    icons: [HICON; 3],
    config: Config,
    state: State,
    /// Counts dictations; a result from a cancelled one is recognised by
    /// its stale number and dropped.
    generation: u32,
    standing_hotkeys: Vec<(i32, Action)>,
    recording_hotkeys: Vec<(i32, Action)>,
    /// Picks the monitor for the bar and the toast.
    anchor: POINT,
    last: Option<Last>,
    /// Kept in memory only, so a failed transcription can be retried.
    last_audio: Option<Arc<Vec<i16>>>,
    mic_file: Option<Arc<Vec<i16>>>,
    taskbar_created: u32,
}

static APP: UiCell<App> = UiCell::new();

fn main() {
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut mic_file = None;
    match arguments.first().map(String::as_str) {
        None => {}
        Some("--version" | "-V") => {
            cli::print(concat!("saydrop ", env!("CARGO_PKG_VERSION")));
            return;
        }
        Some("--help" | "-h" | "/?") => {
            cli::print(cli::USAGE);
            return;
        }
        Some("--check") => std::process::exit(cli::check()),
        Some("--transcribe") => {
            let Some(file) = arguments.get(1) else {
                cli::print(cli::USAGE);
                std::process::exit(2);
            };
            let as_json = arguments.iter().any(|a| a == "--json");
            std::process::exit(cli::transcribe(file, as_json));
        }
        Some("--mic-file") => {
            let Some(file) = arguments.get(1) else {
                cli::print(cli::USAGE);
                std::process::exit(2);
            };
            match audio::read_wav(std::path::Path::new(file)) {
                Ok(samples) => mic_file = Some(Arc::new(samples)),
                Err(e) => {
                    cli::print(&format!("error: {e}"));
                    std::process::exit(2);
                }
            }
        }
        Some(other) => {
            cli::print(&format!("Unknown option: {other}\n\n{}", cli::USAGE));
            std::process::exit(2);
        }
    }

    // SAFETY: start-up Win32 calls with valid, NUL-terminated arguments.
    unsafe {
        // A second copy would fight the first one for the hotkey. Launching
        // it again (desktop shortcut, Start menu) asks the running copy to
        // say it is there: its only UI may be hidden in the tray overflow.
        let mutex_name = wide(MUTEX_NAME);
        let mutex = CreateMutexW(null(), 0, mutex_name.as_ptr());
        if !mutex.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            let running = FindWindowW(wide(CLASS_NAME).as_ptr(), wide(APP_NAME).as_ptr());
            if !running.is_null() {
                PostMessageW(running, WM_ALREADY_RUNNING, 0, 0);
            }
            return;
        }

        // Physical pixels everywhere, so the bar and the toast are crisp
        // and correctly placed on scaled displays.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance = GetModuleHandleW(null());
        let class_name = wide(CLASS_NAME);
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(main_proc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0
            || !overlay::register_class(instance)
            || !toast::register_class(instance)
        {
            log("startup: could not register window classes");
            return;
        }

        // Never shown; it exists to receive hotkey, tray and audio messages.
        let title = wide(APP_NAME);
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            log("startup: could not create the main window");
            return;
        }

        let icons = [Status::Idle, Status::Recording, Status::Working].map(|status| {
            let icon = tray::create_icon(status);
            if icon.is_null() {
                LoadIconW(null_mut(), IDI_APPLICATION)
            } else {
                icon
            }
        });

        let loaded = config::load();
        APP.set(App {
            hwnd,
            instance,
            icons,
            config: loaded.config,
            state: State::Idle,
            generation: 0,
            standing_hotkeys: Vec::new(),
            recording_hotkeys: Vec::new(),
            anchor: POINT { x: 0, y: 0 },
            last: None,
            last_audio: None,
            mic_file,
            // Explorer broadcasts this when it (re)starts; the icon has to
            // be added again or the app becomes unreachable.
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
        });

        add_tray_icon();
        let failed = register_standing_hotkeys();
        report_startup(&failed, &loaded.warnings, loaded.first_run);
        if loaded.first_run {
            probe_speech_server();
        }

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

// ---- Tray icon and messages ------------------------------------------------

fn tip() -> String {
    match APP.get() {
        Some(app) => format!(
            "{APP_NAME} \u{2014} {} to dictate",
            app.config.main_hotkey()
        ),
        None => APP_NAME.to_string(),
    }
}

fn add_tray_icon() {
    let text = tip();
    if let Some(app) = APP.get() {
        let icon = app.icons[status_index(current_status(&app.state))];
        if !tray::add(app.hwnd, icon, WM_TRAY, &text) {
            log("tray: could not add the icon");
        }
    }
}

fn current_status(state: &State) -> Status {
    match state {
        State::Idle => Status::Idle,
        State::Recording { .. } => Status::Recording,
        State::Working => Status::Working,
    }
}

fn status_index(status: Status) -> usize {
    match status {
        Status::Idle => 0,
        Status::Recording => 1,
        Status::Working => 2,
    }
}

fn set_status(status: Status) {
    if let Some(app) = APP.get() {
        tray::set_icon(app.hwnd, app.icons[status_index(status)]);
    }
}

/// The centre of the window the user is working in, falling back to the
/// mouse pointer. The bar and the toast appear on that monitor.
fn anchor_point() -> POINT {
    // SAFETY: valid out-pointers.
    unsafe {
        let window = GetForegroundWindow();
        let mut rect: RECT = std::mem::zeroed();
        if !window.is_null() && GetWindowRect(window, &mut rect) != 0 {
            return POINT {
                x: (rect.left + rect.right) / 2,
                y: (rect.top + rect.bottom) / 2,
            };
        }
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        cursor
    }
}

/// Show a toast. Our own window rather than a Windows notification, which
/// Do Not Disturb would swallow.
fn notify(kind: Kind, title: &str, detail: &str) {
    let Some(app) = APP.get() else { return };
    let instance = app.instance;
    toast::show(
        instance,
        &toast::Message {
            kind,
            title,
            detail,
            anchor: anchor_point(),
        },
    );
}

fn report_startup(failed_hotkeys: &[String], warnings: &[String], first_run: bool) {
    let Some(app) = APP.get() else { return };
    let hwnd = app.hwnd;
    let hotkey = app.config.main_hotkey().to_string();
    for warning in warnings {
        log(&format!("config: {warning}"));
    }
    if !failed_hotkeys.is_empty() {
        let keys = failed_hotkeys.join(", ");
        log(&format!("hotkey: {keys} could not be registered"));
        tray::balloon(
            hwnd,
            "saydrop: hotkey unavailable",
            &format!(
                "{keys} is already used by another program. Right-click the tray icon \u{2192} Edit config to choose another."
            ),
            true,
        );
    } else if let Some(first) = warnings.first() {
        tray::balloon(hwnd, "saydrop: config problem", first, true);
    } else if first_run {
        tray::balloon(
            hwnd,
            "saydrop is running",
            &format!("Press {hotkey}, speak, and press {hotkey} again."),
            false,
        );
    }
}

/// Tell the user the app is alive and how to use it.
fn announce_running() {
    let Some(app) = APP.get() else { return };
    let detail = format!(
        "Press {} to dictate, or left-click the tray icon.",
        app.config.main_hotkey()
    );
    notify(Kind::Ok, "saydrop is already running", &detail);
}

// ---- Hotkeys ---------------------------------------------------------------

/// Register `groups` of bindings with ids counting up from `first_id`.
/// Returns what was registered and the hotkeys that are already taken.
fn register(
    hwnd: HWND,
    first_id: i32,
    groups: &[(&[Binding], Action)],
) -> (Vec<(i32, Action)>, Vec<String>) {
    let mut registered = Vec::new();
    let mut failed = Vec::new();
    let mut id = first_id;
    for (bindings, action) in groups {
        for binding in *bindings {
            // SAFETY: our own window handle and id.
            let ok = unsafe {
                RegisterHotKey(hwnd, id, binding.key.mods | MOD_NOREPEAT, binding.key.vk)
            } != 0;
            if ok {
                registered.push((id, *action));
            } else {
                failed.push(binding.text.clone());
            }
            id += 1;
        }
    }
    (registered, failed)
}

fn unregister(hwnd: HWND, hotkeys: &mut Vec<(i32, Action)>) {
    for (id, _) in hotkeys.drain(..) {
        // SAFETY: our own window handle and id.
        unsafe { UnregisterHotKey(hwnd, id) };
    }
}

/// (Re)register the keys that start a dictation. Returns the ones another
/// program already owns.
fn register_standing_hotkeys() -> Vec<String> {
    let Some(app) = APP.get() else {
        return Vec::new();
    };
    unregister(app.hwnd, &mut app.standing_hotkeys);
    let (registered, failed) = register(
        app.hwnd,
        STANDING_IDS,
        &[
            (&app.config.toggle, Action::Toggle),
            (&app.config.start, Action::Start),
        ],
    );
    app.standing_hotkeys = registered;
    failed
}

fn on_hotkey(id: i32) {
    let Some(app) = APP.get() else { return };
    let action = app
        .standing_hotkeys
        .iter()
        .chain(&app.recording_hotkeys)
        .find(|(registered, _)| *registered == id)
        .map(|(_, action)| *action);
    let recording = matches!(app.state, State::Recording { .. });
    match action {
        Some(Action::Toggle) => toggle(),
        Some(Action::Start) if !recording => start_recording(),
        Some(Action::Stop) if recording => stop_recording(false),
        Some(Action::Cancel) => cancel(),
        _ => {}
    }
}

// ---- Recording -------------------------------------------------------------

fn toggle() {
    let Some(app) = APP.get() else { return };
    match app.state {
        State::Idle => start_recording(),
        State::Recording { .. } => stop_recording(false),
        // One dictation at a time; the bar shows that work is in progress.
        State::Working => {}
    }
}

fn start_recording() {
    let Some(app) = APP.get() else { return };
    if !matches!(app.state, State::Idle) {
        return;
    }
    toast::dismiss();
    let (hwnd, instance) = (app.hwnd, app.instance);
    let anchor = anchor_point();

    let recorder = if app.mic_file.is_some() {
        None
    } else {
        match audio::Recorder::start(hwnd, &app.config.microphone) {
            Ok((recorder, warning)) => {
                if let Some(warning) = warning {
                    log(&format!("audio: {warning}"));
                }
                Some(recorder)
            }
            Err(error) => {
                log(&format!("audio: {error}"));
                notify(Kind::Error, "Cannot record", &error);
                return;
            }
        }
    };

    let max_seconds = app.config.max_seconds;
    app.anchor = anchor;
    app.state = State::Recording {
        recorder,
        started: tick(),
    };
    let (registered, failed) = register(
        hwnd,
        RECORDING_IDS,
        &[
            (&app.config.stop, Action::Stop),
            (&app.config.cancel, Action::Cancel),
        ],
    );
    app.recording_hotkeys = registered;
    if !failed.is_empty() {
        log(&format!(
            "hotkey: {} could not be registered for this recording",
            failed.join(", ")
        ));
    }
    let fake = app.mic_file.is_some();

    set_status(Status::Recording);
    overlay::show(instance, hwnd, anchor, Phase::Recording, max_seconds);
    // SAFETY: our own window handle.
    unsafe {
        if max_seconds > 0 {
            SetTimer(hwnd, LIMIT_TIMER, max_seconds.saturating_mul(1000), None);
        }
        if fake {
            SetTimer(hwnd, FAKE_LEVEL_TIMER, 50, None);
        }
    }
}

/// Leave the recording state: release its hotkeys and timers and hand back
/// the recorder and the start time. `None` when not recording.
fn end_recording() -> Option<(Option<audio::Recorder>, u64)> {
    let app = APP.get()?;
    if !matches!(app.state, State::Recording { .. }) {
        return None;
    }
    let State::Recording { recorder, started } = std::mem::replace(&mut app.state, State::Idle)
    else {
        return None;
    };
    unregister(app.hwnd, &mut app.recording_hotkeys);
    // SAFETY: our own window handle.
    unsafe {
        KillTimer(app.hwnd, LIMIT_TIMER);
        KillTimer(app.hwnd, FAKE_LEVEL_TIMER);
    }
    Some((recorder, started))
}

fn back_to_idle() {
    overlay::hide();
    set_status(Status::Idle);
}

fn stop_recording(limit_reached: bool) {
    let Some((recorder, started)) = end_recording() else {
        return;
    };
    let elapsed = tick().saturating_sub(started);
    let samples = match recorder {
        Some(recorder) => recorder.finish(),
        None => APP
            .get()
            .and_then(|app| app.mic_file.as_ref())
            .map(|file| file.to_vec())
            .unwrap_or_default(),
    };

    if elapsed < MIN_RECORDING_MS || samples.is_empty() {
        back_to_idle();
        notify(Kind::Warning, "Too short", "Nothing was sent.");
        return;
    }
    if audio::peak(&samples) < SILENCE_PEAK {
        back_to_idle();
        log("audio: the recording was silent");
        notify(
            Kind::Error,
            "No sound from the microphone",
            "Check that it is not muted and that Windows lets desktop apps use it.",
        );
        return;
    }
    process(Arc::new(samples), limit_reached);
}

fn cancel() {
    let Some(app) = APP.get() else { return };
    match app.state {
        State::Idle => {}
        State::Recording { .. } => {
            // Dropping the recorder closes the microphone.
            end_recording();
            back_to_idle();
        }
        State::Working => {
            // The worker cannot be interrupted; its result is ignored.
            app.generation = app.generation.wrapping_add(1);
            app.state = State::Idle;
            back_to_idle();
        }
    }
}

/// Feed the level meter from the stand-in WAV file.
fn fake_level() {
    let Some(app) = APP.get() else { return };
    let (State::Recording { started, .. }, Some(file)) = (&app.state, &app.mic_file) else {
        return;
    };
    let position = (tick().saturating_sub(*started) as usize) * audio::SAMPLE_RATE as usize / 1000;
    let end = position.min(file.len());
    let start = end.saturating_sub(800);
    overlay::push_level(audio::peak(&file[start..end]));
}

// ---- From recording to text ------------------------------------------------

/// Hand `samples` to a worker thread for transcription.
fn process(samples: Arc<Vec<i16>>, limit_reached: bool) {
    let Some(app) = APP.get() else { return };
    app.generation = app.generation.wrapping_add(1);
    app.state = State::Working;
    app.last_audio = Some(samples.clone());
    let generation = app.generation;
    let config = app.config.clone();
    let (hwnd, instance, anchor) = (app.hwnd, app.instance, app.anchor);

    set_status(Status::Working);
    if overlay::is_visible() {
        overlay::set_phase(Phase::Transcribing);
    } else {
        overlay::show(instance, hwnd, anchor, Phase::Transcribing, 0);
    }

    // A window handle is just a number to other threads; posting to it is
    // the one thing they may do with it.
    let target = hwnd as isize;
    std::thread::spawn(move || {
        // A panic here must still produce an outcome, or the app would stay
        // in its "working" state for good.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let wav = audio::wav_bytes(&samples, audio::SAMPLE_RATE);
            pipeline::run(&config, &wav, config.llm, || {
                // SAFETY: posting to a window is allowed from any thread.
                unsafe { PostMessageW(target as HWND, WM_STAGE, generation as usize, 0) };
            })
        }))
        .unwrap_or_else(|_| Err("Internal error; see the log".to_string()));
        if config.paste {
            wait_for_modifiers();
        }
        let outcome = Box::into_raw(Box::new(Outcome {
            generation,
            result,
            limit_reached,
        }));
        // SAFETY: ownership of the box moves to the UI thread with the
        // message; if the window is gone it is taken back and freed.
        unsafe {
            if PostMessageW(target as HWND, WM_OUTCOME, 0, outcome as LPARAM) == 0 {
                drop(Box::from_raw(outcome));
            }
        }
    });
}

/// Fast servers answer while the hotkey is still held down. Pasting with
/// Ctrl+V while Win or Alt is down would trigger some other shortcut.
fn wait_for_modifiers() {
    let mut waited = 0;
    while clipboard::modifiers_held() && waited < MODIFIER_WAIT_MS {
        // SAFETY: Sleep has no preconditions.
        unsafe { Sleep(20) };
        waited += 20;
    }
}

fn on_outcome(outcome: Outcome) {
    let Some(app) = APP.get() else { return };
    if outcome.generation != app.generation || !matches!(app.state, State::Working) {
        return; // cancelled meanwhile
    }
    app.state = State::Idle;
    back_to_idle();
    deliver(outcome);
}

fn deliver(outcome: Outcome) {
    let Some(app) = APP.get() else { return };
    let hwnd = app.hwnd;

    let transcript = match outcome.result {
        Ok(transcript) => transcript,
        Err(error) => {
            log(&format!("dictation failed: {error}"));
            notify(Kind::Error, "Dictation failed", &one_line(&error, 120));
            return;
        }
    };
    log(&format!(
        "dictation: speech {} ms, cleanup {}, {} characters{}",
        transcript.asr_ms,
        transcript
            .llm_ms
            .map_or("off".to_string(), |ms| format!("{ms} ms")),
        transcript.text.chars().count(),
        transcript
            .note
            .as_ref()
            .map_or(String::new(), |note| format!(", cleanup failed: {note}")),
    ));
    if transcript.text.is_empty() {
        notify(
            Kind::Warning,
            "Nothing heard",
            "The recording had no speech in it.",
        );
        return;
    }

    let mut text = transcript.text.clone();
    if app.config.append_space {
        text.push(' ');
    }
    let (paste, paste_keys) = (app.config.paste, app.config.paste_keys);
    let paste_hint = format!("Copied \u{2014} paste with {}", app.config.paste_keys_text);
    app.last = Some(Last {
        text: transcript.text.clone(),
        raw: transcript.raw.clone(),
    });

    if let Err(error) = clipboard::set_text(hwnd, &text) {
        log(&format!("clipboard: {error}"));
        notify(Kind::Error, "Could not copy the text", &error);
        return;
    }

    let mut kind = Kind::Ok;
    let mut title = paste_hint.clone();
    if paste {
        match clipboard::paste_target() {
            clipboard::Target::Ready => {
                // Give clipboard listeners a moment before the paste reads it.
                // SAFETY: Sleep has no preconditions.
                unsafe { Sleep(30) };
                if clipboard::send_combo(paste_keys) {
                    title = "Pasted".to_string();
                }
            }
            clipboard::Target::Elevated => {
                kind = Kind::Warning;
                title = format!("{paste_hint} (this window runs as administrator)");
            }
            clipboard::Target::NoWindow => {}
        }
    }

    let detail = if let Some(note) = &transcript.note {
        kind = Kind::Warning;
        format!("Not cleaned up: {}", one_line(note, 100))
    } else if outcome.limit_reached {
        kind = Kind::Warning;
        "The recording limit was reached; this is everything up to it.".to_string()
    } else {
        one_line(&transcript.text, 72)
    };
    notify(kind, &title, &detail);
}

fn copy_last(raw: bool) {
    let Some(app) = APP.get() else { return };
    let Some(last) = &app.last else { return };
    let text = if raw { &last.raw } else { &last.text };
    match clipboard::set_text(app.hwnd, text) {
        Ok(()) => notify(Kind::Ok, "Copied", &one_line(text, 72)),
        Err(error) => notify(Kind::Error, "Could not copy the text", &error),
    }
}

fn retry_last() {
    let Some(app) = APP.get() else { return };
    if !matches!(app.state, State::Idle) {
        return;
    }
    if let Some(samples) = app.last_audio.clone() {
        app.anchor = anchor_point();
        process(samples, false);
    }
}

// ---- Server checks ---------------------------------------------------------

/// Quietly see whether the speech server answers; speak up only if not.
fn probe_speech_server() {
    let Some(app) = APP.get() else { return };
    let config = app.config.clone();
    let target = app.hwnd as isize;
    std::thread::spawn(move || {
        if let Err(error) = pipeline::probe(&config) {
            let message = Box::into_raw(Box::new(error));
            // SAFETY: as for the outcome message in `process`.
            unsafe {
                if PostMessageW(target as HWND, WM_UNREACHABLE, 0, message as LPARAM) == 0 {
                    drop(Box::from_raw(message));
                }
            }
        }
    });
}

/// "Test connection": try both servers and show what happened.
fn test_connection() {
    let Some(app) = APP.get() else { return };
    let config = app.config.clone();
    let target = app.hwnd as isize;
    notify(Kind::Ok, "Testing the servers\u{2026}", "");
    std::thread::spawn(move || {
        let (report, ok) = pipeline::check(&config);
        let message = Box::into_raw(Box::new(report));
        // SAFETY: as for the outcome message in `process`.
        unsafe {
            if PostMessageW(target as HWND, WM_CHECKED, ok as usize, message as LPARAM) == 0 {
                drop(Box::from_raw(message));
            }
        }
    });
}

fn show_check_result(ok: bool, report: &str) {
    toast::dismiss();
    let text = wide(report.trim_end());
    let title = wide(if ok {
        "saydrop: the servers answer"
    } else {
        "saydrop: a server does not answer"
    });
    let icon = if ok {
        MB_ICONINFORMATION
    } else {
        MB_ICONWARNING
    };
    // SAFETY: NUL-terminated strings that outlive the call.
    unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | icon | MB_SETFOREGROUND,
        );
    }
}

// ---- Menu commands ---------------------------------------------------------

fn shell_open(file: &str, parameters: Option<&str>) {
    let verb = wide("open");
    let file = wide(file);
    let parameters = parameters.map(wide);
    // SAFETY: all strings are NUL-terminated and outlive the call.
    unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ref().map_or(null(), |p| p.as_ptr()),
            null(),
            SW_SHOWNORMAL,
        );
    }
}

fn open_in_notepad(path: &std::path::Path) {
    shell_open("notepad.exe", Some(&format!("\"{}\"", path.display())));
}

fn edit_config() {
    let path = config::config_path();
    if !path.exists() {
        // Recreates the default file.
        config::load();
    }
    open_in_notepad(&path);
}

fn edit_prompt() {
    config::ensure_prompt_file();
    open_in_notepad(&config::prompt_path());
}

fn open_log() {
    let path = util::log_path();
    if !path.exists() {
        log("log opened");
    }
    open_in_notepad(&path);
}

fn reload_config() {
    let loaded = config::load();
    let Some(app) = APP.get() else { return };
    app.config = loaded.config;
    let hwnd = app.hwnd;
    let hotkey = app.config.main_hotkey().to_string();

    let failed = register_standing_hotkeys();
    tray::set_tip(hwnd, &tip());
    if failed.is_empty() && loaded.warnings.is_empty() {
        tray::balloon(
            hwnd,
            "saydrop: config reloaded",
            &format!("Hotkey: {hotkey}"),
            false,
        );
    } else {
        report_startup(&failed, &loaded.warnings, false);
    }
    probe_speech_server();
}

/// Write one switch back to config.ini, so it survives a restart.
fn save_setting(key: &str, value: &str) {
    if let Err(error) = config::set_value(key, value) {
        log(&format!("config: {error}"));
    }
}

fn toggle_llm() {
    let Some(app) = APP.get() else { return };
    if !app.config.llm && app.config.llm_url.is_empty() {
        let hwnd = app.hwnd;
        tray::balloon(
            hwnd,
            "saydrop: no cleanup model yet",
            "Set llm_url (and usually llm_model) in the config, then Reload config.",
            true,
        );
        edit_config();
        return;
    }
    app.config.llm = !app.config.llm;
    save_setting("llm", if app.config.llm { "on" } else { "off" });
}

fn toggle_paste() {
    let Some(app) = APP.get() else { return };
    app.config.paste = !app.config.paste;
    save_setting("paste", if app.config.paste { "on" } else { "off" });
}

fn choose_microphone(name: &str) {
    let Some(app) = APP.get() else { return };
    app.config.microphone = name.to_string();
    save_setting("microphone", name);
}

fn toggle_autostart() {
    let enable = !tray::autostart_enabled();
    if let Err(e) = tray::set_autostart(enable) {
        log(&format!("autostart: {e}"));
        if let Some(app) = APP.get() {
            tray::balloon(app.hwnd, "saydrop: autostart failed", &e, true);
        }
    }
}

fn show_menu(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let hotkey = app.config.main_hotkey().to_string();
    let (recording, working) = match app.state {
        State::Idle => (false, false),
        State::Recording { .. } => (true, false),
        State::Working => (false, true),
    };
    let checked = |on: bool| if on { MF_CHECKED } else { 0 };
    let enabled = |on: bool| if on { 0 } else { MF_GRAYED };
    let (llm, paste) = (app.config.llm, app.config.paste);
    let has_last = app.last.is_some();
    let has_raw = app.last.as_ref().is_some_and(|last| last.raw != last.text);
    let can_retry = app.last_audio.is_some() && !recording && !working;
    let microphone = app.config.microphone.clone();
    let microphones = audio::devices();
    let autostart = tray::autostart_enabled();

    // SAFETY: the menus are created, shown and destroyed here; label buffers
    // outlive the AppendMenuW calls that copy them. Destroying the main menu
    // also destroys the submenu attached to it.
    let command = unsafe {
        let menu = CreatePopupMenu();
        let microphone_menu = CreatePopupMenu();
        if menu.is_null() || microphone_menu.is_null() {
            return;
        }
        let item = |target, flags: u32, id: usize, label: &str| {
            AppendMenuW(target, flags, id, wide(label).as_ptr());
        };
        let separator = || {
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
        };

        let toggle_label = if recording {
            format!("Stop and transcribe\t{hotkey}")
        } else {
            format!("Start dictation\t{hotkey}")
        };
        item(
            menu,
            MF_STRING | enabled(!working),
            CMD_TOGGLE,
            &toggle_label,
        );
        if recording || working {
            item(menu, MF_STRING, CMD_CANCEL, "Cancel");
        }
        separator();
        item(
            menu,
            MF_STRING | checked(llm),
            CMD_LLM,
            "Clean up with the language model",
        );
        item(
            menu,
            MF_STRING | checked(paste),
            CMD_PASTE,
            "Paste into the active window",
        );
        separator();
        item(
            menu,
            MF_STRING | enabled(has_last),
            CMD_COPY_LAST,
            "Copy last result",
        );
        item(
            menu,
            MF_STRING | enabled(has_raw),
            CMD_COPY_RAW,
            "Copy last raw transcript",
        );
        item(
            menu,
            MF_STRING | enabled(can_retry),
            CMD_RETRY,
            "Transcribe last recording again",
        );
        separator();

        let default_selected = !microphones.contains(&microphone);
        item(
            microphone_menu,
            MF_STRING | checked(default_selected),
            CMD_MIC_DEFAULT,
            "Windows default",
        );
        for (index, name) in microphones.iter().enumerate() {
            item(
                microphone_menu,
                MF_STRING | checked(*name == microphone),
                CMD_MIC_FIRST + index,
                name,
            );
        }
        item(menu, MF_POPUP, microphone_menu as usize, "Microphone");
        item(menu, MF_STRING, CMD_TEST, "Test connection");
        separator();
        item(menu, MF_STRING, CMD_EDIT, "Edit config");
        item(menu, MF_STRING, CMD_PROMPT, "Edit cleanup prompt");
        item(menu, MF_STRING, CMD_RELOAD, "Reload config");
        item(menu, MF_STRING, CMD_LOG, "Open log");
        item(
            menu,
            MF_STRING | checked(autostart),
            CMD_AUTOSTART,
            "Start with Windows",
        );
        separator();
        item(menu, MF_STRING, CMD_EXIT, "Exit");

        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        // Without foreground the menu would not close on an outside click;
        // the WM_NULL afterwards is the documented companion to that.
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        command as usize
    };

    match command {
        CMD_TOGGLE => toggle(),
        CMD_CANCEL => cancel(),
        CMD_LLM => toggle_llm(),
        CMD_PASTE => toggle_paste(),
        CMD_COPY_LAST => copy_last(false),
        CMD_COPY_RAW => copy_last(true),
        CMD_RETRY => retry_last(),
        CMD_TEST => test_connection(),
        CMD_EDIT => edit_config(),
        CMD_PROMPT => edit_prompt(),
        CMD_RELOAD => reload_config(),
        CMD_LOG => open_log(),
        CMD_AUTOSTART => toggle_autostart(),
        // SAFETY: our own window handle.
        CMD_EXIT => unsafe {
            DestroyWindow(hwnd);
        },
        CMD_MIC_DEFAULT => choose_microphone(""),
        id if id >= CMD_MIC_FIRST => {
            if let Some(name) = microphones.get(id - CMD_MIC_FIRST) {
                choose_microphone(name);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn main_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_HOTKEY => {
            on_hotkey(wparam as i32);
            0
        }

        // A buffer of audio from the microphone.
        MM_WIM_DATA => {
            if let Some(app) = APP.get()
                && let State::Recording {
                    recorder: Some(recorder),
                    ..
                } = &mut app.state
                && let Some(peak) = recorder.on_data(lparam as *const WAVEHDR)
            {
                overlay::push_level(peak);
            }
            0
        }

        WM_TRAY => {
            match (lparam & 0xFFFF) as u32 {
                WM_LBUTTONUP => toggle(),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            0
        }

        overlay::WM_OVERLAY_STOP => {
            stop_recording(false);
            0
        }
        overlay::WM_OVERLAY_CANCEL => {
            cancel();
            0
        }

        WM_TIMER if wparam == LIMIT_TIMER => {
            stop_recording(true);
            0
        }
        WM_TIMER if wparam == FAKE_LEVEL_TIMER => {
            fake_level();
            0
        }

        WM_STAGE => {
            if let Some(app) = APP.get()
                && wparam == app.generation as usize
                && matches!(app.state, State::Working)
            {
                overlay::set_phase(Phase::Cleaning);
            }
            0
        }
        WM_OUTCOME => {
            // SAFETY: the pointer was produced by Box::into_raw in `process`
            // and is received exactly once.
            let outcome = unsafe { Box::from_raw(lparam as *mut Outcome) };
            on_outcome(*outcome);
            0
        }
        WM_CHECKED => {
            // SAFETY: as for WM_OUTCOME; produced in `test_connection`.
            let report = unsafe { Box::from_raw(lparam as *mut String) };
            show_check_result(wparam != 0, &report);
            0
        }
        WM_UNREACHABLE => {
            // SAFETY: as for WM_OUTCOME; produced in `probe_speech_server`.
            let error = unsafe { Box::from_raw(lparam as *mut String) };
            log(&format!("speech server: {error}"));
            tray::balloon(
                hwnd,
                "saydrop: speech server not reachable",
                &format!("{error}. Right-click the tray icon \u{2192} Edit config to set asr_url."),
                true,
            );
            0
        }

        WM_ALREADY_RUNNING => {
            announce_running();
            0
        }

        WM_DESTROY => {
            // Dropping the state closes the microphone if it is open.
            end_recording();
            overlay::hide();
            toast::dismiss();
            tray::remove(hwnd);
            if let Some(app) = APP.get() {
                unregister(hwnd, &mut app.standing_hotkeys);
            }
            // SAFETY: ends the message loop in `main`.
            unsafe { PostQuitMessage(0) };
            0
        }

        _ => {
            if APP
                .get()
                .is_some_and(|app| message == app.taskbar_created && message != 0)
            {
                add_tray_icon();
                return 0;
            }
            // SAFETY: forwarding unhandled messages unchanged.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}
