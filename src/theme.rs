//! Paseo's default dark theme ("Paseo"), from the app's v0.10.1 tokens.
//! Views read colors and sizes from here instead of inlining hex values.

use gpui_kit::gpui::{Hsla, Pixels, Rgba, px, rgb, rgba};

pub struct Palette {
    pub surface0: Rgba,
    pub surface1: Rgba,
    pub surface2: Rgba,
    pub surface3: Rgba,
    pub surface4: Rgba,
    pub sidebar: Rgba,
    pub sidebar_hover: Rgba,
    pub sidebar_selected: Rgba,
    pub foreground: Rgba,
    pub muted: Rgba,
    pub extra_muted: Rgba,
    pub border: Rgba,
    pub border_accent: Rgba,
    pub accent: Rgba,
    pub accent_bright: Rgba,
    pub destructive: Rgba,
    pub status_success: Rgba,
    pub status_danger: Rgba,
    pub status_warning: Rgba,
    pub status_merged: Rgba,
    pub dot_success: Rgba,
    pub dot_danger: Rgba,
    pub dot_warning: Rgba,
    pub dot_running: Rgba,
}

pub fn palette() -> &'static Palette {
    static DARK: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();
    DARK.get_or_init(|| Palette {
        surface0: rgb(0x181B1A),
        surface1: rgb(0x1E2120),
        surface2: rgb(0x272A29),
        surface3: rgb(0x434645),
        surface4: rgb(0x595B5B),
        sidebar: rgb(0x141716),
        sidebar_hover: rgb(0x1E2120),
        sidebar_selected: rgb(0x272A29),
        foreground: rgb(0xfafafa),
        muted: rgb(0xA1A5A4),
        extra_muted: rgb(0x717574),
        border: rgb(0x252B2A),
        border_accent: rgb(0x2F3534),
        accent: rgb(0x20744A),
        accent_bright: rgb(0x7ccba0),
        destructive: rgb(0xc64f43),
        status_success: rgb(0x6cb17b),
        status_danger: rgb(0xd8847b),
        status_warning: rgb(0xc09664),
        status_merged: rgb(0xa890d5),
        dot_success: rgb(0x35c264),
        dot_danger: rgb(0xf7796d),
        dot_warning: rgb(0xdb932e),
        dot_running: rgb(0x5caaf6),
    })
}

/// Paseo's identity colors, in the order projects hash into them:
/// (fill behind a white initial, foreground on dark, label-lane swatch).
pub const IDENTITY: [(&str, u32, u32, &str); 10] = [
    ("violet", 0x7a6aa8, 0xa392d5, "Violet"),
    ("sky", 0x3d7ea6, 0x6aa6ce, "Sky"),
    ("emerald", 0x388068, 0x6cae96, "Mint"),
    ("orange", 0xa4673a, 0xcc8f64, "Orange"),
    ("pink", 0xb05c80, 0xd87da3, "Rose"),
    ("indigo", 0x6a70b8, 0x9299d5, "Indigo"),
    ("teal", 0x368080, 0x6cacab, "Teal"),
    ("red", 0xb06260, 0xd88381, "Brick"),
    ("amber", 0x8f7838, 0xb29d64, "Amber"),
    ("blue", 0x5179b0, 0x7ba1d5, "Azure"),
];

