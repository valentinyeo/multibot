use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

pub const DEFAULT_ACCOUNT_ORIGIN: &str = "https://account.multiprompt.dev";

#[derive(Clone, Serialize, Deserialize)]
pub struct AccountSession {
    pub email: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for AccountSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSession")
            .field("email", &self.email)
            .field("tokens", &"[redacted]")
            .finish()
    }
}

impl AccountSession {
    pub fn path() -> PathBuf {
        crate::MultibotSettings::data_dir().join("account.json")
    }

    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => bail!("Could not open the saved account session."),
        };
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("Could not read the saved account session."))?;
        if bytes.len() > 64 * 1024 {
            bail!("Saved account session is too large.");
        }
        #[cfg(target_os = "windows")]
        let bytes = windows_storage::decode(&bytes)?;
        let session: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("Saved account session is invalid. Sign in again."))?;
        session.validate()?;
        Ok(Some(session))
    }

    fn validate(&self) -> Result<()> {
        if self.access_token.is_empty()
            || self.refresh_token.is_empty()
            || self.access_token.chars().any(char::is_control)
        {
            bail!("Account returned an invalid session. Sign in again.");
        }
        Ok(())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| anyhow!("Could not encode the account session."))?;
        #[cfg(target_os = "windows")]
        let bytes = windows_storage::encode(&bytes)?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("Invalid account storage path."))?;
        fs::create_dir_all(parent).map_err(|_| anyhow!("Could not create account storage."))?;
        let temp = parent.join(format!(".account-{}.tmp", random_string(16)?));
        let result = (|| -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            #[cfg(target_os = "windows")]
            windows_storage::replace(&temp, path)?;
            #[cfg(not(target_os = "windows"))]
            fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
            bail!("Could not save the account session securely.");
        }
        Ok(())
    }
}

pub struct AccountClient {
    origin: String,
    path: PathBuf,
    pub session: Option<AccountSession>,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

impl AccountClient {
    pub fn load() -> Result<Self> {
        Self::new(DEFAULT_ACCOUNT_ORIGIN, AccountSession::path())
    }

    pub fn new(origin: &str, path: PathBuf) -> Result<Self> {
        let mut client = Self::signed_out(origin, path)?;
        client.session = AccountSession::load_from(&client.path)?;
        Ok(client)
    }

    pub fn signed_out(origin: &str, path: PathBuf) -> Result<Self> {
        let url = Url::parse(origin).map_err(|_| anyhow!("Invalid account service address."))?;
        if url.scheme() != "https"
            && !(url.scheme() == "http" && url.host_str() == Some("127.0.0.1"))
        {
            bail!("Account service requires HTTPS.");
        }
        Ok(Self {
            origin: origin.trim_end_matches('/').to_owned(),
            session: None,
            path,
            agent: ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .max_redirects(0)
                    .http_status_as_error(false)
                    .timeout_global(Some(Duration::from_secs(15)))
                    .build(),
            ),
        })
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, String)> {
        let url = format!("{}{path}", self.origin);
        let auth = token.map(|t| format!("Bearer {t}"));
        let response = match method {
            "GET" => {
                let mut request = self.agent.get(&url).header("Accept", "application/json");
                if let Some(auth) = &auth {
                    request = request.header("Authorization", auth);
                }
                request.call()
            }
            _ => {
                let request = if method == "PUT" {
                    self.agent.put(&url)
                } else {
                    self.agent.post(&url)
                };
                let mut request = request.header("Accept", "application/json");
                if let Some(auth) = &auth {
                    request = request.header("Authorization", auth);
                }
                request.send_json(body.unwrap_or(serde_json::Value::Null))
            }
        };
        let mut response =
            response.map_err(|_| anyhow!("Cannot reach the account service. Try again."))?;
        let status = response.status().as_u16();
        // Error responses can contain credentials. Never expose their contents.
        let text = if status == 200 {
            response
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .map_err(|_| anyhow!("Could not read the account response."))?
        } else {
            String::new()
        };
        Ok((status, text))
    }

