//! What happens to a recording: speech-to-text, then the optional cleanup by
//! a language model. Everything here blocks on the network and runs on a
//! worker thread; nothing touches the UI.

use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::audio;
use crate::config::Config;
use crate::net;

/// Text produced for one recording.
pub struct Transcript {
    /// What gets pasted: the cleaned text, or the raw transcript.
    pub text: String,
    /// What the speech server returned.
    pub raw: String,
    /// Set when cleanup was wanted but failed and `text` is the raw transcript.
    pub note: Option<String>,
    pub asr_ms: u128,
    pub llm_ms: Option<u128>,
}

/// Complete an endpoint address. A bare server address gets the standard
/// OpenAI path, an address ending in `/v1` gets the rest of it, and anything
/// with its own path is used as written.
pub fn endpoint(address: &str, suffix: &str) -> String {
    let mut url = address.trim().trim_end_matches('/').to_string();
    if !url.contains("://") {
        url.insert_str(0, "http://");
    }
    let after_scheme = url.split_once("://").map_or("", |(_, rest)| rest);
    if !after_scheme.contains('/') {
        format!("{url}/v1/{suffix}")
    } else if url.ends_with("/v1") {
        format!("{url}/{suffix}")
    } else {
        url
    }
}

pub fn asr_endpoint(config: &Config) -> String {
    endpoint(&config.asr_url, "audio/transcriptions")
}

pub fn llm_endpoint(config: &Config) -> String {
    endpoint(&config.llm_url, "chat/completions")
}

struct Part<'a> {
    name: &'a str,
    file_name: Option<&'a str>,
    content_type: Option<&'a str>,
    data: &'a [u8],
}

/// Encode a `multipart/form-data` body. Returns the Content-Type header
/// value and the body.
fn multipart(parts: &[Part], boundary: &str) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    for part in parts {
        body.extend(format!("--{boundary}\r\n").as_bytes());
        body.extend(format!("Content-Disposition: form-data; name=\"{}\"", part.name).as_bytes());
        if let Some(file_name) = part.file_name {
            body.extend(format!("; filename=\"{file_name}\"").as_bytes());
        }
        body.extend(b"\r\n");
        if let Some(content_type) = part.content_type {
            body.extend(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend(b"\r\n");
        body.extend(part.data);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// A boundary that cannot occur in WAV data by accident.
fn boundary() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("----saydrop-{nanos:032x}")
}

/// The message a server put in an error response, or the start of the body.
fn server_message(body: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let message = value
            .pointer("/error/message")
            .or_else(|| value.get("error"))
            .or_else(|| value.get("message"))
            .or_else(|| value.get("detail"));
        if let Some(text) = message.and_then(Value::as_str) {
            return text.to_string();
        }
    }
    let text = String::from_utf8_lossy(body);
    text.trim().chars().take(200).collect()
}

fn http_error(what: &str, status: u32, body: &[u8]) -> String {
    let message = server_message(body);
    let hint = match status {
        401 | 403 => " Check the API key.",
        404 => " Check the address and the model name.",
        413 => " The recording is too long for this server.",
        _ => "",
    };
    if message.is_empty() {
        format!("{what} answered HTTP {status}.{hint}")
    } else {
        format!("{what} answered HTTP {status}: {message}.{hint}")
    }
}

fn bearer(key: &str) -> Option<String> {
    (!key.is_empty()).then(|| format!("Bearer {key}"))
}

/// Remove what some speech models put around the transcript.
///
/// Qwen3-ASR served with an older chat template starts every answer with
/// `language English<asr_text>`; the text after the tag is the transcript.
pub fn tidy_transcript(text: &str) -> String {
    let text = match text.rfind("<asr_text>") {
        Some(at) => &text[at + "<asr_text>".len()..],
        None => text,
    };
    text.replace("</asr_text>", "").trim().to_string()
}

/// Send a WAV file (or any audio file the server understands) to the speech
/// server and return the transcript.
pub fn transcribe(config: &Config, audio_file: &[u8]) -> Result<String, String> {
    let glossary = (!config.glossary.trim().is_empty())
        .then(|| format!("Glossary: {}", config.glossary.trim()));
    let mut parts = vec![
        Part {
            name: "file",
            file_name: Some("dictation.wav"),
            content_type: Some("audio/wav"),
            data: audio_file,
        },
        Part {
            name: "model",
            file_name: None,
            content_type: None,
            data: config.asr_model.as_bytes(),
        },
        Part {
            name: "response_format",
            file_name: None,
            content_type: None,
            data: b"json",
        },
    ];
    if !config.asr_language.is_empty() {
        parts.push(Part {
            name: "language",
            file_name: None,
            content_type: None,
            data: config.asr_language.as_bytes(),
        });
    }
    if let Some(glossary) = &glossary {
        parts.push(Part {
            name: "prompt",
            file_name: None,
            content_type: None,
            data: glossary.as_bytes(),
        });
    }
    let (content_type, body) = multipart(&parts, &boundary());

    let authorization = bearer(&config.asr_api_key);
    let mut headers = vec![("Content-Type", content_type.as_str())];
    if let Some(value) = &authorization {
        headers.push(("Authorization", value));
    }

    let response = net::request(
        "POST",
        &asr_endpoint(config),
        &headers,
        &body,
        config.asr_timeout,
    )
    .map_err(|e| format!("Speech server: {e}"))?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(
            "The speech server",
            response.status,
            &response.body,
        ));
    }

    let text = match serde_json::from_slice::<Value>(&response.body) {
        Ok(value) => match value.get("text").and_then(Value::as_str) {
            Some(text) => text.to_string(),
            None => {
                return Err(format!(
                    "The speech server sent no text: {}",
                    server_message(&response.body)
                ));
            }
        },
        // Some servers answer with the bare transcript.
        Err(_) => String::from_utf8_lossy(&response.body).into_owned(),
    };
    Ok(tidy_transcript(&text))
}

