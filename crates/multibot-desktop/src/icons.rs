use gpui_kit::{Styled, component::Icon, px};

// Lucide geometry from gpui-kit-assets 0.11.0; see LICENSE-LUCIDE.
// Inline only the icons we need, without enabling the optional asset bundle.
macro_rules! lucide_icons {
    ($($name:ident => $geometry:literal,)*) => {
        $(#[allow(dead_code)]
        pub const $name: &[u8] = concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">"#,
            $geometry,
            "</svg>",
        ).as_bytes();)*

        #[cfg(test)]
        const ALL: &[(&str, &[u8])] = &[$((stringify!($name), $name),)*];
    };
}

lucide_icons! {
    INBOX => r#"<polyline points="22 12 16 12 14 15 10 15 8 12 2 12" /><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z" />"#,
    MORE_HORIZONTAL => r#"<circle cx="12" cy="12" r="1" /><circle cx="19" cy="12" r="1" /><circle cx="5" cy="12" r="1" />"#,
    MIC => r#"<path d="M12 19v3" /><path d="M19 10v2a7 7 0 0 1-14 0v-2" /><rect x="9" y="2" width="6" height="13" rx="3" />"#,
    CHEVRON_DOWN => r#"<path d="m6 9 6 6 6-6" />"#,
    CHEVRON_UP => r#"<path d="m18 15-6-6-6 6" />"#,
    CHEVRON_RIGHT => r#"<path d="m9 18 6-6-6-6" />"#,
    X => r#"<path d="M18 6 6 18" /><path d="m6 6 12 12" />"#,
    STAR => r#"<path d="M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" />"#,
    PLUS => r#"<path d="M5 12h14" /><path d="M12 5v14" />"#,
    SEND => r#"<path d="M14.536 21.686a.5.5 0 0 0 .937-.024l6.5-19a.496.496 0 0 0-.635-.635l-19 6.5a.5.5 0 0 0-.024.937l7.93 3.18a2 2 0 0 1 1.112 1.11z" /><path d="m21.854 2.147-10.94 10.939" />"#,
    ARROW_UP => r#"<path d="m5 12 7-7 7 7" /><path d="M12 19V5" />"#,
    ARROW_DOWN => r#"<path d="M12 5v14" /><path d="m19 12-7 7-7-7" />"#,
    CORNER_DOWN_LEFT => r#"<path d="M20 4v7a4 4 0 0 1-4 4H4" /><path d="m9 10-5 5 5 5" />"#,
    CHECK => r#"<path d="M20 6 9 17l-5-5" />"#,
    CHECK_CIRCLE => r#"<circle cx="12" cy="12" r="10" /><path d="m16 9-5.5 5.5L8 12" />"#,
    CIRCLE => r#"<circle cx="12" cy="12" r="10" />"#,
    SEARCH => r#"<path d="m21 21-4.34-4.34" /><circle cx="11" cy="11" r="8" />"#,
    SETTINGS => r#"<path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915" /><circle cx="12" cy="12" r="3" />"#,
    BELL => r#"<path d="M10.268 21a2 2 0 0 0 3.464 0" /><path d="M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326" />"#,
    CLOCK => r#"<circle cx="12" cy="12" r="10" /><path d="M12 6v6l4 2" />"#,
    TERMINAL => r#"<path d="M12 19h8" /><path d="m4 17 6-6-6-6" />"#,
    SPARKLES => r#"<path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z" /><path d="M20 2v4" /><path d="M22 4h-4" /><circle cx="4" cy="20" r="2" />"#,
    BOT => r#"<path d="M12 8V4H8" /><rect width="16" height="12" x="4" y="8" rx="2" /><path d="M2 14h2" /><path d="M20 14h2" /><path d="M15 13v2" /><path d="M9 13v2" />"#,
    FOLDER => r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />"#,
    REFRESH_CW => r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />"#,
    COPY => r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2" /><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" />"#,
    EXTERNAL_LINK => r#"<path d="M15 3h6v6" /><path d="M10 14 21 3" /><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />"#,
    ARCHIVE => r#"<rect width="20" height="5" x="2" y="3" rx="1" /><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8" /><path d="M10 12h4" />"#,
    TRASH_2 => r#"<path d="M10 11v6" /><path d="M14 11v6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /><path d="M3 6h18" /><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />"#,
    COMMAND => r#"<path d="M15 6v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3" />"#,
    SQUARE => r#"<rect width="18" height="18" x="3" y="3" rx="2" />"#,
    STAR_FILLED => r#"<path fill="currentColor" d="M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" />"#,
    SQUARE_FILLED => r#"<rect width="12" height="12" x="6" y="6" rx="2" fill="currentColor" />"#,
    PENCIL => r#"<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" /><path d="m15 5 4 4" />"#,
    MAXIMIZE_2 => r#"<polyline points="15 3 21 3 21 9" /><polyline points="9 21 3 21 3 15" /><line x1="21" x2="14" y1="3" y2="10" /><line x1="3" x2="10" y1="21" y2="14" />"#,
    DOT => r#"<circle cx="12" cy="12" r="6" fill="currentColor" />"#,
}

