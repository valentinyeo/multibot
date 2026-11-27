use crate::*;
use anyhow::{Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "multibot-updater-{label}-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct MockResponse {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    suppress_content_length: bool,
}

impl MockResponse {
    fn ok_json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            reason: "OK",
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into().into_bytes(),
            suppress_content_length: false,
        }
    }

    fn ok_bytes(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            reason: "OK",
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body,
            suppress_content_length: false,
        }
    }

    fn status(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
            suppress_content_length: false,
        }
    }

    fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn without_content_length(mut self) -> Self {
        self.suppress_content_length = true;
        self
    }
}

type RouteHandler = Arc<dyn Fn(&RecordedRequest) -> MockResponse + Send + Sync>;

struct MockHttpServer {
    addr: String,
    shutdown: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockHttpServer {
    fn start<F>(handler: F) -> Self
    where
        F: Fn(&RecordedRequest) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: RouteHandler = Arc::new(handler);

        let shutdown_bg = Arc::clone(&shutdown);
        let requests_bg = Arc::clone(&requests);

        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if shutdown_bg.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

                let req = match read_http_request(&stream) {
                    Some(r) => r,
                    None => continue,
                };
                requests_bg.lock().unwrap().push(req.clone());
                let resp = handler(&req);
                let _ = write_http_response(&mut stream, &resp);
            }
        });

        Self {
            addr,
            shutdown,
            requests,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn localhost_url(&self, path: &str) -> String {
        let port = self.addr.split(':').nth(1).unwrap();
        format!("http://localhost:{port}{path}")
    }

    fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_http_request(stream: &TcpStream) -> Option<RecordedRequest> {
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).ok()? == 0 {
        return None;
    }
    let mut parts = first_line.trim().split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Some(RecordedRequest {
        method,
        path,
        headers,
    })
}

fn write_http_response(stream: &mut TcpStream, resp: &MockResponse) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\nConnection: close\r\n", resp.status, resp.reason);
    let mut has_cl = false;
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_cl = true;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_cl && !resp.suppress_content_length {
        head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

fn make_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

fn make_deflated_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

fn make_valid_release_fixture(
    exe_bytes: &[u8],
    daemon_bytes: &[u8],
    extra_entries: &[(&str, &[u8])],
    zip_url: String,
    version: &str,
) -> (Vec<u8>, ReleaseManifest, ReleaseInfo) {
    let mut entries = vec![
        ("multibot.exe", exe_bytes),
        ("multibotd-linux-x86_64", daemon_bytes),
    ];
    entries.extend_from_slice(extra_entries);
    let zip_bytes = make_deflated_zip_bytes(&entries);
    let zip_sha = sha256_hex(&zip_bytes);
    let exe_sha = sha256_hex(exe_bytes);
    let daemon_sha = sha256_hex(daemon_bytes);
    let manifest = ReleaseManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        version: version.to_string(),
        tag: format!("v{version}"),
        published_at: "2026-11-26T20:00:00Z".into(),
        notes: vec![
            "Faster chat switching".into(),
            "ci: bump github actions".into(),
            "In-app updater".into(),
        ],
        windows_x64: WindowsAssetInfo {
            asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
            url: zip_url,
            sha256: zip_sha,
            size_bytes: zip_bytes.len() as u64,
            multibot_exe_sha256: exe_sha,
            multibotd_linux_x86_64_sha256: daemon_sha,
        },
    };
    let info = ReleaseInfo::from(manifest.clone());
    (zip_bytes, manifest, info)
}

// =========================================================================
// 1. M1 Backward-Compatibility Tests (6 existing tests preserved verbatim)
// =========================================================================

#[test]
fn release_manifest_serde_roundtrip() {
    let manifest = ReleaseManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        version: "0.1.42".into(),
        tag: "v0.1.42".into(),
        published_at: "2026-11-26T20:00:00Z".into(),
        notes: vec!["Faster chat switching".into(), "In-app updater".into()],
        windows_x64: WindowsAssetInfo {
            asset_name: "multibot-windows-x64.zip".into(),
            url: "https://github.com/valentinyeo/multibot/releases/download/v0.1.42/multibot-windows-x64.zip".into(),
            sha256: "a".repeat(64),
            size_bytes: 12_345_678,
            multibot_exe_sha256: "b".repeat(64),
            multibotd_linux_x86_64_sha256: "c".repeat(64),
        },
    };

    let json = serde_json::to_string(&manifest).unwrap();
    let decoded = parse_release_manifest(&json).unwrap();
    assert_eq!(decoded, manifest);

    let without_schema = r#"{
        "version": "0.1.42",
        "tag": "v0.1.42",
        "published_at": "2026-11-26T20:00:00Z",
        "windows_x64": {
            "asset_name": "multibot-windows-x64.zip",
            "url": "https://github.com/valentinyeo/multibot/releases/download/v0.1.42/multibot-windows-x64.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size_bytes": 123,
            "multibot_exe_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "multibotd_linux_x86_64_sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        }
    }"#;
    let parsed = parse_release_manifest(without_schema).unwrap();
    assert_eq!(parsed.schema_version, MANIFEST_SCHEMA_VERSION);
    assert!(parsed.notes.is_empty());

    let info = ReleaseInfo::from(manifest.clone());
    assert_eq!(info.version, "0.1.42");
    assert_eq!(info.asset, manifest.windows_x64);
}

#[test]
fn update_state_default_is_idle_and_platform_stub_works() {
    assert_eq!(UpdateState::default(), UpdateState::Idle);

    let platform = StubUpdaterPlatform::new(
        PathBuf::from("/tmp/multibot-updates"),
        vec!["--gpu=software".into()],
    );
    assert_eq!(platform.updates_dir(), PathBuf::from("/tmp/multibot-updates"));
    assert_eq!(platform.preserved_cli_args(), vec!["--gpu=software"]);

    let plan = SwapPlan {
        install_dir: PathBuf::from("/tmp/install"),
        current_exe: PathBuf::from("/tmp/install/multibot.exe"),
        staged_exe: PathBuf::from("/tmp/updates/multibot.exe"),
        current_daemon: PathBuf::from("/tmp/install/multibotd-linux-x86_64"),
        staged_daemon: PathBuf::from("/tmp/updates/multibotd-linux-x86_64"),
    };
    let outcome = platform.file_swapper().swap_binaries(&plan).unwrap();
    assert_eq!(
        outcome.backup_exe,
        PathBuf::from("/tmp/install/multibot.exe.old")
    );
    platform.file_swapper().rollback(&plan, &outcome).unwrap();
    assert_eq!(
        platform
            .file_swapper()
            .cleanup_stale(&plan.install_dir)
            .unwrap(),
        0
    );
    assert_eq!(
        platform
            .process_launcher()
            .spawn_relaunch(&plan.current_exe, &platform.preserved_cli_args())
            .unwrap(),
        0
    );
    assert!(
        platform
            .process_launcher()
            .verify_readiness(&plan.current_exe, 1_000)
            .unwrap()
    );
}

#[test]
fn version_comparison_semver_precedence_prerelease_and_prefix() {
    assert!(is_newer_version("0.1.0", "0.1.1").unwrap());
    assert!(is_newer_version("0.1.0", "v0.1.1").unwrap());
    assert!(is_newer_version("v0.1.0", "v0.2.0").unwrap());
    assert!(is_newer_version("0.1.10", "0.2.0").unwrap());
    assert!(is_newer_version("0.1.0-rc.1", "0.1.0").unwrap());

    assert!(!is_newer_version("0.1.5", "0.1.5").unwrap());
    assert!(!is_newer_version("0.2.0", "0.1.9").unwrap());
    assert!(!is_newer_version("0.1.0", "v0.1.0+build.1").unwrap());
    assert!(!is_newer_version("0.1.0", "0.1.1-alpha.1").unwrap());
    assert!(is_newer_version("0.1.0", "vv0.2.0").is_err());
    assert!(is_newer_version("0.1.0", "vvvv0.1.1").is_err());
}

#[test]
fn url_allowlist_credentials_ports_and_local_http_gating() {
    assert!(is_allowed_https_url_with_local(
        "https://github.com/valentinyeo/multibot/releases/download/v0.1.1/multibot-windows-x64.zip",
        false
    ));
    assert!(is_allowed_https_url_with_local(
        "https://GITHUB.COM/valentinyeo/multibot/releases/download/v0.1.1/multibot-windows-x64.zip",
        false
    ));
    assert!(is_allowed_https_url_with_local(
        "https://github.com:443/valentinyeo/multibot/releases/download/v0.1.1/a.zip",
        false
    ));
    assert!(is_allowed_https_url_with_local(
        "https://api.github.com/repos/valentinyeo/multibot/releases/latest",
        false
    ));
    assert!(is_allowed_https_url_with_local(
        "https://objects.githubusercontent.com/github-production-release-asset/foo.zip",
        false
    ));
    assert!(is_allowed_https_url_with_local(
        "https://release-assets.githubusercontent.com/github-production-release-asset/foo.zip",
        false
    ));
    assert!(is_allowed_https_url_with_local(
        "https://github-releases.githubusercontent.com/12345/foo.zip",
        false
    ));

    assert!(!is_allowed_https_url_with_local(
        "https://raw.githubusercontent.com/attacker/repo/main/evil.zip",
        false
    ));
    assert!(!is_allowed_https_url_with_local(
        "https://gist.githubusercontent.com/attacker/123/raw/evil.zip",
        false
    ));
    assert!(!is_allowed_https_url_with_local(
        "https://.githubusercontent.com/payload.zip",
        false
    ));

    assert!(!is_allowed_https_url_with_local(
        "https://user:pass@github.com/a", // Fake URL-rejection credentials. trufflehog:ignore
        true
    ));
    assert!(!is_allowed_https_url_with_local(
        "https://user@github.com/a",
        true
    ));
    assert!(!is_allowed_https_url_with_local(
        "https://:pass@github.com/a", // Fake URL-rejection credentials. trufflehog:ignore
        true
    ));
    assert!(!is_allowed_https_url_with_local(
        "http://user:pass@127.0.0.1:8080/a", // Fake URL-rejection credentials. trufflehog:ignore
        true
    ));
    assert!(!is_allowed_https_url_with_local(
        "https://github.com:8443/valentinyeo/multibot/a.zip",
        true
    ));

    assert!(!is_allowed_https_url_with_local(
        "http://127.0.0.1:8080/a",
        false
    ));
    assert!(!is_allowed_https_url_with_local(
        "http://localhost:8080/a",
        false
    ));
    assert!(is_allowed_https_url_with_local(
        "http://127.0.0.1:8080/a",
        true
    ));
    assert!(is_allowed_https_url_with_local(
        "http://localhost:8080/a",
        true
    ));
    assert!(is_allowed_https_url(
        "http://127.0.0.1:8080/update-manifest.json"
    ));

    assert!(!is_allowed_https_url(
        "http://github.com/valentinyeo/multibot"
    ));
    assert!(!is_allowed_https_url(
        "https://evil.example.com/payload.zip"
    ));
}

