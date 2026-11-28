use crate::providers::MetadataStore;
use multibot_proto::{AgentKind, SessionSummary, SubworkerInfo};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub(crate) struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub cmdline: String,
    pub cwd: Option<String>,
    pub started_at_ms: Option<i64>,
}

#[cfg(target_os = "linux")]
fn parse_stat(stat: &str, boot_ms: Option<i64>, ticks: i64) -> Option<(u32, Option<i64>)> {
    // comm can contain spaces and closing parentheses; fields after its final ')' are stable.
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let started = fields.get(19)?.parse::<i64>().ok()?;
    Some((
        ppid,
        boot_ms.and_then(|boot| boot.checked_add(started.checked_mul(1000)? / ticks)),
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn read_proc_snapshot() -> Vec<ProcInfo> {
    use std::fs;
    use std::io::{BufRead, BufReader, Read};
    unsafe extern "C" {
        fn sysconf(name: std::ffi::c_int) -> std::ffi::c_long;
    }
    // Linux's _SC_CLK_TCK is 2. No child process or per-process status/environ reads.
    let ticks = unsafe { sysconf(2) } as i64;
    let ticks = if ticks > 0 { ticks } else { 100 };
    let boot_ms = fs::File::open("/proc/stat").ok().and_then(|file| {
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .find_map(|line| {
                line.strip_prefix("btime ")?
                    .trim()
                    .parse::<i64>()
                    .ok()?
                    .checked_mul(1000)
            })
    });
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let path = entry.path();
            let (ppid, started_at_ms) =
                parse_stat(&fs::read_to_string(path.join("stat")).ok()?, boot_ms, ticks)?;
            // Only argv's leading flags are needed; prompts can be megabytes long.
            let mut command = Vec::new();
            fs::File::open(path.join("cmdline"))
                .ok()?
                .take(4096)
                .read_to_end(&mut command)
                .ok()?;
            let cmdline = String::from_utf8_lossy(&command).into_owned();
            if cmdline.is_empty() {
                return None;
            }
            let cwd = fs::read_link(path.join("cwd"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            Some(ProcInfo {
                pid,
                ppid,
                cmdline,
                cwd,
                started_at_ms,
            })
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_proc_snapshot() -> Vec<ProcInfo> {
    Vec::new()
}

fn args(command: &str) -> Vec<&str> {
    if command.contains('\0') {
        command.split('\0').filter(|s| !s.is_empty()).collect()
    } else {
        command.split_whitespace().collect()
    }
}

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

fn executable<'a>(args: &[&'a str]) -> &'a str {
    let first = args.first().map_or("", |s| basename(s));
    if matches!(
        first,
        "node" | "nodejs" | "python" | "python3" | "uv" | "bun"
    ) {
        let Some(script) = args.iter().skip(1).find(|s| !s.starts_with('-')) else {
            return first;
        };
        let name = basename(script);
        if matches!(name, "cli.js" | "index.js") {
            for (package, binary) in [
                ("/claude-code/", "claude"),
                ("/codex/", "codex"),
                ("/pi-coding-agent/", "pi"),
                ("/cursor-agent/", "cursor-agent"),
            ] {
                if script.contains(package) {
                    return binary;
                }
            }
        }
        name
    } else {
        first
    }
}

fn flag<'a>(args: &[&'a str], name: &str) -> Option<&'a str> {
    args.iter()
        .enumerate()
        .find_map(|(i, arg)| {
            arg.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
                .or_else(|| (*arg == name).then(|| args.get(i + 1).copied()).flatten())
        })
        .filter(|s| !s.is_empty())
}

fn runtime(exe: &str) -> Option<&str> {
    match exe {
        "claude" | "codex" | "agy" | "pi" | "hax" => Some(exe),
        "claude.js" => Some("claude"),
        "codex.js" => Some("codex"),
        "cursor-agent" => Some("cursor"),
        _ => None,
    }
}

fn launcher(exe: &str, args: &[&str]) -> bool {
    (exe == "lease" && args.contains(&"run"))
        || (exe == "plan-pick" && args.contains(&"--run"))
        || exe == "token-ladder"
}

