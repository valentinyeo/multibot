//! Read-only view of which chats are live right now.
//!
//! The legacy `multiprompt` runtime keeps one record per chat under
//! `~/.local/state/multiprompt/agent-host/` and publishes display names in
//! `chat-names.json`. A chat is live only while its host process runs or its terminal session
//! exists (`src/mp_send.py::live_chats`, SOFT-3093). Reading those records turns 18k stale
//! transcripts into the ~60 chats a person is actually working in.
//!
//! A `.config.json` is written once per chat ever launched, so the 165 files here outnumber
//! the 66 state files that still name a running host. Only the state records (or a published
//! name backed by one) count, otherwise dead chats leak into the listing.
//!
//! Hard Invariant #2: this module only ever **reads** legacy files. It never creates,
//! writes, or deletes anything under `~/.local/state/multiprompt`.

use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Names of the transcript-id fields a legacy record can carry. A chat stores its current
/// session id under one of them depending on how it was launched or switched providers, the
/// legacy archive list writes the Claude id as `csid`, and the Antigravity conversation id
/// arrives as `conversation` in `chat-names.json`.
const ID_FIELDS: [&str; 5] = [
    "sid",
    "native_session",
    "context_session",
    "csid",
    "conversation",
];

/// Upper bound on the legacy archive list we will read (one entry per archived chat).
const MAX_LEGACY_ARCHIVE_ENTRIES: usize = 4_000;

/// The legacy records change at human speed, so one snapshot serves every discovery call
/// in this window. Without it each transcript append would re-read 1.7 MiB and spawn tmux.
const SNAPSHOT_TTL: Duration = Duration::from_secs(2);

static SNAPSHOT: std::sync::Mutex<Option<(PathBuf, std::time::Instant, LiveActivity)>> =
    std::sync::Mutex::new(None);

/// Live chats plus the legacy archive, all as transcript ids and terminal names.
#[derive(Debug, Default, Clone)]
pub struct LiveActivity {
    /// Transcript ids of chats whose host process or terminal session is alive.
    pub ids: HashSet<String>,
    /// Terminal (tmux) names of the same chats, when the record carries one.
    pub tmux_names: HashSet<String>,
    /// Transcript ids the legacy desktop archived.
    pub legacy_archived_ids: HashSet<String>,
    /// Terminal names the legacy desktop archived.
    pub legacy_archived_tmux: HashSet<String>,
}