#[test]
fn zip_entry_path_cross_platform_defense_and_archive_inspection() {
    assert!(is_safe_zip_entry_path(Path::new("multibot.exe")));
    assert!(is_safe_zip_entry_path(Path::new("dir/sub/file.exe")));

    let unsafe_paths = [
        "../evil.exe",
        "foo/../../evil.exe",
        r"foo\..\..\evil.exe",
        r"\Windows\System32\cmd.exe",
        r"C:\evil.exe",
        "C:evil.exe",
        "C:/evil.exe",
        r"\\server\share\evil.exe",
        "multibot.exe:ads",
        "foo/bar\0baz",
        "/abs/path",
        "/",
        "",
        ".",
        "./",
        "./.",
    ];
    for bad in unsafe_paths {
        assert!(
            !is_safe_zip_entry_path(Path::new(bad)),
            "expected unsafe path to be rejected: {bad:?}"
        );
    }

    let good_zip = make_zip_bytes(&[
        ("multibot.exe", b"exe-bytes"),
        ("dir/sub/file.exe", b"sub-bytes"),
    ]);
    let names = inspect_zip_entries(&good_zip).unwrap();
    assert_eq!(names, vec!["multibot.exe", "dir/sub/file.exe"]);

    for bad_entry in [
        "../evil.exe",
        r"foo\..\..\evil.exe",
        r"\Windows\System32\cmd.exe",
        r"C:\evil.exe",
        "C:evil.exe",
        "C:/evil.exe",
        r"\\server\share\evil.exe",
        "multibot.exe:ads",
        ".",
        "./.",
    ] {
        let bad_zip = make_zip_bytes(&[(bad_entry, b"pwned")]);
        assert!(
            inspect_zip_entries(&bad_zip).is_err(),
            "expected inspect_zip_entries to reject {bad_entry:?}"
        );
    }
}

#[test]
fn sha256_gzip_and_http_agent_helpers() {
    assert_eq!(
        sha256_hex(b"hello"),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );

    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(b"multibot-updater").unwrap();
    let gz = enc.finish().unwrap();
    assert_eq!(
        decompress_gzip_limited(&gz, 64).unwrap(),
        b"multibot-updater"
    );

    let _agent = default_http_agent();
}

// =========================================================================
// 2. Security, Sanitization, SHA-256 & Backoff Tests
// =========================================================================

#[test]
fn sanitize_update_url_override_handles_empty_control_chars_and_allowlist() {
    assert_eq!(sanitize_update_url_override(None).unwrap(), None);
    assert_eq!(sanitize_update_url_override(Some("")).unwrap(), None);
    assert_eq!(sanitize_update_url_override(Some("   ")).unwrap(), None);

    assert_eq!(
        sanitize_update_url_override(Some("  http://127.0.0.1:9999/latest  ")).unwrap(),
        Some("http://127.0.0.1:9999/latest".to_string())
    );
    assert_eq!(
        sanitize_update_url_override(Some(
            "https://api.github.com/repos/valentinyeo/multibot/releases/latest"
        ))
        .unwrap(),
        Some("https://api.github.com/repos/valentinyeo/multibot/releases/latest".to_string())
    );

    // Reject control chars (header/CRLF injection, NUL, tab, DEL).
    assert!(sanitize_update_url_override(Some("http://127.0.0.1:9000/a\r\nEvil: 1")).is_err());
    assert!(sanitize_update_url_override(Some("http://127.0.0.1:9000/a\0")).is_err());
    assert!(sanitize_update_url_override(Some("\thttp://127.0.0.1:9000/a")).is_err());
    assert!(sanitize_update_url_override(Some("http://127.0.0.1:9000/\x7f")).is_err());

    // Reject disallowed hosts & plaintext non-loopback.
    assert!(sanitize_update_url_override(Some("https://evil.example.com/latest")).is_err());
    assert!(sanitize_update_url_override(Some("http://github.com/latest")).is_err());
    assert!(sanitize_update_url_override(Some("https://raw.githubusercontent.com/a/b")).is_err());
}

#[test]
fn sha256_file_and_hex_validation() {
    let dir = TempTestDir::new("sha256-file");
    let file_path = dir.path().join("blob.bin");
    // > 128 KiB to cross multiple 64 KiB streaming chunks.
    let payload: Vec<u8> = (0..(150 * 1024)).map(|i| (i % 251) as u8).collect();
    fs::write(&file_path, &payload).unwrap();

    let streamed = sha256_file(&file_path).unwrap();
    let expected = sha256_hex(&payload);
    assert_eq!(streamed, expected);
    assert!(is_valid_sha256_hex(&streamed));
    assert!(!is_valid_sha256_hex("short"));
    assert!(!is_valid_sha256_hex(&"z".repeat(64)));

    verify_sha256_hex(&streamed.to_ascii_uppercase(), &expected, "blob").unwrap();
    assert!(verify_sha256_hex(&"0".repeat(64), &expected, "blob").is_err());
    assert!(sha256_file(dir.path()).is_err());
}

#[test]
fn inspect_zip_entries_rejects_symlinks() {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("multibot.exe", opts).unwrap();
        writer.write_all(b"exe").unwrap();
        writer
            .add_symlink("multibotd-linux-x86_64", "/etc/passwd", opts)
            .unwrap();
        writer.finish().unwrap();
    }
    let bytes = cursor.into_inner();
    let err = inspect_zip_entries(&bytes).unwrap_err();
    assert!(
        err.to_string().contains("symlink"),
        "expected symlink error, got: {err}"
    );
}

#[test]
fn rate_limit_backoff_respects_headers_and_15m_minimum() {
    let now = 1_700_000_000u64;
    // Absent headers -> 15m minimum.
    assert_eq!(
        parse_rate_limit_backoff(None, None, now),
        MIN_RATE_LIMIT_BACKOFF
    );
    // Small Retry-After -> clamped up to 15m minimum.
    assert_eq!(
        parse_rate_limit_backoff(Some("30"), None, now),
        MIN_RATE_LIMIT_BACKOFF
    );
    // Larger Retry-After -> honored.
    assert_eq!(
        parse_rate_limit_backoff(Some("1800"), None, now),
        Duration::from_secs(1800)
    );
    // X-RateLimit-Reset in future -> honored when larger.
    assert_eq!(
        parse_rate_limit_backoff(Some("1200"), Some(&(now + 2400).to_string()), now),
        Duration::from_secs(2400)
    );
    // Past X-RateLimit-Reset -> 15m minimum.
    assert_eq!(
        parse_rate_limit_backoff(None, Some(&(now - 500).to_string()), now),
        MIN_RATE_LIMIT_BACKOFF
    );
    // Huge Retry-After -> clamped to 24h maximum.
    assert_eq!(
        parse_rate_limit_backoff(Some("9999999"), None, now),
        MAX_RATE_LIMIT_BACKOFF
    );
}

// =========================================================================
// 3. Notes & rejected.txt Tests
// =========================================================================

#[test]
fn notes_filter_strips_ci_test_infra_chore_merge_and_filler() {
    let raw = vec![
        "- Faster chat switching".to_string(),
        "• In-app automatic updater".to_string(),
        "faster chat switching".to_string(), // case-insensitive duplicate
        "ci: speed up windows workflow".to_string(),
        "chore(deps): bump ureq".to_string(),
        "test: add unit tests for parser".to_string(),
        "Add proof for lockfile integrity".to_string(),
        "Merge pull request #42 from valentinyeo/feat".to_string(),
        "Various improvements".to_string(),
        "Bug fixes".to_string(),
        "General improvements and bug fixes".to_string(),
        "Behind the scenes refactoring".to_string(),
        "[infra] cache tweaks".to_string(),
        "Clean dark theme polish".to_string(),
        "bad\x07control".to_string(),
    ];
    let filtered = filter_plain_english_notes(&raw);
    assert_eq!(
        filtered,
        vec![
            "Faster chat switching",
            "In-app automatic updater",
            "Clean dark theme polish",
        ]
    );
}

#[test]
fn notes_accumulate_newest_first_across_skipped_versions() {
    let releases = vec![
        ("v0.1.0".into(), vec!["Old baseline note".into()]),
        (
            "v0.1.2".into(),
            vec![
                "Keyboard inbox navigation".into(),
                "Shared note".into(),
                "ci: update runner".into(),
            ],
        ),
        (
            "v0.1.3-rc.1".into(),
            vec!["Prerelease note should be ignored".into()],
        ),
        (
            "v0.1.4".into(),
            vec!["Instant sidebar search".into(), "Shared note".into()],
        ),
        ("v0.1.5".into(), vec!["Future release beyond target".into()]),
    ];

    let notes = accumulate_release_notes(&releases, "0.1.0", "0.1.4");
    assert_eq!(
        notes,
        vec![
            "Instant sidebar search",
            "Shared note",
            "Keyboard inbox navigation",
        ]
    );
}

#[test]
fn release_body_markdown_notes_extraction() {
    let body = r#"
## What's Changed
- Smooth sidebar update card
* Improved SSH reconnect reliability
- test: verify sha256 sums

```text
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  multibot-windows-x64.zip
```
Full Changelog: https://github.com/valentinyeo/multibot/compare/v0.1.1...v0.1.2
"#;
    let notes = parse_release_body_notes(body);
    assert_eq!(
        notes,
        vec![
            "Smooth sidebar update card",
            "Improved SSH reconnect reliability",
        ]
    );
}

#[test]
fn rejected_versions_read_record_normalize_and_idempotency() {
    let dir = TempTestDir::new("rejected");
    let updates_dir = dir.path().join("updates");

    assert!(read_rejected_versions(&updates_dir).unwrap().is_empty());
    assert!(!is_version_rejected(&updates_dir, "v0.1.9").unwrap());

    record_rejected_version(&updates_dir, "v0.1.9").unwrap();
    record_rejected_version(&updates_dir, "0.1.9").unwrap(); // idempotent
    record_rejected_version(&updates_dir, "0.1.10").unwrap();

    assert!(is_version_rejected(&updates_dir, "0.1.9").unwrap());
    assert!(is_version_rejected(&updates_dir, "v0.1.9").unwrap());
    assert!(is_version_rejected(&updates_dir, "v0.1.10").unwrap());
    assert!(!is_version_rejected(&updates_dir, "0.1.11").unwrap());

    let listed = read_rejected_versions(&updates_dir).unwrap();
    assert_eq!(listed, vec!["0.1.9", "0.1.10"]);

    // Reject control chars / invalid semver.
    assert!(record_rejected_version(&updates_dir, "0.1.9\n0.1.12").is_err());
    assert!(record_rejected_version(&updates_dir, "").is_err());
}

// =========================================================================
// 4. Mock TcpListener GitHub Releases API Tests (10 tests)
// =========================================================================