fn owner_id<'a>(args: &[&'a str]) -> Option<&'a str> {
    match executable(args) {
        "reader.py" => args
            .iter()
            .position(|s| *s == "--agent-host")
            .and_then(|i| args.get(i + 2).copied()),
        "agy" => flag(args, "--conversation").or_else(|| flag(args, "--resume")),
        "claude" | "claude.js" => flag(args, "--resume").or_else(|| flag(args, "--session-id")),
        "codex" | "codex.js" => args
            .iter()
            .position(|s| *s == "resume")
            .and_then(|i| args.get(i + 1).copied())
            .filter(|s| !s.starts_with('-')),
        _ => None,
    }
}

fn ticket(text: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b[A-Z][A-Z0-9]+-[0-9]+\b").unwrap())
        .find(text)
        .map(|m| m.as_str().to_string())
}

struct ProcessTree<'a> {
    processes: HashMap<u32, &'a ProcInfo>,
    children: HashMap<u32, Vec<u32>>,
}

impl<'a> ProcessTree<'a> {
    fn new(snapshot: &'a [ProcInfo]) -> Self {
        let processes = snapshot.iter().map(|p| (p.pid, p)).collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in snapshot {
            children.entry(p.ppid).or_default().push(p.pid);
        }
        for pids in children.values_mut() {
            pids.sort_unstable();
        }
        Self {
            processes,
            children,
        }
    }

    fn primary(&self, owner: u32) -> u32 {
        let mut stack = vec![owner];
        let mut seen = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            let Some(p) = self.processes.get(&pid) else {
                continue;
            };
            let argv = args(&p.cmdline);
            let exe = executable(&argv);
            if runtime(exe).is_some() {
                return pid;
            }
            if launcher(exe, &argv) {
                continue;
            }
            stack.extend(self.children.get(&pid).into_iter().flatten().rev().copied());
        }
        owner
    }

    fn workers(&self, owner: u32) -> Vec<SubworkerInfo> {
        let primary = self.primary(owner);
        let mut primary_pids = HashSet::from([owner, primary]);
        let mut current = primary;
        while let Some(children) = self.children.get(&current) {
            if children.len() != 1 {
                break;
            }
            let child = children[0];
            let parent_runtime = self
                .processes
                .get(&current)
                .and_then(|p| runtime(executable(&args(&p.cmdline))));
            let child_runtime = self
                .processes
                .get(&child)
                .and_then(|p| runtime(executable(&args(&p.cmdline))));
            let parent_is_wrapper = self.processes.get(&current).is_some_and(|p| {
                matches!(
                    args(&p.cmdline).first().map(|s| basename(s)),
                    Some("node" | "nodejs" | "bun" | "python" | "python3")
                )
            });
            if !parent_is_wrapper
                || parent_runtime.is_none()
                || parent_runtime != child_runtime
                || !primary_pids.insert(child)
            {
                break;
            }
            current = child;
        }
        let mut workers: Vec<(SubworkerInfo, Option<String>, usize)> = Vec::new();
        let mut stack = vec![(owner, None, 0, false)];
        let mut seen = HashSet::new();
        while let Some((pid, mut chain, depth, mut has_agent)) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            let Some(p) = self.processes.get(&pid) else {
                continue;
            };
            let argv = args(&p.cmdline);
            let exe = executable(&argv);
            let is_launcher = launcher(exe, &argv);
            let agent = runtime(exe);
            if !primary_pids.contains(&pid) && (is_launcher || agent.is_some()) {
                // A launcher below a running worker starts another logical worker, not a wrapper.
                if chain.is_none() || (has_agent && is_launcher) {
                    chain = Some(workers.len());
                    has_agent = false;
                    workers.push((
                        SubworkerInfo {
                            pid,
                            started_at_ms: p.started_at_ms,
                            ..Default::default()
                        },
                        None,
                        0,
                    ));
                }
                let (info, provider, deepest) = &mut workers[chain.unwrap()];
                let lease_ticket = (exe == "lease")
                    .then(|| {
                        argv.iter()
                            .position(|s| basename(s) == "lease")
                            .and_then(|i| argv.get(i + 1))
                            .and_then(|s| ticket(s))
                    })
                    .flatten();
                if info.ticket.is_none() {
                    info.ticket = lease_ticket
                        .or_else(|| ticket(&p.cmdline))
                        .or_else(|| p.cwd.as_deref().and_then(ticket));
                }
                if let Some(value) = flag(&argv, "--provider") {
                    *provider = Some(value.into());
                }
                if let Some(value) = flag(&argv, "--model") {
                    info.model = Some(value.into());
                }
                if depth >= *deepest {
                    *deepest = depth;
                    if p.cwd.is_some() {
                        info.cwd = p.cwd.clone();
                    }
                    if let Some(value) = agent {
                        info.runtime = Some(value.into());
                    } else if matches!(exe, "plan-pick" | "token-ladder") {
                        info.runtime = Some("plan-pick".into());
                    }
                }
                has_agent |= agent.is_some();
            }
            for child in self.children.get(&pid).into_iter().flatten().rev() {
                stack.push((*child, chain, depth + 1, has_agent));
            }
        }
        let mut result: Vec<_> = workers
            .into_iter()
            .map(|(mut info, provider, _)| {
                if provider.is_some() {
                    info.runtime = provider;
                }
                info.label = info
                    .ticket
                    .clone()
                    .or_else(|| {
                        info.cwd
                            .as_deref()
                            .map(basename)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                    .or_else(|| info.runtime.clone())
                    .unwrap_or_else(|| "Worker".into());
                info
            })
            .collect();
        result.sort_by_key(|info| (info.started_at_ms, info.pid));
        result
    }
}

