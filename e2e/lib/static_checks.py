"""Frozen external contract & Hard Invariant verifiers (zero third-party deps)."""

from __future__ import annotations

import re
import tomllib
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple

from . import REPO_ROOT

ALLOWED_GPUI_KIT_FEATURES = {"component", "syntect", "x11", "test-support"}
FORBIDDEN_GPUI_FEATURES = {"gpui-fast", "tree-sitter"}

FORBIDDEN_UPDATE_COPY_RE = re.compile(
    r"(?:\bseconds?\b|\bminutes?\b|\ba few\b|\btakes\b|\bETA\b|~\d+s)",
    re.IGNORECASE,
)

FORBIDDEN_CHAT_CHROME_LITERALS = (
    "Sending...",
    "Sending\u2026",
    "Waiting for connection",
    "\u2713 Sent",
    "queued in Claude",
    "\u25f7",
)

OLED_REQUIRED_TOKENS = (
    "0x0D0D0D",
    "0x111111",
    "0x1A1A1A",
    "0x2B2B2B",
    "0x2A2A2A",
    "18.0",
    "12.0",
    "9999.0",
)

PHASE2_RESERVED_CHORDS = {
    "alt-a",
    "alt-q",
    "ctrl-shift-s",
    "ctrl-shift-space",
    "alt-c",
}

SLICE1_REQUIRED_CHORDS = {
    "ctrl-k",
    "ctrl-shift-k",
    "alt-i",
    "ctrl-shift-i",
    "ctrl-shift-b",
    "ctrl-,",
    "f1",
    "ctrl-tab",
    "ctrl-shift-tab",
    "ctrl-1",
    "ctrl-2",
    "ctrl-3",
    "ctrl-4",
    "ctrl-5",
    "ctrl-6",
    "ctrl-7",
    "ctrl-8",
    "ctrl-9",
    "ctrl-n",
    "escape",
}

SLICE34_REQUIRED_WIRED_CHORDS = {
    "alt-1",
    "alt-2",
    "alt-3",
    "alt-4",
    "alt-5",
    "alt-6",
    "alt-7",
    "alt-8",
    "alt-9",
    "alt-s",
    "ctrl-g",
    "f2",
    "ctrl-shift-d",
    "ctrl-shift-u",
    "alt-j",
    "alt-k",
    "ctrl-f",
    "ctrl-/",
    "ctrl-shift-y",
    "ctrl-shift-up",
    "ctrl-shift-down",
}


_HEX_DIGITS = frozenset("0123456789abcdefABCDEF")


def _match_rust_char_literal(src: str, i: int) -> Optional[int]:
    """If src[i] == \"'\" starts a Rust char literal, return exclusive end index; else None (lifetime)."""
    n = len(src)
    if i + 1 >= n:
        return None
    nxt = src[i + 1]
    if nxt == "\\":
        if i + 2 >= n:
            return None
        esc = src[i + 2]
        if esc in ("n", "r", "t", "\\", "0", "'", '"'):
            return i + 4 if (i + 3 < n and src[i + 3] == "'") else None
        if esc == "x":
            if (
                i + 5 < n
                and src[i + 3] in _HEX_DIGITS
                and src[i + 4] in _HEX_DIGITS
                and src[i + 5] == "'"
            ):
                return i + 6
            return None
        if esc == "u" and i + 3 < n and src[i + 3] == "{":
            j = i + 4
            hex_count = 0
            while j < n and (src[j] in _HEX_DIGITS or src[j] == "_"):
                if src[j] != "_":
                    hex_count += 1
                j += 1
            if 1 <= hex_count <= 6 and j + 1 < n and src[j] == "}" and src[j + 1] == "'":
                return j + 2
            return None
        return i + 4 if (i + 3 < n and src[i + 3] == "'") else None
    if nxt not in ("'", "\\", "\n", "\r") and i + 2 < n and src[i + 2] == "'":
        return i + 3
    return None


