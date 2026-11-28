use super::*;

pub(crate) fn fit_image_size(width: u32, height: u32, max_w: f32, max_h: f32) -> (f32, f32) {
    if width == 0 || height == 0 {
        return (0.0, 0.0);
    }
    let scale = (max_w.max(0.0) / width as f32)
        .min(max_h.max(0.0) / height as f32)
        .min(1.0);
    (width as f32 * scale, height as f32 * scale)
}

pub(crate) fn read_image_dimensions(path: &Path) -> Option<(u32, u32)> {
    use std::sync::{Mutex, OnceLock};
    type DimensionsCache = HashMap<PathBuf, Option<(u32, u32)>>;
    static CACHE: OnceLock<Mutex<DimensionsCache>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Mutex::default).lock().ok()?;
    if let Some(size) = cache.get(path) {
        return *size;
    }
    // Missing downloads may appear on the next frame; don't cache missing files.
    let file = std::fs::File::open(path).ok()?;
    let size = image_header_dimensions(&mut std::io::BufReader::new(file));
    if cache.len() >= 256 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), size);
    size
}

fn image_header_dimensions(
    reader: &mut (impl std::io::Read + std::io::Seek),
) -> Option<(u32, u32)> {
    use std::io::SeekFrom;
    let mut header = [0u8; 12];
    reader.read_exact(&mut header).ok()?;
    let dimensions = if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut ihdr = [0u8; 12];
        reader.read_exact(&mut ihdr).ok()?;
        if &ihdr[..4] != b"IHDR" || header[8..12] != 13u32.to_be_bytes() {
            return None;
        }
        (
            u32::from_be_bytes(ihdr[4..8].try_into().ok()?),
            u32::from_be_bytes(ihdr[8..12].try_into().ok()?),
        )
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        (
            u16::from_le_bytes(header[6..8].try_into().ok()?) as u32,
            u16::from_le_bytes(header[8..10].try_into().ok()?) as u32,
        )
    } else if header.starts_with(b"\xff\xd8") {
        reader.seek(SeekFrom::Start(2)).ok()?;
        loop {
            let mut byte = [0];
            reader.read_exact(&mut byte).ok()?;
            if byte[0] != 0xff {
                return None;
            }
            while byte[0] == 0xff {
                reader.read_exact(&mut byte).ok()?;
            }
            let marker = byte[0];
            if matches!(marker, 0xd9 | 0xda | 0x00) {
                return None;
            }
            if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                continue;
            }
            let mut length = [0; 2];
            reader.read_exact(&mut length).ok()?;
            let length = u16::from_be_bytes(length);
            if length < 2 {
                return None;
            }
            if matches!(marker, 0xc0..=0xc2) {
                if length < 8 {
                    return None;
                }
                let mut sof = [0; 5];
                reader.read_exact(&mut sof).ok()?;
                break (
                    u16::from_be_bytes([sof[3], sof[4]]) as u32,
                    u16::from_be_bytes([sof[1], sof[2]]) as u32,
                );
            }
            reader.seek(SeekFrom::Current(i64::from(length) - 2)).ok()?;
        }
    } else if header.starts_with(b"RIFF") && &header[8..12] == b"WEBP" {
        let end = u64::from(u32::from_le_bytes(header[4..8].try_into().ok()?)) + 8;
        loop {
            if reader.stream_position().ok()?.checked_add(8)? > end {
                return None;
            }
            let mut chunk = [0; 8];
            reader.read_exact(&mut chunk).ok()?;
            let len = u32::from_le_bytes(chunk[4..8].try_into().ok()?);
            let next = reader.stream_position().ok()?.checked_add(u64::from(len))?;
            if next > end {
                return None;
            }
            match &chunk[..4] {
                b"VP8X" if len >= 10 => {
                    let mut data = [0; 10];
                    reader.read_exact(&mut data).ok()?;
                    break (
                        1 + u32::from_le_bytes([data[4], data[5], data[6], 0]),
                        1 + u32::from_le_bytes([data[7], data[8], data[9], 0]),
                    );
                }
                b"VP8L" if len >= 5 => {
                    let mut data = [0; 5];
                    reader.read_exact(&mut data).ok()?;
                    if data[0] != 0x2f {
                        return None;
                    }
                    let bits = u32::from_le_bytes(data[1..5].try_into().ok()?);
                    break ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1);
                }
                b"VP8 " if len >= 10 => {
                    let mut data = [0; 10];
                    reader.read_exact(&mut data).ok()?;
                    if data[0] & 1 != 0 || &data[3..6] != b"\x9d\x01\x2a" {
                        return None;
                    }
                    break (
                        u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff),
                        u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff),
                    );
                }
                _ => {
                    reader
                        .seek(SeekFrom::Start(next + u64::from(len % 2)))
                        .ok()?;
                }
            }
        }
    } else {
        return None;
    };
    (dimensions.0 > 0 && dimensions.1 > 0).then_some(dimensions)
}