#[test]
fn check_update_manifest_happy_path_and_headers() {
    let dir = TempTestDir::new("api-manifest-ok");
    let exe = b"MZ-new-exe-payload";
    let daemon = b"\x7fELF-new-daemon-payload";

    // Use a placeholder URL first; we'll build the real URL once server starts.
    let manifest_holder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let manifest_for_srv = Arc::clone(&manifest_holder);
    let release_holder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let release_for_srv = Arc::clone(&release_holder);

    let srv = MockHttpServer::start(move |req| match req.path.as_str() {
        "/releases/latest" => MockResponse::ok_json(release_for_srv.lock().unwrap().clone())
            .with_header("ETag", "\"etag-v012\""),
        "/assets/update-manifest.json" => {
            MockResponse::ok_json(manifest_for_srv.lock().unwrap().clone())
        }
        _ => MockResponse::status(404, "Not Found"),
    });

    let (_zip, manifest, _info) =
        make_valid_release_fixture(exe, daemon, &[], srv.url("/assets/multibot-windows-x64.zip"), "0.1.2");
    *manifest_holder.lock().unwrap() = serde_json::to_string(&manifest).unwrap();
    *release_holder.lock().unwrap() = serde_json::json!({
        "tag_name": "v0.1.2",
        "draft": false,
        "prerelease": false,
        "published_at": "2026-11-26T20:00:00Z",
        "assets": [
            {
                "name": "update-manifest.json",
                "browser_download_url": srv.url("/assets/update-manifest.json"),
                "size": 512
            },
            {
                "name": "multibot-windows-x64.zip",
                "browser_download_url": srv.url("/assets/multibot-windows-x64.zip"),
                "size": manifest.windows_x64.size_bytes
            }
        ]
    })
    .to_string();

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/releases/latest"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);

    let outcome = client
        .check_for_update(Some("\"etag-old\""), dir.path())
        .unwrap();
    match outcome {
        CheckOutcome::UpdateAvailable { release, etag } => {
            assert_eq!(etag.as_deref(), Some("\"etag-v012\""));
            assert_eq!(release.version, "0.1.2");
            assert_eq!(release.tag, "v0.1.2");
            // CI note filtered out by notes filter.
            assert_eq!(release.notes, vec!["Faster chat switching", "In-app updater"]);
            assert_eq!(release.asset.sha256, manifest.windows_x64.sha256);
        }
        other => panic!("expected UpdateAvailable, got {other:?}"),
    }

    let reqs = srv.recorded();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].headers.get("user-agent").map(String::as_str), Some("Multibot/0.1.0"));
    assert_eq!(
        reqs[0].headers.get("accept").map(String::as_str),
        Some("application/vnd.github+json")
    );
    assert_eq!(
        reqs[0].headers.get("if-none-match").map(String::as_str),
        Some("\"etag-old\"")
    );
}

#[test]
fn check_update_fallback_to_sha256sums_when_manifest_missing() {
    let dir = TempTestDir::new("api-fallback-sums");
    let exe = b"MZ-fallback-exe";
    let daemon = b"\x7fELF-fallback-daemon";

    let sums_holder = Arc::new(Mutex::new(String::new()));
    let sums_srv = Arc::clone(&sums_holder);
    let rel_holder = Arc::new(Mutex::new(String::new()));
    let rel_srv = Arc::clone(&rel_holder);

    let srv = MockHttpServer::start(move |req| match req.path.as_str() {
        "/releases/latest" => MockResponse::ok_json(rel_srv.lock().unwrap().clone())
            .with_header("ETag", "\"etag-sums\""),
        "/assets/SHA256SUMS.txt" => {
            MockResponse::ok_bytes(sums_srv.lock().unwrap().clone().into_bytes())
        }
        _ => MockResponse::status(404, "Not Found"),
    });

    let (zip_bytes, manifest, _) =
        make_valid_release_fixture(exe, daemon, &[], srv.url("/assets/multibot-windows-x64.zip"), "0.2.0");
    *sums_holder.lock().unwrap() = format!(
        "{}  multibot-windows-x64.zip\n{} *./multibot.exe\n{} multibotd-linux-x86_64\n",
        manifest.windows_x64.sha256,
        manifest.windows_x64.multibot_exe_sha256,
        manifest.windows_x64.multibotd_linux_x86_64_sha256,
    );
    *rel_holder.lock().unwrap() = serde_json::json!({
        "tag_name": "v0.2.0",
        "draft": false,
        "prerelease": false,
        "published_at": "2026-11-26T21:00:00Z",
        "body": "## Changes\n- Fallback release notes work\n- chore: skip me",
        "assets": [
            {
                "name": "SHA256SUMS.txt",
                "browser_download_url": srv.url("/assets/SHA256SUMS.txt"),
                "size": 256
            },
            {
                "name": "multibot-windows-x64.zip",
                "browser_download_url": srv.url("/assets/multibot-windows-x64.zip"),
                "size": zip_bytes.len()
            }
        ]
    })
    .to_string();

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/releases/latest"), "0.1.9", true).unwrap();
    let client = UpdateClient::new(cfg);

    let outcome = client.check_for_update(None, dir.path()).unwrap();
    match outcome {
        CheckOutcome::UpdateAvailable { release, etag } => {
            assert_eq!(etag.as_deref(), Some("\"etag-sums\""));
            assert_eq!(release.version, "0.2.0");
            assert_eq!(release.notes, vec!["Fallback release notes work"]);
            assert_eq!(release.asset.sha256, manifest.windows_x64.sha256);
            assert_eq!(
                release.asset.multibot_exe_sha256,
                manifest.windows_x64.multibot_exe_sha256
            );
            assert_eq!(
                release.asset.multibotd_linux_x86_64_sha256,
                manifest.windows_x64.multibotd_linux_x86_64_sha256
            );
        }
        other => panic!("expected UpdateAvailable via SHA256SUMS fallback, got {other:?}"),
    }
}

#[test]
fn check_update_304_not_modified() {
    let dir = TempTestDir::new("api-304");
    let srv = MockHttpServer::start(|_| MockResponse::status(304, "Not Modified"));
    let cfg = UpdateClientConfig::for_endpoint(srv.url("/releases/latest"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);

    let outcome = client
        .check_for_update(Some("\"etag-same\""), dir.path())
        .unwrap();
    assert_eq!(outcome, CheckOutcome::NotModified);
    let reqs = srv.recorded();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].headers.get("if-none-match").map(String::as_str),
        Some("\"etag-same\"")
    );
}

#[test]
fn check_update_403_and_429_rate_limit_backoff() {
    let dir = TempTestDir::new("api-ratelimit");
    let srv = MockHttpServer::start(|req| match req.path.as_str() {
        "/403" => MockResponse::status(403, "Forbidden")
            .with_header("Retry-After", "1800")
            .with_header("ETag", "\"etag-rl\""),
        "/429" => MockResponse::status(429, "Too Many Requests").with_header("Retry-After", "60"),
        _ => MockResponse::status(404, "Not Found"),
    });

    let c403 = UpdateClient::new(
        UpdateClientConfig::for_endpoint(srv.url("/403"), "0.1.0", true).unwrap(),
    );
    match c403.check_for_update(None, dir.path()).unwrap() {
        CheckOutcome::RateLimited { retry_after, etag } => {
            assert_eq!(retry_after, Duration::from_secs(1800));
            assert_eq!(etag.as_deref(), Some("\"etag-rl\""));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }

    let c429 = UpdateClient::new(
        UpdateClientConfig::for_endpoint(srv.url("/429"), "0.1.0", true).unwrap(),
    );
    match c429.check_for_update(None, dir.path()).unwrap() {
        CheckOutcome::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, MIN_RATE_LIMIT_BACKOFF);
        }
        other => panic!("expected RateLimited with 15m min, got {other:?}"),
    }
}

#[test]
fn check_update_skips_draft_prerelease_older_and_rejected_versions() {
    let dir = TempTestDir::new("api-filters");
    record_rejected_version(dir.path(), "0.1.9").unwrap();

    let srv = MockHttpServer::start(|req| {
        let json = match req.path.as_str() {
            "/draft" => serde_json::json!({ "tag_name": "v0.2.0", "draft": true, "prerelease": false, "assets": [] }),
            "/pre" => serde_json::json!({ "tag_name": "v0.2.0-rc.1", "draft": false, "prerelease": true, "assets": [] }),
            "/older" => serde_json::json!({ "tag_name": "v0.1.0", "draft": false, "prerelease": false, "assets": [] }),
            "/rejected" => serde_json::json!({ "tag_name": "v0.1.9", "draft": false, "prerelease": false, "assets": [] }),
            _ => return MockResponse::status(404, "Not Found"),
        };
        MockResponse::ok_json(json.to_string())
    });

    for path in ["/draft", "/pre", "/older", "/rejected"] {
        let cfg = UpdateClientConfig::for_endpoint(srv.url(path), "0.1.5", true).unwrap();
        let client = UpdateClient::new(cfg);
        let outcome = client.check_for_update(None, dir.path()).unwrap();
        assert!(
            matches!(outcome, CheckOutcome::UpToDate { .. }),
            "expected UpToDate for {path}, got {outcome:?}"
        );
    }
}

#[test]
fn check_update_fails_closed_on_tampered_manifest() {
    let dir = TempTestDir::new("api-tampered-manifest");
    let manifest_body = Arc::new(Mutex::new(String::new()));
    let manifest_srv = Arc::clone(&manifest_body);
    let release_body = Arc::new(Mutex::new(String::new()));
    let release_srv = Arc::clone(&release_body);

    let srv = MockHttpServer::start(move |req| match req.path.as_str() {
        "/releases/latest" => MockResponse::ok_json(release_srv.lock().unwrap().clone()),
        "/assets/update-manifest.json" => {
            MockResponse::ok_json(manifest_srv.lock().unwrap().clone())
        }
        "/assets/SHA256SUMS.txt" => MockResponse::ok_bytes(b"should never be fetched".to_vec()),
        _ => MockResponse::status(404, "Not Found"),
    });

    *release_body.lock().unwrap() = serde_json::json!({
        "tag_name": "v0.2.0",
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": "update-manifest.json",
                "browser_download_url": srv.url("/assets/update-manifest.json"),
                "size": 300
            },
            {
                "name": "SHA256SUMS.txt",
                "browser_download_url": srv.url("/assets/SHA256SUMS.txt"),
                "size": 100
            },
            {
                "name": "multibot-windows-x64.zip",
                "browser_download_url": srv.url("/assets/multibot-windows-x64.zip"),
                "size": 100
            }
        ]
    })
    .to_string();

    let valid_sha = "a".repeat(64);
    let bad_manifests = vec![
        // Wrong schema version
        ReleaseManifest {
            schema_version: 99,
            version: "0.2.0".into(),
            tag: "v0.2.0".into(),
            published_at: "2026-11-26T20:00:00Z".into(),
            notes: vec![],
            windows_x64: WindowsAssetInfo {
                asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
                url: srv.url("/assets/multibot-windows-x64.zip"),
                sha256: valid_sha.clone(),
                size_bytes: 100,
                multibot_exe_sha256: valid_sha.clone(),
                multibotd_linux_x86_64_sha256: valid_sha.clone(),
            },
        },
        // Tag/version mismatch
        ReleaseManifest {
            schema_version: 1,
            version: "0.1.9".into(),
            tag: "v0.1.9".into(),
            published_at: "2026-11-26T20:00:00Z".into(),
            notes: vec![],
            windows_x64: WindowsAssetInfo {
                asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
                url: srv.url("/assets/multibot-windows-x64.zip"),
                sha256: valid_sha.clone(),
                size_bytes: 100,
                multibot_exe_sha256: valid_sha.clone(),
                multibotd_linux_x86_64_sha256: valid_sha.clone(),
            },
        },
        // Disallowed host URL
        ReleaseManifest {
            schema_version: 1,
            version: "0.2.0".into(),
            tag: "v0.2.0".into(),
            published_at: "2026-11-26T20:00:00Z".into(),
            notes: vec![],
            windows_x64: WindowsAssetInfo {
                asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
                url: "https://evil.example.com/payload.zip".into(),
                sha256: valid_sha.clone(),
                size_bytes: 100,
                multibot_exe_sha256: valid_sha.clone(),
                multibotd_linux_x86_64_sha256: valid_sha.clone(),
            },
        },
        // Invalid SHA-256 hex
        ReleaseManifest {
            schema_version: 1,
            version: "0.2.0".into(),
            tag: "v0.2.0".into(),
            published_at: "2026-11-26T20:00:00Z".into(),
            notes: vec![],
            windows_x64: WindowsAssetInfo {
                asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
                url: srv.url("/assets/multibot-windows-x64.zip"),
                sha256: "not-a-valid-sha".into(),
                size_bytes: 100,
                multibot_exe_sha256: valid_sha.clone(),
                multibotd_linux_x86_64_sha256: valid_sha.clone(),
            },
        },
        // Empty manifest.tag must fail closed
        ReleaseManifest {
            schema_version: 1,
            version: "0.2.0".into(),
            tag: "".into(),
            published_at: "2026-11-26T20:00:00Z".into(),
            notes: vec![],
            windows_x64: WindowsAssetInfo {
                asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
                url: srv.url("/assets/multibot-windows-x64.zip"),
                sha256: valid_sha.clone(),
                size_bytes: 100,
                multibot_exe_sha256: valid_sha.clone(),
                multibotd_linux_x86_64_sha256: valid_sha.clone(),
            },
        },
        // Whitespace-only manifest.tag must fail closed
        ReleaseManifest {
            schema_version: 1,
            version: "0.2.0".into(),
            tag: "   ".into(),
            published_at: "2026-11-26T20:00:00Z".into(),
            notes: vec![],
            windows_x64: WindowsAssetInfo {
                asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
                url: srv.url("/assets/multibot-windows-x64.zip"),
                sha256: valid_sha.clone(),
                size_bytes: 100,
                multibot_exe_sha256: valid_sha.clone(),
                multibotd_linux_x86_64_sha256: valid_sha,
            },
        },
    ];

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/releases/latest"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);

    for bad in bad_manifests {
        *manifest_body.lock().unwrap() = serde_json::to_string(&bad).unwrap();
        assert!(client.check_for_update(None, dir.path()).is_err());
    }
    // Confirm we NEVER fell back to SHA256SUMS.txt when manifest existed and failed validation!
    assert!(
        srv.recorded()
            .iter()
            .all(|r| r.path != "/assets/SHA256SUMS.txt")
    );
}

