//! Command-line modes: checking the servers and transcribing a file without
//! the tray app. Useful for setting up, and for testing without a microphone.

use std::io::Write;
use std::path::Path;

use serde_json::json;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
};

use crate::{config, pipeline};

pub const USAGE: &str = "\
saydrop: dictate into any Windows app.

  saydrop                        start the tray app
  saydrop --check                test the speech and cleanup servers from the config
  saydrop --transcribe FILE      transcribe an audio file and print the text
  saydrop --transcribe FILE --json
                                 the same, with the raw transcript and timings
  saydrop --mic-file FILE.wav    start the tray app with a WAV file standing in
                                 for the microphone (for testing)
  saydrop --version

Output appears in the console the command was started from. In PowerShell or
cmd, prefix with \"start /wait\" or pipe the output to wait for the result.";

/// Print a line. A program built for the Windows GUI subsystem has no
/// console of its own: output goes to a redirected stdout when there is one,
/// and otherwise to the console of the process that started it.
pub fn print(text: &str) {
    // SAFETY: plain queries of the process's standard handle.
    let redirected = unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        !handle.is_null() && handle != INVALID_HANDLE_VALUE
    };
    if redirected {
        let _ = writeln!(std::io::stdout(), "{text}");
        return;
    }
    // SAFETY: no preconditions; fails harmlessly when there is no console.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0
        && let Ok(mut console) = std::fs::OpenOptions::new().write(true).open("CONOUT$")
    {
        let _ = writeln!(console, "{text}");
    }
}

/// `--check`: returns the process exit code.
pub fn check() -> i32 {
    let loaded = config::load();
    for warning in &loaded.warnings {
        print(&format!("config: {warning}"));
    }
    print(&format!("Config: {}\n", config::config_path().display()));
    let (report, ok) = pipeline::check(&loaded.config);
    print(report.trim_end());
    if ok { 0 } else { 1 }
}

/// `--transcribe FILE [--json]`: returns the process exit code.
pub fn transcribe(file: &str, as_json: bool) -> i32 {
    let loaded = config::load();
    let audio = match std::fs::read(Path::new(file)) {
        Ok(bytes) => bytes,
        Err(e) => {
            print(&format!("error: {file}: {e}"));
            return 2;
        }
    };
    match pipeline::run(&loaded.config, &audio, loaded.config.llm, || {}) {
        Ok(transcript) => {
            if as_json {
                print(
                    &json!({
                        "text": transcript.text,
                        "raw": transcript.raw,
                        "cleanup_error": transcript.note,
                        "asr_ms": transcript.asr_ms as u64,
                        "llm_ms": transcript.llm_ms.map(|ms| ms as u64),
                    })
                    .to_string(),
                );
            } else {
                print(&transcript.text);
                if let Some(note) = &transcript.note {
                    print(&format!("(not cleaned up: {note})"));
                }
            }
            0
        }
        Err(e) => {
            print(&format!("error: {e}"));
            1
        }
    }
}