// ============================================================================
// 6. Image Paste/Drop, File Attach, SSH Atomic Staging & Thumbnails
// ============================================================================

pub(super) fn extract_basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Checks strictly whether `path`'s basename ends with an allowlisted image extension
/// (`.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`, ASCII case-insensitive).
pub(crate) fn is_supported_image_ext(path: &str) -> bool {
    let base = extract_basename(path);
    let Some((_, ext)) = base.rsplit_once('.') else {
        return false;
    };
    if !ext.is_ascii() {
        return false;
    }
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    )
}

#[cfg(test)]
pub(crate) fn classify_and_validate_attachments(
    items: &[AttachmentCandidate],
) -> AttachmentClassification {
    classify_and_validate_attachments_with_existing(items, 0, 0)
}

pub(crate) fn classify_and_validate_attachments_with_existing(
    items: &[AttachmentCandidate],
    existing_images: usize,
    existing_files: usize,
) -> AttachmentClassification {
    let mut out = AttachmentClassification::default();

    for item in items {
        let path_str = item.path.to_string_lossy();
        let is_image = is_supported_image_ext(&path_str);
        if is_image {
            if item.size_bytes > MAX_IMAGE_BYTES {
                out.rejected_oversize.push((
                    item.path.clone(),
                    format!("Image exceeds 10 MiB limit ({} bytes)", item.size_bytes),
                ));
                continue;
            }
            if existing_images.saturating_add(out.images.len()) >= MAX_IMAGE_ATTACHMENTS {
                out.rejected_too_many += 1;
                continue;
            }
            out.images.push(item.clone());
        } else {
            if item.size_bytes > MAX_FILE_BYTES {
                out.rejected_oversize.push((
                    item.path.clone(),
                    format!("File exceeds 200 MiB limit ({} bytes)", item.size_bytes),
                ));
                continue;
            }
            if existing_files.saturating_add(out.files.len()) >= MAX_FILE_ATTACHMENTS {
                out.rejected_too_many += 1;
                continue;
            }
            out.files.push(item.clone());
        }
    }

    out
}

/// Replaces staged local filename placeholders in `payload` with their uploaded remote
/// `~/.multibot/uploads/<session>/<file>` paths in order, appending any unreferenced uploaded path.
pub(crate) fn substitute_uploaded_attachment_paths(
    payload: &str,
    replacements: &[(String, String)],
) -> String {
    let mut out = payload.to_string();
    let mut cursor = 0usize;
    let mut extra_paths = Vec::new();

    for (token, remote_path) in replacements {
        if token.is_empty() {
            extra_paths.push(remote_path.as_str());
            continue;
        }
        let search_slice = if cursor <= out.len() && out.is_char_boundary(cursor) {
            &out[cursor..]
        } else {
            cursor = 0;
            &out[..]
        };
        let found_offset = find_standalone_token(search_slice, token)
            .map(|rel| cursor + rel)
            .or_else(|| find_standalone_token(&out, token));
        if let Some(pos) = found_offset {
            let end = pos + token.len();
            out.replace_range(pos..end, remote_path);
            cursor = pos + remote_path.len();
        } else {
            extra_paths.push(remote_path.as_str());
        }
    }

    if !extra_paths.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&extra_paths.join("\n"));
    }
    out
}

