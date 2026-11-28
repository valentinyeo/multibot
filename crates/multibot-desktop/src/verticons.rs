use gpui_kit::{Div, ParentElement, Styled, div, px, rgb, rgba};
use multibot_proto::AgentKind;

use crate::icons::{self, lucide_icon};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum VerticonKind {
    Terminal,
    Mail,
    Bot,
    Equalizer,
    Telemetry,
    Travel,
    CodeBook,
    Design,
    Browser,
    Shield,
    Roadmap,
    Finance,
    Growth,
    Support,
    Database,
    Rocket,
    Docs,
    Git,
    Media,
    People,
    Search,
    TestLab,
    Infra,
    Spark,
}

impl VerticonKind {
    const ALL: [Self; 24] = [
        Self::Terminal,
        Self::Mail,
        Self::Bot,
        Self::Equalizer,
        Self::Telemetry,
        Self::Travel,
        Self::CodeBook,
        Self::Design,
        Self::Browser,
        Self::Shield,
        Self::Roadmap,
        Self::Finance,
        Self::Growth,
        Self::Support,
        Self::Database,
        Self::Rocket,
        Self::Docs,
        Self::Git,
        Self::Media,
        Self::People,
        Self::Search,
        Self::TestLab,
        Self::Infra,
        Self::Spark,
    ];

    pub(crate) fn from_index(index: u8) -> Option<Self> {
        Self::ALL.get(index as usize).copied()
    }
}

// Index order is the persisted SessionMeta.face contract.
const KEYWORDS: [&[&str]; 24] = [
    &["multibot", "term", "shell", "bash", "zsh", "cli", "console"],
    &["postbox", "mail", "email", "inbox", "smtp", "imap"],
    &["multi bot", "bot", "agent", "swarm", "poteto"],
    &["btop", "htop", "top", "perf", "gpu", "cpu", "bench"],
    &[
        "vps", "health", "status", "uptime", "monitor", "doctor", "watch",
    ],
    &["travel", "trip", "flight", "map", "hotel", "vacation"],
    &[
        "ratstack", "pstack", "vstack", "rust", "zig", "sdk", "lib", "crate", "code",
    ],
    &[
        "design",
        "figma",
        "ui",
        "ux",
        "icon",
        "theme",
        "style",
        "gridline",
        "impeccable",
    ],
    &[
        "browser",
        "chrome",
        "web",
        "live chat",
        "site",
        "url",
        "scrape",
    ],
    &[
        "triage", "security", "guard", "bug", "fix", "incident", "alert", "auth",
    ],
    &[
        "product",
        "roadmap",
        "plan",
        "calendar",
        "schedule",
        "sprint",
        "milestone",
    ],
    &[
        "finance",
        "pecunia",
        "wise",
        "freeagent",
        "money",
        "budget",
        "cost",
        "billing",
        "invoice",
    ],
    &[
        "marketing",
        "growth",
        "seo",
        "lead",
        "ads",
        "ugc",
        "campaign",
        "analytics",
    ],
    &[
        "support", "help", "hefe", "customer", "feedback", "chat", "ticket",
    ],
    &[
        "db", "database", "sql", "sqlite", "postgres", "d1", "r2", "kv", "redis", "storage",
    ],
    &[
        "ship",
        "deploy",
        "release",
        "ci",
        "launch",
        "build",
        "cloudflare",
        "worker",
    ],
    &[
        "docs", "doc", "readme", "guide", "spec", "wiki", "notes", "writing",
    ],
    &[
        "git", "pr", "merge", "rebase", "branch", "worktree", "diff", "tidy",
    ],
    &[
        "video",
        "ketchup",
        "media",
        "voice",
        "audio",
        "dictation",
        "youtube",
        "tiktok",
    ],
    &[
        "recruit",
        "hire",
        "candidate",
        "team",
        "staff",
        "ceo",
        "owner",
        "user",
    ],
    &[
        "search",
        "research",
        "audit",
        "explore",
        "recall",
        "interrogate",
        "find",
    ],
    &["test", "verify", "qa", "e2e", "check", "lint", "statechart"],
    &[
        "infra",
        "server",
        "docker",
        "linux",
        "systemd",
        "daemon",
        "multibotd",
        "host",
        "ssh",
    ],
    &[],
];

