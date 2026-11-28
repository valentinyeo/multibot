use multibot_proto::{AgentKind, SlashCommandItem};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const MAX_FRONTMATTER_READ_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 200;
const MAX_ANCESTOR_DEPTH: usize = 16;
const MAX_JSON_READ_BYTES: u64 = 4 * 1024 * 1024;

// Precedence tiers. A higher value wins; project overrides user, user overrides
// plugin, and plugin overrides the hardcoded builtin list.
const PREC_BUILTIN: u16 = 10;
const PREC_PLUGIN: u16 = 15;
const PREC_USER: u16 = 20;
const PREC_PROJECT: u16 = 30;

/// Key: (trigger, lowercase name) -> (precedence, item)
type CommandMap = HashMap<(String, String), (u16, SlashCommandItem)>;

pub fn list_commands(home: &Path, agent: AgentKind, cwd: Option<&Path>) -> Vec<SlashCommandItem> {
    let mut map: CommandMap = HashMap::new();

    for item in builtin_commands(agent) {
        insert_with_precedence(&mut map, PREC_BUILTIN, item);
    }

    let ancestors = collect_ancestors(cwd, home);

    match agent {
        AgentKind::Shell => return Vec::new(),
        AgentKind::Claude => {
            scan_skills_dir(
                &home.join(".claude/skills"),
                "/",
                "",
                "user_skill",
                PREC_USER,
                &mut map,
            );
            scan_markdown_commands_dir(
                &home.join(".claude/commands"),
                "/",
                "",
                "user_command",
                PREC_USER,
                &mut map,
            );
            scan_claude_plugins(home, cwd, &mut map);
            for anc in &ancestors {
                scan_skills_dir(
                    &anc.join(".claude/skills"),
                    "/",
                    "",
                    "project_skill",
                    PREC_PROJECT,
                    &mut map,
                );
                scan_markdown_commands_dir(
                    &anc.join(".claude/commands"),
                    "/",
                    "",
                    "project_command",
                    PREC_PROJECT,
                    &mut map,
                );
            }
        }
        AgentKind::Codex => {
            scan_markdown_commands_dir(
                &home.join(".codex/prompts"),
                "/",
                "prompts:",
                "user_prompt",
                PREC_USER,
                &mut map,
            );
            scan_skills_dir(
                &home.join(".agents/skills"),
                "$",
                "",
                "user_skill",
                PREC_USER,
                &mut map,
            );
            scan_skills_dir(
                &home.join(".codex/skills"),
                "$",
                "",
                "user_skill",
                PREC_USER,
                &mut map,
            );
            for anc in &ancestors {
                scan_skills_dir(
                    &anc.join(".agents/skills"),
                    "$",
                    "",
                    "project_skill",
                    PREC_PROJECT,
                    &mut map,
                );
            }
        }
        AgentKind::Pi | AgentKind::Cursor => {}
        AgentKind::Antigravity => {
            for user_dir in [
                home.join(".gemini/config/skills"),
                home.join(".gemini/antigravity-cli/skills"),
                home.join(".agents/skills"),
                home.join(".gemini/skills"),
                home.join(".claude/skills"),
            ] {
                scan_skills_dir(&user_dir, "/", "", "user_skill", PREC_USER, &mut map);
            }
            scan_antigravity_plugins(home, &mut map);
            for anc in &ancestors {
                for proj_dir in [
                    anc.join(".agents/skills"),
                    anc.join(".agent/skills"),
                    anc.join(".claude/skills"),
                ] {
                    scan_skills_dir(&proj_dir, "/", "", "project_skill", PREC_PROJECT, &mut map);
                }
                scan_markdown_commands_dir(
                    &anc.join(".claude/commands"),
                    "/",
                    "",
                    "project_command",
                    PREC_PROJECT,
                    &mut map,
                );
            }
        }
    }

    let mut items: Vec<SlashCommandItem> = map.into_values().map(|(_, item)| item).collect();
    items.sort_by(|a, b| {
        a.opens_in_terminal
            .cmp(&b.opens_in_terminal)
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
            .then_with(|| a.trigger.cmp(&b.trigger))
    });
    items
}