pub fn lucide_icon(svg: &'static [u8], size_px: f32) -> Icon {
    // Button applies its own icon size; constraints keep our chrome sizes exact.
    Icon::default()
        .data(svg)
        .size(px(size_px))
        .min_w(px(size_px))
        .max_w(px(size_px))
        .min_h(px(size_px))
        .max_h(px(size_px))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_uses_the_inline_icon_set_instead_of_glyphs() {
        for (name, source) in [
            ("app", include_str!("app.rs")),
            ("sidebar", include_str!("sidebar.rs")),
            ("chat_view", include_str!("chat_view.rs")),
            ("chat_composer", include_str!("chat_composer.rs")),
            ("chat_blocks", include_str!("chat_blocks.rs")),
            ("chat_interaction", include_str!("chat_interaction.rs")),
            ("chat_info", include_str!("chat_info.rs")),
            ("inbox", include_str!("inbox.rs")),
            ("palette", include_str!("palette.rs")),
            ("settings", include_str!("settings.rs")),
            ("update_ui", include_str!("update_ui.rs")),
            ("table_modal", include_str!("table_modal.rs")),
            ("slash_picker", include_str!("slash_picker.rs")),
            ("terminal_pane", include_str!("terminal_pane.rs")),
            ("computer_view", include_str!("computer_view.rs")),
            ("browser_pane", include_str!("browser_pane.rs")),
        ] {
            let chrome = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
            for (line_index, line) in chrome.lines().enumerate() {
                // Table dimensions are data, not a close control.
                if line.trim_start().starts_with("//") || line.contains("{} × {} table") {
                    continue;
                }
                for glyph in [
                    '×', '★', '▸', '▾', '↓', '↑', '↩', '✓', '○', '●', '⌘', '⚙', '🔍',
                ] {
                    assert!(
                        !line.contains(glyph),
                        "{name}:{} uses glyph {glyph}",
                        line_index + 1
                    );
                }
                for scattered_icon in [
                    "<svg",
                    "Icon::",
                    "IconName::",
                    ".child(\"+\")",
                    ".label(\"+\")",
                ] {
                    assert!(
                        !line.contains(scattered_icon),
                        "{name}:{} bypasses icons.rs",
                        line_index + 1
                    );
                }
            }
        }
    }

    #[test]
    fn busy_dot_is_a_visible_filled_circle() {
        let svg = std::str::from_utf8(DOT).unwrap();
        let document = roxmltree::Document::parse(svg).unwrap();
        let circle = document.root_element().first_element_child().unwrap();
        assert_eq!(circle.tag_name().name(), "circle");
        assert_eq!(circle.attribute("fill"), Some("currentColor"));
        let radius: f32 = circle.attribute("r").unwrap().parse().unwrap();
        assert!(radius >= 5.0);
    }

    #[test]
    fn every_lucide_icon_is_valid_and_consistently_styled() {
        for &(name, bytes) in ALL {
            let svg = std::str::from_utf8(bytes).expect(name);
            let document = roxmltree::Document::parse(svg).expect(name);
            let root = document.root_element();
            assert_eq!(root.tag_name().name(), "svg", "{name}");
            assert_eq!(
                root.tag_name().namespace(),
                Some("http://www.w3.org/2000/svg"),
                "{name}"
            );
            for (attribute, expected) in [
                ("viewBox", "0 0 24 24"),
                ("fill", "none"),
                ("stroke", "currentColor"),
                ("stroke-width", "1.75"),
                ("stroke-linecap", "round"),
                ("stroke-linejoin", "round"),
            ] {
                assert_eq!(
                    root.attribute(attribute),
                    Some(expected),
                    "{name}: {attribute}"
                );
            }
            let geometry: Vec<_> = root.children().filter(|n| n.is_element()).collect();
            assert!(!geometry.is_empty(), "{name} has no geometry");
            for node in geometry {
                let required: &[&str] = match node.tag_name().name() {
                    "path" => &["d"],
                    "circle" => &["cx", "cy", "r"],
                    "rect" => &["width", "height"],
                    "line" => &["x1", "y1", "x2", "y2"],
                    "polyline" | "polygon" => &["points"],
                    other => panic!("{name}: unexpected geometry {other}"),
                };
                for attribute in required {
                    assert!(
                        !node.attribute(*attribute).unwrap_or_default().is_empty(),
                        "{name}: empty {attribute}"
                    );
                }
            }
        }
    }
}