#[test]
fn redirect_relative_and_allowed_multi_hop_succeeds() {
    let dir = TempTestDir::new("redirect-ok");
    let srv = MockHttpServer::start(|req| match req.path.as_str() {
        "/start" => MockResponse::status(302, "Found").with_header("Location", "/hop1"),
        "/hop1" => MockResponse::status(307, "Temporary Redirect").with_header("Location", "/final"),
        "/final" => MockResponse::status(304, "Not Modified"),
        _ => MockResponse::status(404, "Not Found"),
    });

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/start"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);
    assert_eq!(
        client.check_for_update(None, dir.path()).unwrap(),
        CheckOutcome::NotModified
    );
    let paths: Vec<String> = srv.recorded().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, vec!["/start", "/hop1", "/final"]);
}

#[test]
fn redirect_to_disallowed_host_blocked_before_connect() {
    let dir = TempTestDir::new("redirect-blocked");
    let srv = MockHttpServer::start(|req| match req.path.as_str() {
        "/evil" => MockResponse::status(302, "Found")
            .with_header("Location", "https://evil.example.com/payload.zip"),
        "/raw" => MockResponse::status(302, "Found")
            .with_header("Location", "https://raw.githubusercontent.com/a/b/main/c.zip"),
        _ => MockResponse::status(404, "Not Found"),
    });

    for path in ["/evil", "/raw"] {
        let cfg = UpdateClientConfig::for_endpoint(srv.url(path), "0.1.0", true).unwrap();
        let client = UpdateClient::new(cfg);
        let err = client.check_for_update(None, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("blocked redirect"),
            "expected blocked redirect error for {path}, got: {err}"
        );
    }
}

#[test]
fn redirect_loop_exceeding_max_hops_rejected() {
    let dir = TempTestDir::new("redirect-loop");
    let srv = MockHttpServer::start(|req| {
        let next = if req.path == "/a" { "/b" } else { "/a" };
        MockResponse::status(302, "Found").with_header("Location", next)
    });

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/a"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);
    let err = client.check_for_update(None, dir.path()).unwrap_err();
    assert!(
        err.to_string().contains("too many redirects"),
        "expected too many redirects error, got: {err}"
    );
    assert_eq!(srv.recorded().len(), MAX_REDIRECT_HOPS + 1);
}

#[test]
fn redirect_does_not_leak_api_headers_cross_origin() {
    let dir = TempTestDir::new("redirect-cross-origin");
    let target_url = Arc::new(Mutex::new(String::new()));
    let target_srv = Arc::clone(&target_url);

    let srv = MockHttpServer::start(move |req| match req.path.as_str() {
        "/origin-a" => {
            MockResponse::status(302, "Found").with_header("Location", target_srv.lock().unwrap().clone())
        }
        "/origin-b" => MockResponse::status(304, "Not Modified"),
        _ => MockResponse::status(404, "Not Found"),
    });
    // Redirect from 127.0.0.1:<port> to localhost:<port> (distinct origin, both loopback allowed).
    *target_url.lock().unwrap() = srv.localhost_url("/origin-b");

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/origin-a"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);
    assert_eq!(
        client
            .check_for_update(Some("\"secret-etag\""), dir.path())
            .unwrap(),
        CheckOutcome::NotModified
    );

    let reqs = srv.recorded();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].headers.contains_key("if-none-match"));
    assert_eq!(
        reqs[0].headers.get("accept").map(String::as_str),
        Some("application/vnd.github+json")
    );
    assert!(
        !reqs[1].headers.contains_key("if-none-match"),
        "If-None-Match leaked across origins!"
    );
    assert_ne!(
        reqs[1].headers.get("accept").map(String::as_str),
        Some("application/vnd.github+json"),
        "GitHub API Accept header leaked across origins!"
    );
}

// =========================================================================
// 5. Mock TcpListener Download & Zip Extraction Tests
// =========================================================================

#[test]
fn download_and_stage_happy_path_progress_and_allowlist_filtering() {
    let dir = TempTestDir::new("download-happy");
    let exe_bytes = b"MZ-real-windows-exe-bytes-0123456789".repeat(20);
    let daemon_bytes = b"\x7fELF-real-linux-daemon-bytes-abcdef".repeat(20);

    let zip_holder = Arc::new(Mutex::new(Vec::<u8>::new()));
    let zip_srv = Arc::clone(&zip_holder);
    let srv = MockHttpServer::start(move |req| match req.path.as_str() {
        "/download" => MockResponse::ok_bytes(zip_srv.lock().unwrap().clone()),
        _ => MockResponse::status(404, "Not Found"),
    });

    let (zip_bytes, _manifest, info) = make_valid_release_fixture(
        &exe_bytes,
        &daemon_bytes,
        &[("README.txt", b"benign extra file that must not be extracted")],
        srv.url("/download"),
        "0.2.1",
    );
    *zip_holder.lock().unwrap() = zip_bytes.clone();

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/download"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);

    let mut progress_samples = Vec::new();
    let staged = download_and_stage_update(&client, &info, dir.path(), |done, total| {
        progress_samples.push((done, total));
    })
    .unwrap();

    assert!(!progress_samples.is_empty());
    let mut prev = 0;
    for (done, total) in &progress_samples {
        assert!(*done >= prev);
        assert_eq!(*total, Some(zip_bytes.len() as u64));
        prev = *done;
    }
    assert_eq!(prev, zip_bytes.len() as u64);

    assert_eq!(fs::read(&staged.multibot_exe_path).unwrap(), exe_bytes);
    assert_eq!(fs::read(&staged.multibotd_linux_path).unwrap(), daemon_bytes);
    // Non-allowlisted README.txt must NOT be extracted, and archive.zip.part must be removed.
    assert!(!staged.staging_dir.join("README.txt").exists());
    assert!(!staged.staging_dir.join("archive.zip.part").exists());
}