fn insert_with_precedence(map: &mut CommandMap, prec: u16, item: SlashCommandItem) {
    let key = (item.trigger.clone(), item.name.to_ascii_lowercase());
    match map.get(&key) {
        Some((existing_prec, _)) if *existing_prec >= prec => {}
        _ => {
            map.insert(key, (prec, item));
        }
    }
}

fn collect_ancestors(cwd: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    let Some(start) = cwd.filter(|p| p.is_dir()) else {
        return Vec::new();
    };
    let canon_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut cur = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut out = Vec::new();
    for _ in 0..MAX_ANCESTOR_DEPTH {
        if cur == canon_home {
            break;
        }
        out.push(cur.clone());
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent == cur {
            break;
        }
        cur = parent.to_path_buf();
    }
    // Nearest directory last -> wait: we want nearest directory to win!
    // Since `insert_with_precedence` keeps the first entry at equal precedence (`>=`),
    // keeping `out` ordered nearest-first means the closest ancestor wins!
    out
}

/// Skills and commands shipped inside installed Antigravity plugins, for example
/// `~/.gemini/config/plugins/pstack/skills/poteto-mode/SKILL.md`, plus the
/// builtin skill tree the CLI installs for itself.
fn scan_antigravity_plugins(home: &Path, map: &mut CommandMap) {
    for plugin_dir in subdirs(&home.join(".gemini/config/plugins")) {
        scan_skills_dir(
            &plugin_dir.join("skills"),
            "/",
            "",
            "plugin_skill",
            PREC_PLUGIN,
            map,
        );
        scan_markdown_commands_dir(
            &plugin_dir.join("commands"),
            "/",
            "",
            "plugin_command",
            PREC_PLUGIN,
            map,
        );
    }
    scan_skills_dir(
        &home.join(".gemini/antigravity-cli/builtin/skills"),
        "/",
        "",
        "plugin_skill",
        PREC_PLUGIN,
        map,
    );
}

/// Skills and commands from Claude plugins the user enabled in
/// `~/.claude/settings.json`, resolved through
/// `~/.claude/plugins/installed_plugins.json`. Claude exposes plugin entries as
/// `<plugin>:<name>`, so each scan is namespaced by the plugin name.
fn scan_claude_plugins(home: &Path, cwd: Option<&Path>, map: &mut CommandMap) {
    let settings = read_json_file(&home.join(".claude/settings.json"));
    let Some(enabled) = settings
        .as_ref()
        .and_then(|v| v.get("enabledPlugins"))
        .and_then(|v| v.as_object())
    else {
        return;
    };
    let installed_path = home.join(".claude/plugins/installed_plugins.json");
    let Some(installed) = read_json_file(&installed_path) else {
        return;
    };
    let plugins = installed.get("plugins").unwrap_or(&installed);
    let Some(plugins) = plugins.as_object() else {
        return;
    };

    for (key, records) in plugins {
        if enabled.get(key).and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let Some(install_path) = plugin_install_path(records, cwd) else {
            continue;
        };
        let plugin_name = key.split('@').next().unwrap_or(key);
        let prefix = format!("{plugin_name}:");
        scan_skills_dir(
            &install_path.join("skills"),
            "/",
            &prefix,
            "plugin_skill",
            PREC_PLUGIN,
            map,
        );
        scan_markdown_commands_dir(
            &install_path.join("commands"),
            "/",
            &prefix,
            "plugin_command",
            PREC_PLUGIN,
            map,
        );
    }
}

/// Pick the newest installed record whose `installPath` exists and whose scope
/// applies here. `local` records only apply inside their own project.
fn plugin_install_path(records: &serde_json::Value, cwd: Option<&Path>) -> Option<PathBuf> {
    let records: Vec<&serde_json::Value> = match records {
        serde_json::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for record in records.iter().rev() {
        let Some(raw_path) = record.get("installPath").and_then(|v| v.as_str()) else {
            continue;
        };
        let install_path = PathBuf::from(raw_path);
        if !install_path.is_dir() {
            continue;
        }
        let scope = record.get("scope").and_then(|v| v.as_str()).unwrap_or("");
        if scope.eq_ignore_ascii_case("local") {
            let Some(project) = record.get("projectPath").and_then(|v| v.as_str()) else {
                continue;
            };
            if !path_is_self_or_ancestor(cwd, project) {
                continue;
            }
        }
        return Some(install_path);
    }
    None
}

fn path_is_self_or_ancestor(cwd: Option<&Path>, ancestor: &str) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let target = canonical_or_original(Path::new(ancestor));
    let mut cur = canonical_or_original(cwd);
    loop {
        if cur == target {
            return true;
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent.to_path_buf(),
            _ => return false,
        }
    }
}

fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn subdirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    out.sort();
    out
}

fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_JSON_READ_BYTES).read_to_end(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn scan_skills_dir(
    root: &Path,
    trigger: &str,
    prefix: &str,
    source: &str,
    prec: u16,
    map: &mut CommandMap,
) {
    if !root.is_dir() {
        return;
    }
    for entry in WalkDir::new(root)
        .max_depth(4)
        .follow_links(true)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_skill_md = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("SKILL.md"));
        if !is_skill_md {
            continue;
        }
        let Some(content) = read_limited_utf8(path) else {
            continue;
        };
        let fm = parse_frontmatter(&content);
        if fm
            .user_invocable
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("false"))
        {
            continue;
        }
        let raw_name = fm.name.or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(String::from)
        });
        let Some(name) = raw_name.and_then(|n| clean_cmd_name(&format!("{prefix}{n}"))) else {
            continue;
        };
        let description = fm
            .description
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| first_body_line(&fm.body));

        insert_with_precedence(
            map,
            prec,
            SlashCommandItem {
                name,
                description: cap_utf8_bytes(&description, MAX_DESCRIPTION_BYTES),
                trigger: trigger.to_string(),
                source: source.to_string(),
                opens_in_terminal: false,
            },
        );
    }
}

fn scan_markdown_commands_dir(
    root: &Path,
    trigger: &str,
    prefix: &str,
    source: &str,
    prec: u16,
    map: &mut CommandMap,
) {
    if !root.is_dir() {
        return;
    }
    for entry in WalkDir::new(root)
        .max_depth(4)
        .follow_links(true)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Some(content) = read_limited_utf8(path) else {
            continue;
        };
        let fm = parse_frontmatter(&content);
        if fm
            .user_invocable
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("false"))
        {
            continue;
        }
        let rel_no_ext = rel.with_extension("");
        let derived = rel_no_ext
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join(":");
        let combined = format!("{prefix}{derived}");
        let Some(name) = clean_cmd_name(&combined) else {
            continue;
        };
        let description = fm
            .description
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| first_body_line(&fm.body));

        insert_with_precedence(
            map,
            prec,
            SlashCommandItem {
                name,
                description: cap_utf8_bytes(&description, MAX_DESCRIPTION_BYTES),
                trigger: trigger.to_string(),
                source: source.to_string(),
                opens_in_terminal: false,
            },
        );
    }
}

fn read_limited_utf8(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_FRONTMATTER_READ_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    user_invocable: Option<String>,
    body: String,
}

fn parse_frontmatter(raw: &str) -> Frontmatter {
    let normalized = raw
        .strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n");
    if !normalized.starts_with("---\n") {
        return Frontmatter {
            name: None,
            description: None,
            user_invocable: None,
            body: normalized,
        };
    }

    let rest = &normalized[4..];
    let (header, body) = if let Some(after_dash) = rest.strip_prefix("---") {
        let after_ws = after_dash.trim_start_matches([' ', '\t']);
        if let Some(after_nl) = after_ws.strip_prefix('\n') {
            ("", after_nl.to_string())
        } else if after_ws.is_empty() {
            ("", String::new())
        } else if let Some(close_idx) = rest.find("\n---") {
            let after_close = &rest[close_idx + 4..];
            let body = after_close
                .trim_start_matches([' ', '\t'])
                .strip_prefix('\n')
                .unwrap_or(after_close)
                .to_string();
            (&rest[..close_idx], body)
        } else {
            return Frontmatter {
                name: None,
                description: None,
                user_invocable: None,
                body: normalized,
            };
        }
    } else if let Some(close_idx) = rest.find("\n---") {
        let after_close = &rest[close_idx + 4..];
        let body = after_close
            .trim_start_matches([' ', '\t'])
            .strip_prefix('\n')
            .unwrap_or(after_close)
            .to_string();
        (&rest[..close_idx], body)
    } else {
        return Frontmatter {
            name: None,
            description: None,
            user_invocable: None,
            body: normalized,
        };
    };

    let mut fields: HashMap<String, String> = HashMap::new();
    let mut active_key: Option<String> = None;

    for line in header.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if is_indented {
            if let Some(ref k) = active_key {
                let piece = line.trim();
                if !piece.is_empty() {
                    let entry = fields.entry(k.clone()).or_default();
                    if !entry.is_empty() {
                        entry.push(' ');
                    }
                    entry.push_str(piece);
                }
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let raw_val = v.trim();
            let block_indicator = strip_inline_yaml_comment(raw_val);
            if matches!(block_indicator, ">" | "|" | ">-" | "|-") {
                fields.insert(key.clone(), String::new());
                active_key = Some(key);
            } else {
                let unquoted = parse_yaml_scalar(raw_val);
                fields.insert(key.clone(), unquoted);
                active_key = Some(key);
            }
        } else {
            active_key = None;
        }
    }

    let clean = |k: &str| {
        fields
            .get(k)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
    };

    Frontmatter {
        name: clean("name"),
        description: clean("description"),
        user_invocable: clean("user-invocable"),
        body,
    }
}