def _match_rust_raw_string(src: str, i: int) -> Optional[Tuple[int, int, int]]:
    """If r#*"..."#*, br#*"..."#*, or cr#*"..."#* starts at i, return (content_start, content_end, tok_end)."""
    n = len(src)
    if i > 0 and (src[i - 1].isalnum() or src[i - 1] == "_"):
        return None
    ch = src[i]
    if ch == "r":
        r_pos = i
    elif ch in ("b", "c") and i + 1 < n and src[i + 1] == "r":
        r_pos = i + 1
    else:
        return None
    j = r_pos + 1
    hashes = 0
    while j < n and src[j] == "#":
        hashes += 1
        j += 1
    if j >= n or src[j] != '"':
        return None
    content_start = j + 1
    end_delim = '"' + ("#" * hashes)
    end_idx = src.find(end_delim, content_start)
    if end_idx == -1:
        return (content_start, n, n)
    return (content_start, end_idx, end_idx + len(end_delim))


def strip_rust_comments(src: str) -> str:
    """Strip // and nested /* /* */ */ comments while preserving Rust strings, raw strings, char literals, and line numbers."""
    out: List[str] = []
    i = 0
    n = len(src)
    while i < n:
        ch = src[i]
        nxt = src[i + 1] if i + 1 < n else ""
        raw_m = _match_rust_raw_string(src, i)
        if raw_m is not None:
            _, _, tok_end = raw_m
            out.append(src[i:tok_end])
            i = tok_end
            continue
        if ch == '"':
            start = i
            i += 1
            while i < n:
                if src[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                if src[i] == '"':
                    i += 1
                    break
                i += 1
            out.append(src[start:i])
            continue
        if ch == "'":
            char_end = _match_rust_char_literal(src, i)
            if char_end is not None:
                out.append(src[i:char_end])
                i = char_end
                continue
        if ch == "/" and nxt == "/":
            while i < n and src[i] != "\n":
                i += 1
            continue
        if ch == "/" and nxt == "*":
            i += 2
            depth = 1
            nl_count = 0
            while i < n and depth > 0:
                if i + 1 < n and src[i] == "/" and src[i + 1] == "*":
                    depth += 1
                    i += 2
                elif i + 1 < n and src[i] == "*" and src[i + 1] == "/":
                    depth -= 1
                    i += 2
                else:
                    if src[i] == "\n":
                        nl_count += 1
                    i += 1
            out.append("\n" * nl_count if nl_count else " ")
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def extract_rust_brace_block(cleaned: str, open_brace_idx: int) -> Tuple[str, int]:
    """Return (balanced '{...}' block, exclusive_end_idx) in comment-stripped Rust source."""
    if open_brace_idx < 0 or open_brace_idx >= len(cleaned) or cleaned[open_brace_idx] != "{":
        raise ValueError(f"Expected '{{' at index {open_brace_idx}")
    i = open_brace_idx
    n = len(cleaned)
    depth = 0
    while i < n:
        ch = cleaned[i]
        if ch == "'":
            char_end = _match_rust_char_literal(cleaned, i)
            if char_end is not None:
                i = char_end
                continue
        raw_m = _match_rust_raw_string(cleaned, i)
        if raw_m is not None:
            i = raw_m[2]
            continue
        if ch == '"':
            i += 1
            while i < n:
                if cleaned[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                if cleaned[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return cleaned[open_brace_idx : i + 1], i + 1
        i += 1
    return cleaned[open_brace_idx:], n


def mask_rust_string_and_char_literals(cleaned: str) -> str:
    """Replace contents of Rust string, raw-string, and char literals with spaces (preserving offsets/newlines)."""
    out: List[str] = []
    i = 0
    n = len(cleaned)
    while i < n:
        ch = cleaned[i]
        if ch == "'":
            char_end = _match_rust_char_literal(cleaned, i)
            if char_end is not None:
                out.append("'" + " " * (char_end - i - 2) + "'")
                i = char_end
                continue
        raw_m = _match_rust_raw_string(cleaned, i)
        if raw_m is not None:
            c_start, c_end, tok_end = raw_m
            out.append(cleaned[i:c_start])
            out.append("".join("\n" if c == "\n" else " " for c in cleaned[c_start:c_end]))
            out.append(cleaned[c_end:tok_end])
            i = tok_end
            continue
        if ch == '"':
            i += 1
            buf: List[str] = ['"']
            while i < n:
                if cleaned[i] == "\\" and i + 1 < n:
                    buf.append("\n" if cleaned[i] == "\n" else " ")
                    buf.append("\n" if cleaned[i + 1] == "\n" else " ")
                    i += 2
                    continue
                if cleaned[i] == '"':
                    buf.append('"')
                    i += 1
                    break
                buf.append("\n" if cleaned[i] == "\n" else " ")
                i += 1
            out.append("".join(buf))
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def production_rust_source(src: str) -> str:
    """Return Rust source with comments AND #[cfg(test)] items/modules removed (preserving line numbers)."""
    cleaned = strip_rust_comments(src)
    masked = mask_rust_string_and_char_literals(cleaned)
    out: List[str] = []
    pos = 0
    n = len(cleaned)
    pat = re.compile(r"#\[cfg\(\s*test\s*\)\]")

    while True:
        m = pat.search(masked, pos)
        if not m:
            out.append(cleaned[pos:])
            break
        out.append(cleaned[pos : m.start()])
        i = m.end()
        pdepth = 0
        bdepth = 0
        while i < n:
            c = masked[i]
            if c == "(":
                pdepth += 1
            elif c == ")" and pdepth > 0:
                pdepth -= 1
            elif c == "[":
                bdepth += 1
            elif c == "]" and bdepth > 0:
                bdepth -= 1
            elif c == ";" and pdepth == 0 and bdepth == 0:
                i += 1
                break
            elif c == "{" and pdepth == 0 and bdepth == 0:
                _, close_idx = extract_rust_brace_block(masked, i)
                i = close_idx
                break
            i += 1
        removed = cleaned[m.start() : i]
        out.append("\n" * removed.count("\n"))
        pos = i
    return "".join(out)


def read_crate_production_sources(src_dir: Path) -> str:
    """Concatenate production (non-comment, non-#[cfg(test)], non-tests.rs) Rust files under src_dir."""
    assert src_dir.is_dir(), f"Missing Rust source directory: {src_dir}"
    files = [
        p
        for p in sorted(src_dir.rglob("*.rs"))
        if p.name != "tests.rs" and "tests" not in p.relative_to(src_dir).parts[:-1]
    ]
    assert files, f"No production Rust source files under {src_dir}"
    return "\n".join(production_rust_source(p.read_text(encoding="utf-8")) for p in files)


def extract_rust_string_literals(src: str) -> List[Tuple[int, str]]:
    """Extract (1-based line_no, literal_text) from non-comment, non-test Rust source code."""
    cleaned = production_rust_source(src)

    literals: List[Tuple[int, str]] = []
    i = 0
    n = len(cleaned)
    line_no = 1
    while i < n:
        ch = cleaned[i]
        if ch == "\n":
            line_no += 1
            i += 1
            continue
        if ch == "'":
            char_end = _match_rust_char_literal(cleaned, i)
            if char_end is not None:
                i = char_end
                continue
        raw_m = _match_rust_raw_string(cleaned, i)
        if raw_m is not None:
            c_start, c_end, tok_end = raw_m
            start_line = line_no
            lit = cleaned[c_start:c_end]
            line_no += lit.count("\n")
            literals.append((start_line, lit))
            i = tok_end
            continue
        if ch == '"':
            start_line = line_no
            i += 1
            buf: List[str] = []
            while i < n:
                c = cleaned[i]
                if c == "\n":
                    line_no += 1
                if c == "\\" and i + 1 < n:
                    # Decode Rust \u{XXXX} escapes so Unicode-escaped forbidden chrome cannot evade scans
                    if cleaned[i + 1] == "u" and i + 2 < n and cleaned[i + 2] == "{":
                        j = i + 3
                        hex_chars: List[str] = []
                        while j < n and (cleaned[j] in _HEX_DIGITS or cleaned[j] == "_"):
                            if cleaned[j] != "_":
                                hex_chars.append(cleaned[j])
                            j += 1
                        if 1 <= len(hex_chars) <= 6 and j < n and cleaned[j] == "}":
                            cp = int("".join(hex_chars), 16)
                            if cp <= 0x10FFFF and not (0xD800 <= cp <= 0xDFFF):
                                buf.append(chr(cp))
                                i = j + 1
                                continue
                    buf.append(c)
                    buf.append(cleaned[i + 1])
                    if cleaned[i + 1] == "\n":
                        line_no += 1
                    i += 2
                    continue
                if c == '"':
                    i += 1
                    break
                buf.append(c)
                i += 1
            literals.append((start_line, "".join(buf)))
            continue
        i += 1
    return literals


def parse_multibot_keys_shortcuts(repo_root: Path = REPO_ROOT) -> List[Dict[str, object]]:
    """Parse ShortcutSpec entries from crates/multibot-keys/src/lib.rs."""
    path = repo_root / "crates" / "multibot-keys" / "src" / "lib.rs"
    src = strip_rust_comments(path.read_text(encoding="utf-8"))
    block_re = re.compile(r"ShortcutSpec\s*\{(.*?)\}", re.DOTALL)
    entries: List[Dict[str, object]] = []
    for m in block_re.finditer(src):
        body = m.group(1)
        id_m = re.search(r'id:\s*"([^"]+)"', body)
        disp_m = re.search(r'display:\s*"([^"]+)"', body)
        key_m = re.search(r'gpui_key:\s*"([^"]+)"', body)
        wired_m = re.search(r"wired:\s*(true|false)", body)
        if id_m and key_m:
            entries.append(
                {
                    "id": id_m.group(1),
                    "display": disp_m.group(1) if disp_m else "",
                    "gpui_key": key_m.group(1),
                    "wired": (wired_m.group(1) == "true") if wired_m else False,
                }
            )
    return entries


def parse_main_keybindings(repo_root: Path = REPO_ROOT) -> List[str]:
    """Extract all keystroke strings passed to KeyBinding::new(...) in multibot-desktop/src/."""
    desktop_src = repo_root / "crates" / "multibot-desktop" / "src"
    keys: List[str] = []
    pat = re.compile(r'KeyBinding::new\(\s*"([^"]+)"')
    for rs in sorted(desktop_src.rglob("*.rs")):
        text = production_rust_source(rs.read_text(encoding="utf-8"))
        keys.extend(pat.findall(text))
    return keys


_CTRL_MOD_TOKENS = frozenset({"ctrl", "control"})
_ALT_MOD_TOKENS = frozenset({"alt"})


def _gpui_key_has_ctrl_alt(gpui_key: str) -> bool:
    for stroke in gpui_key.strip().lower().split():
        tokens = {t for t in stroke.split("-") if t}
        if (tokens & _CTRL_MOD_TOKENS and tokens & _ALT_MOD_TOKENS) or "altgr" in tokens:
            return True
    return False


def _display_chord_has_ctrl_alt(display: str) -> bool:
    norm = re.sub(r"\s*\+\s*", "+", display.strip().lower())
    for chord in re.split(r"[\s/,;]+", norm):
        if not chord:
            continue
        tokens = {t for t in chord.split("+") if t}
        if (tokens & _CTRL_MOD_TOKENS and tokens & _ALT_MOD_TOKENS) or "altgr" in tokens:
            return True
    return False


def find_ctrl_alt_bindings(repo_root: Path = REPO_ROOT) -> List[str]:
    """Detect any AltGr-conflicting Ctrl+Alt(+*) keystrokes in multibot-keys or multibot-desktop."""
    violations: List[str] = []
    for entry in parse_multibot_keys_shortcuts(repo_root):
        key = str(entry["gpui_key"])
        disp = str(entry["display"])
        if _gpui_key_has_ctrl_alt(key) or _display_chord_has_ctrl_alt(disp):
            violations.append(f"multibot-keys:{entry['id']}={key} ({disp})")
    for key in parse_main_keybindings(repo_root):
        if _gpui_key_has_ctrl_alt(key):
            violations.append(f"desktop KeyBinding:{key}")
    return violations


def check_dxgi_patch_invariants(repo_root: Path = REPO_ROOT) -> None:
    """Verify DXGI minimum-power, 0x10DE skip, WARP fallback, and zero Optimus/PowerXpress exports."""
    dxgi_path = repo_root / "patches" / "gpui-pre-windows" / "src" / "directx_devices.rs"
    assert dxgi_path.is_file(), f"Missing DXGI patch file: {dxgi_path}"
    dxgi_src = dxgi_path.read_text(encoding="utf-8")
    for token in (
        "0x10DE",
        "DXGI_GPU_PREFERENCE_MINIMUM_POWER",
        "DXGI_ADAPTER_FLAG_SOFTWARE",
        "auto_integrated",
        "specific_luid",
        "software_only",
        "MULTIBOT_GPU_REPORT_PATH",
    ):
        assert token in dxgi_src, f"DXGI patch missing required invariant token: {token}"

    # Ensure zero NvOptimusEnablement / AmdPowerXpress anywhere in crates/ or patches/
    forbidden_re = re.compile(r"\b(NvOptimusEnablement|AmdPowerXpress)\b")
    for base in (repo_root / "crates", repo_root / "patches"):
        if not base.exists():
            continue
        for p in sorted(base.rglob("*")):
            if p.is_file() and p.suffix in (".rs", ".def", ".c", ".cpp", ".h"):
                content = p.read_text(encoding="utf-8", errors="ignore")
                if forbidden_re.search(content):
                    raise AssertionError(
                        f"Forbidden GPU export symbol found in {p.relative_to(repo_root)}"
                    )


def check_desktop_cargo_gpui_features(repo_root: Path = REPO_ROOT) -> Set[str]:
    """Verify crates/multibot-desktop/Cargo.toml uses gpui-kit with default-features=false and safe features."""
    cargo_path = repo_root / "crates" / "multibot-desktop" / "Cargo.toml"
    data = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
    found_features: Set[str] = set()
    gpui_aliases: Set[str] = {"gpui-kit"}
    found_any_dep = False

    def _inspect_dep(dep_obj: object, where: str) -> None:
        nonlocal found_any_dep
        found_any_dep = True
        if not isinstance(dep_obj, dict):
            raise AssertionError(
                f"gpui-kit in {where} MUST be a table with default-features = false "
                f"(string shorthand {dep_obj!r} implicitly enables default features)"
            )
        default_feats = dep_obj.get("default-features", dep_obj.get("default_features", True))
        if default_feats is not False:
            raise AssertionError(f"gpui-kit in {where} MUST set default-features = false")
        feats = dep_obj.get("features", [])
        if not isinstance(feats, list):
            raise AssertionError(f"gpui-kit features in {where} must be a list")
        for feat in feats:
            feat_s = str(feat)
            found_features.add(feat_s)
            if feat_s in FORBIDDEN_GPUI_FEATURES:
                raise AssertionError(
                    f"Forbidden gpui-kit feature {feat_s!r} enabled in {where}"
                )
            if feat_s not in ALLOWED_GPUI_KIT_FEATURES:
                raise AssertionError(
                    f"Unexpected gpui-kit feature {feat_s!r} in {where}; "
                    f"allowed={sorted(ALLOWED_GPUI_KIT_FEATURES)}"
                )

    def _inspect_section(deps: object, where: str) -> None:
        if not isinstance(deps, dict):
            return
        for dep_name, dep_obj in deps.items():
            if dep_name == "gpui-kit" or (
                isinstance(dep_obj, dict) and dep_obj.get("package") == "gpui-kit"
            ):
                gpui_aliases.add(str(dep_name))
                _inspect_dep(dep_obj, f"{where}.{dep_name}")

    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        _inspect_section(data.get(section, {}), section)
    for target_cfg, target_tbl in data.get("target", {}).items():
        if not isinstance(target_tbl, dict):
            continue
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            _inspect_section(target_tbl.get(section, {}), f"target.{target_cfg}.{section}")

    # Inspect [features] for any feature forwarding to gpui-kit (including weak dep syntax `gpui-kit?/feat`)
    for feat_name, feat_items in data.get("features", {}).items():
        if not isinstance(feat_items, list):
            continue
        for item in feat_items:
            s = str(item).strip()
            if "/" not in s:
                continue
            dep_part, forwarded = s.split("/", 1)
            dep_key = dep_part.rstrip("?")
            if dep_key in gpui_aliases:
                if forwarded == "default" or forwarded in FORBIDDEN_GPUI_FEATURES:
                    raise AssertionError(
                        f"Forbidden gpui-kit feature forwarding {s!r} in [features].{feat_name}"
                    )
                if forwarded not in ALLOWED_GPUI_KIT_FEATURES:
                    raise AssertionError(
                        f"Unexpected gpui-kit feature forwarding {s!r} in [features].{feat_name}; "
                        f"allowed={sorted(ALLOWED_GPUI_KIT_FEATURES)}"
                    )

    assert found_any_dep and found_features, (
        "No gpui-kit dependency found in crates/multibot-desktop/Cargo.toml"
    )
    return found_features


def check_oled_theme_tokens(repo_root: Path = REPO_ROOT) -> None:
    """Verify OLED dark theme hex and border-radius tokens in crates/multibot-desktop/src/theme.rs."""
    theme_path = repo_root / "crates" / "multibot-desktop" / "src" / "theme.rs"
    assert theme_path.is_file(), f"Missing theme file: {theme_path}"
    text = theme_path.read_text(encoding="utf-8")
    missing = [tok for tok in OLED_REQUIRED_TOKENS if tok.lower() not in text.lower()]
    assert not missing, f"theme.rs missing OLED palette/radius tokens: {missing}"


def scan_update_copy_violations(repo_root: Path = REPO_ROOT) -> List[str]:
    """Scan string literals in update_ui.rs (and updater UI strings) for forbidden duration wording."""
    violations: List[str] = []
    targets = [
        repo_root / "crates" / "multibot-desktop" / "src" / "update_ui.rs",
    ]
    for path in targets:
        if not path.is_file():
            continue
        src = path.read_text(encoding="utf-8")
        for line_no, lit in extract_rust_string_literals(src):
            m = FORBIDDEN_UPDATE_COPY_RE.search(lit)
            if m:
                violations.append(
                    f"{path.relative_to(repo_root)}:{line_no}: forbidden duration wording {m.group(0)!r} in {lit!r}"
                )
    return violations


def scan_chat_chrome_violations(repo_root: Path = REPO_ROOT) -> List[str]:
    """Scan chat rendering files for forbidden legacy per-message status chrome."""
    violations: List[str] = []
    desktop_src = repo_root / "crates" / "multibot-desktop" / "src"
    for name in ("chat_view.rs", "app.rs"):
        path = desktop_src / name
        if not path.is_file():
            continue
        src = path.read_text(encoding="utf-8")
        for line_no, lit in extract_rust_string_literals(src):
            for bad in FORBIDDEN_CHAT_CHROME_LITERALS:
                if bad in lit:
                    violations.append(
                        f"{path.relative_to(repo_root)}:{line_no}: forbidden chat chrome {bad!r} in {lit!r}"
                    )
    return violations


def check_conditional_footprint_notify(repo_root: Path = REPO_ROOT) -> None:
    """Verify demand-driven rendering: zero 60 FPS loops and conditional footprint cx.notify()."""
    desktop_src = repo_root / "crates" / "multibot-desktop" / "src"
    tight_loop_re = re.compile(r"Duration::from_millis\(\s*(\d+)\s*\)")
    loop_head_re = re.compile(r"\b(?:loop|while\b[^{;]*)\s*\{")
    sample_call_re = re.compile(r"\bsample_self_footprint\s*\(")
    notify_call_re = re.compile(r"\bcx\s*\.\s*notify\s*\(\s*\)")
    diff_signal_re = re.compile(
        r"(?:==|!=|\b(?:un)?changed\b|\bsame\b|\bequal\b|\bdirty\b|\bupdated\b|\bdiff\b|footprint|label|snap|prev|last|next)",
        re.IGNORECASE,
    )

    timer_loops: List[Tuple[Path, str]] = []
    for rs in sorted(desktop_src.rglob("*.rs")):
        raw = rs.read_text(encoding="utf-8")
        cleaned = production_rust_source(raw)
        masked = mask_rust_string_and_char_literals(cleaned)

        file_loops: List[Tuple[int, int, str]] = []
        for lm in loop_head_re.finditer(masked):
            open_idx = lm.end() - 1
            block, close_idx = extract_rust_brace_block(masked, open_idx)
            if notify_call_re.search(block):
                loop_text = masked[lm.start() : close_idx]
                for tm in tight_loop_re.finditer(loop_text):
                    ms = int(tm.group(1))
                    if ms < 200:
                        raise AssertionError(
                            f"Tight redraw loop ({ms}ms) calling cx.notify() in {rs.relative_to(repo_root)}"
                        )
            if sample_call_re.search(block):
                file_loops.append((open_idx, close_idx, block))

        # Keep only innermost loop blocks containing a real sample_self_footprint(...) call
        for s, e, block in file_loops:
            if not any(s < s2 and e2 < e for (s2, e2, _) in file_loops):
                timer_loops.append((rs, block))

    assert timer_loops, (
        "Could not locate any loop/while timer calling sample_self_footprint(...) in multibot-desktop/src"
    )

    for rs, block in timer_loops:
        rel = rs.relative_to(repo_root)
        call_m = sample_call_re.search(block)
        assert call_m is not None
        after_sample = block[call_m.start() :]
        notify_matches = list(notify_call_re.finditer(after_sample))
        assert notify_matches, (
            f"Footprint timer loop in {rel} calls sample_self_footprint(...) without a cx.notify() site to audit"
        )

        # Parse all `if <cond> { ... }` blocks occurring after sample_self_footprint(...)
        if_blocks: List[Tuple[int, int, int, str, str]] = []
        for im in re.finditer(r"\bif\b", after_sample):
            k = im.end()
            pdepth = 0
            bdepth = 0
            while k < len(after_sample):
                c = after_sample[k]
                if c == "(":
                    pdepth += 1
                elif c == ")":
                    pdepth -= 1
                elif c == "[":
                    bdepth += 1
                elif c == "]":
                    bdepth -= 1
                elif c == "{" and pdepth == 0 and bdepth == 0:
                    break
                elif c == ";" and pdepth == 0 and bdepth == 0:
                    k = len(after_sample)
                    break
                k += 1
            if k < len(after_sample) and after_sample[k] == "{":
                cond = after_sample[im.end() : k].strip()
                ibody, iclose = extract_rust_brace_block(after_sample, k)
                if_blocks.append((im.start(), k, iclose, cond, ibody))

        for nm in notify_matches:
            npos = nm.start()
            # Guard 1: cx.notify() sits inside `if <diff_cond> { ... cx.notify() ... }`
            enclosing_ok = any(
                open_b < npos < close_b
                and cond not in ("true", "false")
                and diff_signal_re.search(cond)
                for (_, open_b, close_b, cond, _) in if_blocks
            )
            # Guard 2: early-exit `if <diff_cond> { ... continue/return ... }` precedes cx.notify()
            early_exit_ok = any(
                close_b <= npos
                and cond not in ("true", "false")
                and diff_signal_re.search(cond)
                and re.search(r"\b(?:continue|return)\b", ibody)
                for (_, _, close_b, cond, ibody) in if_blocks
            )
            if not (enclosing_ok or early_exit_ok):
                raise AssertionError(
                    f"Demand-driven invariant violated in {rel}: "
                    f"footprint timer calls cx.notify() without a diff/change guard after sample_self_footprint()"
                )