const PALETTES: [(u32, u32); 12] = [
    (0x22C55E, 0xBBF7D0),
    (0xF59E0B, 0xFEF3C7),
    (0x38BDF8, 0xBAE6FD),
    (0x2DD4BF, 0xCCFBF1),
    (0xA78BFA, 0xEDE9FE),
    (0xFB7185, 0xFFE4E6),
    (0x60A5FA, 0xDBEAFE),
    (0xE879F9, 0xFAE8FF),
    (0xFB923C, 0xFFEDD5),
    (0xA3E635, 0xECFCCB),
    (0x818CF8, 0xE0E7FF),
    (0x22D3EE, 0xCFFAFE),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VerticonSpec {
    pub(crate) kind: VerticonKind,
    pub(crate) palette: u8,
    pub(crate) color: Option<u32>,
}

fn keyword_score(text: &str, keywords: &[&str]) -> u32 {
    let bytes = text.as_bytes();
    keywords
        .iter()
        .filter_map(|keyword| {
            let word = keyword.as_bytes();
            bytes
                .windows(word.len())
                .enumerate()
                .any(|(start, part)| {
                    part.eq_ignore_ascii_case(word)
                        && (start == 0 || !bytes[start - 1].is_ascii_alphanumeric())
                        && (start + word.len() == bytes.len()
                            || !bytes[start + word.len()].is_ascii_alphanumeric())
                })
                .then_some(word.len() as u32)
        })
        .max()
        .unwrap_or(0)
}

pub(crate) fn resolve_verticon(
    title: &str,
    cwd: Option<&str>,
    workspace_name: &str,
    agent: AgentKind,
    session_key: &str,
    manual_face: Option<u8>,
    manual_color: Option<&str>,
) -> VerticonSpec {
    let kind = manual_face
        .and_then(VerticonKind::from_index)
        .unwrap_or_else(|| {
            let mut best = if agent == AgentKind::Shell {
                VerticonKind::Terminal
            } else {
                VerticonKind::Spark
            };
            let mut best_score = 0;
            for kind in VerticonKind::ALL {
                let keywords = KEYWORDS[kind as usize];
                let title_score = keyword_score(title, keywords);
                let folder_score = cwd
                    .unwrap_or("")
                    .rsplit(['/', '\\'])
                    .filter(|part| !part.is_empty())
                    .take(2)
                    .map(|part| keyword_score(part, keywords))
                    .max()
                    .unwrap_or(0);
                let workspace_score = keyword_score(workspace_name, keywords);
                // A title topic always wins over folders and workspace defaults.
                let score = if title_score > 0 {
                    10_000 + title_score * 100
                } else {
                    0
                } + if folder_score > 0 {
                    100 + folder_score * 2
                } else {
                    0
                } + if workspace_score > 0 {
                    10 + workspace_score
                } else {
                    0
                };
                if score > best_score {
                    best = kind;
                    best_score = score;
                }
            }
            best
        });
    let hash = session_key
        .bytes()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    let color = manual_color
        .filter(|color| {
            color.len() == 7
                && color.starts_with('#')
                && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
        })
        .and_then(|color| u32::from_str_radix(&color[1..], 16).ok());
    VerticonSpec {
        kind,
        palette: (hash % 12) as u8,
        color,
    }
}

fn mix(base: u32, tint: u32) -> u32 {
    let channel = |shift| (((base >> shift) & 255) * 4 + ((tint >> shift) & 255)) / 5;
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

pub(crate) fn render_verticon(spec: VerticonSpec, width: f32, height: f32) -> Div {
    use VerticonKind::*;
    let (palette, light) = PALETTES[spec.palette as usize];
    let accent = |base| spec.color.unwrap_or_else(|| mix(base, palette));
    let zone = |x: f32, y: f32, w: f32, h: f32, color: u32, radius: f32| {
        div()
            .absolute()
            .left(px(x * width / 24.0))
            .top(px(y * height / 34.0))
            .w(px(w * width / 24.0))
            .h(px(h * height / 34.0))
            .rounded(px(radius * width / 24.0))
            .bg(rgb(color))
    };
    let badge = |x, y, w, h, color, radius| {
        zone(x, y, w, h, 0, radius)
            .bg(rgba(0x00000038))
            .top(px(y * height / 34.0 + 1.0))
            .child(zone(0.0, -34.0 / height, w, h, color, radius))
    };
    let glyph = |x: f32, y: f32, size: f32, icon, color| {
        div()
            .absolute()
            .left(px(x * width / 24.0))
            .top(px(y * height / 34.0))
            .child(lucide_icon(icon, size * width / 24.0).text_color(rgb(color)))
    };
    let base = match spec.kind {
        Terminal => 0x111317,
        Mail => accent(0xF59E0B),
        Bot => accent(0x0284C7),
        Equalizer => 0x0F1219,
        Telemetry => accent(0x0D9488),
        Travel => 0xFFFBF5,
        CodeBook => accent(0x7C3AED),
        Design => 0x111318,
        Browser => 0xEFF6FF,
        Shield => accent(0xDC2626),
        Roadmap => 0xF8FAFC,
        Finance => accent(0x059669),
        Growth => accent(0xE11D48),
        Support => accent(0x4F46E5),
        Database => 0x1E3A8A,
        Rocket => accent(0x1D4ED8),
        Docs => 0xFFFBEB,
        Git => 0x18181B,
        Media => accent(0x6D28D9),
        People => accent(0x0369A1),
        Search => 0x1E293B,
        TestLab => 0x065F46,
        Infra => 0x0F172A,
        Spark => accent(0x4338CA),
    };
    let mut card = div()
        .relative()
        .overflow_hidden()
        .flex_shrink_0()
        .w(px(width))
        .h(px(height))
        .rounded(px((width * 0.18).clamp(3.5, 6.0)))
        .bg(rgb(base));
    card = match spec.kind {
        Terminal => card
            .child(zone(0.0, 0.0, 24.0, 6.0, 0x1D2027, 0.0))
            .child(zone(3.0, 2.0, 1.5, 1.5, 0xF87171, 1.0))
            .child(zone(6.0, 2.0, 1.5, 1.5, 0xFBBF24, 1.0))
            .child(zone(9.0, 2.0, 1.5, 1.5, 0x22C55E, 1.0))
            .child(glyph(3.0, 8.0, 14.0, icons::TERMINAL, 0xFFFFFF))
            .child(zone(4.0, 23.0, 13.0, 2.0, accent(0x22C55E), 1.0))
            .child(zone(4.0, 27.0, 9.0, 1.5, 0x64748B, 0.75))
            .child(zone(0.0, 31.0, 24.0, 3.0, 0x14532D, 0.0)),
        Mail => card
            .child(zone(0.0, 0.0, 24.0, 6.0, 0xD97706, 0.0))
            .child(badge(3.0, 9.0, 18.0, 14.0, 0xFFFBEB, 2.0))
            .child(glyph(4.0, 8.0, 16.0, icons::CHEVRON_DOWN, 0xEA580C))
            .child(zone(0.0, 27.0, 24.0, 7.0, 0xB45309, 0.0))
            .child(zone(4.0, 29.5, 2.0, 2.0, 0xFEF3C7, 1.0))
            .child(zone(9.0, 30.0, 10.0, 1.5, light, 0.75)),
        Bot => card
            .child(zone(0.0, 0.0, 24.0, 5.0, 0x075985, 0.0))
            .child(zone(11.0, 5.0, 2.0, 4.0, 0xBAE6FD, 1.0))
            .child(zone(10.0, 3.0, 4.0, 4.0, 0x67E8F9, 2.0))
            .child(badge(3.0, 10.0, 18.0, 14.0, 0x0F172A, 3.0))
            .child(zone(7.0, 14.0, 3.0, 5.0, accent(0x22D3EE), 1.5))
            .child(zone(14.0, 14.0, 3.0, 5.0, accent(0x22D3EE), 1.5))
            .child(zone(0.0, 28.0, 24.0, 6.0, 0x075985, 0.0))
            .child(zone(5.0, 30.0, 2.0, 2.0, 0xBAE6FD, 1.0))
            .child(zone(10.0, 30.0, 2.0, 2.0, 0xBAE6FD, 1.0))
            .child(zone(15.0, 30.0, 5.0, 2.0, light, 1.0)),
        Equalizer => {
            card = card.child(zone(0.0, 0.0, 24.0, 14.0, 0x161B26, 0.0));
            for (i, y) in [20.0, 24.0, 18.0, 22.0].into_iter().enumerate() {
                let x = 4.0 + i as f32 * 4.5;
                card = card
                    .child(zone(x, 17.0, 1.5, 12.0, 0x334155, 0.75))
                    .child(zone(x, y, 1.5, 29.0 - y, accent(0x22D3EE), 0.75))
                    .child(zone(x - 1.0, y - 1.0, 3.5, 3.5, 0xFFFFFF, 1.75));
            }
            card.child(glyph(3.0, 0.0, 18.0, icons::ACTIVITY, accent(0x22D3EE)))
                .child(zone(0.0, 32.0, 24.0, 2.0, 0x0284C7, 0.0))
        }
        Telemetry => card
            .child(zone(8.0, 0.0, 8.0, 34.0, 0x0F766E, 0.0))
            .child(badge(3.0, 7.0, 18.0, 19.0, 0x0F172A, 2.5))
            .child(zone(5.0, 17.0, 5.0, 2.0, accent(0x2DD4BF), 1.0))
            .child(zone(9.0, 12.0, 2.0, 7.0, accent(0x2DD4BF), 1.0))
            .child(zone(11.0, 18.0, 2.0, 5.0, accent(0x2DD4BF), 1.0))
            .child(zone(13.0, 15.0, 2.0, 8.0, accent(0x2DD4BF), 1.0))
            .child(zone(14.0, 15.0, 5.0, 2.0, accent(0x2DD4BF), 1.0))
            .child(zone(0.0, 28.0, 24.0, 6.0, 0x115E59, 0.0))
            .child(zone(6.0, 30.0, 12.0, 1.5, light, 0.75)),
        Travel => card
            .child(zone(2.0, 2.0, 20.0, 24.0, accent(0xF59E0B), 2.0))
            .child(zone(5.0, 5.0, 6.0, 6.0, 0xFEF3C7, 3.0))
            .child(zone(3.0, 19.0, 12.0, 7.0, 0x92400E, 1.5))
            .child(zone(11.0, 15.0, 10.0, 11.0, 0x78350F, 1.5))
            .child(zone(4.0, 29.0, 9.0, 1.5, 0xD97706, 0.75))
            .child(zone(16.0, 29.0, 4.0, 1.5, 0xD6D3D1, 0.75)),
        CodeBook => card
            .child(zone(0.0, 0.0, 5.0, 34.0, 0x5B21B6, 0.0))
            .child(zone(16.0, 0.0, 3.5, 9.0, 0xF472B6, 0.8))
            .child(glyph(7.0, 11.0, 15.0, icons::CODE, 0xFFFFFF))
            .child(zone(8.0, 26.0, 12.0, 2.0, 0xC4B5FD, 1.0))
            .child(zone(8.0, 30.0, 8.0, 1.5, light, 0.75)),
        Design => card
            .child(zone(4.0, 5.0, 7.0, 9.0, accent(0xFB923C), 3.0))
            .child(zone(13.0, 5.0, 7.0, 9.0, 0x38BDF8, 3.0))
            .child(zone(4.0, 15.0, 7.0, 9.0, 0xF43F5E, 3.0))
            .child(zone(13.0, 15.0, 7.0, 9.0, 0xA855F7, 3.0))
            .child(zone(0.0, 28.0, 24.0, 6.0, 0x1E2028, 0.0))
            .child(zone(4.0, 30.0, 4.0, 1.5, 0x94A3B8, 0.75))
            .child(zone(10.0, 30.0, 4.0, 1.5, 0x94A3B8, 0.75))
            .child(zone(16.0, 30.0, 4.0, 1.5, light, 0.75)),
        Browser => card
            .child(zone(0.0, 0.0, 24.0, 7.0, accent(0x2563EB), 0.0))
            .child(zone(3.0, 2.5, 12.0, 2.0, 0xBFDBFE, 1.0))
            .child(zone(19.0, 2.5, 2.0, 2.0, 0xBFDBFE, 1.0))
            .child(badge(4.0, 12.0, 16.0, 16.0, 0x3B82F6, 8.0))
            .child(glyph(7.0, 15.0, 10.0, icons::SEND, 0xFFFFFF))
            .child(zone(12.0, 16.0, 2.0, 3.0, 0xF43F5E, 1.0)),
        Shield => card
            .child(zone(16.0, 0.0, 8.0, 9.0, 0x991B1B, 1.0))
            .child(badge(5.0, 9.0, 14.0, 17.0, 0xFFFFFF, 6.0))
            .child(zone(11.0, 12.0, 2.0, 6.0, 0xDC2626, 1.0))
            .child(zone(11.0, 21.0, 2.0, 2.0, 0xDC2626, 1.0))
            .child(zone(0.0, 29.0, 24.0, 5.0, 0x991B1B, 0.0))
            .child(zone(5.0, 31.0, 14.0, 1.0, light, 0.5)),
        Roadmap => {
            card = card
                .child(zone(0.0, 0.0, 24.0, 12.0, accent(0x22C55E), 0.0))
                .child(zone(6.0, 1.0, 2.0, 4.0, 0xDCFCE7, 1.0))
                .child(zone(16.0, 1.0, 2.0, 4.0, 0xDCFCE7, 1.0));
            for y in [16.0, 23.0] {
                for x in [4.0, 10.0, 16.0] {
                    card = card.child(zone(x, y, 4.0, 4.5, accent(0x10B981), 1.0));
                }
            }
            card
        }
        Finance => card
            .child(zone(0.0, 0.0, 24.0, 6.0, 0x047857, 0.0))
            .child(badge(6.0, 9.0, 12.0, 12.0, 0xFBBF24, 6.0))
            .child(zone(11.0, 11.0, 2.0, 8.0, 0xB45309, 1.0))
            .child(zone(4.0, 26.0, 16.0, 2.0, 0xA7F3D0, 1.0))
            .child(zone(4.0, 30.0, 10.0, 1.5, light, 0.75)),
        Growth => card
            .child(badge(3.0, 5.0, 18.0, 22.0, 0x1F1220, 2.0))
            .child(zone(6.0, 18.0, 3.0, 6.0, accent(0xFB7185), 0.8))
            .child(zone(11.0, 13.0, 3.0, 11.0, 0xFBBF24, 0.8))
            .child(zone(16.0, 9.0, 3.0, 15.0, 0x4ADE80, 0.8))
            .child(zone(0.0, 30.0, 24.0, 4.0, 0x9F1239, 0.0)),
        Support => card
            .child(zone(0.0, 0.0, 24.0, 6.0, 0x3730A3, 0.0))
            .child(badge(3.0, 10.0, 18.0, 13.0, 0xFFFFFF, 3.0))
            .child(zone(6.0, 21.0, 4.0, 5.0, 0xFFFFFF, 1.0))
            .child(zone(6.0, 14.0, 12.0, 1.5, 0x4F46E5, 0.75))
            .child(zone(6.0, 18.0, 8.0, 1.5, 0x4F46E5, 0.75))
            .child(zone(5.0, 30.0, 14.0, 1.5, light, 0.75)),
        Database => {
            for (y, color) in [(5.0, 0x38BDF8), (13.0, 0x0EA5E9), (21.0, 0x0284C7)] {
                card = card
                    .child(badge(3.0, y, 18.0, 7.0, accent(color), 3.0))
                    .child(zone(16.0, y + 2.5, 2.0, 2.0, 0x4ADE80, 1.0));
            }
            card.child(zone(0.0, 31.0, 24.0, 3.0, 0x172554, 0.0))
        }
        Rocket => card
            .child(zone(0.0, 28.0, 24.0, 6.0, 0xF97316, 0.0))
            .child(zone(8.0, 25.0, 8.0, 9.0, 0xFBBF24, 3.0))
            .child(badge(8.0, 7.0, 8.0, 18.0, 0xFFFFFF, 4.0))
            .child(glyph(3.0, 7.0, 18.0, icons::ARROW_UP, 0xFFFFFF))
            .child(zone(10.0, 13.0, 4.0, 4.0, 0x38BDF8, 2.0)),
        Docs => card
            .child(zone(0.0, 0.0, 4.0, 34.0, accent(0xD97706), 0.0))
            .child(zone(7.0, 5.0, 13.0, 3.0, 0xF59E0B, 1.0))
            .child(zone(7.0, 12.0, 13.0, 1.5, 0x475569, 0.75))
            .child(zone(7.0, 16.0, 11.0, 1.5, 0x475569, 0.75))
            .child(zone(7.0, 20.0, 13.0, 1.5, 0x475569, 0.75))
            .child(zone(7.0, 24.0, 8.0, 1.5, 0x475569, 0.75))
            .child(zone(4.0, 31.0, 20.0, 3.0, 0xFDE68A, 0.0)),
        Git => card
            .child(zone(0.0, 0.0, 24.0, 6.0, accent(0xEA580C), 0.0))
            .child(zone(7.0, 10.0, 2.0, 16.0, 0xFB923C, 1.0))
            .child(zone(15.0, 12.0, 2.0, 9.0, 0x38BDF8, 1.0))
            .child(zone(8.0, 20.0, 9.0, 2.0, 0x38BDF8, 1.0))
            .child(zone(5.0, 9.0, 6.0, 6.0, 0xFB923C, 3.0))
            .child(zone(13.0, 10.0, 6.0, 6.0, 0x38BDF8, 3.0))
            .child(zone(5.0, 23.0, 6.0, 6.0, 0xFB923C, 3.0))
            .child(zone(0.0, 32.0, 24.0, 2.0, 0x3F3F46, 0.0)),
        Media => card
            .child(badge(2.0, 7.0, 20.0, 18.0, 0x1E1033, 2.0))
            .child(glyph(5.0, 9.0, 14.0, icons::PLAY, 0xFFFFFF))
            .child(zone(3.0, 29.0, 18.0, 2.0, 0x4C1D95, 1.0))
            .child(zone(3.0, 29.0, 12.0, 2.0, 0xEC4899, 1.0))
            .child(zone(14.0, 28.0, 3.0, 4.0, light, 1.5)),
        People => card
            .child(zone(8.0, 2.0, 8.0, 2.0, 0x0C4A6E, 1.0))
            .child(badge(4.0, 7.0, 16.0, 17.0, 0xE0F2FE, 3.0))
            .child(zone(9.0, 9.0, 6.0, 6.0, 0x0369A1, 3.0))
            .child(zone(7.0, 16.0, 10.0, 6.0, 0x0369A1, 3.0))
            .child(zone(5.0, 27.0, 14.0, 1.5, light, 0.75))
            .child(zone(7.0, 31.0, 10.0, 1.0, 0xBAE6FD, 0.5)),
        Search => card
            .child(zone(0.0, 0.0, 24.0, 6.0, accent(0xF59E0B), 0.0))
            .child(glyph(5.0, 8.0, 14.0, icons::SEARCH, 0xFDE68A))
            .child(zone(4.0, 25.0, 16.0, 1.5, 0x94A3B8, 0.75))
            .child(zone(4.0, 29.0, 11.0, 1.5, light, 0.75)),
        TestLab => card
            .child(zone(0.0, 0.0, 24.0, 6.0, accent(0x10B981), 0.0))
            .child(badge(5.0, 9.0, 14.0, 13.0, 0xECFDF5, 2.0))
            .child(glyph(6.0, 9.0, 12.0, icons::CHECK, 0x059669))
            .child(zone(4.0, 26.0, 12.0, 2.0, 0x34D399, 1.0))
            .child(zone(17.0, 26.0, 3.0, 2.0, 0xF87171, 1.0))
            .child(zone(4.0, 30.0, 16.0, 1.5, light, 0.75)),
        Infra => {
            for y in [4.0, 12.0, 20.0] {
                card = card
                    .child(badge(2.0, y, 20.0, 6.0, 0x1E293B, 1.5))
                    .child(zone(4.0, y + 2.0, 2.0, 2.0, 0x22C55E, 1.0));
                for x in [10.0, 13.0, 16.0] {
                    card = card.child(zone(x, y + 2.0, 1.0, 2.0, 0x64748B, 0.5));
                }
            }
            card.child(zone(0.0, 30.0, 24.0, 4.0, accent(0x0284C7), 0.0))
        }
        Spark => card
            .child(zone(0.0, 0.0, 24.0, 6.0, 0x312E81, 0.0))
            .child(glyph(3.0, 9.0, 18.0, icons::SPARKLES, 0xA5B4FC))
            .child(zone(11.0, 15.0, 2.0, 7.0, 0xFFFFFF, 1.0))
            .child(zone(0.0, 30.0, 24.0, 4.0, 0x6366F1, 0.0)),
    };
    card.child(zone(2.0, 0.5, 20.0, 0.6, 0xFFFFFF, 0.3).bg(rgba(0xFFFFFF24)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(title: &str, cwd: Option<&str>, key: &str) -> VerticonSpec {
        resolve_verticon(title, cwd, "General", AgentKind::Codex, key, None, None)
    }

    #[test]
    fn matches_real_session_titles_and_project_folders() {
        use VerticonKind::*;
        for (title, expected) in [
            ("multibot", Terminal),
            ("Postbox Rust 2", Mail),
            ("Multi Bot", Bot),
            ("BTOP", Equalizer),
            ("VPS Health", Telemetry),
            ("Valentin Travel", Travel),
            ("HT Ratstack", CodeBook),
            ("inne Design Lead", Design),
            ("MP Live Chat", Browser),
            ("Triage LC", Shield),
            ("HT Product Lead", Roadmap),
            ("HT Finance", Finance),
            ("Growth", Growth),
            ("Support", Support),
            ("Postgres", Database),
            ("Deploy", Rocket),
            ("Readme", Docs),
            ("Worktree", Git),
            ("Dictation", Media),
            ("Candidate", People),
            ("Research", Search),
            ("Verify", TestLab),
            ("multibotd", Infra),
            ("Untitled", Spark),
        ] {
            assert_eq!(resolve(title, None, "session").kind, expected, "{title}");
            if expected != Spark {
                for path in [
                    format!("/projects/{title}/src"),
                    format!("C:\\projects\\{title}\\"),
                ] {
                    assert_eq!(
                        resolve("Untitled", Some(&path), "session").kind,
                        expected,
                        "{path}"
                    );
                }
            }
        }
    }

    #[test]
    fn topics_outrank_folder_workspace_and_shell_defaults() {
        assert_eq!(
            resolve("VPS Health", Some("/projects/multibot"), "s").kind,
            VerticonKind::Telemetry
        );
        assert_eq!(
            resolve("Untitled", Some("/postbox/projects/generic/src"), "s").kind,
            VerticonKind::Spark
        );
        assert_eq!(
            resolve("Candidate", Some("/repo/shell"), "s").kind,
            VerticonKind::People
        );
        for (title, expected) in [
            ("Untitled", VerticonKind::Terminal),
            ("BTOP", VerticonKind::Equalizer),
        ] {
            assert_eq!(
                resolve_verticon(title, None, "General", AgentKind::Shell, "s", None, None).kind,
                expected
            );
        }
        assert_eq!(
            resolve_verticon(
                "Untitled",
                None,
                "HT Finance",
                AgentKind::Codex,
                "s",
                None,
                None
            )
            .kind,
            VerticonKind::Finance
        );
        for title in ["private", "stop", "websiteish", "buildable", "guideline"] {
            assert_eq!(
                resolve(title, None, "s").kind,
                VerticonKind::Spark,
                "{title}"
            );
        }
    }

    #[test]
    fn siblings_keep_their_topic_with_deterministic_palette_variation() {
        let first = resolve("Untitled", Some("/repo/postbox"), "host:session-1");
        let second = resolve("Untitled", Some("/repo/postbox"), "host:session-2");
        assert_eq!(first.kind, VerticonKind::Mail);
        assert_eq!(first.kind, second.kind);
        assert_ne!(first.palette, second.palette);
        assert_ne!(
            PALETTES[first.palette as usize],
            PALETTES[second.palette as usize]
        );
        assert_eq!(
            first,
            resolve("Untitled", Some("/repo/postbox"), "host:session-1")
        );
    }

    #[test]
    fn persisted_face_indices_and_hex_color_overrides_are_honored() {
        for index in 0..24 {
            let spec = resolve_verticon(
                "Support",
                None,
                "General",
                AgentKind::Shell,
                "s",
                Some(index),
                Some("#aB12Ef"),
            );
            assert_eq!(spec.kind as u8, index);
            assert_eq!(spec.color, Some(0xAB12EF));
        }
        for face in [24, 255] {
            assert_eq!(
                resolve_verticon(
                    "Support",
                    None,
                    "General",
                    AgentKind::Shell,
                    "s",
                    Some(face),
                    None
                )
                .kind,
                VerticonKind::Support
            );
        }
        for color in ["#fff", "FFFFFF", "#12345678", "#12zz45", "#１２"] {
            assert!(
                resolve_verticon(
                    "Support",
                    None,
                    "",
                    AgentKind::Codex,
                    "s",
                    None,
                    Some(color)
                )
                .color
                .is_none()
            );
        }
        assert_eq!(VerticonKind::from_index(24), None);
    }
}