impl LiveActivity {
    /// Cached [`LiveActivity::load`] for hot paths (directory watcher, repeated listings).
    pub fn cached(home: &Path) -> Self {
        let mut slot = SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_home, at, activity)) = slot.as_ref()
            && cached_home == home
            && at.elapsed() <= SNAPSHOT_TTL
        {
            return activity.clone();
        }
        let activity = Self::load(home);
        *slot = Some((home.to_path_buf(), std::time::Instant::now(), activity.clone()));
        activity
    }

    /// Reads the legacy runtime state under `home`. Missing files and unreadable records are
    /// normal: a workstation without legacy `multiprompt` simply has no live chats.
    pub fn load(home: &Path) -> Self {
        let state_root = home.join(".local").join("state").join("multiprompt");
        let host_dir = state_root.join("agent-host");
        let mut activity = Self::default();

        let live_tmux = live_tmux_sessions();
        let names = read_json(&state_root.join("chat-names.json"));
        let mut record_ids: HashSet<String> = HashSet::new();
        let mut record_tmux: HashSet<String> = HashSet::new();

        // A config and a state file share one stem per chat. A chat whose *own* record was
        // disabled stays out even if a stale state file still names a live pid. Matching on the
        // stem rather than the tmux name matters: switching provider leaves the old provider's
        // record disabled under the same tmux, and that must not hide the chat now running.
        let mut disabled_stems: HashSet<String> = HashSet::new();
        let mut disabled_ids: HashSet<String> = HashSet::new();
        for path in json_files(&host_dir, ".config.json") {
            let Some(record) = read_json(&path) else {
                continue;
            };
            if record.get("enabled").and_then(Value::as_bool) != Some(false) {
                continue;
            }
            if let Some(stem) = record_stem(&path, ".config.json") {
                disabled_stems.insert(stem);
            }
            collect_ids(&record, &mut disabled_ids);
        }

        // Only a state record whose host process or terminal session is alive counts.
        for path in json_files(&host_dir, ".state.json") {
            let Some(record) = read_json(&path) else {
                continue;
            };
            if record_stem(&path, ".state.json").is_some_and(|s| disabled_stems.contains(&s)) {
                continue;
            }
            let hosted = record
                .get("pid")
                .and_then(Value::as_i64)
                .is_some_and(pid_alive);
            let tmux = record.get("tmux").and_then(Value::as_str);
            let terminal = tmux.is_some_and(|t| live_tmux.contains(t));
            if !hosted && !terminal {
                continue;
            }
            let mut ids: HashSet<String> = HashSet::new();
            collect_ids(&record, &mut ids);
            if ids.iter().any(|id| disabled_ids.contains(id)) {
                continue;
            }
            record_ids.extend(ids);
            if let Some(t) = tmux {
                record_tmux.insert(t.to_string());
            }
        }

        // Published chat names are the desktop's own record of a live chat, but the file keeps
        // every chat it ever named. Keep an entry only while its own host record is live.
        let mut named_ids: HashSet<String> = HashSet::new();
        let mut named_tmux: HashSet<String> = HashSet::new();
        if let Some(Value::Object(map)) = names {
            for (tmux, entry) in map {
                let Some(name) = entry.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if name.trim().is_empty() {
                    continue;
                }
                let mut ids: HashSet<String> = HashSet::new();
                collect_ids(&entry, &mut ids);
                let hosted = ids.iter().any(|id| record_ids.contains(id));
                if !hosted && !record_tmux.contains(&tmux) {
                    continue;
                }
                if ids.iter().any(|id| disabled_ids.contains(id)) {
                    continue;
                }
                named_ids.extend(ids);
                named_tmux.insert(tmux.clone());
            }
        }

        activity.ids.extend(record_ids);
        activity.ids.extend(named_ids);
        activity.tmux_names.extend(record_tmux);
        activity.tmux_names.extend(named_tmux);

        activity.read_legacy_archive(home);
        // Nothing the legacy desktop archived is live, whatever its stale record says.
        let archived_ids = activity.legacy_archived_ids.clone();
        let archived_tmux = activity.legacy_archived_tmux.clone();
        activity.ids.retain(|id| !archived_ids.contains(id));
        activity
            .tmux_names
            .retain(|name| !archived_tmux.contains(name));
        activity
    }

    /// Returns `true` when the id belongs to a live chat.
    pub fn is_live(&self, session_id: &str) -> bool {
        self.ids.contains(session_id)
    }

    /// Returns `true` when the legacy desktop archived this chat.
    pub fn is_legacy_archived(&self, session_id: &str, source_path: &Path) -> bool {
        if self.legacy_archived_ids.contains(session_id) {
            return true;
        }
        !self.legacy_archived_tmux.is_empty()
            && source_path
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .any(|part| self.legacy_archived_tmux.contains(part))
    }

    /// The legacy desktop archive is a flat JSON array under `%APPDATA%\ZigShell` on
    /// Windows and `~/.config/ZigShell` here. Read-only, and bounded.
    fn read_legacy_archive(&mut self, home: &Path) {
        let candidates = [
            legacy_zigshell_dir(home).join("archive.json"),
            home.join(".config").join("ZigShell").join("archive.json"),
            home.join(".local")
                .join("share")
                .join("ZigShell")
                .join("archive.json"),
        ];
        for path in candidates {
            let Some(Value::Array(entries)) = read_json(&path) else {
                continue;
            };
            for entry in entries.iter().take(MAX_LEGACY_ARCHIVE_ENTRIES) {
                collect_ids(entry, &mut self.legacy_archived_ids);
                if let Some(tmux) = entry.get("tmux_session").and_then(Value::as_str)
                    && !tmux.trim().is_empty()
                {
                    self.legacy_archived_tmux.insert(tmux.to_string());
                }
            }
            return;
        }
    }
}

/// Where the legacy desktop keeps its state, mirroring `MultibotSettings::data_dir`:
/// `%APPDATA%%\ZigShell` on Windows and `~/.config/ZigShell` elsewhere.
fn legacy_zigshell_dir(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("ZigShell");
        }
    }
    home.join(".config").join("ZigShell")
}

/// Collects every non-empty transcript id a legacy record carries.
fn collect_ids(record: &Value, out: &mut HashSet<String>) {
    for field in ID_FIELDS {
        if let Some(id) = record.get(field).and_then(Value::as_str)
            && !id.trim().is_empty()
        {
            out.insert(id.trim().to_string());
        }
    }
}

fn json_files(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
        })
        .collect()
}

/// The shared stem a `.config.json` and `.state.json` pair is keyed by.
fn record_stem(path: &Path, suffix: &str) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(suffix))
        .map(str::to_string)
}