#[test]
fn download_rejects_oversized_content_length_and_streaming_body() {
    let dir = TempTestDir::new("download-oversize");
    let big_body = vec![b'X'; 8192];
    let big_for_srv = big_body.clone();
    let srv = MockHttpServer::start(move |req| match req.path.as_str() {
        "/with-cl" => MockResponse::ok_bytes(big_for_srv.clone()),
        "/no-cl" => MockResponse::ok_bytes(big_for_srv.clone()).without_content_length(),
        _ => MockResponse::status(404, "Not Found"),
    });

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/with-cl"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);

    let sha = sha256_hex(&big_body);
    let make_info = |url: String| ReleaseInfo {
        version: "0.2.0".into(),
        tag: "v0.2.0".into(),
        published_at: "2026-11-26T20:00:00Z".into(),
        notes: vec![],
        asset: WindowsAssetInfo {
            asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
            url,
            sha256: sha.clone(),
            size_bytes: 1024, // lies about size in metadata
            multibot_exe_sha256: sha.clone(),
            multibotd_linux_x86_64_sha256: sha.clone(),
        },
    };

    // Content-Length > 4096 rejected immediately.
    let err1 = client
        .download_and_stage_update_with_limit(
            &make_info(srv.url("/with-cl")),
            dir.path(),
            4096,
            |_, _| {},
        )
        .unwrap_err();
    assert!(err1.to_string().contains("Content-Length"));

    // Stream without Content-Length exceeding 4096 rejected mid-stream.
    let err2 = client
        .download_and_stage_update_with_limit(
            &make_info(srv.url("/no-cl")),
            dir.path(),
            4096,
            |_, _| {},
        )
        .unwrap_err();
    assert!(err2.to_string().contains("exceeded size cap"));

    // Staging dirs must be completely cleaned up on error.
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn download_rejects_zip_sha256_mismatch_and_cleans_staging() {
    let dir = TempTestDir::new("download-bad-zip-sha");
    let srv = MockHttpServer::start(|_| MockResponse::ok_bytes(b"corrupted-zip".to_vec()));
    let cfg = UpdateClientConfig::for_endpoint(srv.url("/z"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);

    let info = ReleaseInfo {
        version: "0.2.0".into(),
        tag: "v0.2.0".into(),
        published_at: "2026-11-26T20:00:00Z".into(),
        notes: vec![],
        asset: WindowsAssetInfo {
            asset_name: WINDOWS_ZIP_ASSET_NAME.into(),
            url: srv.url("/z"),
            sha256: "f".repeat(64),
            size_bytes: 13,
            multibot_exe_sha256: "e".repeat(64),
            multibotd_linux_x86_64_sha256: "d".repeat(64),
        },
    };

    let err = client
        .download_and_stage_update(&info, dir.path(), |_, _| {})
        .unwrap_err();
    assert!(err.to_string().contains("SHA-256 mismatch"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn extract_rejects_binary_sha256_mismatch_and_cleans_staging() {
    let dir = TempTestDir::new("download-bad-bin-sha");
    let exe_bytes = b"MZ-real-exe";
    let daemon_bytes = b"\x7fELF-real-daemon";

    let zip_holder = Arc::new(Mutex::new(Vec::<u8>::new()));
    let zip_srv = Arc::clone(&zip_holder);
    let srv = MockHttpServer::start(move |_| MockResponse::ok_bytes(zip_srv.lock().unwrap().clone()));

    let (zip_bytes, _manifest, mut info) =
        make_valid_release_fixture(exe_bytes, daemon_bytes, &[], srv.url("/z"), "0.2.0");
    *zip_holder.lock().unwrap() = zip_bytes;
    // Tamper with expected exe hash while zip hash matches!
    info.asset.multibot_exe_sha256 = "0".repeat(64);

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/z"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);
    let err = client
        .download_and_stage_update(&info, dir.path(), |_, _| {})
        .unwrap_err();
    assert!(err.to_string().contains("SHA-256 mismatch"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn extract_rejects_zip_slip_and_symlink_and_duplicate_and_missing_binary() {
    let dir = TempTestDir::new("extract-adversarial");
    let exe_sha = sha256_hex(b"exe");
    let daemon_sha = sha256_hex(b"daemon");

    // 1. Zip-slip entry alongside valid binaries.
    let slip_zip = dir.path().join("slip.zip");
    fs::write(
        &slip_zip,
        make_zip_bytes(&[
            ("multibot.exe", b"exe"),
            ("multibotd-linux-x86_64", b"daemon"),
            ("../escape.txt", b"pwn"),
        ]),
    )
    .unwrap();
    assert!(extract_and_verify_archive(&slip_zip, dir.path(), &exe_sha, &daemon_sha).is_err());

    // 2. Symlink entry in archive.
    let sym_zip = dir.path().join("sym.zip");
    {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let opts = zip::write::SimpleFileOptions::default();
            writer.start_file("multibot.exe", opts).unwrap();
            writer.write_all(b"exe").unwrap();
            writer
                .add_symlink("multibotd-linux-x86_64", "/bin/sh", opts)
                .unwrap();
            writer.finish().unwrap();
        }
        fs::write(&sym_zip, cursor.into_inner()).unwrap();
    }
    assert!(extract_and_verify_archive(&sym_zip, dir.path(), &exe_sha, &daemon_sha).is_err());

    // 3. Duplicate / shadowed allowlisted entries (./multibot.exe and Windows case collision MULTIBOT.EXE).
    for shadowed in ["./multibot.exe", "MULTIBOT.EXE", "./multibotd-linux-x86_64"] {
        let dup_zip = dir.path().join("dup.zip");
        fs::write(
            &dup_zip,
            make_zip_bytes(&[
                ("multibot.exe", b"exe"),
                (shadowed, b"evil-shadow"),
                ("multibotd-linux-x86_64", b"daemon"),
            ]),
        )
        .unwrap();
        let err = extract_and_verify_archive(&dup_zip, dir.path(), &exe_sha, &daemon_sha)
            .unwrap_err();
        assert!(
            err.to_string().contains("duplicate or shadowed"),
            "expected duplicate/shadowed error for {shadowed:?}, got: {err}"
        );
    }

    // 4. Missing required daemon binary.
    let missing_zip = dir.path().join("missing.zip");
    fs::write(&missing_zip, make_zip_bytes(&[("multibot.exe", b"exe")])).unwrap();
    assert!(extract_and_verify_archive(&missing_zip, dir.path(), &exe_sha, &daemon_sha).is_err());
}

#[test]
fn extract_rejects_zip_bomb_exceeding_uncompressed_cap() {
    let dir = TempTestDir::new("zip-bomb");
    // 64 KiB of zeros compresses to ~100 bytes, well below 4 KiB zip size, but explodes past 4 KiB uncompressed cap!
    let zeros = vec![0u8; 64 * 1024];
    let daemon = b"\x7fELF-small";
    let zip_bytes = make_deflated_zip_bytes(&[
        ("multibot.exe", &zeros),
        ("multibotd-linux-x86_64", daemon),
    ]);
    assert!(
        zip_bytes.len() < 4096,
        "zip bomb fixture should be tiny compressed"
    );
    let zip_path = dir.path().join("bomb.zip");
    fs::write(&zip_path, &zip_bytes).unwrap();

    let err = crate::download::extract_and_verify_archive_with_limit(
        &zip_path,
        dir.path(),
        &sha256_hex(&zeros),
        &sha256_hex(daemon),
        4096,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("zip bomb"),
        "expected zip bomb protection error, got: {err}"
    );
}

// =========================================================================
// 6. Atomic Swapper, Rollback, CLI Forwarding & Cleanup Tests
// =========================================================================

#[test]
fn build_relaunch_args_preserves_gpu_and_strips_update_ready_and_version() {
    let preserved = vec![
        "--gpu=software".to_string(),
        "--update-ready".to_string(),
        "--update-ready=1".to_string(),
        "--version".to_string(),
        "-V".to_string(),
        "--gpu".to_string(),
        "luid:0x1234".to_string(),
    ];
    let args = build_relaunch_args(&preserved);
    assert_eq!(
        args,
        vec![
            "--gpu=software",
            "--gpu",
            "luid:0x1234",
            UPDATE_READY_ARG,
        ]
    );
}

#[test]
fn default_file_swapper_swaps_both_binaries_and_creates_backups() {
    let dir = TempTestDir::new("swapper-ok");
    let install_dir = dir.path().join("install");
    let stage_dir = dir.path().join("stage");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&stage_dir).unwrap();

    let current_exe = install_dir.join("multibot.exe");
    let current_daemon = install_dir.join("multibotd-linux-x86_64");
    let staged_exe = stage_dir.join("multibot.exe");
    let staged_daemon = stage_dir.join("multibotd-linux-x86_64");

    fs::write(&current_exe, b"old-exe").unwrap();
    fs::write(&current_daemon, b"old-daemon").unwrap();
    fs::write(&staged_exe, b"new-exe").unwrap();
    fs::write(&staged_daemon, b"new-daemon").unwrap();

    let plan = SwapPlan {
        install_dir,
        current_exe: current_exe.clone(),
        staged_exe,
        current_daemon: current_daemon.clone(),
        staged_daemon,
    };

    let outcome = DefaultFileSwapper.swap_binaries(&plan).unwrap();
    assert_eq!(fs::read(&current_exe).unwrap(), b"new-exe");
    assert_eq!(fs::read(&current_daemon).unwrap(), b"new-daemon");
    assert_eq!(fs::read(&outcome.backup_exe).unwrap(), b"old-exe");
    assert_eq!(
        fs::read(outcome.backup_daemon.as_ref().unwrap()).unwrap(),
        b"old-daemon"
    );
}

#[test]
fn default_file_swapper_rolls_back_both_binaries_on_mid_swap_daemon_error() {
    let dir = TempTestDir::new("swapper-mid-rollback");
    let install_dir = dir.path().join("install");
    let stage_dir = dir.path().join("stage");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&stage_dir).unwrap();

    let current_exe = install_dir.join("multibot.exe");
    let current_daemon = install_dir.join("multibotd-linux-x86_64");
    let staged_exe = stage_dir.join("multibot.exe");
    let staged_daemon = stage_dir.join("multibotd-linux-x86_64");

    fs::write(&current_exe, b"original-exe").unwrap();
    fs::write(&current_daemon, b"original-daemon").unwrap();
    fs::write(&staged_exe, b"new-exe").unwrap();
    fs::write(&staged_daemon, b"new-daemon").unwrap();

    // Create a non-empty directory at `multibotd-linux-x86_64.old` so backing up current_daemon fails mid-swap.
    fs::create_dir_all(install_dir.join("multibotd-linux-x86_64.old/nonempty")).unwrap();

    let plan = SwapPlan {
        install_dir,
        current_exe: current_exe.clone(),
        staged_exe,
        current_daemon: current_daemon.clone(),
        staged_daemon,
    };

    assert!(DefaultFileSwapper.swap_binaries(&plan).is_err());
    // Both original binaries must remain intact after mid-swap rollback!
    assert_eq!(fs::read(&current_exe).unwrap(), b"original-exe");
    assert_eq!(fs::read(&current_daemon).unwrap(), b"original-daemon");
}

#[test]
fn default_file_swapper_explicit_rollback_restores_both_binaries() {
    let dir = TempTestDir::new("swapper-explicit-rb");
    let install_dir = dir.path().join("install");
    let stage_dir = dir.path().join("stage");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&stage_dir).unwrap();

    let current_exe = install_dir.join("multibot.exe");
    let current_daemon = install_dir.join("multibotd-linux-x86_64");
    let staged_exe = stage_dir.join("multibot.exe");
    let staged_daemon = stage_dir.join("multibotd-linux-x86_64");

    fs::write(&current_exe, b"v1-exe").unwrap();
    fs::write(&current_daemon, b"v1-daemon").unwrap();
    fs::write(&staged_exe, b"v2-exe").unwrap();
    fs::write(&staged_daemon, b"v2-daemon").unwrap();

    let plan = SwapPlan {
        install_dir,
        current_exe: current_exe.clone(),
        staged_exe,
        current_daemon: current_daemon.clone(),
        staged_daemon,
    };

    let outcome = DefaultFileSwapper.swap_binaries(&plan).unwrap();
    assert_eq!(fs::read(&current_exe).unwrap(), b"v2-exe");
    assert_eq!(fs::read(&current_daemon).unwrap(), b"v2-daemon");

    DefaultFileSwapper.rollback(&plan, &outcome).unwrap();
    assert_eq!(fs::read(&current_exe).unwrap(), b"v1-exe");
    assert_eq!(fs::read(&current_daemon).unwrap(), b"v1-daemon");
}

struct MockLauncher {
    spawn_result: Mutex<Result<u32, String>>,
    readiness_result: Mutex<Result<bool, String>>,
    captured_args: Mutex<Vec<String>>,
}

impl MockLauncher {
    fn ok(pid: u32, ready: bool) -> Self {
        Self {
            spawn_result: Mutex::new(Ok(pid)),
            readiness_result: Mutex::new(Ok(ready)),
            captured_args: Mutex::new(Vec::new()),
        }
    }

    fn spawn_err(msg: &str) -> Self {
        Self {
            spawn_result: Mutex::new(Err(msg.to_string())),
            readiness_result: Mutex::new(Ok(false)),
            captured_args: Mutex::new(Vec::new()),
        }
    }
}

impl ProcessLauncher for MockLauncher {
    fn spawn_relaunch(&self, _exe_path: &Path, args: &[String]) -> Result<u32> {
        *self.captured_args.lock().unwrap() = args.to_vec();
        match &*self.spawn_result.lock().unwrap() {
            Ok(pid) => Ok(*pid),
            Err(msg) => bail!("{msg}"),
        }
    }

    fn verify_readiness(&self, _exe_path: &Path, _timeout_ms: u64) -> Result<bool> {
        match &*self.readiness_result.lock().unwrap() {
            Ok(ready) => Ok(*ready),
            Err(msg) => bail!("{msg}"),
        }
    }
}

struct TestPlatform {
    current_exe: PathBuf,
    updates_dir: PathBuf,
    preserved_args: Vec<String>,
    swapper: DefaultFileSwapper,
    launcher: MockLauncher,
}

impl UpdaterPlatform for TestPlatform {
    fn current_exe(&self) -> Result<PathBuf> {
        Ok(self.current_exe.clone())
    }

    fn updates_dir(&self) -> PathBuf {
        self.updates_dir.clone()
    }

    fn preserved_cli_args(&self) -> Vec<String> {
        self.preserved_args.clone()
    }

    fn file_swapper(&self) -> &dyn FileSwapper {
        &self.swapper
    }

    fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.launcher
    }
}

#[test]
fn atomic_swapper_happy_path_and_toctou_hash_check() {
    let dir = TempTestDir::new("atomic-happy");
    let install_dir = dir.path().join("install");
    let updates_dir = dir.path().join("updates");
    let staging_dir = updates_dir.join("staging-0.2.0");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&staging_dir).unwrap();

    let current_exe = install_dir.join("multibot.exe");
    let current_daemon = install_dir.join("multibotd-linux-x86_64");
    let staged_exe = staging_dir.join("multibot.exe");
    let staged_daemon = staging_dir.join("multibotd-linux-x86_64");

    fs::write(&current_exe, b"old-exe").unwrap();
    fs::write(&current_daemon, b"old-daemon").unwrap();
    fs::write(&staged_exe, b"new-exe").unwrap();
    fs::write(&staged_daemon, b"new-daemon").unwrap();

    let mut staged = StagedUpdate {
        version: "0.2.0".into(),
        tag: "v0.2.0".into(),
        staging_dir: staging_dir.clone(),
        multibot_exe_path: staged_exe.clone(),
        multibotd_linux_path: staged_daemon.clone(),
        multibot_exe_sha256: "0".repeat(64), // wrong hash -> TOCTOU must abort before touching live files
        multibotd_linux_x86_64_sha256: sha256_hex(b"new-daemon"),
        notes: vec![],
    };

    let platform = TestPlatform {
        current_exe: current_exe.clone(),
        updates_dir: updates_dir.clone(),
        preserved_args: vec!["--gpu=software".into(), "--update-ready".into()],
        swapper: DefaultFileSwapper,
        launcher: MockLauncher::ok(4242, true),
    };

    let swapper = AtomicSwapper::new(500);
    assert!(swapper.apply_staged_update(&platform, &staged).is_err());
    assert_eq!(fs::read(&current_exe).unwrap(), b"old-exe");

    // Now fix expected exe hash -> succeeds and forwards --gpu=software + --update-ready.
    staged.multibot_exe_sha256 = sha256_hex(b"new-exe");
    let result = swapper.apply_staged_update(&platform, &staged).unwrap();
    assert_eq!(result.launched_pid, 4242);
    assert_eq!(result.launch_args, vec!["--gpu=software", UPDATE_READY_ARG]);
    assert_eq!(fs::read(&current_exe).unwrap(), b"new-exe");
    assert_eq!(fs::read(&current_daemon).unwrap(), b"new-daemon");
    assert!(!is_version_rejected(&updates_dir, "0.2.0").unwrap());
}

#[test]
fn installed_app_updates_both_siblings_from_manifest_and_preserves_uninstaller() {
    let dir = TempTestDir::new("per-user-install");
    let install_dir = dir.path().join("Test User/AppData/Local/Multibot");
    let updates_dir = dir
        .path()
        .join("Test User/AppData/Roaming/Multibot/updates");
    fs::create_dir_all(&install_dir).unwrap();
    let current_exe = install_dir.join("multibot.exe");
    let current_daemon = install_dir.join("multibotd-linux-x86_64");
    fs::write(&current_exe, b"old-exe").unwrap();
    fs::write(&current_daemon, b"old-daemon").unwrap();
    fs::write(install_dir.join("unins000.exe"), b"uninstaller").unwrap();

    let zip_holder = Arc::new(Mutex::new(Vec::new()));
    let zip_srv = Arc::clone(&zip_holder);
    let srv =
        MockHttpServer::start(move |_| MockResponse::ok_bytes(zip_srv.lock().unwrap().clone()));
    let (zip_bytes, manifest, _) = make_valid_release_fixture(
        b"new-exe",
        b"new-daemon",
        &[],
        srv.url("/multibot-windows-x64.zip"),
        "0.2.1",
    );
    *zip_holder.lock().unwrap() = zip_bytes;
    let parsed = parse_release_manifest(&serde_json::to_string(&manifest).unwrap()).unwrap();
    let sums = parse_sha256sums(&format!(
        "{}  multibot-windows-x64.zip\r\n{}  Multibot-Setup-x64.exe\r\n",
        parsed.windows_x64.sha256,
        sha256_hex(b"installer"),
    ))
    .unwrap();
    assert_eq!(sums[WINDOWS_ZIP_ASSET_NAME], parsed.windows_x64.sha256);
    let client = UpdateClient::new(
        UpdateClientConfig::for_endpoint(srv.url("/latest"), "0.1.0", true).unwrap(),
    );
    let staged =
        download_and_stage_update(&client, &ReleaseInfo::from(parsed), &updates_dir, |_, _| {})
            .unwrap();
    let platform = TestPlatform {
        current_exe: current_exe.clone(),
        updates_dir: updates_dir.clone(),
        preserved_args: vec!["--gpu=software".into()],
        swapper: DefaultFileSwapper,
        launcher: MockLauncher::ok(4242, true),
    };
    let result = AtomicSwapper::new(500)
        .apply_staged_update(&platform, &staged)
        .unwrap();
    assert_eq!(result.launched_pid, 4242);
    assert_eq!(result.launch_args, vec!["--gpu=software", UPDATE_READY_ARG]);
    assert_eq!(
        result.outcome.backup_exe.parent(),
        Some(install_dir.as_path())
    );
    assert_eq!(
        result.outcome.backup_daemon.as_ref().unwrap().parent(),
        Some(install_dir.as_path())
    );
    assert_eq!(fs::read(&current_exe).unwrap(), b"new-exe");
    assert_eq!(fs::read(&current_daemon).unwrap(), b"new-daemon");
    cleanup_old_update_artifacts(&install_dir, &updates_dir).unwrap();
    assert!(!staged.staging_dir.exists());
    assert!(!result.outcome.backup_exe.exists());
    assert!(!result.outcome.backup_daemon.unwrap().exists());
    assert_eq!(
        fs::read(install_dir.join("unins000.exe")).unwrap(),
        b"uninstaller"
    );
}

#[test]
fn atomic_swapper_rolls_back_and_records_rejected_on_spawn_error_or_readiness_timeout() {
    // Case A: spawn_relaunch returns error -> rollback + rejected.txt.
    {
        let dir = TempTestDir::new("atomic-spawn-err");
        let install_dir = dir.path().join("install");
        let updates_dir = dir.path().join("updates");
        let staging_dir = updates_dir.join("staging-0.2.5");
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let current_exe = install_dir.join("multibot.exe");
        let current_daemon = install_dir.join("multibotd-linux-x86_64");
        let staged_exe = staging_dir.join("multibot.exe");
        let staged_daemon = staging_dir.join("multibotd-linux-x86_64");

        fs::write(&current_exe, b"good-exe").unwrap();
        fs::write(&current_daemon, b"good-daemon").unwrap();
        fs::write(&staged_exe, b"bad-exe").unwrap();
        fs::write(&staged_daemon, b"bad-daemon").unwrap();

        let staged = StagedUpdate {
            version: "0.2.5".into(),
            tag: "v0.2.5".into(),
            staging_dir,
            multibot_exe_path: staged_exe,
            multibotd_linux_path: staged_daemon,
            multibot_exe_sha256: sha256_hex(b"bad-exe"),
            multibotd_linux_x86_64_sha256: sha256_hex(b"bad-daemon"),
            notes: vec![],
        };
        let platform = TestPlatform {
            current_exe: current_exe.clone(),
            updates_dir: updates_dir.clone(),
            preserved_args: vec![],
            swapper: DefaultFileSwapper,
            launcher: MockLauncher::spawn_err("OS refused to spawn"),
        };

        assert!(AtomicSwapper::new(200).apply_staged_update(&platform, &staged).is_err());
        assert_eq!(fs::read(&current_exe).unwrap(), b"good-exe");
        assert_eq!(fs::read(&current_daemon).unwrap(), b"good-daemon");
        assert!(is_version_rejected(&updates_dir, "0.2.5").unwrap());
    }

    // Case B: readiness returns false (timeout) -> rollback + rejected.txt.
    {
        let dir = TempTestDir::new("atomic-ready-timeout");
        let install_dir = dir.path().join("install");
        let updates_dir = dir.path().join("updates");
        let staging_dir = updates_dir.join("staging-0.2.6");
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let current_exe = install_dir.join("multibot.exe");
        let current_daemon = install_dir.join("multibotd-linux-x86_64");
        let staged_exe = staging_dir.join("multibot.exe");
        let staged_daemon = staging_dir.join("multibotd-linux-x86_64");

        fs::write(&current_exe, b"good-exe").unwrap();
        fs::write(&current_daemon, b"good-daemon").unwrap();
        fs::write(&staged_exe, b"hanging-exe").unwrap();
        fs::write(&staged_daemon, b"hanging-daemon").unwrap();

        let staged = StagedUpdate {
            version: "0.2.6".into(),
            tag: "v0.2.6".into(),
            staging_dir,
            multibot_exe_path: staged_exe,
            multibotd_linux_path: staged_daemon,
            multibot_exe_sha256: sha256_hex(b"hanging-exe"),
            multibotd_linux_x86_64_sha256: sha256_hex(b"hanging-daemon"),
            notes: vec![],
        };
        let platform = TestPlatform {
            current_exe: current_exe.clone(),
            updates_dir: updates_dir.clone(),
            preserved_args: vec!["--gpu=software".into()],
            swapper: DefaultFileSwapper,
            launcher: MockLauncher::ok(777, false),
        };

        assert!(AtomicSwapper::new(200).apply_staged_update(&platform, &staged).is_err());
        assert_eq!(fs::read(&current_exe).unwrap(), b"good-exe");
        assert_eq!(fs::read(&current_daemon).unwrap(), b"good-daemon");
        assert!(is_version_rejected(&updates_dir, "0.2.6").unwrap());
    }
}

#[test]
fn readiness_marker_signal_and_verify_roundtrip() {
    let dir = TempTestDir::new("readiness-marker");
    let exe_path = dir.path().join("multibot.exe");
    fs::write(&exe_path, b"exe").unwrap();

    // Without marker -> verify_readiness times out quickly as false.
    assert!(!DefaultProcessLauncher.verify_readiness(&exe_path, 40).unwrap());

    // After signal_update_ready -> verify_readiness returns true and deletes marker.
    signal_update_ready(&exe_path).unwrap();
    assert!(readiness_marker_path(&exe_path).exists());
    assert!(DefaultProcessLauncher.verify_readiness(&exe_path, 200).unwrap());
    assert!(!readiness_marker_path(&exe_path).exists());
}

#[test]
fn cleanup_old_update_artifacts_removes_stale_files_and_staging_but_keeps_rejected_txt() {
    let dir = TempTestDir::new("cleanup");
    let exe_dir = dir.path().join("install");
    let updates_dir = dir.path().join("updates");
    let staging = updates_dir.join("staging-0.2.0-123-0");
    fs::create_dir_all(&exe_dir).unwrap();
    fs::create_dir_all(&staging).unwrap();

    fs::write(exe_dir.join("multibot.exe"), b"live").unwrap();
    fs::write(exe_dir.join("multibot.exe.old"), b"stale1").unwrap();
    fs::write(exe_dir.join("multibot.exe.previous"), b"stale2").unwrap();
    fs::write(exe_dir.join("multibotd-linux-x86_64.old"), b"stale3").unwrap();
    fs::write(staging.join("multibot.exe"), b"staged").unwrap();
    record_rejected_version(&updates_dir, "0.1.99").unwrap();

    let removed = cleanup_old_update_artifacts(&exe_dir, &updates_dir).unwrap();
    assert!(removed >= 4);
    assert!(exe_dir.join("multibot.exe").exists());
    assert!(!exe_dir.join("multibot.exe.old").exists());
    assert!(!exe_dir.join("multibot.exe.previous").exists());
    assert!(!exe_dir.join("multibotd-linux-x86_64.old").exists());
    assert!(!staging.exists());
    assert!(is_version_rejected(&updates_dir, "0.1.99").unwrap());
}

#[test]
fn ci_emitted_update_manifest_parses_and_passes_notes_filter() {
    let ci_manifest_json = r#"{
  "schema_version": 1,
  "version": "0.1.42",
  "tag": "v0.1.42",
  "published_at": "2026-11-26T21:17:04Z",
  "notes": [
    "Performance and stability improvements"
  ],
  "windows_x64": {
    "asset_name": "multibot-windows-x64.zip",
    "url": "https://github.com/valentinyeo/multibot/releases/download/v0.1.42/multibot-windows-x64.zip",
    "sha256": "11d43ce6792a16803b8589b78ec3bde79f9b48041770e874beeb44722a195ce4",
    "size_bytes": 262,
    "multibot_exe_sha256": "3473c9f28b5fe0723bb646d560cc6abaee0b3b7f794d03c2e8d60553964ad40c",
    "multibotd_linux_x86_64_sha256": "7c6f07c51ce3d31726a61117d1848db6f62b6cb7328ffd82cad156f32054f6d7"
  }
}"#;
    let parsed = parse_release_manifest(ci_manifest_json).unwrap();
    assert_eq!(parsed.schema_version, MANIFEST_SCHEMA_VERSION);
    assert_eq!(parsed.version, "0.1.42");
    assert_eq!(parsed.tag, "v0.1.42");
    assert!(is_allowed_https_url_with_local(&parsed.windows_x64.url, false));
    assert_eq!(
        filter_plain_english_notes(&parsed.notes),
        vec!["Performance and stability improvements"]
    );
}

// =========================================================================
// 7. Iteration 2 Hardening Regression Tests
// =========================================================================

#[test]
fn parse_sha256sums_fails_closed_on_duplicate_entries() {
    let h1 = "a".repeat(64);
    let h2 = "b".repeat(64);

    // Duplicate exact filename with identical hash.
    let dup_same = format!("{h1}  multibot.exe\n{h1}  multibot.exe\n");
    let err1 = parse_sha256sums(&dup_same).unwrap_err();
    assert!(
        err1.to_string().contains("duplicate entry"),
        "expected duplicate error, got: {err1}"
    );

    // Duplicate exact filename with conflicting hash.
    let dup_conflict = format!("{h1}  multibot.exe\n{h2}  multibot.exe\n");
    let err2 = parse_sha256sums(&dup_conflict).unwrap_err();
    assert!(
        err2.to_string().contains("duplicate entry"),
        "expected duplicate error, got: {err2}"
    );

    // Normalized duplicates (`./multibot.exe` vs `*multibot.exe` and `*./multibot.exe`).
    let dup_norm = format!("{h1}  ./multibot.exe\n{h2}  *multibot.exe\n");
    let err3 = parse_sha256sums(&dup_norm).unwrap_err();
    assert!(
        err3.to_string().contains("duplicate entry"),
        "expected duplicate error for normalized prefix collision, got: {err3}"
    );

    let dup_norm2 = format!("{h1}  *./multibot.exe\n{h2}  multibot.exe\n");
    assert!(parse_sha256sums(&dup_norm2).is_err());
}

#[test]
fn notes_reject_forbidden_duration_wording_backticks_and_control_chars() {
    let forbidden = [
        "Update takes 5 seconds",
        "Reloads in ~3s",
        "Downloads in ~2m",
        "Startup in 5min or 30s",
        "ETA: 2 minutes",
        "Ready in a\x0bfew moments",
        "Completes in 4 m",
        "Done in 10 secs",
        "Markdown break ``` Evil injection",
        "Fix CI build runner",
        "Line with\nnewline",
        "Line with\rcarriage",
    ];
    for line in forbidden {
        assert!(
            !is_user_facing_note(line),
            "expected forbidden note to be rejected: {line:?}"
        );
    }

    // Legitimate notes (including `<16ms` and words containing substrings like metadata/minimal/secondary).
    let allowed = [
        "Sub-16ms chat switching",
        "Details and metadata polish",
        "Minimal dark UI refinements",
        "Secondary actions in palette",
        "Progress reaches ~50% smoothly",
    ];
    for line in allowed {
        assert!(
            is_user_facing_note(line),
            "expected safe note to be allowed: {line:?}"
        );
    }

    // Verify UpdateClient::check_for_update never leaks duration wording or code fences into ReleaseInfo.notes.
    let dir = TempTestDir::new("notes-leak-guard");
    let exe = b"MZ-notes-exe";
    let daemon = b"\x7fELF-notes-daemon";
    let manifest_holder = Arc::new(Mutex::new(String::new()));
    let manifest_srv = Arc::clone(&manifest_holder);
    let release_holder = Arc::new(Mutex::new(String::new()));
    let release_srv = Arc::clone(&release_holder);

    let srv = MockHttpServer::start(move |req| match req.path.as_str() {
        "/releases/latest" => MockResponse::ok_json(release_srv.lock().unwrap().clone()),
        "/assets/update-manifest.json" => {
            MockResponse::ok_json(manifest_srv.lock().unwrap().clone())
        }
        _ => MockResponse::status(404, "Not Found"),
    });

    let (_zip, mut manifest, _) = make_valid_release_fixture(
        exe,
        daemon,
        &[],
        srv.url("/assets/multibot-windows-x64.zip"),
        "0.2.0",
    );
    manifest.notes = vec![
        "Update takes 5 seconds".into(),
        "Reloads in ~3s".into(),
        "Downloads in ~2m".into(),
        "Startup in 5min or 30s".into(),
        "ETA: 2 minutes".into(),
        "Markdown break ``` Evil injection".into(),
        "Faster sidebar search".into(),
    ];
    *manifest_holder.lock().unwrap() = serde_json::to_string(&manifest).unwrap();
    *release_holder.lock().unwrap() = serde_json::json!({
        "tag_name": "v0.2.0",
        "draft": false,
        "prerelease": false,
        "published_at": "2026-11-26T20:00:00Z",
        "assets": [
            {
                "name": "update-manifest.json",
                "browser_download_url": srv.url("/assets/update-manifest.json"),
                "size": 512
            }
        ]
    })
    .to_string();

    let cfg = UpdateClientConfig::for_endpoint(srv.url("/releases/latest"), "0.1.0", true).unwrap();
    let client = UpdateClient::new(cfg);
    match client.check_for_update(None, dir.path()).unwrap() {
        CheckOutcome::UpdateAvailable { release, .. } => {
            assert_eq!(release.notes, vec!["Faster sidebar search"]);
        }
        other => panic!("expected UpdateAvailable, got {other:?}"),
    }
}

#[test]
fn url_whitespace_rejection_and_https_override_disables_local_http() {
    // Leading/trailing whitespace rejected before Url::parse.
    assert!(!is_allowed_https_url_with_local(
        " https://github.com/valentinyeo/multibot",
        false
    ));
    assert!(!is_allowed_https_url_with_local(
        "https://github.com/valentinyeo/multibot ",
        false
    ));
    assert!(!is_allowed_https_url_with_local(
        " https://github.com/valentinyeo/multibot ",
        true
    ));
    assert!(!is_allowed_https_url_with_local(
        " http://127.0.0.1:8080/a",
        true
    ));

    // HTTPS override MUST NOT enable allow_local_http!
    let https_cfg = UpdateClientConfig::from_override(
        Some("https://api.github.com/repos/valentinyeo/multibot/releases/latest"),
        "0.1.0",
    )
    .unwrap();
    assert!(!https_cfg.allow_local_http);
    assert_eq!(
        https_cfg.endpoint_url,
        "https://api.github.com/repos/valentinyeo/multibot/releases/latest"
    );

    // Unset / blank override MUST NOT enable allow_local_http!
    let none_cfg = UpdateClientConfig::from_override(None, "0.1.0").unwrap();
    assert!(!none_cfg.allow_local_http);
    let blank_cfg = UpdateClientConfig::from_override(Some("   "), "0.1.0").unwrap();
    assert!(!blank_cfg.allow_local_http);

    // Plaintext loopback http:// override DOES enable allow_local_http.
    let http_cfg =
        UpdateClientConfig::from_override(Some("http://127.0.0.1:9000/latest"), "0.1.0").unwrap();
    assert!(http_cfg.allow_local_http);
    assert_eq!(http_cfg.endpoint_url, "http://127.0.0.1:9000/latest");

    let localhost_cfg =
        UpdateClientConfig::from_override(Some("http://localhost:9000/latest"), "0.1.0").unwrap();
    assert!(localhost_cfg.allow_local_http);
}

#[test]
fn extract_and_verify_archive_direct_call_cleans_up_unverified_files_on_error() {
    let dir = TempTestDir::new("extract-direct-cleanup");
    let staging = dir.path().join("staging");
    fs::create_dir_all(&staging).unwrap();

    let exe_bytes = b"MZ-tampered-exe-bytes";
    let daemon_bytes = b"\x7fELF-valid-daemon-bytes";
    let zip_path = dir.path().join("update.zip");
    fs::write(
        &zip_path,
        make_zip_bytes(&[
            ("multibot.exe", exe_bytes),
            ("multibotd-linux-x86_64", daemon_bytes),
        ]),
    )
    .unwrap();

    // Case 1: tampered multibot.exe hash -> Err and multibot.exe must NOT remain in staging_dir.
    let err1 = extract_and_verify_archive(
        &zip_path,
        &staging,
        &"0".repeat(64),
        &sha256_hex(daemon_bytes),
    )
    .unwrap_err();
    assert!(err1.to_string().contains("SHA-256 mismatch"));
    assert!(
        !staging.join("multibot.exe").exists(),
        "unverified multibot.exe leaked in staging_dir"
    );
    assert!(!staging.join("multibotd-linux-x86_64").exists());

    // Case 2: valid multibot.exe hash, tampered daemon hash -> Err and BOTH files removed!
    let err2 = extract_and_verify_archive(
        &zip_path,
        &staging,
        &sha256_hex(exe_bytes),
        &"f".repeat(64),
    )
    .unwrap_err();
    assert!(err2.to_string().contains("SHA-256 mismatch"));
    assert!(
        !staging.join("multibot.exe").exists(),
        "previously extracted multibot.exe must be cleaned up on daemon hash failure"
    );
    assert!(
        !staging.join("multibotd-linux-x86_64").exists(),
        "unverified multibotd-linux-x86_64 leaked in staging_dir"
    );
}

#[test]
fn rejected_versions_strip_semver_build_metadata() {
    let dir = TempTestDir::new("rejected-build-meta");
    let updates_dir = dir.path().join("updates");

    record_rejected_version(&updates_dir, "0.2.0").unwrap();
    assert!(is_version_rejected(&updates_dir, "0.2.0").unwrap());
    assert!(is_version_rejected(&updates_dir, "v0.2.0+build.1").unwrap());
    assert!(is_version_rejected(&updates_dir, "0.2.0+sha.deadbeef").unwrap());
    // Recording with +build metadata is idempotent and does not duplicate.
    record_rejected_version(&updates_dir, "v0.2.0+build.2").unwrap();
    assert_eq!(read_rejected_versions(&updates_dir).unwrap(), vec!["0.2.0"]);

    // Recording a new version with +build metadata stores stripped canonical form.
    record_rejected_version(&updates_dir, "v0.3.1+ci.42").unwrap();
    assert!(is_version_rejected(&updates_dir, "0.3.1").unwrap());
    assert!(is_version_rejected(&updates_dir, "v0.3.1+other").unwrap());
    assert_eq!(
        read_rejected_versions(&updates_dir).unwrap(),
        vec!["0.2.0", "0.3.1"]
    );
}

#[test]
fn rate_limit_backoff_parses_rfc9110_imf_fixdate_retry_after() {
    // Wed, 21 Oct 2026 09:28:00 GMT == 1_792_574_880 Unix seconds;
    // at now = 1_792_567_680, delta is exactly 7200s (2 hours).
    assert_eq!(
        parse_rate_limit_backoff(
            Some("Wed, 21 Oct 2026 09:28:00 GMT"),
            None,
            1_792_567_680
        ),
        Duration::from_secs(7200)
    );

    // Past HTTP-date -> clamped to 900s (MIN_RATE_LIMIT_BACKOFF).
    assert_eq!(
        parse_rate_limit_backoff(
            Some("Wed, 21 Oct 2026 09:28:00 GMT"),
            None,
            1_792_580_000
        ),
        MIN_RATE_LIMIT_BACKOFF
    );

    // Far-future HTTP-date -> clamped to 24h (MAX_RATE_LIMIT_BACKOFF).
    assert_eq!(
        parse_rate_limit_backoff(
            Some("Sat, 24 Oct 2026 09:28:00 GMT"),
            None,
            1_792_567_680
        ),
        MAX_RATE_LIMIT_BACKOFF
    );

    // Leap day 2024-02-29 12:00:00 GMT == 1_709_208_000; at now = 1_709_204_400 -> 3600s.
    assert_eq!(
        parse_rate_limit_backoff(
            Some("Thu, 29 Feb 2024 12:00:00 GMT"),
            None,
            1_709_204_400
        ),
        Duration::from_secs(3600)
    );

    // Malformed HTTP-dates (non-leap Feb 29, wrong timezone, garbage) -> 900s fallback.
    for bad in [
        "not-a-date",
        "Sun, 29 Feb 2026 09:28:00 GMT",
        "Wed, 21 Oct 2026 09:28:00 PST",
        "Wed, 32 Oct 2026 09:28:00 GMT",
        "Wed, 21 Oct 2026 25:00:00 GMT",
    ] {
        assert_eq!(
            parse_rate_limit_backoff(Some(bad), None, 1_792_567_680),
            MIN_RATE_LIMIT_BACKOFF,
            "expected malformed HTTP-date {bad:?} to fall back to MIN_RATE_LIMIT_BACKOFF"
        );
    }
}

#[test]
fn default_file_swapper_rejects_directory_at_current_daemon_without_renaming_to_old() {
    let dir = TempTestDir::new("swapper-daemon-dir");
    let install_dir = dir.path().join("install");
    let stage_dir = dir.path().join("stage");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&stage_dir).unwrap();

    let current_exe = install_dir.join("multibot.exe");
    let current_daemon = install_dir.join("multibotd-linux-x86_64");
    let staged_exe = stage_dir.join("multibot.exe");
    let staged_daemon = stage_dir.join("multibotd-linux-x86_64");

    fs::write(&current_exe, b"live-exe").unwrap();
    fs::create_dir(&current_daemon).unwrap();
    fs::write(&staged_exe, b"new-exe").unwrap();
    fs::write(&staged_daemon, b"new-daemon").unwrap();

    let plan = SwapPlan {
        install_dir: install_dir.clone(),
        current_exe: current_exe.clone(),
        staged_exe,
        current_daemon: current_daemon.clone(),
        staged_daemon,
    };

    let err = DefaultFileSwapper.swap_binaries(&plan).unwrap_err();
    assert!(
        err.to_string().contains("not a regular file"),
        "expected non-regular-file error, got: {err}"
    );
    assert_eq!(fs::read(&current_exe).unwrap(), b"live-exe");
    assert!(current_daemon.is_dir());
    assert!(!install_dir.join("multibotd-linux-x86_64.old").exists());
    assert!(!install_dir.join("multibot.exe.old").exists());
}