pub(super) fn find_standalone_token(haystack: &str, needle: &str) -> Option<usize> {
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(needle) {
        let pos = start + rel;
        let end = pos + needle.len();
        let left_ok = pos == 0
            || haystack[..pos]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let right_ok = end == haystack.len()
            || haystack[end..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
        if left_ok && right_ok {
            return Some(pos);
        }
        let step = haystack[pos..].chars().next().map_or(1, char::len_utf8);
        start = pos + step;
    }
    None
}

pub(super) fn sanitize_ascii_token(raw: &str, max_bytes: usize, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    let mut at_leading_dots = true;
    for ch in raw.chars() {
        if out.len() >= max_bytes {
            break;
        }
        let safe = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            ch
        } else {
            '_'
        };
        if at_leading_dots && safe == '.' {
            out.push('_');
        } else {
            at_leading_dots = false;
            out.push(safe);
        }
    }
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

pub(crate) fn sanitize_remote_session_dir(session: &str) -> String {
    sanitize_ascii_token(session, REMOTE_NAME_MAX_BYTES, "session")
}

pub(crate) fn sanitize_remote_filename(path: &str) -> String {
    let base = extract_basename(path);
    if base.is_empty() {
        return "file".to_string();
    }
    let sanitized_full = sanitize_ascii_token(base, usize::MAX, "file");
    if sanitized_full.len() <= REMOTE_NAME_MAX_BYTES {
        return sanitized_full;
    }
    if let Some(dot) = sanitized_full.rfind('.') {
        let ext = &sanitized_full[dot..];
        if dot > 0 && ext.len() < REMOTE_NAME_MAX_BYTES {
            let stem_cap = REMOTE_NAME_MAX_BYTES - ext.len();
            let mut truncated = sanitized_full[..stem_cap].to_string();
            truncated.push_str(ext);
            return truncated;
        }
    }
    sanitized_full[..REMOTE_NAME_MAX_BYTES].to_string()
}

/// Builds a shell-safe, Windows-argv-safe (zero `"`), atomic SSH staging upload command
/// targeting `$HOME/.multibot/uploads/<sanitized_session>/<sanitized_name>` (NEVER `~/.multiprompt`).
pub(crate) fn build_ssh_upload_command(
    session_dir: &str,
    filename: &str,
    expected_size: u64,
) -> String {
    let dir = sanitize_remote_session_dir(session_dir);
    let file = sanitize_remote_filename(filename);
    format!(
        "sh -c 'set -ef; IFS=; umask 077; d=$HOME/.multibot/uploads/{dir}; mkdir -p $d; \
find $d -maxdepth 1 -type d -name .staging.* -mmin +1440 -exec rm -rf -- {{}} +; \
t=$(mktemp -d $d/.staging.XXXXXXXXXX); p=; \
trap '\\''[ -z $p ] || kill $p 2>/dev/null || :; rm -rf -- $t'\\'' 0; \
trap '\\''exit 1'\\'' HUP INT TERM; exec 3<&0; \
if dd if=/dev/null of=/dev/null status=progress 2>/dev/null; then \
dd bs=65536 status=progress <&3 > $t/data & else cat <&3 > $t/data & fi; \
p=$!; exec 3<&-; wait $p; p=; \
[ $(wc -c < $t/data) -eq {expected_size} ] || exit 1; \
f=$d/{file}; n=0; \
until ln -T -- $t/data $f 2>/dev/null; do \
[ -e $f ] || [ -L $f ] || exit 1; n=$((n+1)); f=$d/$n-{file}; \
done; rm -rf -- $t; trap - 0 HUP INT TERM; printf %s\\\\n $f'"
    )
}

/// Re-stats a staged attachment right before SSH upload, verifies it is still a regular file
/// within the configured image/file byte cap, and builds a fresh SSH upload command targeting
/// `session_dir` with the current file byte size.
pub(crate) async fn prepare_attachment_upload_command(
    session_dir: &str,
    att: &StagedAttachment,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let meta = tokio::fs::metadata(&att.path)
        .await
        .with_context(|| format!("failed to stat attachment {}", att.path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("attachment is not a regular file: {}", att.path.display());
    }
    let current_size = meta.len();
    let (limit, kind) = if att.is_image {
        (MAX_IMAGE_BYTES, "Image")
    } else {
        (MAX_FILE_BYTES, "File")
    };
    if current_size > limit {
        anyhow::bail!(
            "{kind} {} exceeds size limit ({current_size} bytes > {limit} bytes)",
            att.inserted_token
        );
    }
    Ok(build_ssh_upload_command(
        session_dir,
        &att.inserted_token,
        current_size,
    ))
}

/// Inserts an uploaded image/file reference at `(selection_start, selection_end)` with
/// exact single-space boundary rules (preserving `\t`/`\r`/`\n` and UTF-8 boundaries).
pub(crate) fn plan_image_insert(
    current: &str,
    selection_start: usize,
    selection_end: usize,
    reference: &str,
) -> Option<ImageInsertion> {
    let start = selection_start.min(selection_end).min(current.len());
    let mut end = selection_start.max(selection_end).min(current.len());
    if !current.is_char_boundary(start) || !current.is_char_boundary(end) {
        return None;
    }
    let trimmed_ref = reference.trim();
    if trimmed_ref.is_empty() {
        return None;
    }
    let needs_lead_space =
        start > 0 && !matches!(current.as_bytes()[start - 1], b' ' | b'\t' | b'\r' | b'\n');
    let bytes = current.as_bytes();
    while end < bytes.len() && bytes[end] == b' ' {
        end += 1;
    }

    let mut out = String::with_capacity(current.len() + trimmed_ref.len() + 2);
    out.push_str(&current[..start]);
    if needs_lead_space {
        out.push(' ');
    }
    out.push_str(trimmed_ref);
    out.push(' ');
    let caret_byte = out.len();
    out.push_str(&current[end..]);
    Some(ImageInsertion {
        text: out,
        caret_byte,
    })
}

/// Extracts the image file extension from a URL after stripping `?query` and `#fragment`.
pub(crate) fn image_ext_from_url(url: &str) -> &str {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let base = extract_basename(without_query);
    match base.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => "png",
    }
}

pub(crate) fn image_cache_path_in(cache_dir: &Path, key: &str, ext: &str) -> PathBuf {
    let hash = sha256_hex(key.as_bytes());
    let clean_ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let safe_ext = match clean_ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => clean_ext.as_str(),
        _ => "png",
    };
    cache_dir.join(format!("{hash}.{safe_ext}"))
}