/// Categorical swatches selected by farthest-point sampling in CIELAB, then
/// ordered by hue (D65, sRGB).
pub const LANE_COLORS: [(&str, u32); 32] = [
    ("Rosewood", 0xa0526c),
    ("Raspberry", 0xc6266d),
    ("Coral", 0xf05468),
    ("Terracotta", 0xc27866),
    ("Brick", 0xbe3e27),
    ("Orange", 0xeb5f1a),
    ("Peach", 0xfebf94),
    ("Ochre", 0x965e16),
    ("Amber", 0xea9a37),
    ("Sunflower", 0xf1c81a),
    ("Gold", 0xe9c96c),
    ("Olive", 0x968e2f),
    ("Lime", 0xc6d552),
    ("Chartreuse", 0x84bc0a),
    ("Sage", 0xc0d492),
    ("Moss", 0x53743c),
    ("Apple", 0x99df6a),
    ("Leaf", 0x379f3d),
    ("Mint", 0x1de97d),
    ("Jade", 0x15c19a),
    ("Teal", 0x129c95),
    ("Cyan", 0x62dcf5),
    ("Sky", 0x6db1e9),
    ("Denim", 0x366da5),
    ("Azure", 0x1b8cfc),
    ("Indigo", 0x5a58e0),
    ("Lavender", 0xdec0fe),
    ("Plum", 0x8f53a1),
    ("Lilac", 0xdf8af8),
    ("Violet", 0xa13bc2),
    ("Magenta", 0xee43bd),
    ("Rose", 0xf887bc),
];

/// Activity header colors, mixed with the active surface.
pub const RECENCY_RAMP: [u32; 4] = [0x5a9ded, 0x3da975, 0xcba43a, 0x9b3b42];

/// Project icon color, hashed the way the app does (`hash*31 + char`, mod 10).
pub fn identity_for(key: &str) -> (u32, u32) {
    let hash = paseo_hash(key) as i32;
    let (_, fill, fg, _) = IDENTITY[(hash.unsigned_abs() as usize) % IDENTITY.len()];
    (fill, fg)
}

pub fn identity_named(name: &str) -> Option<(u32, u32)> {
    IDENTITY
        .iter()
        .find(|(id, _, _, _)| *id == name)
        .map(|(_, fill, fg, _)| (*fill, *fg))
}

/// Paseo's dark terminal ANSI palette, plus the xterm 256-colour cube.
pub fn ansi(index: u8) -> Rgba {
    const NAMED: [u32; 16] = [
        0x141716, 0xe07070, 0x5dba80, 0xd4a44a, 0x6a9de0, 0xb07ad0, 0x4aabb8, 0xd4d4d8, 0x434645,
        0xe89090, 0x7ecf9a, 0xe0be6e, 0x8ab4e8, 0xc49ae0, 0x6ec2cc, 0xf0f0f2,
    ];
    if index < 16 {
        return rgb(NAMED[index as usize]);
    }
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return rgb(u32::from(level) << 16 | u32::from(level) << 8 | u32::from(level));
    }
    let cube = index - 16;
    let levels = [0u8, 95, 135, 175, 215, 255];
    let r = levels[(cube / 36) as usize];
    let g = levels[((cube / 6) % 6) as usize];
    let b = levels[(cube % 6) as usize];
    rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))
}

pub fn with_alpha(color: Rgba, alpha: f32) -> Hsla {
    let mut hsla: Hsla = color.into();
    hsla.a = alpha;
    hsla
}

pub mod size {
    use super::*;

    pub fn sm() -> Pixels {
        px(12.)
    }
    pub fn base() -> Pixels {
        px(14.)
    }
    pub fn content() -> Pixels {
        px(15.)
    }
    pub fn code() -> Pixels {
        px(12.)
    }

    pub const SIDEBAR_WIDTH: f32 = 320.;
    pub const SIDEBAR_MIN: f32 = 200.;
    pub const SIDEBAR_MAX: f32 = 600.;
    pub const HEADER_HEIGHT: f32 = 36.;
    pub const TAB_HEIGHT: f32 = 28.;
    pub const ROW_MIN_HEIGHT: f32 = 36.;
    pub const COMPOSER_MAX_WIDTH: f32 = 820.;
    pub const PROMPT_INDEX_WIDTH: f32 = 32.;
    pub const PROMPT_TICK_MAX_WIDTH: f32 = 26.;
    // Left inset reserves the index gutter; a fixed total preserves content width.
    const CONVERSATION_PADDING_TOTAL: f32 = 48.;
    pub const CONVERSATION_PADDING_LEFT: f32 = PROMPT_INDEX_WIDTH;
    pub const CONVERSATION_PADDING_RIGHT: f32 =
        CONVERSATION_PADDING_TOTAL - CONVERSATION_PADDING_LEFT;
    const _: () = assert!(PROMPT_TICK_MAX_WIDTH < PROMPT_INDEX_WIDTH);
    const _: () = assert!(CONVERSATION_PADDING_RIGHT >= 0.);
}

