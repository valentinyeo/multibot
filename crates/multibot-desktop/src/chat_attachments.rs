use super::*;

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