/// Shortcut labels come from the same registry as F1 and key bindings.
pub(crate) fn shortcut_tooltip(label: &str, shortcut_id: &str) -> String {
    let shortcut = multibot_keys::by_id(shortcut_id).expect("unknown tooltip shortcut");
    assert!(
        shortcut.wired,
        "tooltip shortcut must be wired: {shortcut_id}"
    );
    format!("{label} · {}", shortcut.display)
}

#[cfg(test)]
mod tooltip_tests {
    #[test]
    fn shortcut_tooltips_use_wired_registry_entries() {
        assert_eq!(
            super::shortcut_tooltip("Send message", "chat.send"),
            "Send message · Enter"
        );
        assert_eq!(
            format!("{} ({} to force send)", super::shortcut_tooltip("Queue message", "chat.send"), multibot_keys::by_id("chat.force_send").unwrap().display),
            "Queue message · Enter (Ctrl+Enter to force send)"
        );
        assert_eq!(
            super::shortcut_tooltip("Send now", "chat.force_send"),
            "Send now · Ctrl+Enter"
        );
        for source in [
            include_str!("app.rs"),
            include_str!("chat_composer.rs"),
            include_str!("sidebar.rs"),
            include_str!("sidebar_rows.rs"),
            include_str!("chat_interaction.rs"),
            include_str!("chat_blocks.rs"),
            include_str!("chat_info.rs"),
            include_str!("table_modal.rs"),
            include_str!("settings.rs"),
            include_str!("computer_view.rs"),
            include_str!("terminal_pane.rs"),
            include_str!("palette.rs"),
            include_str!("chat_view.rs"),
            include_str!("update_ui.rs"),
        ] {
            for button in source.split("Button::new(").skip(1) {
                let before_click = button.split(".on_click").next().unwrap();
                if before_click.contains(".icon(lucide_icon(")
                    && !before_click.contains(".when(selected")
                {
                    assert!(
                        before_click.contains(".tooltip("),
                        "icon button without tooltip: {before_click}"
                    );
                }
            }
            for call in source.split("shortcut_tooltip(").skip(1) {
                let args: Vec<_> = call.split('"').take(5).collect();
                let id = args[3];
                let spec =
                    multibot_keys::by_id(id).unwrap_or_else(|| panic!("unknown tooltip ID: {id}"));
                assert!(spec.wired, "unwired tooltip ID: {id}");
                assert_eq!(
                    super::shortcut_tooltip(args[1], id),
                    format!("{} · {}", args[1], spec.display)
                );
            }
        }
    }
}