fn strip_inline_yaml_comment(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.starts_with('#') {
        return "";
    }
    let bytes = trimmed.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'#' && matches!(bytes[i - 1], b' ' | b'\t') {
            return trimmed[..i].trim_end();
        }
    }
    trimmed
}

fn parse_yaml_scalar(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(quote) = trimmed.chars().next().filter(|&c| c == '"' || c == '\'') {
        // Find matching closing quote (respecting \" escape for double quotes)
        let mut escaped = false;
        for (idx, ch) in trimmed.char_indices().skip(1) {
            if quote == '"' && ch == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if ch == quote && !escaped {
                let remainder = trimmed[idx + ch.len_utf8()..].trim();
                if remainder.is_empty() || remainder.starts_with('#') {
                    return trimmed[1..idx].to_string();
                }
            }
            escaped = false;
        }
        return trimmed.to_string();
    }
    strip_inline_yaml_comment(trimmed).to_string()
}

fn clean_cmd_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches(['/', '$']).trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn first_body_line(body: &str) -> String {
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

fn cap_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        if idx + ch.len_utf8() > max_bytes {
            break;
        }
        end = idx + ch.len_utf8();
    }
    s[..end].to_string()
}

fn builtin_item(name: &str, description: &str, opens_in_terminal: bool) -> SlashCommandItem {
    SlashCommandItem {
        name: name.to_string(),
        description: description.to_string(),
        trigger: "/".to_string(),
        source: "builtin".to_string(),
        opens_in_terminal,
    }
}