/// The system prompt: the user's instructions plus the glossary.
fn system_prompt(config: &Config) -> String {
    let mut prompt = config.prompt.trim().to_string();
    let glossary = config.glossary.trim();
    if !glossary.is_empty() {
        prompt.push_str("\n\nNames and terms that may occur; spell them exactly like this: ");
        prompt.push_str(glossary);
    }
    prompt
}

/// The chat-completions request for cleaning `transcript`.
pub fn llm_request(config: &Config, transcript: &str) -> Value {
    let mut body = Map::new();
    if !config.llm_model.is_empty() {
        body.insert("model".to_string(), json!(config.llm_model));
    }
    body.insert(
        "messages".to_string(),
        json!([
            {"role": "system", "content": system_prompt(config)},
            {"role": "user", "content": format!("<transcript>\n{transcript}\n</transcript>")},
        ]),
    );
    body.insert("stream".to_string(), json!(false));
    // The user's extras win, so they can also override the lines above.
    for (key, value) in &config.llm_extra {
        body.insert(key.clone(), value.clone());
    }
    Value::Object(body)
}

/// Pull the answer text out of a chat-completions response.
pub fn llm_answer(response: &Value) -> Option<String> {
    let content = response.pointer("/choices/0/message/content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        // Some servers return a list of typed parts.
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    Some(tidy_answer(&text))
}

/// Strip reasoning blocks and echoed tags that do not belong in the text.
fn tidy_answer(text: &str) -> String {
    let mut text = text.trim();
    // Models that reason inline put it first, in <think> tags.
    if text.starts_with("<think>")
        && let Some(end) = text.find("</think>")
    {
        text = text[end + "</think>".len()..].trim();
    }
    text.replace("<transcript>", "")
        .replace("</transcript>", "")
        .trim()
        .to_string()
}

/// Ask the language model to clean up `transcript`.
pub fn cleanup(config: &Config, transcript: &str) -> Result<String, String> {
    let body = llm_request(config, transcript).to_string();
    let authorization = bearer(&config.llm_api_key);
    let mut headers = vec![("Content-Type", "application/json")];
    if let Some(value) = &authorization {
        headers.push(("Authorization", value));
    }

    let response = net::request(
        "POST",
        &llm_endpoint(config),
        &headers,
        body.as_bytes(),
        config.llm_timeout,
    )?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(
            "The cleanup model",
            response.status,
            &response.body,
        ));
    }
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| "The cleanup model sent an unreadable answer".to_string())?;
    let answer = llm_answer(&value).unwrap_or_default();
    if answer.is_empty() {
        return Err("The cleanup model returned no text".to_string());
    }
    // A cleaned transcript is about as long as the original. Something far
    // longer means the model answered the dictation instead of editing it.
    if answer.chars().count() > transcript.chars().count() * 2 + 200 {
        return Err("The cleanup model wrote much more than was dictated".to_string());
    }
    Ok(answer)
}