pub(crate) fn image_cache_path(key: &str, ext: &str) -> PathBuf {
    image_cache_path_in(&MultibotSettings::image_cache_dir(), key, ext)
}

impl MultibotView {
    #[cfg(windows)]
    pub(crate) fn stage_browser_screenshot(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.stage_dropped_paths(&[path.to_path_buf()], window, cx);
        self.chat_state
            .pending_attachments
            .iter()
            .any(|attachment| attachment.path == path)
    }

    pub(crate) fn stage_dropped_paths(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_pending_draft(window, cx);
        let mut candidates = Vec::with_capacity(paths.len());
        let mut rejected_invalid = 0usize;
        for p in paths {
            match std::fs::metadata(p) {
                Ok(meta) if meta.is_file() => {
                    candidates.push(AttachmentCandidate {
                        path: p.clone(),
                        size_bytes: meta.len(),
                    });
                }
                _ => {
                    rejected_invalid += 1;
                }
            }
        }
        let existing_images = self
            .chat_state
            .pending_attachments
            .iter()
            .filter(|a| a.is_image)
            .count();
        let existing_files = self
            .chat_state
            .pending_attachments
            .iter()
            .filter(|a| !a.is_image)
            .count();
        let classified = classify_and_validate_attachments_with_existing(
            &candidates,
            existing_images,
            existing_files,
        );
        let mut current = self.inputs.prompt_input.read(cx).value().to_string();
        let mut caret = current.len();
        let staged_any = !classified.images.is_empty() || !classified.files.is_empty();

        for img_cand in classified.images {
            let name = img_cand
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image.png".to_string());
            if let Some(inserted) = plan_image_insert(&current, caret, caret, &name) {
                current = inserted.text;
                caret = inserted.caret_byte;
            }
            self.chat_state.pending_attachments.push(StagedAttachment {
                path: img_cand.path,
                is_image: true,
                size_bytes: img_cand.size_bytes,
                inserted_token: name,
            });
        }

        for file_cand in classified.files {
            let name = file_cand
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            if let Some(inserted) = plan_image_insert(&current, caret, caret, &name) {
                current = inserted.text;
                caret = inserted.caret_byte;
            }
            self.chat_state.pending_attachments.push(StagedAttachment {
                path: file_cand.path,
                is_image: false,
                size_bytes: file_cand.size_bytes,
                inserted_token: name,
            });
        }

        let mut notice_parts = Vec::new();
        if rejected_invalid > 0 {
            notice_parts.push(format!(
                "Skipped {rejected_invalid} non-file or inaccessible path(s) (only regular files can be attached)"
            ));
        }
        if !classified.rejected_oversize.is_empty() || classified.rejected_too_many > 0 {
            notice_parts.push(format!(
                "Skipped {} oversized and {} extra attachment(s) (max 10 images <=10 MiB, 10 files <=200 MiB)",
                classified.rejected_oversize.len(),
                classified.rejected_too_many
            ));
        }
        if !notice_parts.is_empty() {
            self.notice = Some(format!("{}.", notice_parts.join("; ")));
        }

        if staged_any {
            self.inputs.prompt_input.update(cx, |state, cx| {
                state.select_all(window, cx);
                state.replace(current, window, cx);
            });
        }
        cx.notify();
    }