pub(crate) fn populate_from_snapshot(
    sessions: &mut [SessionSummary],
    metadata: &MetadataStore,
    active: &HashMap<String, u32>,
    panes: &HashMap<String, Vec<u32>>,
    snapshot: &[ProcInfo],
) {
    let tree = ProcessTree::new(snapshot);
    let mut owners: HashMap<&str, Vec<u32>> = HashMap::new();
    for p in snapshot {
        if let Some(id) = owner_id(&args(&p.cmdline)) {
            owners.entry(id).or_default().push(p.pid);
        }
    }
    for session in sessions {
        let native = metadata
            .get(&session.id)
            .and_then(|m| m.native_session_id.as_deref());
        let mut pids = Vec::new();
        for id in std::iter::once(session.id.as_str()).chain(native) {
            pids.extend(active.get(id).copied());
            pids.extend(owners.get(id).into_iter().flatten().copied());
        }
        if session.agent == AgentKind::Shell {
            pids.extend(panes.get(&session.id).into_iter().flatten().copied());
        }
        pids.sort_unstable();
        pids.dedup();
        let mut workers = HashMap::new();
        for pid in pids {
            for info in tree.workers(pid) {
                workers.entry(info.pid).or_insert(info);
            }
        }
        session.subworkers = workers.into_values().collect();
        session
            .subworkers
            .sort_by_key(|info| (info.started_at_ms, info.pid));
        session.working |= !session.subworkers.is_empty();
    }
}