    fn store_token_reply(&mut self, text: &str) -> Result<()> {
        let reply: TokenReply = serde_json::from_str(text)
            .map_err(|_| anyhow!("Account returned an invalid token response."))?;
        if reply.expires_in == 0 {
            bail!("Account returned an expired session.");
        }
        let previous = self.session.as_ref();
        let email = reply
            .email
            .or_else(|| reply.id_token.as_deref().and_then(display_email))
            .or_else(|| previous.map(|s| s.email.clone()))
            .unwrap_or_default();
        let session = AccountSession {
            email,
            access_token: reply.access_token,
            refresh_token: reply
                .refresh_token
                .or_else(|| previous.map(|s| s.refresh_token.clone()))
                .ok_or_else(|| anyhow!("Account returned no refresh token."))?,
            expires_at_ms: now_ms().saturating_add(reply.expires_in.saturating_mul(1000)),
        };
        session.save_to(&self.path)?;
        self.session = Some(session);
        Ok(())
    }

    pub fn exchange_code(&mut self, code: &str, verifier: &str, redirect_uri: &str) -> Result<()> {
        let (status, text) = self.request(
            "POST",
            "/oauth/token",
            None,
            Some(serde_json::json!({
                "grant_type": "authorization_code", "code": code,
                "code_verifier": verifier, "redirect_uri": redirect_uri,
            })),
        )?;
        require_ok(status)?;
        self.store_token_reply(&text)
    }

    pub fn refresh_if_needed(&mut self) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("Sign in to your Multibot account first."))?;
        if session.expires_at_ms > now_ms().saturating_add(30_000) {
            return Ok(());
        }
        self.refresh()
    }

    fn refresh(&mut self) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("Sign in to your Multibot account first."))?;
        let (status, text) = self.request(
            "POST",
            "/oauth/token",
            None,
            Some(serde_json::json!({
                "grant_type": "refresh_token", "refresh_token": session.refresh_token,
            })),
        )?;
        require_ok(status)?;
        self.store_token_reply(&text)
    }

    fn secret_request(
        &mut self,
        method: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, String)> {
        self.refresh_if_needed()?;
        let token = &self.session.as_ref().unwrap().access_token;
        let response = self.request(method, "/v1/secrets/deepgram", Some(token), body.clone())?;
        if response.0 != 401 {
            return Ok(response);
        }
        self.refresh()?;
        self.request(
            method,
            "/v1/secrets/deepgram",
            Some(&self.session.as_ref().unwrap().access_token),
            body,
        )
    }

    pub fn fetch_deepgram_key(&mut self) -> Result<Option<String>> {
        let (status, text) = self.secret_request("GET", None)?;
        if status == 404 {
            return Ok(None);
        }
        require_ok(status)?;
        #[derive(Deserialize)]
        struct Secret {
            key: String,
        }
        let secret: Secret = serde_json::from_str(&text)
            .map_err(|_| anyhow!("Account returned an invalid Deepgram key response."))?;
        validate_key(&secret.key)?;
        Ok(Some(secret.key))
    }

    pub fn save_deepgram_key(&mut self, key: &str) -> Result<()> {
        validate_key(key)?;
        let (status, _) = self.secret_request("PUT", Some(serde_json::json!({ "key": key })))?;
        require_ok(status)
    }

    pub fn sign_out(&mut self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("Could not clear the saved account session."),
        }
        self.session = None;
        Ok(())
    }

    pub fn sign_in(&mut self, open_browser: impl FnOnce(&str) -> Result<()>) -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|_| anyhow!("Could not start the sign-in callback."))?;
        listener.set_nonblocking(true)?;
        let redirect = format!("http://{}/callback", listener.local_addr()?);
        let verifier = random_string(48)?;
        let state = random_string(32)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = Url::parse(&format!("{}/login", self.origin))?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", "multiprompt-desktop"),
            ("redirect_uri", redirect.as_str()),
            ("scope", "openid profile email"),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("device_name", "Multibot desktop"),
        ]);
        open_browser(url.as_str())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let code = runtime.block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener)?;
            tokio::time::timeout(Duration::from_secs(180), async {
                loop {
                    let (mut stream, _) = listener.accept().await?;
                    let target = tokio::time::timeout(Duration::from_secs(3), async {
                        let mut request = Vec::new();
                        while request.len() < 8192 && !request.ends_with(b"\r\n\r\n") {
                            request.push(stream.read_u8().await?);
                        }
                        Ok::<_, std::io::Error>(request)
                    }).await;
                    let Ok(Ok(bytes)) = target else { continue };
                    let request = String::from_utf8_lossy(&bytes);
                    let mut parts = request.lines().next().unwrap_or_default().split_whitespace();
                    if parts.next() != Some("GET") { continue; }
                    let Some(target) = parts.next().filter(|t| t.starts_with("/callback?")) else { continue };
                    let Ok(callback) = Url::parse(&format!("http://127.0.0.1{target}")) else { continue };
                    let pairs: Vec<_> = callback.query_pairs().collect();
                    let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.to_string());
                    if get("state").as_deref() != Some(state.as_str()) { continue; }
                    let denied = get("error").is_some();
                    let code = get("code").filter(|s| !s.is_empty());
                    if !denied && code.is_none() { continue; }
                    let message = if denied { "Sign-in declined. Return to Multibot to retry." }
                        else { "Return to Multibot. You may close this window." };
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}", message.len());
                    let _ = tokio::time::timeout(Duration::from_secs(3), stream.write_all(response.as_bytes())).await;
                    if denied { bail!("Sign-in was declined. Try again."); }
                    return Ok(code.unwrap());
                }
            }).await.map_err(|_| anyhow!("Sign-in timed out. Try again."))?
        })?;
        self.exchange_code(&code, &verifier, &redirect)
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.len() > 256 {
        bail!("Account Deepgram key is too long.");
    }
    crate::voice::VoiceSettings {
        api_key: key.to_owned(),
        ..Default::default()
    }
    .validate()
}