fn builtin_commands(agent: AgentKind) -> Vec<SlashCommandItem> {
    match agent {
        AgentKind::Shell => return Vec::new(),
        AgentKind::Claude => {
            let live = [
                ("clear", "Clear conversation history"),
                ("compact", "Compact conversation context"),
                ("context", "Visualize current context usage"),
                ("cost", "Show token usage and cost"),
                ("doctor", "Check Claude Code health"),
                ("help", "Show available commands"),
                ("init", "Initialize project CLAUDE.md"),
                ("login", "Log in to Anthropic account"),
                ("model", "Switch or inspect active model"),
                ("permissions", "Manage tool permissions"),
                ("plan", "Enter or review planning mode"),
                ("rename", "Rename current session"),
                ("review", "Review code changes"),
                ("status", "Show session and account status"),
            ];
            let terminal = [
                "add-dir",
                "artifacts",
                "autofix-pr",
                "background",
                "branch",
                "btw",
                "bug",
                "cd",
                "chrome",
                "copy",
                "diff",
                "exit",
                "export",
                "feedback",
                "fork",
                "hooks",
                "ide",
                "install-github-app",
                "install-slack-app",
                "keybindings",
                "logout",
                "memory",
                "mobile",
                "plugin",
                "privacy-settings",
                "release-notes",
                "remote-control",
                "resume",
                "rewind",
                "sandbox",
                "skills",
                "statusline",
                "subtask",
                "tasks",
                "teleport",
                "terminal-setup",
                "theme",
                "tui",
                "upgrade",
                "voice",
                "workflows",
            ];
            let mut out = Vec::with_capacity(live.len() + terminal.len());
            for (n, d) in live {
                out.push(builtin_item(n, d, false));
            }
            for n in terminal {
                out.push(builtin_item(n, "Interactive Claude CLI command", true));
            }
            out
        }
        AgentKind::Codex => {
            let live = [
                ("cd", "Change working directory"),
                ("clear", "Clear current conversation"),
                ("compact", "Compact conversation context"),
                ("copy", "Copy latest assistant response"),
                ("diff", "Show git diff"),
                ("exit", "Exit Codex session"),
                ("export", "Export conversation transcript"),
                ("fast", "Toggle fast execution mode"),
                ("feedback", "Send feedback"),
                ("init", "Initialize project instructions"),
                ("logout", "Log out of Codex"),
                ("mention", "Mention a file in context"),
                ("ps", "List background processes"),
                ("pwd", "Show current working directory"),
                ("raw", "Show raw output"),
                ("recap", "Summarize recent work"),
                ("review", "Review code changes"),
                ("status", "Show session status"),
                ("stop", "Stop active turn"),
            ];
            let terminal = [
                "agents",
                "approve",
                "archive",
                "delete",
                "experimental",
                "fork",
                "goal",
                "hooks",
                "ide",
                "import",
                "keymap",
                "mcp",
                "memories",
                "model",
                "new",
                "permissions",
                "personality",
                "pets",
                "plan",
                "plugins",
                "rename",
                "resume",
                "side",
                "skills",
                "statusline",
                "subagents",
                "theme",
                "title",
                "usage",
                "vim",
            ];
            let mut out = Vec::with_capacity(live.len() + terminal.len());
            for (n, d) in live {
                out.push(builtin_item(n, d, false));
            }
            for n in terminal {
                out.push(builtin_item(n, "Interactive Codex CLI command", true));
            }
            out
        }
        AgentKind::Pi | AgentKind::Cursor => Vec::new(),
        AgentKind::Antigravity => {
            let live = [
                ("compact", "Compact conversation context"),
                ("plan", "Plan complex multi-step work"),
                ("goal", "Run long-running autonomous goal"),
                ("schedule", "Schedule recurring agent task"),
                ("browser", "Inspect or interact via browser"),
                ("grill-me", "Interactive interview to align on a plan"),
                ("teamwork", "Coordinate a team of autonomous agents"),
                ("learn", "Analyze and remember codebase patterns"),
                ("boost", "Boost reasoning effort"),
            ];
            live.into_iter()
                .map(|(n, d)| builtin_item(n, d, false))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CMD_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_home(tag: &str) -> PathBuf {
        let n = CMD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("multibotd-cmds-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_builtins_and_scans_project_and_user_skills_and_commands() {
        let home = temp_home("scan");
        let repo = home.join("workspace/app");
        let sub = repo.join("crates/core");
        fs::create_dir_all(&sub).unwrap();

        // Claude project command & skill + non-invocable skill + builtin override
        fs::create_dir_all(repo.join(".claude/commands/ops")).unwrap();
        fs::write(
            repo.join(".claude/commands/ops/deploy.md"),
            "---\ndescription: Deploy production release\n---\nBody text\n",
        )
        .unwrap();

        fs::create_dir_all(repo.join(".claude/skills/my-skill")).unwrap();
        fs::write(
            repo.join(".claude/skills/my-skill/SKILL.md"),
            "---\nname: my-skill\ndescription: >\n  Multi-line skill\n  description\n---\n# Heading\n",
        )
        .unwrap();

        fs::create_dir_all(repo.join(".claude/skills/hidden")).unwrap();
        fs::write(
            repo.join(".claude/skills/hidden/SKILL.md"),
            "---\nname: hidden-skill\nuser-invocable: false\ndescription: Hidden\n---\n",
        )
        .unwrap();

        // User skill overriding a terminal-only Claude builtin ("theme")
        fs::create_dir_all(home.join(".claude/skills/theme")).unwrap();
        fs::write(
            home.join(".claude/skills/theme/SKILL.md"),
            "---\nname: theme\ndescription: Custom theme skill\n---\n",
        )
        .unwrap();

        let claude_cmds = list_commands(&home, AgentKind::Claude, Some(&sub));
        assert!(
            claude_cmds
                .iter()
                .any(|c| c.name == "compact" && c.source == "builtin" && !c.opens_in_terminal)
        );
        assert!(claude_cmds.iter().any(|c| c.name == "ops:deploy"
            && c.source == "project_command"
            && c.description == "Deploy production release"));
        assert!(claude_cmds.iter().any(|c| c.name == "my-skill"
            && c.source == "project_skill"
            && c.description == "Multi-line skill description"));
        assert!(!claude_cmds.iter().any(|c| c.name == "hidden-skill"));
        // User skill overrides builtin `theme` and flips opens_in_terminal to false
        let theme = claude_cmds.iter().find(|c| c.name == "theme").unwrap();
        assert_eq!(theme.source, "user_skill");
        assert!(!theme.opens_in_terminal);

        // Codex prompts + `$` skills
        fs::create_dir_all(home.join(".codex/prompts")).unwrap();
        fs::write(
            home.join(".codex/prompts/review.md"),
            "# Review PR carefully\n",
        )
        .unwrap();
        fs::create_dir_all(repo.join(".agents/skills/helper")).unwrap();
        fs::write(
            repo.join(".agents/skills/helper/SKILL.md"),
            "---\ndescription: Repo helper\n---\n",
        )
        .unwrap();

        let codex_cmds = list_commands(&home, AgentKind::Codex, Some(&sub));
        assert!(
            codex_cmds.iter().any(|c| c.name == "prompts:review"
                && c.trigger == "/"
                && c.source == "user_prompt")
        );
        assert!(
            codex_cmds
                .iter()
                .any(|c| c.name == "helper" && c.trigger == "$" && c.source == "project_skill")
        );

        // Antigravity builtins + .agents/skills
        let agy_cmds = list_commands(&home, AgentKind::Antigravity, Some(&sub));
        assert!(
            agy_cmds
                .iter()
                .any(|c| c.name == "teamwork" && c.source == "builtin")
        );
        assert!(
            agy_cmds
                .iter()
                .any(|c| c.name == "helper" && c.trigger == "/" && c.source == "project_skill")
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn frontmatter_strips_inline_comments_and_handles_empty_block() {
        let home = temp_home("fm-edge");
        let repo = home.join("repo");
        fs::create_dir_all(repo.join(".claude/skills/hidden2")).unwrap();
        fs::write(
            repo.join(".claude/skills/hidden2/SKILL.md"),
            "---\nname: hidden2\nuser-invocable: false # internal only\ndescription: Hide me too\n---\n",
        )
        .unwrap();

        fs::create_dir_all(repo.join(".claude/skills/emptyfm")).unwrap();
        fs::write(
            repo.join(".claude/skills/emptyfm/SKILL.md"),
            "---\n---\n# Real heading\n",
        )
        .unwrap();

        fs::create_dir_all(repo.join(".claude/skills/quoted-hash")).unwrap();
        fs::write(
            repo.join(".claude/skills/quoted-hash/SKILL.md"),
            "---\nname: quoted-hash\ndescription: \"C# guide\" # trailing comment\n---\n",
        )
        .unwrap();

        fs::create_dir_all(repo.join(".claude/skills/indented-comment")).unwrap();
        fs::write(
            repo.join(".claude/skills/indented-comment/SKILL.md"),
            "---\nname: indented-comment\nuser-invocable: false\n  # indented comment under scalar\ndescription: Hidden too\n---\n",
        )
        .unwrap();

        let cmds = list_commands(&home, AgentKind::Claude, Some(&repo));
        assert!(
            !cmds.iter().any(|c| c.name == "hidden2"),
            "user-invocable: false # comment must exclude skill"
        );
        assert!(
            !cmds.iter().any(|c| c.name == "indented-comment"),
            "indented comment line under user-invocable: false must not corrupt value"
        );
        let emptyfm = cmds
            .iter()
            .find(|c| c.name == "emptyfm")
            .expect("emptyfm skill must be listed");
        assert_eq!(emptyfm.description, "Real heading");

        let quoted = cmds
            .iter()
            .find(|c| c.name == "quoted-hash")
            .expect("quoted-hash skill must be listed");
        assert_eq!(quoted.description, "C# guide");

        let _ = fs::remove_dir_all(&home);
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn antigravity_lists_plugin_skills_with_precedence() {
        let home = temp_home("agy-plugins");
        let repo = home.join("workspace/agy-app");
        fs::create_dir_all(repo.join("src")).unwrap();
        let plugins = home.join(".gemini/config/plugins");

        write(
            &plugins.join("pstack/skills/unslop/SKILL.md"),
            "---\nname: unslop\ndescription: Cut AI tells\n---\n",
        );
        write(
            &plugins.join("pstack/skills/poteto-mode/SKILL.md"),
            "---\nname: poteto-mode\ndescription: Plugin style\n---\n",
        );
        write(
            &plugins.join("pstack/skills/plan/SKILL.md"),
            "---\nname: plan\ndescription: Plugin plan\n---\n",
        );
        write(
            &plugins.join("pstack/skills/hidden-plugin/SKILL.md"),
            "---\nname: hidden-plugin\nuser-invocable: false\ndescription: Hidden\n---\n",
        );
        write(
            &plugins.join("pstack/commands/ship.md"),
            "---\ndescription: Ship a ticket\n---\n",
        );
        write(
            &home.join(".gemini/antigravity-cli/builtin/skills/builtin-guide/SKILL.md"),
            "---\ndescription: Builtin guide\n---\n",
        );

        // User and project skills of the same name must win over the plugin copies.
        write(
            &home.join(".gemini/config/skills/poteto-mode/SKILL.md"),
            "---\nname: poteto-mode\ndescription: User style\n---\n",
        );
        write(
            &repo.join(".agents/skills/builtin-guide/SKILL.md"),
            "---\ndescription: Project guide\n---\n",
        );

        let cmds = list_commands(&home, AgentKind::Antigravity, Some(&repo.join("src")));

        let unslop = cmds
            .iter()
            .find(|c| c.name == "unslop")
            .expect("plugin skill must be listed");
        assert_eq!(unslop.source, "plugin_skill");
        assert_eq!(unslop.trigger, "/");
        assert_eq!(unslop.description, "Cut AI tells");

        let poteto = cmds.iter().find(|c| c.name == "poteto-mode").unwrap();
        assert_eq!(
            poteto.source, "user_skill",
            "user skill must win over plugin"
        );

        let plan = cmds.iter().find(|c| c.name == "plan").unwrap();
        assert_eq!(plan.source, "plugin_skill", "plugin must win over builtin");
        assert_eq!(plan.description, "Plugin plan");

        let guide = cmds.iter().find(|c| c.name == "builtin-guide").unwrap();
        assert_eq!(guide.source, "project_skill", "project skill must win");
        assert_eq!(guide.description, "Project guide");

        assert!(!cmds.iter().any(|c| c.name == "hidden-plugin"));
        assert!(cmds.iter().any(|c| c.name == "ship"
            && c.source == "plugin_command"
            && c.description == "Ship a ticket"));

        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_follows_symlinked_plugin_dirs() {
        use std::os::unix::fs::symlink;

        let home = temp_home("agy-plugin-link");
        let outside = home.join("checkout/shared-plugin");
        write(
            &outside.join("skills/linked-skill/SKILL.md"),
            "---\nname: linked-skill\ndescription: Symlinked plugin skill\n---\n",
        );
        let plugins = home.join(".gemini/config/plugins");
        fs::create_dir_all(&plugins).unwrap();
        symlink(&outside, plugins.join("linked-plugin")).unwrap();

        let cmds = list_commands(&home, AgentKind::Antigravity, None);
        let linked = cmds
            .iter()
            .find(|c| c.name == "linked-skill")
            .expect("symlinked plugin skill must be listed");
        assert_eq!(linked.source, "plugin_skill");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_lists_enabled_plugin_skills_and_respects_local_scope() {
        let home = temp_home("claude-plugins");
        let repo = home.join("projects/app");
        fs::create_dir_all(repo.join("src")).unwrap();

        let install = home.join(".claude/plugins/cache/market/pstack/1.0.0");
        write(
            &install.join("skills/poteto-mode/SKILL.md"),
            "---\nname: poteto-mode\ndescription: Concise style\n---\n",
        );
        write(
            &install.join("commands/ship.md"),
            "---\ndescription: Ship ticket\n---\n",
        );

        let disabled = home.join(".claude/plugins/cache/market/off/1.0.0");
        write(
            &disabled.join("skills/off-skill/SKILL.md"),
            "---\ndescription: Disabled skill\n---\n",
        );

        let elsewhere = home.join(".claude/plugins/cache/market/elsewhere/1.0.0");
        write(
            &elsewhere.join("skills/elsewhere-skill/SKILL.md"),
            "---\ndescription: Other project skill\n---\n",
        );

        let here = home.join(".claude/plugins/cache/market/here/1.0.0");
        write(
            &here.join("skills/here-skill/SKILL.md"),
            "---\ndescription: This project skill\n---\n",
        );

        write(
            &home.join(".claude/settings.json"),
            r#"{"enabledPlugins":{"pstack@market":true,"off@market":false,"elsewhere@market":true,"here@market":true}}"#,
        );
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            &format!(
                r#"{{"version":2,"plugins":{{
                  "pstack@market":[{{"scope":"user","installPath":{}}}],
                  "off@market":[{{"scope":"user","installPath":{}}}],
                  "elsewhere@market":[{{"scope":"local","projectPath":"/somewhere/else","installPath":{}}}],
                  "here@market":[{{"scope":"local","projectPath":{},"installPath":{}}}]
                }}}}"#,
                json_str(&install),
                json_str(&disabled),
                json_str(&elsewhere),
                json_str(&repo),
                json_str(&here),
            ),
        );

        let cmds = list_commands(&home, AgentKind::Claude, Some(&repo.join("src")));

        let poteto = cmds
            .iter()
            .find(|c| c.name == "pstack:poteto-mode")
            .expect("enabled plugin skill must be namespaced by plugin name");
        assert_eq!(poteto.source, "plugin_skill");
        assert_eq!(poteto.description, "Concise style");

        assert!(
            cmds.iter()
                .any(|c| c.name == "pstack:ship" && c.source == "plugin_command")
        );
        assert!(
            cmds.iter()
                .any(|c| c.name == "here:here-skill" && c.source == "plugin_skill")
        );

        assert!(!cmds.iter().any(|c| c.name.contains("off-skill")));
        assert!(
            !cmds.iter().any(|c| c.name.contains("elsewhere-skill")),
            "local-scope plugin for another project must not be listed"
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_plugin_scan_survives_missing_and_broken_json() {
        let home = temp_home("claude-plugin-json");
        fs::create_dir_all(home.join(".claude/plugins")).unwrap();

        // No settings file at all.
        let cmds = list_commands(&home, AgentKind::Claude, None);
        assert!(
            cmds.iter()
                .any(|c| c.name == "clear" && c.source == "builtin")
        );

        // Broken settings JSON.
        write(&home.join(".claude/settings.json"), "{not json");
        let cmds = list_commands(&home, AgentKind::Claude, None);
        assert!(cmds.iter().any(|c| c.name == "clear"));

        // Valid settings, broken install manifest.
        write(
            &home.join(".claude/settings.json"),
            r#"{"enabledPlugins":{"pkg@m":true}}"#,
        );
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            "not json at all",
        );
        let cmds = list_commands(&home, AgentKind::Claude, None);
        assert!(cmds.iter().any(|c| c.name == "clear"));

        // Valid JSON with junk record shapes and a missing install path.
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            r#"{"plugins":{"pkg@m":[{"scope":"user"},"nope",{"installPath":123},{"scope":"local","installPath":"/definitely/missing/dir"}]}}"#,
        );
        let cmds = list_commands(&home, AgentKind::Claude, None);
        assert!(cmds.iter().any(|c| c.name == "clear"));

        // plugins map in wrapped form, with a project record missing projectPath.
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            r#"{"plugins":{"pkg@m":[{"scope":"local","installPath":"/definitely/missing/dir"}]}}"#,
        );
        let with_cwd = list_commands(&home, AgentKind::Claude, Some(&home));
        assert!(with_cwd.iter().any(|c| c.name == "clear"));
        assert!(with_cwd.iter().all(|c| !c.name.starts_with("pkg:")));

        let _ = fs::remove_dir_all(&home);
    }

    fn json_str(path: &Path) -> String {
        let escaped = path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}
