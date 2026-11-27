use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    pub api_key: String,
    pub language: String,
}

impl std::fmt::Debug for VoiceSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceSettings")
            .field("api_key", &"[redacted]")
            .field("language", &self.language)
            .finish()
    }
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            language: "en".into(),
        }
    }
}

impl VoiceSettings {
    pub fn validate(&self) -> Result<()> {
        let key = self.api_key.trim();
        if key.is_empty() {
            bail!("Set your Deepgram API key in Settings → Voice.");
        }
        if !key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            bail!("Deepgram API key must contain only the key, without a Token prefix.");
        }
        if self.language.is_empty()
            || self.language.len() > 32
            || !self
                .language
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            bail!("Use a Deepgram language code, such as en, de, or en-US.");
        }
        Ok(())
    }

    pub fn listen_url(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "https://api.deepgram.com/v1/listen?model=nova-3&smart_format=true&punctuate=true&encoding=linear16&sample_rate=16000&channels=1&mip_opt_out=true&language={}",
            self.language
        ))
    }

    // Resolve at use time: synced keys can change while Multibot is running.
    // Keep detected keys out of saved settings and the manual-override input.
    pub fn resolved(&self, account_key: Option<&str>) -> (Self, Option<VoiceKeySource>) {
        self.resolve_with_sources(
            account_key,
            || std::env::var("DEEPGRAM_API_KEY").ok(),
            || {
                #[cfg(target_os = "windows")]
                {
                    windows_environment_key()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    None
                }
            },
            || legacy_config_key(&crate::zigshel_import::legacy_zigshel_dir()),
        )
    }

    fn resolve_with_sources(
        &self,
        account_key: Option<&str>,
        process_environment: impl FnOnce() -> Option<String>,
        windows_environment: impl FnOnce() -> Option<String>,
        legacy_config: impl FnOnce() -> Option<String>,
    ) -> (Self, Option<VoiceKeySource>) {
        let mut resolved = self.clone();
        resolved.api_key = normalize_api_key(&self.api_key);
        if !resolved.api_key.is_empty() {
            return (resolved, Some(VoiceKeySource::Manual));
        }
        let candidate = |key: Option<String>, source| {
            key.map(|key| normalize_api_key(&key))
                .filter(|key| !key.is_empty())
                .map(|key| (key, source))
        };
        let detected = candidate(account_key.map(str::to_owned), VoiceKeySource::Account)
            .or_else(|| candidate(process_environment(), VoiceKeySource::ProcessEnvironment))
            .or_else(|| candidate(windows_environment(), VoiceKeySource::WindowsEnvironment))
            .or_else(|| candidate(legacy_config(), VoiceKeySource::LegacyConfig));
        if let Some((key, source)) = detected {
            resolved.api_key = key;
            return (resolved, Some(source));
        }
        (resolved, None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKeySource {
    Manual,
    Account,
    ProcessEnvironment,
    WindowsEnvironment,
    LegacyConfig,
}

impl VoiceKeySource {
    pub fn hint(self) -> &'static str {
        match self {
            Self::Manual => "Using your Multibot API key override",
            Self::Account => "Using the Deepgram key synced from your Multibot account",
            Self::ProcessEnvironment => "Using DEEPGRAM_API_KEY from the process environment",
            Self::WindowsEnvironment => "Using synced DEEPGRAM_API_KEY from Windows environment",
            Self::LegacyConfig => "Using the legacy ZigShell API key (read-only)",
        }
    }
}

pub fn normalize_api_key(raw: &str) -> String {
    let bytes = raw.as_bytes();
    // Legacy deepgram_key.zig accepts a bounded 40-digit hex key in pasted text.
    for (i, key) in bytes.windows(40).enumerate() {
        if key.iter().all(u8::is_ascii_hexdigit)
            && (i == 0 || !bytes[i - 1].is_ascii_hexdigit())
            && (i + 40 == bytes.len() || !bytes[i + 40].is_ascii_hexdigit())
        {
            return raw[i..i + 40].to_ascii_lowercase();
        }
    }
    raw.trim_matches(|c: char| !c.is_ascii_graphic() || matches!(c, '\'' | '"'))
        .to_owned()
}

fn legacy_config_key(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("deepgram_api_key")?.as_str().map(str::to_owned)
}

#[cfg(target_os = "windows")]
fn windows_environment_key() -> Option<String> {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegGetValueW(
            key: *mut c_void,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    let subkey: Vec<u16> = "Environment\0".encode_utf16().collect();
    let name: Vec<u16> = "DEEPGRAM_API_KEY\0".encode_utf16().collect();
    let mut buffer = [0u16; 513];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    // HKEY_CURRENT_USER is a sign-extended predefined handle on Win64.
    let result = unsafe {
        RegGetValueW(
            0x8000_0001u32 as i32 as isize as *mut c_void,
            subkey.as_ptr(),
            name.as_ptr(),
            2,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 {
        return None;
    }
    let end = buffer.iter().position(|&c| c == 0)?;
    let key = String::from_utf16(&buffer[..end]).ok()?;
    Some(key.trim().to_owned())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DictationPhase {
    #[default]
    Off,
    Listening,
    Finishing,
}

impl DictationPhase {
    pub fn start(&mut self) {
        *self = Self::Listening;
    }
    pub fn stop(&mut self) {
        if *self == Self::Listening {
            *self = Self::Finishing;
        }
    }
    pub fn finish(&mut self) {
        *self = Self::Off;
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "",
            Self::Listening => "● Listening",
            Self::Finishing => "Transcribing…",
        }
    }
}

pub fn parse_transcript(status: u16, body: &str) -> Result<String> {
    match status {
        200..=299 => {}
        401 | 403 => bail!("Deepgram rejected the API key. Check Settings → Voice."),
        429 => bail!("Deepgram is rate limited. Try dictation again shortly."),
        _ => bail!("Deepgram request failed (HTTP {status})."),
    }
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| anyhow::anyhow!("Deepgram returned an invalid response."))?;
    value
        .pointer("/results/channels/0/alternatives/0/transcript")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_owned())
        .ok_or_else(|| anyhow::anyhow!("Deepgram response contained no transcript."))
}

pub fn append_transcript(current: &str, transcript: &str) -> String {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        return current.to_owned();
    }
    let separator = if current.is_empty() || current.ends_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    format!("{current}{separator}{transcript}")
}

pub fn transcribe_pcm(config: &VoiceSettings, pcm: &[u8]) -> Result<String> {
    transcribe_at(config, pcm, &config.listen_url()?)
}

fn transcribe_at(config: &VoiceSettings, pcm: &[u8], url: &str) -> Result<String> {
    config.validate()?;
    if pcm.is_empty() || pcm.len() % 2 != 0 {
        bail!("Microphone returned no valid 16-bit audio.");
    }
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build(),
    );
    let mut response = agent
        .post(url)
        .header("Authorization", format!("Token {}", config.api_key.trim()))
        .header("Content-Type", "audio/raw")
        .send(pcm)
        .map_err(|_| {
            anyhow::anyhow!("Cannot reach Deepgram. Check your connection and try again.")
        })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return parse_transcript(status, "");
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .map_err(|_| anyhow::anyhow!("Could not read the Deepgram response."))?;
    parse_transcript(status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn account_key_enables_dictation_without_legacy_and_clears_on_signout() {
        let config = VoiceSettings {
            language: "de".into(),
            ..Default::default()
        };
        let (synced, source) = config.resolve_with_sources(
            Some("account-key"),
            || panic!("account must beat the process environment"),
            || panic!("account must beat the Windows environment"),
            || panic!("account must not require legacy settings"),
        );
        assert!(synced.validate().is_ok());
        assert_eq!(source, Some(VoiceKeySource::Account));
        assert_eq!(synced.language, "de");
        assert!(config.api_key.is_empty());
        let (signed_out, source) = config.resolve_with_sources(None, || None, || None, || None);
        assert!(signed_out.validate().is_err());
        assert_eq!(source, None);
        let local = VoiceSettings {
            api_key: "local-key".into(),
            ..Default::default()
        };
        let (resolved, source) = local.resolved(Some("account-key"));
        assert_eq!(resolved.api_key, "local-key");
        assert_eq!(source, Some(VoiceKeySource::Manual));
        assert!(!format!("{synced:?}").contains("account-key"));
    }

    #[test]
    fn dictation_lifecycle_and_repeated_stop() {
        let mut phase = DictationPhase::default();
        assert_eq!(phase, DictationPhase::Off);
        phase.stop();
        assert_eq!(phase, DictationPhase::Off);
        phase.start();
        assert_eq!(phase.label(), "● Listening");
        phase.stop();
        phase.stop();
        assert_eq!(phase, DictationPhase::Finishing);
        phase.finish();
        assert_eq!(phase, DictationPhase::Off);
    }

    #[test]
    fn appends_without_clobbering_typed_text_or_unicode() {
        assert_eq!(
            append_transcript("Typed draft", " dictated words "),
            "Typed draft dictated words"
        );
        assert_eq!(
            append_transcript("日本語\n", "Grüße 🌍"),
            "日本語\nGrüße 🌍"
        );
        assert_eq!(append_transcript("", "Hello."), "Hello.");
        assert_eq!(append_transcript("keep ", "  "), "keep ");
    }

    #[test]
    fn validates_key_language_and_audio_contract() {
        let mut config = VoiceSettings::default();
        assert!(
            config
                .listen_url()
                .unwrap_err()
                .to_string()
                .contains("API key")
        );
        config.api_key = "secret".into();
        assert!(
            config
                .listen_url()
                .unwrap()
                .contains("encoding=linear16&sample_rate=16000&channels=1")
        );
        config.language = "de&key=leak".into();
        assert!(config.listen_url().is_err());
        config.language = "de".into();
        config.api_key = "secret\r\nHeader: value".into();
        assert!(config.validate().is_err());
        assert!(!format!("{config:?}").contains("secret"));
    }

    #[test]
    fn parses_silence_unicode_and_safe_errors() {
        assert_eq!(
            parse_transcript(
                200,
                r#"{"results":{"channels":[{"alternatives":[{"transcript":"Grüße!"}]}]}}"#
            )
            .unwrap(),
            "Grüße!"
        );
        assert_eq!(
            parse_transcript(
                200,
                r#"{"results":{"channels":[{"alternatives":[{"transcript":""}]}]}}"#
            )
            .unwrap(),
            ""
        );
        assert!(parse_transcript(200, "{}").is_err());
        assert!(parse_transcript(200, "not json").is_err());
        assert!(
            parse_transcript(401, "secret response")
                .unwrap_err()
                .to_string()
                .contains("API key")
        );
        assert!(
            parse_transcript(429, "")
                .unwrap_err()
                .to_string()
                .contains("rate limited")
        );
        assert!(
            !parse_transcript(500, "secret response")
                .unwrap_err()
                .to_string()
                .contains("secret")
        );
    }

    #[test]
    fn posts_pcm_with_auth_and_returns_transcript() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/listen", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let headers = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(headers.starts_with("post /v1/listen "));
            assert!(headers.contains("authorization: token test-key"));
            assert!(headers.contains("content-type: audio/raw"));
            let mut audio = [0; 4];
            stream.read_exact(&mut audio).unwrap();
            assert_eq!(audio, [1, 0, 2, 0]);
            let body =
                r#"{"results":{"channels":[{"alternatives":[{"transcript":"Hello world."}]}]}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let config = VoiceSettings {
            api_key: "test-key".into(),
            ..Default::default()
        };
        assert_eq!(
            transcribe_at(&config, &[1, 0, 2, 0], &url).unwrap(),
            "Hello world."
        );
        worker.join().unwrap();
    }

    #[test]
    fn key_resolution_priority_and_empty_sources() {
        let config = VoiceSettings {
            api_key: "  'own-key'  ".into(),
            ..Default::default()
        };
        let (resolved, source) = config.resolve_with_sources(
            None,
            || panic!("manual key must win"),
            || panic!("manual key must win"),
            || panic!("manual key must win"),
        );
        assert_eq!(resolved.api_key, "own-key");
        assert_eq!(source, Some(VoiceKeySource::Manual));

        let config = VoiceSettings::default();
        let (resolved, source) = config.resolve_with_sources(
            None,
            || Some(" \"process-key\"\n".into()),
            || panic!("process environment must win"),
            || panic!("process environment must win"),
        );
        assert_eq!(resolved.api_key, "process-key");
        assert_eq!(source, Some(VoiceKeySource::ProcessEnvironment));
        let (resolved, source) = config.resolve_with_sources(
            None,
            || Some(" \"\" ".into()),
            || Some(" 'registry-key' ".into()),
            || panic!("Windows environment must beat legacy config"),
        );
        assert_eq!(resolved.api_key, "registry-key");
        assert_eq!(source, Some(VoiceKeySource::WindowsEnvironment));
        assert_eq!(
            source.unwrap().hint(),
            "Using synced DEEPGRAM_API_KEY from Windows environment"
        );
        assert!(!source.unwrap().hint().contains(&resolved.api_key));
        let (resolved, source) = config.resolve_with_sources(
            None,
            || None,
            || Some(" \t ".into()),
            || Some("legacy-key".into()),
        );
        assert_eq!(resolved.api_key, "legacy-key");
        assert_eq!(source, Some(VoiceKeySource::LegacyConfig));
        assert!(config.api_key.is_empty());
        assert!(
            !serde_json::to_string(&config)
                .unwrap()
                .contains("legacy-key")
        );
        let (resolved, source) = config.resolve_with_sources(None, || None, || None, || None);
        assert!(resolved.api_key.is_empty());
        assert_eq!(source, None);
    }

    #[test]
    fn normalizes_quotes_whitespace_and_bounded_legacy_hex_keys() {
        assert_eq!(normalize_api_key(" \t'\"opaque-key\"'\r\n"), "opaque-key");
        let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        assert_eq!(upper.len(), 40);
        assert_eq!(
            normalize_api_key(&format!("key: '{upper}'")),
            upper.to_ascii_lowercase()
        );
        let oversized = "A".repeat(41);
        assert_eq!(normalize_api_key(&oversized), oversized);
        let config = VoiceSettings {
            api_key: normalize_api_key("'not a key'"),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn legacy_import_is_read_only_and_never_overrides_own_key() {
        let dir = std::env::temp_dir().join(format!("multibot-voice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = r#"{"deepgram_api_key":"legacy-key"}"#;
        std::fs::write(&path, original).unwrap();
        let mut config = VoiceSettings::default();
        let (resolved, source) =
            config.resolve_with_sources(None, || None, || None, || legacy_config_key(&dir));
        assert_eq!(resolved.api_key, "legacy-key");
        assert_eq!(source, Some(VoiceKeySource::LegacyConfig));
        assert!(config.api_key.is_empty());
        config.api_key = "own-key".into();
        let (resolved, source) =
            config.resolve_with_sources(None, || None, || None, || legacy_config_key(&dir));
        assert_eq!(resolved.api_key, "own-key");
        assert_eq!(source, Some(VoiceKeySource::Manual));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