#[test]
fn unicode_whitespace_duration_wording_is_rejected_everywhere() {
    let cases = [
        "Installs in a\u{00A0}few clicks",
        "Done in 5\u{00A0}s",
        "Done in 2\u{00A0}m",
        "Done in ~3\u{00A0}s",
        "Done in ~\u{00A0}3\u{00A0}s",
        "Ready in a\u{2009}few clicks",
        "Completes in 5\u{3000}s",
        "Restarts in ~\u{2009}2\u{3000}m",
    ];
    for line in cases {
        assert!(
            contains_forbidden_duration_wording(line),
            "expected Unicode-whitespace duration wording to be rejected: {line:?}"
        );
        assert!(
            !is_user_facing_note(line),
            "expected is_user_facing_note to reject: {line:?}"
        );
    }

    let mut raw: Vec<String> = cases.iter().map(|s| (*s).to_string()).collect();
    raw.push("Dark OLED — instant switching".to_string());
    raw.push("Sub-16ms chat switching".to_string());
    raw.push("Progress reaches ~50% smoothly".to_string());
    let filtered = filter_plain_english_notes(&raw);
    assert_eq!(
        filtered,
        vec![
            "Dark OLED — instant switching",
            "Sub-16ms chat switching",
            "Progress reaches ~50% smoothly",
        ]
    );
}