/// Speech-to-text, then cleanup when `use_llm` is set. `on_cleanup` is
/// called when the second step starts, so the UI can say so. A failed
/// cleanup is not an error: the raw transcript is delivered with a note.
pub fn run(
    config: &Config,
    audio_file: &[u8],
    use_llm: bool,
    on_cleanup: impl FnOnce(),
) -> Result<Transcript, String> {
    let started = Instant::now();
    let raw = transcribe(config, audio_file)?;
    let asr_ms = started.elapsed().as_millis();

    let mut transcript = Transcript {
        text: raw.clone(),
        raw,
        note: None,
        asr_ms,
        llm_ms: None,
    };
    if transcript.raw.is_empty() || !use_llm {
        return Ok(transcript);
    }

    on_cleanup();
    let started = Instant::now();
    match cleanup(config, &transcript.raw) {
        Ok(cleaned) => transcript.text = cleaned,
        Err(e) => transcript.note = Some(e),
    }
    transcript.llm_ms = Some(started.elapsed().as_millis());
    Ok(transcript)
}

/// Is anything answering at the speech server's address? Any HTTP response
/// counts; only a failed connection is an error.
pub fn probe(config: &Config) -> Result<(), String> {
    net::request("GET", &asr_endpoint(config), &[], &[], 5).map(|_| ())
}