fn require_ok(status: u16) -> Result<()> {
    match status {
        200 => Ok(()),
        401 | 403 => bail!("Account session was rejected. Sign in again."),
        _ => bail!("Account request failed (HTTP {status}). Try again."),
    }
}

fn random_string(length: usize) -> Result<String> {
    let mut bytes = vec![0; length];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow!("Could not generate secure sign-in randomness."))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn display_email(id_token: &str) -> Option<String> {
    // Display only: the ID token is never used to authorize requests.
    let bytes = URL_SAFE_NO_PAD.decode(id_token.split('.').nth(1)?).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("email")?.as_str().map(str::to_owned)
}

#[cfg(target_os = "windows")]
mod windows_storage {
    use super::*;
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};

    #[repr(C)]
    struct Blob {
        len: u32,
        data: *mut u8,
    }
    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            input: *const Blob,
            description: *const u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut Blob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *const Blob,
            description: *mut *mut u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut Blob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }

    fn crypt(bytes: &[u8], protect: bool) -> Result<Vec<u8>> {
        let input = Blob {
            len: bytes.len().try_into()?,
            data: bytes.as_ptr().cast_mut(),
        };
        let mut output = Blob {
            len: 0,
            data: ptr::null_mut(),
        };
        let success = unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    1,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    1,
                    &mut output,
                )
            }
        };
        if success == 0 {
            bail!("Windows could not protect or unlock the account session.");
        }
        let result =
            unsafe { std::slice::from_raw_parts(output.data, output.len as usize).to_vec() };
        unsafe {
            for index in 0..output.len as usize {
                output.data.add(index).write_volatile(0);
            }
            LocalFree(output.data.cast());
        }
        Ok(result)
    }

    pub fn encode(bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(
            &serde_json::json!({ "protected": URL_SAFE_NO_PAD.encode(crypt(bytes, true)?) }),
        )?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Vec<u8>> {
        #[derive(Deserialize)]
        struct Protected {
            protected: String,
        }
        let envelope: Protected = serde_json::from_slice(bytes)
            .map_err(|_| anyhow!("Saved account session is invalid."))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(envelope.protected)
            .map_err(|_| anyhow!("Saved account session is invalid."))?;
        crypt(&bytes, false)
    }

    pub fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!("multibot-account-{}", random_string(12).unwrap()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> PathBuf {
            self.0.join("account.json")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session(expires_at_ms: u64) -> AccountSession {
        AccountSession {
            email: "user@example.com".into(),
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
            expires_at_ms,
        }
    }

    fn server(
        replies: Vec<(u16, impl AsRef<str> + Send + 'static)>,
        check: impl Fn(usize, &str, serde_json::Value) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            for (index, (status, body)) in replies.into_iter().enumerate() {
                let body = body.as_ref();
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                    assert!(headers.len() <= 16384);
                }
                let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let mut bytes = vec![0; length];
                stream.read_exact(&mut bytes).unwrap();
                let json = if bytes.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                check(index, &headers, json);
                write!(stream, "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (origin, worker)
    }

    #[test]
    fn persistence_roundtrip_private_permissions_replace_and_signout() {
        let temp = Temp::new();
        assert!(AccountSession::load_from(&temp.path()).unwrap().is_none());
        let first = session(now_ms() + 60_000);
        first.save_to(&temp.path()).unwrap();
        let mut second = first.clone();
        second.access_token = "rotated-access".into();
        second.save_to(&temp.path()).unwrap();
        let loaded = AccountSession::load_from(&temp.path()).unwrap().unwrap();
        assert_eq!(loaded.email, first.email);
        assert_eq!(loaded.access_token, second.access_token);
        assert_eq!(loaded.refresh_token, first.refresh_token);
        assert_eq!(loaded.expires_at_ms, first.expires_at_ms);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        #[cfg(target_os = "windows")]
        assert!(
            !String::from_utf8_lossy(&fs::read(temp.path()).unwrap()).contains("rotated-access")
        );
        assert!(!format!("{loaded:?}").contains("rotated-access"));
        assert!(!format!("{loaded:?}").contains("test-refresh"));
        let mut client = AccountClient::new(DEFAULT_ACCOUNT_ORIGIN, temp.path()).unwrap();
        client.sign_out().unwrap();
        client.sign_out().unwrap();
        assert!(client.session.is_none());
        assert!(!temp.path().exists());
        assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 0);
    }

    #[test]
    fn corrupt_session_can_be_replaced_or_signed_out_without_exposing_contents() {
        let temp = Temp::new();
        fs::write(temp.path(), "invalid-secret-session").unwrap();
        let error = AccountSession::load_from(&temp.path())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("invalid-secret-session"));
        let mut client = AccountClient::signed_out(DEFAULT_ACCOUNT_ORIGIN, temp.path()).unwrap();
        client.sign_out().unwrap();
        assert!(!temp.path().exists());
        fs::write(temp.path(), "invalid-secret-session").unwrap();
        session(now_ms() + 600_000).save_to(&temp.path()).unwrap();
        assert!(AccountSession::load_from(&temp.path()).unwrap().is_some());
    }

    #[test]
    fn exchanges_login_json_without_client_secret_and_extracts_display_email() {
        let temp = Temp::new();
        let id_token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(br#"{"email":"user@example.com"}"#),
        );
        let response = serde_json::json!({
            "access_token": "new-access", "refresh_token": "new-refresh",
            "expires_in": 600, "id_token": id_token,
        })
        .to_string();
        let (origin, worker) = server(vec![(200, response)], |_, headers, body| {
            assert!(headers.starts_with("post /oauth/token "));
            assert!(!headers.contains("authorization:"));
            assert!(headers.contains("content-type: application/json"));
            assert_eq!(
                body,
                serde_json::json!({"grant_type":"authorization_code", "code":"a/b+c", "code_verifier":"verifier", "redirect_uri":"http://127.0.0.1:1234/callback"})
            );
        });
        let mut client = AccountClient::new(&origin, temp.path()).unwrap();
        client
            .exchange_code("a/b+c", "verifier", "http://127.0.0.1:1234/callback")
            .unwrap();
        let stored = AccountSession::load_from(&temp.path()).unwrap().unwrap();
        assert_eq!(stored.email, "user@example.com");
        assert!(stored.expires_at_ms >= now_ms() + 590_000);
        worker.join().unwrap();
    }

    #[test]
    fn refreshes_expired_token_and_fetches_exact_secret_payload() {
        let temp = Temp::new();
        session(0).save_to(&temp.path()).unwrap();
        let (origin, worker) = server(
            vec![
                (
                    200,
                    r#"{"access_token":"renewed","refresh_token":"rotated","expires_in":600}"#,
                ),
                (200, r#"{"key":"test-deepgram-key"}"#),
            ],
            |index, headers, body| {
                if index == 0 {
                    assert!(headers.starts_with("post /oauth/token "));
                    assert_eq!(
                        body,
                        serde_json::json!({"grant_type":"refresh_token","refresh_token":"test-refresh"})
                    );
                } else {
                    assert!(headers.starts_with("get /v1/secrets/deepgram "));
                    assert!(headers.contains("authorization: bearer renewed"));
                    assert!(body.is_null());
                }
            },
        );
        let mut client = AccountClient::new(&origin, temp.path()).unwrap();
        assert_eq!(
            client.fetch_deepgram_key().unwrap().as_deref(),
            Some("test-deepgram-key")
        );
        let stored = AccountSession::load_from(&temp.path()).unwrap().unwrap();
        assert_eq!(stored.refresh_token, "rotated");
        assert_eq!(stored.email, "user@example.com");
        worker.join().unwrap();
    }

    #[test]
    fn uploads_key_as_json_and_missing_remote_is_not_an_error() {
        let temp = Temp::new();
        session(now_ms() + 600_000).save_to(&temp.path()).unwrap();
        let (origin, worker) = server(vec![(404, ""), (200, "{}")], |index, headers, body| {
            assert!(headers.contains("authorization: bearer test-access"));
            if index == 0 {
                assert!(headers.starts_with("get /v1/secrets/deepgram "));
            } else {
                assert!(headers.starts_with("put /v1/secrets/deepgram "));
                assert_eq!(body, serde_json::json!({"key":"uploaded-key"}));
            }
        });
        let mut client = AccountClient::new(&origin, temp.path()).unwrap();
        assert!(client.fetch_deepgram_key().unwrap().is_none());
        client.save_deepgram_key("uploaded-key").unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn rejected_access_refreshes_once_and_preserves_unrotated_refresh_token() {
        let temp = Temp::new();
        session(now_ms() + 600_000).save_to(&temp.path()).unwrap();
        let (origin, worker) = server(
            vec![
                (401, "secret-error"),
                (200, r#"{"access_token":"renewed","expires_in":600}"#),
                (401, "secret-error"),
            ],
            |_, _, _| {},
        );
        let mut client = AccountClient::new(&origin, temp.path()).unwrap();
        let error = client.fetch_deepgram_key().unwrap_err().to_string();
        assert!(error.contains("Sign in again"));
        assert!(!error.contains("secret-error"));
        assert_eq!(
            client.session.as_ref().unwrap().refresh_token,
            "test-refresh"
        );
        worker.join().unwrap();
    }

    #[test]
    fn loopback_pkce_checks_state_and_decodes_callback_code() {
        let temp = Temp::new();
        let (challenge_tx, challenge_rx) = std::sync::mpsc::channel();
        let (origin, worker) = server(
            vec![(
                200,
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":600,"email":"user@example.com"}"#,
            )],
            move |_, _, body| {
                assert_eq!(body["code"], "code/with+symbols");
                let verifier = body["code_verifier"].as_str().unwrap();
                assert_eq!(verifier.len(), 64);
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                    challenge_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                );
                assert!(
                    body["redirect_uri"]
                        .as_str()
                        .unwrap()
                        .ends_with("/callback")
                );
            },
        );
        let mut client = AccountClient::new(&origin, temp.path()).unwrap();
        let mut callback_worker = None;
        client
            .sign_in(|url| {
                let url = Url::parse(url).unwrap();
                assert_eq!(url.path(), "/login");
                let pairs: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                assert_eq!(pairs["client_id"], "multiprompt-desktop");
                assert_eq!(pairs["code_challenge_method"], "S256");
                assert_eq!(pairs["scope"], "openid profile email");
                assert_eq!(pairs["code_challenge"].len(), 43);
                challenge_tx.send(pairs["code_challenge"].clone()).unwrap();
                let mut callback = Url::parse(&pairs["redirect_uri"]).unwrap();
                let state = pairs["state"].clone();
                callback_worker = Some(thread::spawn(move || {
                    let agent = ureq::Agent::new_with_config(
                        ureq::Agent::config_builder()
                            .timeout_global(Some(Duration::from_secs(5)))
                            .build(),
                    );
                    callback
                        .query_pairs_mut()
                        .append_pair("code", "wrong-code")
                        .append_pair("state", "wrong-state");
                    let _ = agent.get(callback.as_str()).call();
                    callback.set_query(None);
                    callback
                        .query_pairs_mut()
                        .append_pair("code", "code/with+symbols")
                        .append_pair("state", &state);
                    assert_eq!(agent.get(callback.as_str()).call().unwrap().status(), 200);
                }));
                Ok(())
            })
            .unwrap();
        callback_worker.unwrap().join().unwrap();
        worker.join().unwrap();
        assert_eq!(client.session.unwrap().email, "user@example.com");
    }

    #[test]
    fn browser_failure_and_oauth_denial_leave_no_session() {
        let temp = Temp::new();
        let mut client = AccountClient::new(DEFAULT_ACCOUNT_ORIGIN, temp.path()).unwrap();
        assert!(client.sign_in(|_| bail!("Browser unavailable.")).is_err());
        assert!(!temp.path().exists());
        let mut callback_worker = None;
        let error = client
            .sign_in(|url| {
                let url = Url::parse(url).unwrap();
                let pairs: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                let mut callback = Url::parse(&pairs["redirect_uri"]).unwrap();
                callback
                    .query_pairs_mut()
                    .append_pair("error", "access_denied")
                    .append_pair("state", &pairs["state"]);
                callback_worker = Some(thread::spawn(move || {
                    ureq::get(callback.as_str()).call().unwrap();
                }));
                Ok(())
            })
            .unwrap_err()
            .to_string();
        callback_worker.unwrap().join().unwrap();
        assert!(error.contains("declined"));
        assert!(!temp.path().exists());
    }

    #[test]
    fn rejects_invalid_responses_without_leaking_their_contents() {
        let temp = Temp::new();
        session(now_ms() + 600_000).save_to(&temp.path()).unwrap();
        let (origin, worker) = server(
            vec![
                (200, r#"{"key":"secret\r\nheader"}"#),
                (500, "secret-server-error"),
                (200, "secret-invalid-json"),
            ],
            |_, _, _| {},
        );
        let mut client = AccountClient::new(&origin, temp.path()).unwrap();
        for _ in 0..3 {
            assert!(
                !client
                    .fetch_deepgram_key()
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
        }
        assert!(client.save_deepgram_key("invalid\nkey").is_err());
        worker.join().unwrap();
    }
}