    pub(super) fn handle_paste_from_clipboard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let mut paths = Vec::new();
        for entry in item.entries() {
            if let ClipboardEntry::ExternalPaths(ext) = entry {
                paths.extend(ext.paths().iter().cloned());
            }
        }
        if paths.is_empty() {
            return false;
        }
        self.stage_dropped_paths(&paths, window, cx);
        true
    }
}

pub(crate) const MAX_REMOTE_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFetchState {
    Loading,
    Cached,
    Failed,
}

fn remote_image_url_allowed(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.username().is_empty()
            && url.password().is_none()
            && (url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))))
    })
}

pub(crate) fn message_image_urls(message: &ChatMessage) -> Vec<String> {
    let mut seen: HashSet<String> = message
        .blocks
        .iter()
        .filter_map(|block| match block {
            MessageBlock::Image { url: Some(url), .. } => Some(url.clone()),
            _ => None,
        })
        .collect();
    let texts: Vec<&str> = if message.blocks.is_empty() {
        vec![&message.text]
    } else {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                MessageBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    };
    let mut urls = Vec::new();
    for text in texts {
        for range in crate::external_url::url_ranges(text) {
            let value = &text[range];
            if remote_image_url_allowed(value)
                && url::Url::parse(value).is_ok_and(|url| is_supported_image_ext(url.path()))
                && seen.insert(value.to_string())
            {
                urls.push(value.to_string());
            }
        }
    }
    urls
}

fn has_image_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