pub(crate) fn populate(
    sessions: &mut [SessionSummary],
    metadata: &MetadataStore,
    active: &HashMap<String, u32>,
) {
    if sessions.is_empty() {
        return;
    }
    let snapshot = read_proc_snapshot();
    let mut panes: HashMap<String, Vec<u32>> = HashMap::new();
    if sessions.iter().any(|s| s.agent == AgentKind::Shell)
        && let Ok(output) = std::process::Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{session_name}\t#{pane_pid}"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some((id, pid)) = line.split_once('\t')
                && let Ok(pid) = pid.parse()
            {
                panes.entry(id.into()).or_default().push(pid);
            }
        }
    }
    populate_from_snapshot(sessions, metadata, active, &panes, &snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::SessionMetaEntry;

    fn process(pid: u32, ppid: u32, cmdline: &str, cwd: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            cmdline: cmdline.into(),
            cwd: Some(cwd.into()),
            started_at_ms: Some(i64::from(pid) * 1000),
        }
    }

    fn parallel_workers(runner: &str) -> Vec<ProcInfo> {
        let mut processes = vec![process(10, 1, "agy --resume native-owner", "/repo")];
        for (root, key, folder) in [(20, "MBRU-116", "w4"), (30, "MBRU-117", "w5")] {
            let cwd = format!("/home/valentin/projects/multibot-wt-{folder}-subworkers");
            processes.push(process(root, 10, "bash", "/repo"));
            processes.push(process(
                root + 1,
                root,
                &format!("python3 /skills/hypertask-work/lease {key} run -- {runner}"),
                &cwd,
            ));
            processes.push(process(root + 2, root + 1, runner, &cwd));
            processes.push(process(
                root + 3,
                root + 2,
                "systemd-inhibit --what=sleep sleep infinity",
                "/helper",
            ));
            processes.push(process(root + 4, root + 3, "sleep infinity", "/helper"));
        }
        processes
    }

    fn discover(
        snapshot: &[ProcInfo],
        id: &str,
        agent: AgentKind,
        active: HashMap<String, u32>,
        panes: HashMap<String, Vec<u32>>,
    ) -> SessionSummary {
        let mut sessions = vec![SessionSummary {
            id: id.into(),
            agent,
            ..Default::default()
        }];
        populate_from_snapshot(
            &mut sessions,
            &MetadataStore::default(),
            &active,
            &panes,
            snapshot,
        );
        sessions.remove(0)
    }

    #[test]
    fn postbox_conversation_workers_survive_newer_active_sibling() {
        let native = "de860fa4-e66e-4380-910d-baae1dec5b07";
        let command = format!(
            "/home/valentin/.local/bin/agy --input-format stream-json --output-format stream-json --dangerously-skip-permissions --print= --conversation {native}"
        );
        let mut snapshot = vec![
            process(10, 1, &command, "/repo"),
            process(11, 1, &command, "/repo"),
        ];
        for (pid, key) in [(20, "PORU-30"), (30, "PORU-32"), (40, "PORU-27")] {
            snapshot.push(process(
                pid,
                10,
                &format!("python3 /home/valentin/.local/share/vstack/skills/hypertask-work/lease {key} run -- /home/valentin/.local/bin/plan-pick --run"),
                "/repo",
            ));
            snapshot.push(process(
                pid + 1,
                pid,
                "/home/valentin/.local/bin/plan-pick --run",
                "/repo",
            ));
        }
        let mut metadata = MetadataStore::default();
        metadata.sessions.insert(
            "postbox".into(),
            SessionMetaEntry {
                native_session_id: Some(native.into()),
                ..Default::default()
            },
        );
        let mut sessions = vec![SessionSummary {
            id: "postbox".into(),
            title: "Postbox Rust 2".into(),
            agent: AgentKind::Antigravity,
            ..Default::default()
        }];
        for active in [
            HashMap::new(),
            HashMap::from([("postbox".into(), 11)]),
            HashMap::from([(native.into(), 11)]),
            HashMap::from([("postbox".into(), 11), (native.into(), 10)]),
        ] {
            populate_from_snapshot(
                &mut sessions,
                &metadata,
                &active,
                &HashMap::new(),
                &snapshot,
            );
            assert!(sessions[0].working);
            assert_eq!(
                sessions[0]
                    .subworkers
                    .iter()
                    .map(|w| (w.pid, w.label.as_str(), w.runtime.as_deref()))
                    .collect::<Vec<_>>(),
                vec![
                    (20, "PORU-30", Some("plan-pick")),
                    (30, "PORU-32", Some("plan-pick")),
                    (40, "PORU-27", Some("plan-pick"))
                ]
            );
        }
        let workers = sessions[0].subworkers.clone();
        snapshot.reverse();
        populate_from_snapshot(
            &mut sessions,
            &metadata,
            &HashMap::new(),
            &HashMap::new(),
            &snapshot,
        );
        assert_eq!(sessions[0].subworkers, workers);
        snapshot.retain(|p| p.pid < 20);
        populate_from_snapshot(
            &mut sessions,
            &metadata,
            &HashMap::new(),
            &HashMap::new(),
            &snapshot,
        );
        assert!(sessions[0].subworkers.is_empty());
    }

    #[test]
    fn active_session_and_native_pids_are_both_searched() {
        let snapshot = vec![
            process(10, 1, "hax -p task", "/repo"),
            process(11, 10, "plan-pick --run", "/repo/first"),
            process(20, 1, "hax -p task", "/repo"),
            process(21, 20, "plan-pick --run", "/repo/second"),
        ];
        let mut metadata = MetadataStore::default();
        metadata.sessions.insert(
            "alias".into(),
            SessionMetaEntry {
                native_session_id: Some("native".into()),
                ..Default::default()
            },
        );
        let mut sessions = vec![SessionSummary {
            id: "alias".into(),
            ..Default::default()
        }];
        populate_from_snapshot(
            &mut sessions,
            &metadata,
            &HashMap::from([("alias".into(), 10), ("native".into(), 20)]),
            &HashMap::new(),
            &snapshot,
        );
        assert_eq!(
            sessions[0]
                .subworkers
                .iter()
                .map(|w| w.pid)
                .collect::<Vec<_>>(),
            vec![11, 21]
        );
    }

    #[test]
    fn fresh_claude_session_id_owns_workers() {
        for command in [
            "claude",
            "node /bin/claude.js",
            "node /opt/@anthropic-ai/claude-code/cli.js",
        ] {
            let snapshot = vec![
                process(
                    10,
                    1,
                    &format!("{command} --session-id de860fa4-e66e-4380-910d-baae1dec5b07"),
                    "/repo",
                ),
                process(11, 10, "plan-pick --run", "/repo/worker"),
            ];
            let session = discover(
                &snapshot,
                "de860fa4-e66e-4380-910d-baae1dec5b07",
                AgentKind::Claude,
                HashMap::new(),
                HashMap::new(),
            );
            assert_eq!(session.subworkers.len(), 1);
            assert_eq!(session.subworkers[0].pid, 11);
        }
    }

    #[test]
    fn lease_hax_chains_are_two_workers_not_wrappers() {
        let snapshot =
            parallel_workers("/home/valentin/bin/hax --provider=codex --model=gpt-6.1-sol -p task");
        let session = discover(
            &snapshot,
            "native-owner",
            AgentKind::Antigravity,
            HashMap::new(),
            HashMap::new(),
        );
        assert!(session.working);
        assert_eq!(
            session.subworkers,
            vec![
                SubworkerInfo {
                    pid: 21,
                    label: "MBRU-116".into(),
                    ticket: Some("MBRU-116".into()),
                    cwd: Some("/home/valentin/projects/multibot-wt-w4-subworkers".into()),
                    runtime: Some("codex".into()),
                    model: Some("gpt-6.1-sol".into()),
                    started_at_ms: Some(21000)
                },
                SubworkerInfo {
                    pid: 31,
                    label: "MBRU-117".into(),
                    ticket: Some("MBRU-117".into()),
                    cwd: Some("/home/valentin/projects/multibot-wt-w5-subworkers".into()),
                    runtime: Some("codex".into()),
                    model: Some("gpt-6.1-sol".into()),
                    started_at_ms: Some(31000)
                },
            ]
        );
        let mut reversed = snapshot;
        reversed.reverse();
        assert_eq!(
            session.subworkers,
            discover(
                &reversed,
                "native-owner",
                AgentKind::Antigravity,
                HashMap::new(),
                HashMap::new()
            )
            .subworkers
        );
    }

    #[test]
    fn lease_plan_pick_chains_collapse() {
        let snapshot = parallel_workers("/bin/plan-pick --run --model gpt-6.1-sol");
        let session = discover(
            &snapshot,
            "native-owner",
            AgentKind::Antigravity,
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(session.subworkers.len(), 2);
        for (worker, key) in session.subworkers.iter().zip(["MBRU-116", "MBRU-117"]) {
            assert_eq!(worker.ticket.as_deref(), Some(key));
            assert_eq!(worker.runtime.as_deref(), Some("plan-pick"));
            assert_eq!(worker.model.as_deref(), Some("gpt-6.1-sol"));
        }
    }

    #[test]
    fn reader_child_is_owner_but_its_workers_are_counted_once() {
        for agent in ["claude", "codex", "agy"] {
            let mut snapshot = vec![
                process(
                    10,
                    1,
                    &format!("python3 /legacy/reader.py --agent-host {agent} sid"),
                    "/repo",
                ),
                process(11, 10, &format!("{agent} --resume sid"), "/repo"),
            ];
            assert!(
                discover(
                    &snapshot,
                    "sid",
                    AgentKind::Claude,
                    HashMap::new(),
                    HashMap::new()
                )
                .subworkers
                .is_empty()
            );
            snapshot.push(process(12, 11, "bash", "/repo"));
            snapshot.push(process(
                13,
                12,
                "hax --provider=codex --model gpt-6.1-sol",
                "/repo/worker",
            ));
            snapshot.push(process(
                14,
                13,
                "node /bin/codex.js resume worker",
                "/repo/worker",
            ));
            let workers = discover(
                &snapshot,
                "sid",
                AgentKind::Claude,
                HashMap::new(),
                HashMap::new(),
            )
            .subworkers;
            assert_eq!(workers.len(), 1);
            assert_eq!(workers[0].pid, 13);
            assert_eq!(workers[0].label, "worker");
            assert_eq!(workers[0].runtime.as_deref(), Some("codex"));
        }
    }

    #[test]
    fn tmux_shell_skips_hosted_agy_and_shows_its_two_workers() {
        let mut snapshot = parallel_workers("hax --provider=codex --model=gpt-6.1-sol");
        snapshot[0].ppid = 9;
        snapshot.push(process(9, 1, "bash", "/repo"));
        let session = discover(
            &snapshot,
            "term-56d16ef5",
            AgentKind::Shell,
            HashMap::new(),
            HashMap::from([("term-56d16ef5".into(), vec![9])]),
        );
        assert_eq!(session.subworkers.len(), 2);
        assert!(session.working);
    }

    #[test]
    fn managed_pid_and_native_id_both_resolve_ownership() {
        let snapshot = parallel_workers("hax --provider=codex");
        let managed = discover(
            &snapshot,
            "managed",
            AgentKind::Antigravity,
            HashMap::from([("managed".into(), 10)]),
            HashMap::new(),
        );
        assert_eq!(managed.subworkers.len(), 2);
        let mut metadata = MetadataStore::default();
        metadata.sessions.insert(
            "alias".into(),
            SessionMetaEntry {
                native_session_id: Some("native-owner".into()),
                ..Default::default()
            },
        );
        let mut sessions = vec![SessionSummary {
            id: "alias".into(),
            ..Default::default()
        }];
        populate_from_snapshot(
            &mut sessions,
            &metadata,
            &HashMap::new(),
            &HashMap::new(),
            &snapshot,
        );
        assert_eq!(sessions[0].subworkers, managed.subworkers);
    }

    #[test]
    fn primary_cli_native_child_is_not_a_worker() {
        let mut snapshot = vec![
            process(
                10,
                1,
                "node /opt/@openai/codex/bin/codex.js resume owner",
                "/repo",
            ),
            process(11, 10, "/opt/codex resume owner", "/repo"),
        ];
        assert!(
            discover(
                &snapshot,
                "owner",
                AgentKind::Codex,
                HashMap::new(),
                HashMap::new()
            )
            .subworkers
            .is_empty()
        );
        snapshot.push(process(
            12,
            11,
            "claude --model sonnet -p task",
            "/repo/worker",
        ));
        let session = discover(
            &snapshot,
            "owner",
            AgentKind::Codex,
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(session.subworkers.len(), 1);
        assert_eq!(session.subworkers[0].pid, 12);
        assert_eq!(session.subworkers[0].runtime.as_deref(), Some("claude"));
    }

    #[test]
    fn native_script_agents_and_nested_leases_are_discovered() {
        let snapshot = vec![
            process(
                10,
                1,
                "node /opt/@anthropic-ai/claude-code/cli.js --resume owner",
                "/repo",
            ),
            process(
                11,
                10,
                "node /opt/pi-coding-agent/dist/cli.js -p task",
                "/repo/pi-worker",
            ),
            process(
                12,
                11,
                "python3 /skills/hypertask-work/lease MBRU-118 run -- hax",
                "/repo/nested",
            ),
            process(13, 12, "hax --provider=codex", "/repo/nested"),
        ];
        let session = discover(
            &snapshot,
            "owner",
            AgentKind::Claude,
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(session.subworkers.len(), 2);
        assert_eq!(session.subworkers[0].runtime.as_deref(), Some("pi"));
        assert_eq!(session.subworkers[1].ticket.as_deref(), Some("MBRU-118"));
    }

    #[test]
    fn lease_with_only_helper_descendants_still_counts_once() {
        let snapshot = vec![
            process(10, 1, "agy --resume owner", "/repo"),
            process(11, 10, "bash", "/repo"),
            process(
                12,
                11,
                "python3 /skills/hypertask-work/lease MBRU-117 run -- hax --provider=codex --model=gpt-6.1-sol",
                "/repo/worktree",
            ),
            process(
                13,
                12,
                "systemd-inhibit --what=sleep sleep infinity",
                "/helper",
            ),
            process(14, 13, "sleep infinity", "/helper"),
        ];
        let session = discover(
            &snapshot,
            "owner",
            AgentKind::Antigravity,
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(
            session.subworkers,
            vec![SubworkerInfo {
                pid: 12,
                label: "MBRU-117".into(),
                ticket: Some("MBRU-117".into()),
                cwd: Some("/repo/worktree".into()),
                runtime: Some("codex".into()),
                model: Some("gpt-6.1-sol".into()),
                started_at_ms: Some(12000),
            }]
        );
        assert!(session.working);
    }

    #[test]
    fn unrelated_processes_and_prompt_mentions_are_not_workers() {
        let snapshot = vec![
            process(10, 1, "agy --resume owner", "/repo"),
            process(11, 10, "bash -c echo hax --model=ignored MBRU-117", "/repo"),
            process(12, 1, "hax --provider=codex", "/unrelated"),
        ];
        let session = discover(
            &snapshot,
            "owner",
            AgentKind::Antigravity,
            HashMap::new(),
            HashMap::new(),
        );
        assert!(!session.working);
        assert!(session.subworkers.is_empty());
    }

    #[test]
    fn nul_arguments_keep_prompt_flags_out_of_worker_metadata() {
        let snapshot = vec![
            process(10, 1, "claude\0--resume\0owner\0", "/repo"),
            process(
                11,
                10,
                "hax\0--provider=codex\0--model\0gpt-6.1-sol\0-p\0say --model wrong\0",
                "/repo/worker",
            ),
        ];
        let session = discover(
            &snapshot,
            "owner",
            AgentKind::Claude,
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(session.subworkers.len(), 1);
        assert_eq!(session.subworkers[0].model.as_deref(), Some("gpt-6.1-sol"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_handles_parentheses_and_clock_ticks() {
        let fields = [
            "S", "42", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0",
            "0", "0", "1250",
        ]
        .join(" ");
        assert_eq!(
            parse_stat(&format!("7 (a ) process) {fields}"), Some(100000), 250),
            Some((42, Some(105000)))
        );
        assert_eq!(parse_stat("bad stat", Some(0), 100), None);
        assert_eq!(
            parse_stat(&format!("7 (process) {fields}"), None, 100),
            Some((42, None))
        );
    }
}
