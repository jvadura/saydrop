# saydrop

Dictate into any Windows app. Press a hotkey, speak, press it again: the text
is pasted where your cursor is. A tiny tray app, written in Rust.

saydrop does not recognise speech itself. It records your voice and sends it
to a speech-to-text server **that you choose**: one running on your own
machine or network, or a hosted API. An optional second step lets a language
model fix punctuation and remove the "um"s before the text is pasted.

## Why

Local speech models have become very good and very fast: a model that fits in
3 GB of VRAM transcribes a minute of speech in about a second. What is missing
on Windows is the small piece in between: a hotkey, a microphone, and a paste.
That is all saydrop is. Your audio goes to the server you configured and
nowhere else.

## Install

Download `saydrop-<version>-setup.exe` from the
[latest release](https://github.com/jvadura/saydrop/releases/latest) and run
it. It installs for the current user only (no administrator prompt) and asks
for:

- the address of your speech-to-text server,
- the hotkey,
- optionally a chat model for cleanup,
- whether to start saydrop when you sign in.

The installer is not code-signed, so Windows SmartScreen may show
"Windows protected your PC" the first time: choose **More info → Run anyway**.

Other ways:

- **Portable:** download `saydrop.exe` from the same release and run it. The
  first start creates a config file with defaults; right-click the tray icon →
  **Edit config** to point it at your server.
- **From source:** `cargo install --git https://github.com/jvadura/saydrop`
  (needs [Rust](https://rustup.rs) with the MSVC toolchain).
- **Unattended:** the installer takes its answers on the command line:

  ```
  saydrop-<version>-setup.exe /VERYSILENT /TASKS="autostart" /ASR=http://server:8020 /HOTKEY=Insert /LLM=http://server:8000 /LLMMODEL=my-model
  ```

An icon appears in the notification area. Windows 11 tends to tuck new icons
into the overflow menu; to pin it, open Settings → Personalization → Taskbar →
*Other system tray icons* and switch saydrop on.

## Use

| Action | Result |
| --- | --- |
| **Win+Shift+D** (or left-click the tray icon) | recording starts; a small bar shows the level and the time |
| speak | |
| **Win+Shift+D** again, or **Stop** on the bar | the recording is transcribed and pasted |
| **Esc**, or **✕** on the bar | the recording is thrown away |

The bar never takes keyboard focus, so the text lands in the window you were
typing in. The text is also left on the clipboard: if the paste did not land
(the focus moved, or the window belongs to a program running as
administrator), press Ctrl+V.

Right-click the tray icon for the switches:

- **Clean up with the language model**: on or off.
- **Paste into the active window**: off means the text is only copied.
- **Copy last result** / **Copy last raw transcript**: the cleanup model edits
  your words, so the unedited transcript is always one click away.
- **Transcribe last recording again**: if the server was down, nothing is
  lost; the last recording is kept in memory until the next one. It also
  sends a recording that was held back as silence.
- **Microphone**: pick an input device.
- **Test connection**: checks both servers and shows what they answered.

The tray icon is purple when idle, red while recording and amber while the
text is being produced.

## The speech server

Any server with the OpenAI transcription API (`POST /v1/audio/transcriptions`)
works. Give its address as `asr_url`; the path is added for you.

| Server | `asr_url` | Notes |
| --- | --- | --- |
| [llama.cpp](https://github.com/ggml-org/llama.cpp) `llama-server` with an audio model such as Qwen3-ASR | `http://127.0.0.1:8020` | what saydrop is developed and tested against |
| [whisper.cpp](https://github.com/ggml-org/whisper.cpp) `whisper-server` | `http://127.0.0.1:8080/inference` | a full URL is used as written |
| [speaches](https://github.com/speaches-ai/speaches) | `http://127.0.0.1:8000` | set `asr_model` to the model you loaded |
| OpenAI | `https://api.openai.com` | set `asr_model` and `asr_api_key` |

Only the first row is tested regularly; the others use the same API. Reports
are welcome.

Three things worth knowing:

- **Recording length.** Servers accept only so much audio per request, and
  some cut off longer audio *without an error*: you get the first part of your
  dictation and nothing tells you the rest is missing. `max_seconds` (default
  240) stops the recording by itself before that happens and transcribes what
  was said; the bar counts down the last 30 seconds. For llama-server the
  limit is roughly `context size / 16` seconds, so `-c 4096` is a little over
  four minutes.
- **Silence.** Given a recording with nothing in it, some speech models make
  text up. That is what happens when Windows records from the wrong
  microphone, or a muted one. So a recording that never gets louder than
  `silence_db` (default -50 dB) is not sent at all: you get "No speech heard"
  with the level that was measured, and nothing is pasted. The meter on the
  bar follows the same limit, so flat bars while you speak mean the same
  thing: pick the right input under **Microphone** in the tray menu. If your
  microphone is simply quiet and real speech gets rejected, lower the value
  (for example `-60`); **Transcribe last recording again** sends a rejected
  recording anyway.
- **Glossary.** Put names and terms in `glossary` and they are sent along as a
  hint, which fixes most misspelled product names and jargon.

## Cleanup by a language model

Optional. Set `llm_url` (and usually `llm_model`) to any server with the
OpenAI chat API (`POST /v1/chat/completions`): llama.cpp, vLLM, Ollama,
LM Studio, OpenAI and so on. Then switch it on in the tray menu.

The instructions the model gets are in `prompt.txt` next to the config file
(tray menu → **Edit cleanup prompt**). The default asks it to fix punctuation,
remove filler words and false starts, and otherwise leave your words alone.
Change it to taste: a different tone, a fixed output language, bullet points.

Settings that differ from model to model go into `llm_extra`, a piece of JSON
that is merged into every request:

```ini
# a lower temperature
llm_extra = {"temperature": 0.2}

# OpenAI reasoning models
llm_extra = {"reasoning_effort": "low"}

# Qwen on vLLM or llama.cpp: little or no thinking
llm_extra = {"chat_template_kwargs": {"enable_thinking": true, "reasoning_effort": "low"}}
llm_extra = {"chat_template_kwargs": {"enable_thinking": false}}
```

Reasoning makes cleanup noticeably slower and is rarely needed for this job;
keep it low or off.

If the cleanup fails or takes longer than `llm_timeout`, the raw transcript is
pasted and the confirmation says so. If the model returns something much
longer than what you said (it answered your dictation instead of editing it),
the raw transcript is used as well.

## Configure

Right-click the tray icon → **Edit config**, then **Reload config**. The file
lives at `%APPDATA%\saydrop\config.ini` and explains every setting. The
important ones:

```ini
# Press once to start, again to stop. Several hotkeys: separate with commas.
hotkey = Win+Shift+D

# Optional separate keys, for example for a macro pad.
hotkey_start = F13
hotkey_stop = F14
# Only active while recording.
hotkey_cancel = Esc

asr_url = http://127.0.0.1:8020
asr_model = whisper-1
glossary = Kubernetes, kubectl, PostgreSQL
max_seconds = 240
silence_db = -50

llm = on
llm_url = http://127.0.0.1:8000
llm_model = my-model

paste = on
paste_keys = Ctrl+V
```

Notes:

- Hotkeys take modifiers `Ctrl`, `Alt`, `Shift`, `Win` and one key: `A`-`Z`,
  `0`-`9`, `F1`-`F24`, `Insert`, `Pause`, `ScrollLock`, `PrintScreen`, `Esc`,
  `Space`, `Tab`, `Delete`, `Home`, `End`, `PageUp`, `PageDown`, the arrow
  keys, `Menu` and `Num0`-`Num9`. A single key such as `Insert` or `F13` works
  without a modifier; that key then no longer reaches other programs.
- If a hotkey is already taken by another program, saydrop says so in a
  notification and keeps running; pick another one.
- Some terminals paste with other keys: set `paste_keys` to `Ctrl+Shift+V` or
  `Shift+Insert`.
- API keys can be kept out of the file: `asr_api_key = env:OPENAI_API_KEY`
  reads the environment variable of that name.
- The tray menu switches write their state back to the file.
- Values are taken literally. No quoting, no escaping.

## Command line

```
saydrop --check                  test the speech and cleanup servers from the config
saydrop --transcribe FILE        transcribe an audio file and print the text
saydrop --transcribe FILE --json the same, with the raw transcript and timings
saydrop --mic-file FILE.wav      run the tray app with a WAV file instead of the microphone
```

saydrop is a windowed program, so in PowerShell or cmd use
`start /wait saydrop --check` or pipe the output (`saydrop --check | more`) to
see the result before the prompt returns.

Setting the environment variable `SAYDROP_CONFIG_DIR` makes saydrop use that
folder instead of `%APPDATA%\saydrop`.

## How it works

One UI thread with one message loop, raw Win32 via `windows-sys`, no GUI
framework. The only other dependency is `serde_json`.

- Hotkeys are plain `RegisterHotKey`; there is no keyboard hook. The stop and
  cancel keys are registered only while recording, so `Esc` is untouched the
  rest of the time.
- The microphone is opened at 16 kHz mono, the format speech models work at,
  and only while recording.
- HTTP goes through WinHTTP, the stack that ships with Windows, so `https`
  works without a bundled TLS library. Loopback and private-network addresses
  bypass any system proxy.
- The text is delivered by putting it on the clipboard and sending the paste
  keys, the one method that works from browsers to terminals. saydrop waits
  for you to release the hotkey's modifier keys first.
- Nothing is written to disk except the config and an error log at
  `%LOCALAPPDATA%\saydrop\saydrop.log`. The log records timings and errors,
  never what you said.
- The tray icon is re-added when Explorer restarts.

## Limits

- Windows 10 and 11 only.
- One dictation at a time; the hotkey is ignored while a transcription is in
  progress.
- Programs running as administrator do not accept the paste from a normal
  program. The text is on the clipboard; press Ctrl+V.
- Over Remote Desktop the microphone is the one of the computer you sit at,
  and only if the connection redirects audio recording (in the Remote Desktop
  client: Local Resources → Remote audio → Record from this computer).
- No live text while you speak: the text arrives when you stop.

## Build

```
cargo build --release
cargo test
```

The installer is an [Inno Setup 6](https://jrsoftware.org/isinfo.php) script:

```
cargo build --release
iscc installer\saydrop.iss
```

It writes `target\installer\saydrop-<version>-setup.exe`, taking the version
from the executable.

The tray icon and the executable's icon are the same artwork, drawn in
`src/tray.rs`. After changing it, regenerate `assets/saydrop.ico` by running
the tests once with `SAYDROP_WRITE_ICON=1` set. Setting `SAYDROP_PREVIEW_DIR`
to a folder makes the tests write pictures of the bar, the toast and the icon
there, which is the quick way to look at a visual change.

## License

MIT