pub(crate) fn fetch_remote_image(url: &str, cache_path: &Path) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    static PART_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(15)))
            .build(),
    );
    let mut current = url::Url::parse(url)?;
    let mut response = None;
    for _ in 0..=5 {
        anyhow::ensure!(
            remote_image_url_allowed(current.as_str()),
            "Unsupported image URL"
        );
        let candidate = agent
            .get(current.as_str())
            .header("User-Agent", "Multibot/0.1 image-preview")
            .call()?;
        if candidate.status().is_redirection() {
            let location = candidate
                .headers()
                .get("location")
                .ok_or_else(|| anyhow::anyhow!("Missing image redirect"))?
                .to_str()?;
            current = current.join(location)?;
        } else {
            anyhow::ensure!(candidate.status().is_success(), "Image request failed");
            response = Some(candidate);
            break;
        }
    }
    let mut response = response.ok_or_else(|| anyhow::anyhow!("Too many image redirects"))?;
    if let Some(length) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        anyhow::ensure!(length <= MAX_REMOTE_IMAGE_BYTES, "Image exceeds 10 MB");
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_REMOTE_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_REMOTE_IMAGE_BYTES,
        "Image exceeds 10 MB"
    );
    anyhow::ensure!(has_image_magic(&bytes), "Response is not an image");
    let parent = cache_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing image cache directory"))?;
    std::fs::create_dir_all(parent)?;
    // Windows cannot replace an open file; distinct parts also isolate sibling windows.
    let part = cache_path.with_extension(format!(
        "{}.{}.part",
        std::process::id(),
        PART_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        file.write_all(&bytes)?;
        drop(file);
        std::fs::rename(&part, cache_path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

impl MultibotView {
    pub(crate) fn sync_remote_image_fetches(&mut self, cx: &mut Context<Self>) {
        let urls: HashSet<_> = self
            .history
            .messages
            .iter()
            .flat_map(|message| {
                let mut urls = message_image_urls(message);
                urls.extend(message.blocks.iter().filter_map(|block| match block {
                    MessageBlock::Image {
                        url: Some(url),
                        local_path,
                        ..
                    } if !local_path.as_ref().is_some_and(|p| Path::new(p).is_file()) => {
                        Some(url.clone())
                    }
                    _ => None,
                }));
                urls
            })
            .collect();
        for url in urls {
            self.ensure_remote_image_fetch(&url, cx);
        }
    }

    pub(crate) fn ensure_remote_image_fetch(&mut self, url: &str, cx: &mut Context<Self>) {
        let path = image_cache_path(url, image_ext_from_url(url));
        if path.is_file()
            || !remote_image_url_allowed(url)
            || self.chat_state.image_fetch_state.contains_key(url)
        {
            return;
        }
        self.chat_state
            .image_fetch_state
            .insert(url.into(), ImageFetchState::Loading);
        let url = url.to_string();
        let worker_url = url.clone();
        let fetch = cx
            .background_executor()
            .spawn(async move { fetch_remote_image(&worker_url, &path) });
        cx.spawn(async move |this, cx| {
            let state = if fetch.await.is_ok() {
                ImageFetchState::Cached
            } else {
                ImageFetchState::Failed
            };
            let _ = this.update(cx, |view, cx| {
                view.chat_state.image_fetch_state.insert(url, state);
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod remote_image_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    pub(super) const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4,
        0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5,
        1, 1, 39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    pub(super) fn serve_once(
        body: Vec<u8>,
        content_type: &str,
        declared_size: Option<u64>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/screenshot.png", listener.local_addr().unwrap());
        let content_type = content_type.to_string();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).unwrap();
            let length = declared_size
                .map(|size| format!("Content-Length: {size}\r\n"))
                .unwrap_or_default();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{length}Connection: close\r\n\r\n").unwrap();
            let _ = stream.write_all(&body);
        });
        (url, thread)
    }

    #[test]
    fn remote_image_fetch_caches_png_atomically_and_rejects_html_and_oversized_payloads() {
        let dir = std::env::temp_dir().join(format!("multibot-images-{}", now_unix_ms()));
        for (body, content_type, length, expected) in [
            (PNG.to_vec(), "image/png", Some(PNG.len() as u64), true),
            (
                b"<html>not an image</html>".to_vec(),
                "image/png",
                None,
                false,
            ),
            (
                PNG.to_vec(),
                "image/png",
                Some(MAX_REMOTE_IMAGE_BYTES + 1),
                false,
            ),
            (
                vec![0; MAX_REMOTE_IMAGE_BYTES as usize + 1],
                "image/png",
                None,
                false,
            ),
        ] {
            let (url, server) = serve_once(body, content_type, length);
            let path = image_cache_path_in(&dir, &url, image_ext_from_url(&url));
            assert_eq!(fetch_remote_image(&url, &path).is_ok(), expected);
            server.join().unwrap();
            assert_eq!(path.is_file(), expected);
            if expected {
                assert_eq!(std::fs::read(&path).unwrap(), PNG);
            }
        }
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|entry| !entry
                .unwrap()
                .path()
                .to_string_lossy()
                .ends_with(".part"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remote_image_url_policy_and_magic_reject_non_images() {
        for url in [
            "https://screencast2.com/x.png",
            "http://127.0.0.1:3456/x.png",
            "http://localhost/x.png",
        ] {
            assert!(remote_image_url_allowed(url));
        }
        for url in [
            "http://example.com/x.png",
            "file:///tmp/x.png",
            "https://example.com:bad/x.png",
        ] {
            assert!(!remote_image_url_allowed(url));
        }
        for bytes in [PNG, b"\xff\xd8\xffabc", b"GIF89aabc", b"RIFF1234WEBPabc"] {
            assert!(has_image_magic(bytes));
        }
        assert!(!has_image_magic(b"<html>image/png</html>"));
        assert!(!has_image_magic(b"RIFF1234WAVE"));
    }

    #[gpui_kit::test]
    fn remote_image_background_fetch_is_deduplicated_and_card_opens_viewer(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::{AppContext, test::TestWindowExt};
        let (url, server) = serve_once(PNG.to_vec(), "image/png", Some(PNG.len() as u64));
        let path = image_cache_path(&url, image_ext_from_url(&url));
        let (handle, view) = super::super::tests::open_test_chat_window(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.selection.selected_session = Some(SessionSummary {
                    id: "image-fetch".into(),
                    ..Default::default()
                });
                v.history.messages = vec![ChatMessage {
                    role: MessageRole::User,
                    text: url.clone(),
                    ..Default::default()
                }];
                v.ensure_remote_image_fetch(&url, cx);
                v.ensure_remote_image_fetch(&url, cx);
                assert_eq!(v.chat_state.image_fetch_state.len(), 1);
                assert_eq!(
                    v.chat_state.image_fetch_state[&url],
                    ImageFetchState::Loading
                );
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.find("chat-image-0-url-0").visible());
        })
        .unwrap();
        cx.run_until_parked();
        server.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
        cx.update_window(handle, |_, window, cx| {
            assert_eq!(
                view.read(cx).chat_state.image_fetch_state[&url],
                ImageFetchState::Cached
            );
            view.update(cx, |v, cx| v.ensure_remote_image_fetch(&url, cx));
            window.render_frame(cx);
            let _ = crate::external_url::take_opened_urls();
            window.click("chat-image-0-url-0", cx);
            assert!(crate::external_url::take_opened_urls().is_empty());
            window.render_frame(cx);
            assert!(window.find("image-viewer-modal").visible());
            window.click("image-viewer-open-browser", cx);
            assert_eq!(crate::external_url::take_opened_urls(), vec![url.clone()]);
            window.click("image-viewer-close", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.image_viewer.is_none());
            window.click("chat-image-0-url-0", cx);
            window.render_frame(cx);
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.image_viewer.is_none());
        })
        .unwrap();
        std::fs::remove_file(path).unwrap();

        let (bad_url, server) =
            serve_once(b"<html>not an image</html>".to_vec(), "text/html", None);
        view.update(cx, |v, cx| {
            v.ensure_remote_image_fetch(&bad_url, cx);
            v.ensure_remote_image_fetch(&bad_url, cx);
        });
        cx.run_until_parked();
        server.join().unwrap();
        view.update(cx, |v, cx| {
            assert_eq!(
                v.chat_state.image_fetch_state[&bad_url],
                ImageFetchState::Failed
            );
            v.ensure_remote_image_fetch(&bad_url, cx);
            assert_eq!(
                v.chat_state.image_fetch_state[&bad_url],
                ImageFetchState::Failed
            );
        });
        assert!(!image_cache_path(&bad_url, image_ext_from_url(&bad_url)).is_file());
    }
}

#[cfg(test)]
mod dimension_tests {
    use super::*;

    fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
        image_header_dimensions(&mut std::io::Cursor::new(bytes))
    }

    #[test]
    fn parses_png_gif_and_all_jpeg_sof_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(1920u32.to_be_bytes());
        png.extend(1080u32.to_be_bytes());
        assert_eq!(dimensions(&png), Some((1920, 1080)));
        for signature in [b"GIF87a", b"GIF89a"] {
            let mut gif = signature.to_vec();
            gif.extend(640u16.to_le_bytes());
            gif.extend(480u16.to_le_bytes());
            gif.extend([0; 3]);
            assert_eq!(dimensions(&gif), Some((640, 480)));
        }
        for marker in [0xc0, 0xc1, 0xc2] {
            let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 1, 2, 0xff, marker, 0, 8, 8];
            jpeg.extend(1080u16.to_be_bytes());
            jpeg.extend(1920u16.to_be_bytes());
            jpeg.push(3);
            assert_eq!(dimensions(&jpeg), Some((1920, 1080)));
        }
        for length in 0..png.len() {
            assert_eq!(dimensions(&png[..length]), None, "truncated at {length}");
        }
        assert_eq!(dimensions(b"not an image header"), None);
        png[16..20].fill(0);
        assert_eq!(dimensions(&png), None);
        assert_eq!(dimensions(b"\xff\xd8\xff\xe0\0\x01xxxxxxxx"), None);
    }

    #[test]
    fn parses_webp_lossy_lossless_and_extended_headers() {
        let width = 1920u32;
        let height = 1080u32;
        let mut vp8 = vec![0, 0, 0, 0x9d, 1, 0x2a];
        vp8.extend((width as u16).to_le_bytes());
        vp8.extend((height as u16).to_le_bytes());
        let mut vp8l = vec![0x2f];
        vp8l.extend(((width - 1) | ((height - 1) << 14)).to_le_bytes());
        let mut vp8x = vec![0; 4];
        vp8x.extend(&(width - 1).to_le_bytes()[..3]);
        vp8x.extend(&(height - 1).to_le_bytes()[..3]);
        for (kind, payload) in [(b"VP8 ", vp8), (b"VP8L", vp8l), (b"VP8X", vp8x)] {
            let mut bytes = b"RIFF".to_vec();
            bytes.extend((12u32 + payload.len() as u32).to_le_bytes());
            bytes.extend(b"WEBP");
            bytes.extend(kind);
            bytes.extend((payload.len() as u32).to_le_bytes());
            bytes.extend(payload);
            assert_eq!(dimensions(&bytes), Some((width, height)), "{kind:?}");
            bytes.truncate(bytes.len() - 1);
            assert_eq!(dimensions(&bytes), None);
        }
    }

    #[test]
    fn fits_exact_aspect_ratio_without_upscaling() {
        assert_eq!(fit_image_size(1920, 1080, 320.0, 480.0), (320.0, 180.0));
        assert_eq!(fit_image_size(1080, 1920, 320.0, 480.0), (270.0, 480.0));
        assert_eq!(fit_image_size(16, 9, 320.0, 480.0), (16.0, 9.0));
        assert_eq!(fit_image_size(0, 9, 320.0, 480.0), (0.0, 0.0));
    }

    #[test]
    fn caches_header_dimensions_without_rereading_the_file() {
        let path = std::env::temp_dir().join(format!("multibot-dimensions-{}.png", now_unix_ms()));
        assert_eq!(read_image_dimensions(&path), None);
        let png = remote_image_tests::PNG;
        std::fs::write(&path, png).unwrap();
        assert_eq!(read_image_dimensions(&path), Some((1, 1)));
        std::fs::write(&path, b"not an image").unwrap();
        assert_eq!(read_image_dimensions(&path), Some((1, 1)));
        std::fs::remove_file(path).unwrap();
    }
}