#[test]
fn unicode_control_separators_and_bidi_overrides_are_rejected_in_notes() {
    let malicious = [
        "C1 next-line \u{0085} spoofed note",
        "\u{0085}Leading C1 control",
        "Line separator \u{2028} second line",
        "Paragraph separator \u{2029} injected",
        "Bidi RLO \u{202E} exe.tobitlum",
        "Bidi LRE \u{202A} text",
        "Bidi LRI \u{2066} text",
        "Bidi PDI \u{2069} text",
        "Embedded\ttab inside note",
    ];
    for line in malicious {
        assert!(
            notes::clean_note_line(line).is_none(),
            "expected clean_note_line to reject: {line:?}"
        );
        assert!(
            !is_user_facing_note(line),
            "expected is_user_facing_note to reject: {line:?}"
        );
    }

    // Leading/trailing tab is trimmed by clean_note_line (preserving tab-indented bullet lines),
    // while multi-bullet infra noise is iteratively stripped and rejected.
    assert_eq!(
        notes::clean_note_line("\t- Clean sidebar polish\t"),
        Some("Clean sidebar polish".to_string())
    );
    let filtered = filter_plain_english_notes(&[
        "- - [infra] adjust build cache".to_string(),
        "C1 next-line \u{0085} spoofed note".to_string(),
        "Bidi RLO \u{202E} spoof".to_string(),
        "- * Clean sidebar polish".to_string(),
    ]);
    assert_eq!(filtered, vec!["Clean sidebar polish"]);
}

