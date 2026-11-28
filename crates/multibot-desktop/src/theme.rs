use gpui_kit::{
    App, Hsla, Pixels,
    component::{Theme, ThemeMode},
    px, rgb,
};

/// Deep OLED / Muse / Grok Bot dark palette & soft rounded geometry tokens.
pub const RADIUS_BUBBLE: Pixels = px(18.0);
pub const RADIUS_MODAL: Pixels = px(18.0);
pub const RADIUS_ROW: Pixels = px(12.0);
pub const RADIUS_CHIP: Pixels = px(9999.0);

pub fn bg_canvas() -> Hsla {
    rgb(0x0D0D0D).into()
}

pub fn bg_sidebar() -> Hsla {
    rgb(0x111111).into()
}

pub fn bg_surface() -> Hsla {
    rgb(0x1A1A1A).into()
}

pub fn bg_user_bubble() -> Hsla {
    rgb(0x2B2B2B).into()
}

pub fn bg_hover() -> Hsla {
    rgb(0x242424).into()
}

pub fn bg_active_row() -> Hsla {
    rgb(0x272727).into()
}

pub fn border_subtle() -> Hsla {
    rgb(0x2A2A2A).into()
}

pub fn text_primary() -> Hsla {
    rgb(0xF2F2F2).into()
}

pub fn text_secondary() -> Hsla {
    rgb(0xB4B4B4).into()
}

pub fn text_muted() -> Hsla {
    rgb(0x828282).into()
}

pub fn accent_green() -> Hsla {
    rgb(0x22C55E).into()
}

pub fn accent_amber() -> Hsla {
    rgb(0xF59E0B).into()
}

pub fn accent_blue() -> Hsla {
    rgb(0x60A5FA).into()
}

/// Activates GPUI Kit's dark theme and tunes surface colors to match
/// Multibot's obsidian dark palette so built-in inputs, popovers, and
/// markdown views blend seamlessly.
pub fn apply_multibot_dark_theme(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    theme.background = bg_canvas();
    theme.sidebar = bg_sidebar();
    theme.secondary = bg_surface();
    theme.popover = bg_surface();
    theme.border = border_subtle();
    theme.input = border_subtle();
    theme.foreground = text_primary();
    theme.muted_foreground = text_muted();
    theme.link = accent_blue();
    theme.radius = px(12.0);
    theme.radius_lg = px(18.0);
}
