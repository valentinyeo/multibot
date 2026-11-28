use std::ops::Range;

pub(crate) fn is_http_url(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once("://") else {
        return false;
    };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
        && url::Url::parse(value).is_ok_and(|url| url.host_str().is_some())
}

/// Byte ranges, retaining balanced URL parentheses but not surrounding prose punctuation.
pub(crate) fn url_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let Some(start) = text[offset..].char_indices().find_map(|(i, _)| {
            let rest = &text[offset + i..];
            (rest
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
                || rest
                    .get(..8)
                    .is_some_and(|s| s.eq_ignore_ascii_case("https://")))
            .then_some(offset + i)
        }) else {
            break;
        };
        let mut end = text[start..]
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`'))
            .map_or(text.len(), |(i, _)| start + i);
        loop {
            let candidate = &text[start..end];
            let Some(last) = candidate.chars().last() else {
                break;
            };
            let unmatched = match last {
                ')' => candidate.matches(')').count() > candidate.matches('(').count(),
                ']' => candidate.matches(']').count() > candidate.matches('[').count(),
                '}' => candidate.matches('}').count() > candidate.matches('{').count(),
                _ => false,
            };
            if unmatched || matches!(last, '.' | ',' | ';' | ':' | '!' | '?') {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if is_http_url(&text[start..end]) {
            ranges.push(start..end);
        }
        offset = end.max(start + 1);
    }
    ranges
}

#[cfg(any(windows, test))]
pub(crate) fn url_at_byte(text: &str, byte: usize) -> Option<&str> {
    url_ranges(text)
        .into_iter()
        .find(|range| range.contains(&byte))
        .map(|range| &text[range])
}

pub(crate) fn open_external_url(url: &str) -> anyhow::Result<()> {
    anyhow::ensure!(is_http_url(url), "Only HTTP and HTTPS links can be opened.");
    open_browser(url)
}

#[cfg(test)]
thread_local! {
    static OPENED_URLS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn take_opened_urls() -> Vec<String> {
    OPENED_URLS.with(|urls| std::mem::take(&mut *urls.borrow_mut()))
}

#[cfg(test)]
fn open_browser(url: &str) -> anyhow::Result<()> {
    OPENED_URLS.with(|urls| urls.borrow_mut().push(url.to_string()));
    Ok(())
}

#[cfg(all(not(test), target_os = "windows"))]
fn open_browser(url: &str) -> anyhow::Result<()> {
    use std::ffi::c_void;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            window: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut c_void;
    }
    anyhow::ensure!(is_http_url(url), "Only HTTP and HTTPS links can be opened.");
    let url = url.to_string();
    // ShellExecuteW pumps COM/Win32 messages; never re-enter GPUI from a click handler.
    std::thread::Builder::new()
        .name("open-url".into())
        .spawn(move || {
            let operation: Vec<u16> = "open\0".encode_utf16().collect();
            let url: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
            let result = unsafe {
                ShellExecuteW(
                    std::ptr::null_mut(),
                    operation.as_ptr(),
                    url.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                )
            };
            if result as usize <= 32 {
                eprintln!(
                    "Could not open your browser (ShellExecuteW {}).",
                    result as usize
                );
            }
        })?;
    Ok(())
}

#[cfg(all(not(test), not(target_os = "windows")))]
fn open_browser(url: &str) -> anyhow::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    anyhow::ensure!(status.success(), "Could not open your browser.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_markdown_and_balanced_urls() {
        for (text, expected) in [
            ("See https://example.com/a.", vec!["https://example.com/a"]),
            (
                "[label](https://example.com/a)",
                vec!["https://example.com/a"],
            ),
            (
                "(http://example.com/a_(b)).,;",
                vec!["http://example.com/a_(b)"],
            ),
            (
                "https://example.com/a_(b(c))!",
                vec!["https://example.com/a_(b(c))"],
            ),
            (
                "é https://example.com/é?q=1&x=2; HTTP://example.org,",
                vec!["https://example.com/é?q=1&x=2", "HTTP://example.org"],
            ),
            ("https://[::1]:8080/test).", vec!["https://[::1]:8080/test"]),
            ("javascript:alert(1) file:///etc/passwd http://", vec![]),
        ] {
            let actual: Vec<_> = url_ranges(text).into_iter().map(|r| &text[r]).collect();
            assert_eq!(actual, expected, "{text}");
        }
    }

    #[test]
    fn validates_before_opening() {
        take_opened_urls();
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,test",
            "https:",
            "https:///",
            "https://example.com/\nfile",
            " https://example.com",
        ] {
            assert!(!is_http_url(url), "{url:?}");
            assert!(open_external_url(url).is_err());
        }
        assert!(take_opened_urls().is_empty());
        open_external_url("https://example.com/?a=1&b=2").unwrap();
        assert_eq!(take_opened_urls(), ["https://example.com/?a=1&b=2"]);
    }

    #[test]
    fn hit_test_excludes_surrounding_punctuation() {
        let text = "é (https://example.com/a_(b)).";
        assert_eq!(
            url_at_byte(text, text.find("example").unwrap()),
            Some("https://example.com/a_(b)")
        );
        assert_eq!(url_at_byte(text, text.len() - 2), None);
        assert_eq!(url_at_byte(text, 0), None);
    }
}