#[test]
fn has_forbidden_control_or_bidi_rejects_controls_separators_and_bidi_while_allowing_zwj_emoji() {
    let forbidden = [
        // C0 controls (including NUL, BEL, TAB, LF, VT, FF, CR, ESC, DEL)
        "nul\0byte",
        "bell\x07char",
        "embedded\ttab",
        "multi\nline",
        "\nleading newline",
        "vertical\x0btab",
        "form\x0cfeed",
        "carriage\rreturn",
        "ANSI \x1b[31mred",
        "del\x7fchar",
        // C1 controls (including NEL \u{0085} embedded and leading)
        "C1 \u{0080} pad",
        "C1 next-line \u{0085} spoofed",
        "\u{0085}Leading C1",
        "C1 \u{009F} apc",
        // Unicode line and paragraph separators
        "Line sep \u{2028} injected",
        "\u{2028}Leading line sep",
        "Para sep \u{2029} injected",
        "\u{2029}Leading para sep",
        // Bidi embeddings, overrides, and isolates (\u{202A}..=\u{202E}, \u{2066}..=\u{2069})
        "LRE \u{202A} text",
        "RLE \u{202B} text",
        "PDF \u{202C} text",
        "LRO \u{202D} text",
        "RLO \u{202E} exe.tobitlum",
        "LRI \u{2066} text \u{2069}",
        "RLI \u{2067} text \u{2069}",
        "FSI \u{2068} text \u{2069}",
        "PDI \u{2069} trailing",
    ];
    for sample in forbidden {
        assert!(
            has_forbidden_control_or_bidi(sample),
            "expected has_forbidden_control_or_bidi to reject: {sample:?}"
        );
    }

    let allowed = [
        "Clean plain-English release note",
        "Dark OLED — instant switching",
        "Team mode 👩‍💻",
        "Family workspaces 👨‍👩‍👧",
        "Compound emoji 👩‍💻 & 👨‍👩‍👧 stay intact",
        "Sub-16ms chat switching",
    ];
    for sample in allowed {
        assert!(
            !has_forbidden_control_or_bidi(sample),
            "expected has_forbidden_control_or_bidi to allow: {sample:?}"
        );
    }
}

#[test]
fn zwsp_and_bom_duration_wording_are_rejected_without_disturbing_zwj_emoji_or_clean_notes() {
    let forbidden_cases = [
        "Installs in a\u{200B}few clicks",
        "Done in 5\u{200B}s",
        "a\u{200B}few",
        "5\u{200B}s",
        "Restarts in ~3\u{200B}s",
        "Downloads in ~\u{200B}2\u{200B}m",
        "Ready in a\u{FEFF}few clicks",
        "Done in 5\u{FEFF}s",
        "a\u{FEFF}few",
        "5\u{FEFF}s",
        "Restarts in ~3\u{FEFF}s",
        "Completes in 10\u{FEFF}sec",
        "Finishes in 2\u{200B}min",
    ];
    for line in forbidden_cases {
        assert!(
            contains_forbidden_duration_wording(line),
            "expected ZWSP/BOM duration wording to be detected: {line:?}"
        );
        assert!(
            !is_user_facing_note(line),
            "expected is_user_facing_note to reject ZWSP/BOM duration wording: {line:?}"
        );
    }

    let clean_notes = [
        "Team mode 👩‍💻",
        "Family workspaces 👨‍👩‍👧",
        "Sub-16ms chat switching",
        "Details and metadata polish",
        "Minimal dark UI refinements",
        "Secondary actions in palette",
        "Progress reaches ~50% smoothly",
    ];
    for line in clean_notes {
        assert!(
            !contains_forbidden_duration_wording(line),
            "unexpected false positive on clean note: {line:?}"
        );
        assert!(
            is_user_facing_note(line),
            "expected clean note to pass is_user_facing_note: {line:?}"
        );
    }

    let mut raw: Vec<String> = forbidden_cases.iter().map(|s| (*s).to_string()).collect();
    raw.extend(clean_notes.iter().map(|s| (*s).to_string()));
    let filtered = filter_plain_english_notes(&raw);
    let expected: Vec<String> = clean_notes.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(filtered, expected);
}