fn read_json(path: &Path) -> Option<Value> {
    // Legacy records are a few KiB each; a hard cap keeps a corrupt file from spiking memory.
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 4 * 1024 * 1024 {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // One stat instead of a `kill` process per record: 244 state files on a busy host.
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // `kill(pid, 0)`: ESRCH means gone, EPERM still means the process exists.
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Terminal session names, or an empty set when this host runs no tmux server.
fn live_tmux_sessions() -> HashSet<String> {
    let Ok(child) = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
    else {
        return HashSet::new();
    };
    let mut child = child;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().is_ok_and(|s| s.is_some()) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return HashSet::new();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let Ok(output) = child.wait_with_output() else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "multibotd-live-activity-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A pid that is always alive for the duration of this test process.
    fn self_pid() -> i64 {
        std::process::id() as i64
    }

    #[test]
    fn only_chats_with_a_live_host_or_terminal_count() {
        let home = temp_home("live");
        let host = home.join(".local/state/multiprompt/agent-host");
        let pid = self_pid();
        // Live host process.
        write(
            &host.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.config.json"),
            r#"{"enabled":true,"sid":"11111111-2222-3333-4444-555555555555","tmux":"live-one"}"#,
        );
        write(
            &host.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.state.json"),
            &format!(r#"{{"pid":{pid},"sid":"11111111-2222-3333-4444-555555555555"}}"#),
        );
        // Configured but no live host: a chat that ended long ago.
        write(
            &host.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.config.json"),
            r#"{"enabled":true,"sid":"deadbeef-2222-3333-4444-555555555555","tmux":"dead-one"}"#,
        );
        write(
            &host.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.state.json"),
            r#"{"pid":0,"sid":"deadbeef-2222-3333-4444-555555555555","tmux":"dead-one"}"#,
        );
        // Live host but explicitly disabled.
        write(
            &host.join("cccccccccccccccccccccccccccccccc.config.json"),
            r#"{"enabled":false,"sid":"cccccccc-2222-3333-4444-555555555555"}"#,
        );
        write(
            &host.join("cccccccccccccccccccccccccccccccc.state.json"),
            &format!(r#"{{"pid":{pid},"sid":"cccccccc-2222-3333-4444-555555555555"}}"#),
        );
        // Published names: one backed by the live host, one orphaned, one for an Agy chat.
        write(
            &home.join(".local/state/multiprompt/chat-names.json"),
            concat!(
                r#"{"aaaaaaaaaaaaaaaa":{"name":"Named Chat","sid":"11111111-2222-3333-4444-555555555555"},"#,
                r#""dddddddddddddddd":{"name":"Orphaned Chat","sid":"44444444-2222-3333-4444-555555555555"},"#,
                r#""eeeeeeeeeeeeeeee":{"name":"Multi Bot Rust","conversation":"66666666-2222-3333-4444-555555555555"}}"#,
            ),
        );
        write(
            &host.join("ffffffffffffffffffffffffffffffff.config.json"),
            r#"{"enabled":true,"provider":"antigravity","sid":"77777777-2222-3333-4444-555555555555","tmux":"live-agy"}"#,
        );
        write(
            &host.join("ffffffffffffffffffffffffffffffff.state.json"),
            &format!(
                r#"{{"pid":{pid},"sid":"77777777-2222-3333-4444-555555555555","conversation":"66666666-2222-3333-4444-555555555555"}}"#
            ),
        );

        let activity = LiveActivity::load(&home);
        assert!(activity.is_live("11111111-2222-3333-4444-555555555555"));
        assert!(!activity.is_live("deadbeef-2222-3333-4444-555555555555"));
        assert!(!activity.is_live("cccccccc-2222-3333-4444-555555555555"));
        assert!(!activity.tmux_names.contains("dead-one"));
        assert!(!activity.tmux_names.contains("cccccccccccccccc"));
        assert!(activity.tmux_names.contains("aaaaaaaaaaaaaaaa"));
        // `chat-names.json` keeps a name for every chat it ever saw; orphaned names stay out.
        assert!(!activity.is_live("44444444-2222-3333-4444-555555555555"));
        assert!(!activity.tmux_names.contains("dddddddddddddddd"));
        // Antigravity publishes its conversation id, which is what the transcript is named after.
        assert!(activity.is_live("66666666-2222-3333-4444-555555555555"));
        assert!(activity.is_live("77777777-2222-3333-4444-555555555555"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_archive_marks_chats_archived_without_touching_them() {
        let home = temp_home("archive");
        let archive = home.join(".config/ZigShell/archive.json");
        let body = r#"[{"name":"Old Chat","tmux_session":"live-old","csid":"44444444-2222-3333-4444-555555555555"}]"#;
        write(&archive, body);

        let activity = LiveActivity::load(&home);
        assert!(activity.is_legacy_archived(
            "44444444-2222-3333-4444-555555555555",
            Path::new("/tmp/x.jsonl")
        ));
        assert!(!activity.is_legacy_archived(
            "55555555-2222-3333-4444-555555555555",
            Path::new("/tmp/x.jsonl")
        ));
        assert_eq!(fs::read_to_string(&archive).unwrap(), body);
        let _ = fs::remove_dir_all(&home);
    }
}