/// Exercise both servers with a tiny request and describe the outcome.
/// Returns the report and whether everything that is configured works.
pub fn check(config: &Config) -> (String, bool) {
    let mut report = String::new();
    let mut ok = true;

    let silence = audio::wav_bytes(&vec![0i16; audio::SAMPLE_RATE as usize], audio::SAMPLE_RATE);
    let started = Instant::now();
    report.push_str(&format!("Speech-to-text\n{}\n", asr_endpoint(config)));
    match transcribe(config, &silence) {
        Ok(_) => report.push_str(&format!(
            "OK, answered in {:.1} s\n",
            started.elapsed().as_secs_f32()
        )),
        Err(e) => {
            ok = false;
            report.push_str(&format!("FAILED: {e}\n"));
        }
    }

    report.push_str("\nCleanup model\n");
    if config.llm_url.is_empty() {
        report.push_str("Not configured (llm_url is empty)\n");
    } else {
        report.push_str(&format!("{}\n", llm_endpoint(config)));
        let started = Instant::now();
        match cleanup(config, "so um this is uh a test of the the cleanup") {
            Ok(text) => report.push_str(&format!(
                "OK, answered in {:.1} s: \"{}\"\n",
                started.elapsed().as_secs_f32(),
                crate::util::one_line(&text, 80)
            )),
            Err(e) => {
                ok = false;
                report.push_str(&format!("FAILED: {e}\n"));
            }
        }
        if !config.llm {
            report.push_str("(Cleanup is currently switched off in the tray menu.)\n");
        }
    }
    (report, ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn test_config(text: &str) -> Config {
        let mut warnings = Vec::new();
        let config = config::parse(text, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        config
    }

    #[test]
    fn endpoints_are_completed() {
        let asr = |address| endpoint(address, "audio/transcriptions");
        assert_eq!(
            asr("http://127.0.0.1:8020"),
            "http://127.0.0.1:8020/v1/audio/transcriptions"
        );
        assert_eq!(
            asr("http://127.0.0.1:8020/"),
            "http://127.0.0.1:8020/v1/audio/transcriptions"
        );
        assert_eq!(
            asr("localhost:8020"),
            "http://localhost:8020/v1/audio/transcriptions"
        );
        assert_eq!(
            asr("https://api.example.com/v1"),
            "https://api.example.com/v1/audio/transcriptions"
        );
        assert_eq!(
            asr("https://api.example.com/v1/"),
            "https://api.example.com/v1/audio/transcriptions"
        );
        // A path of its own is kept, e.g. the whisper.cpp server.
        assert_eq!(
            asr("http://host:8088/inference"),
            "http://host:8088/inference"
        );
        assert_eq!(
            endpoint("http://host:8000", "chat/completions"),
            "http://host:8000/v1/chat/completions"
        );
    }

    #[test]
    fn multipart_layout() {
        let parts = [
            Part {
                name: "file",
                file_name: Some("a.wav"),
                content_type: Some("audio/wav"),
                data: b"RIFF",
            },
            Part {
                name: "model",
                file_name: None,
                content_type: None,
                data: b"whisper-1",
            },
        ];
        let (content_type, body) = multipart(&parts, "XX");
        assert_eq!(content_type, "multipart/form-data; boundary=XX");
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\nRIFF\r\n\
             --XX\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n\
             --XX--\r\n"
        );
    }

    #[test]
    fn transcripts_are_tidied() {
        assert_eq!(tidy_transcript("  Hello there. \n"), "Hello there.");
        assert_eq!(
            tidy_transcript("language English<asr_text>Hello there."),
            "Hello there."
        );
        assert_eq!(tidy_transcript("language None<asr_text>"), "");
    }

    #[test]
    fn llm_request_merges_extras() {
        let config = test_config(
            "llm = on\nllm_url = http://host:8000\nllm_model = some-model\nglossary = vLLM, Qwen\n\
             llm_extra = {\"temperature\": 0.1, \"chat_template_kwargs\": {\"enable_thinking\": false}}\n",
        );
        let request = llm_request(&config, "hello");
        assert_eq!(request["model"], "some-model");
        assert_eq!(request["stream"], false);
        assert_eq!(request["temperature"], 0.1);
        assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(request["messages"][0]["role"], "system");
        let system = request["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("speech recognizer"));
        assert!(system.ends_with("spell them exactly like this: vLLM, Qwen"));
        assert_eq!(
            request["messages"][1]["content"],
            "<transcript>\nhello\n</transcript>"
        );

        // Without a model name the field is left out, and extras may replace
        // the standard fields.
        let config = test_config("llm_url = http://host:8000\nllm_extra = {\"stream\": true}\n");
        let request = llm_request(&config, "x");
        assert!(request.get("model").is_none());
        assert_eq!(request["stream"], true);
    }

    #[test]
    fn llm_answers_are_extracted() {
        let plain = json!({"choices": [{"message": {"content": "\n\nHello.  "}}]});
        assert_eq!(llm_answer(&plain).unwrap(), "Hello.");

        let thinking = json!({"choices": [{"message":
            {"content": "<think>let me see</think>\nHello."}}]});
        assert_eq!(llm_answer(&thinking).unwrap(), "Hello.");

        let parts = json!({"choices": [{"message": {"content":
            [{"type": "text", "text": "<transcript>Hel"}, {"type": "text", "text": "lo.</transcript>"}]}}]});
        assert_eq!(llm_answer(&parts).unwrap(), "Hello.");

        // Reasoning used up the budget: content is null.
        let empty = json!({"choices": [{"message": {"content": null}}]});
        assert!(llm_answer(&empty).is_none());
        assert!(llm_answer(&json!({"error": "x"})).is_none());
    }

    #[test]
    fn server_errors_are_readable() {
        assert_eq!(
            http_error(
                "The speech server",
                401,
                br#"{"error": {"message": "Invalid key"}}"#
            ),
            "The speech server answered HTTP 401: Invalid key. Check the API key."
        );
        assert_eq!(
            http_error("The cleanup model", 500, br#"{"detail": "boom"}"#),
            "The cleanup model answered HTTP 500: boom."
        );
        assert_eq!(
            http_error("The cleanup model", 502, b""),
            "The cleanup model answered HTTP 502."
        );
    }
}