/// A monospace family every platform ships.
pub fn mono_font() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

/// Paints the component library (buttons, menus, inputs, scrollbars) in
/// Paseo's dark palette so stock components match hand-drawn ones.
pub fn apply(cx: &mut gpui_kit::gpui::App) {
    use gpui_kit::gpui::Hsla;
    let p = palette();
    let h = |c: Rgba| -> Hsla { c.into() };
    gpui_kit::component::Theme::update(cx, |theme| {
        theme.mono_font_family = mono_font().into();
        theme.radius = px(6.);
        theme.radius_lg = px(8.);
        let c = &mut theme.colors;
        c.background = h(p.surface0);
        c.foreground = h(p.foreground);
        c.border = h(p.border);
        c.input = h(p.surface2);
        c.ring = h(p.accent);
        c.muted = h(p.surface2);
        c.muted_foreground = h(p.muted);
        c.popover = h(p.surface1);
        c.popover_foreground = h(p.foreground);
        c.accent = h(p.surface2);
        c.accent_foreground = h(p.foreground);
        c.primary = h(p.accent);
        c.primary_hover = with_alpha(p.accent, 0.85);
        c.primary_active = with_alpha(p.accent, 0.75);
        c.primary_foreground = h(rgb(0xffffff));
        c.button_primary = h(p.accent);
        c.button_primary_hover = with_alpha(p.accent, 0.85);
        c.button_primary_active = with_alpha(p.accent, 0.75);
        c.button_primary_foreground = h(rgb(0xffffff));
        c.secondary = h(p.surface3);
        c.secondary_hover = h(p.surface4);
        c.secondary_active = h(p.surface4);
        c.secondary_foreground = h(p.foreground);
        c.button = h(p.surface3);
        c.button_hover = h(p.surface4);
        c.button_active = h(p.surface4);
        c.button_foreground = h(p.foreground);
        c.danger = h(p.destructive);
        c.danger_hover = with_alpha(p.destructive, 0.85);
        c.danger_active = with_alpha(p.destructive, 0.75);
        c.danger_foreground = h(rgb(0xffffff));
        c.list_hover = h(p.surface2);
        c.list_active = h(p.surface2);
        c.selection = with_alpha(p.accent, 0.35);
        c.caret = h(p.foreground);
        c.scrollbar_thumb = with_alpha(p.extra_muted, 0.4);
        c.scrollbar_thumb_hover = h(p.muted);
        c.title_bar = h(p.sidebar);
        c.title_bar_border = h(p.border);
        c.window_border = h(p.border);
        c.overlay = h(rgba(0x00000080));
    });
}

/// Stable mapping from shared label presets to the local lane palette.
pub fn lane_swatch_named(name: &str) -> Option<usize> {
    IDENTITY
        .iter()
        .find(|(id, _, _, _)| *id == name)
        .and_then(|(_, _, _, swatch)| LANE_COLORS.iter().position(|(name, _)| name == swatch))
}

/// Paseo's wrapping UTF-16 identity hash; callers choose the palette modulus.
pub fn paseo_hash(key: &str) -> u32 {
    key.encode_utf16().fold(0u32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(u32::from(unit))
    })
}

pub const LANE_COLUMN_TINT: f32 = 0.18;
pub const LANE_HEADER_TINT: f32 = 0.24;
pub const LANE_CARD_TINT: f32 = 0.10;
/// Accent tint of a workspace's box in lanes other than the one it is
/// selected in.
pub const LANE_BOX_TINT: f32 = 0.12;
pub const LANE_RECENCY_TINT: f32 = 0.30;
