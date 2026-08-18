//! Kumo semantic color tokens, ported 1:1 from `@cloudflare/kumo`
//! (`packages/kumo/src/styles/theme-kumo.css`, the auto-generated theme file).
//!
//! Every field here is one CSS custom property from that file. The names match
//! the CSS names with the `--color-kumo-` / `--text-color-kumo-` prefix removed,
//! so `--color-kumo-fill-hover` is [`KumoTokens::fill_hover`] and
//! `--text-color-kumo-subtle` is [`KumoTokens::text_subtle`].
//!
//! Kumo writes its values in oklch. GPUI paints sRGB, so [`kumo_dark`] and
//! [`kumo_light`] hold the same colors after an oklch -> sRGB conversion. The
//! `_tint` tokens keep their alpha, because Kumo layers them over the surface
//! below.
//!
//! [`jade_dark`] and [`jade_light`] keep every token name and role but hold
//! Jade's own colors — the `#1E1F22` charcoal canvas and the `#56B389` emerald
//! accent. Those are the palettes the app ships. The components never read a
//! raw color, only a token, so a palette swap is one constructor change and no
//! component edits.

use gpui::{rgb, rgba, Rgba};

/// A resolved Kumo token set. One instance per theme; the app holds the active
/// one and the components read it at paint time.
#[derive(Debug, Clone, Copy)]
pub struct KumoTokens {
    /// `true` when the canvas is light. Components that must pick a neutral
    /// (shadows, heatmap midpoints) branch on this.
    pub is_light: bool,

    // ---- Text colors (`--text-color-kumo-*`) --------------------------------
    pub text_default: Rgba,
    pub text_inverse: Rgba,
    pub text_strong: Rgba,
    pub text_subtle: Rgba,
    pub text_inactive: Rgba,
    pub text_placeholder: Rgba,
    pub text_brand: Rgba,
    pub text_link: Rgba,
    pub text_info: Rgba,
    pub text_success: Rgba,
    pub text_danger: Rgba,
    pub text_warning: Rgba,
    pub text_badge_orange_subtle: Rgba,
    pub text_badge_teal_subtle: Rgba,
    pub text_badge_neutral_subtle: Rgba,
    pub text_badge_inverted: Rgba,

    // ---- Surfaces (`--color-kumo-*`) ---------------------------------------
    /// The window background — the lowest surface.
    pub canvas: Rgba,
    /// One step above the canvas; the layered-card root.
    pub elevated: Rgba,
    /// One step below `base`; segmented-tab troughs and wells.
    pub recessed: Rgba,
    /// The default card / popover surface.
    pub base: Rgba,
    /// Hover wash over `base`; also the secondary badge fill.
    pub tint: Rgba,
    /// The full-contrast surface (inverts against the canvas).
    pub contrast: Rgba,
    /// Menus and dropdown panels.
    pub overlay: Rgba,
    /// Form-control background (inputs, selects).
    pub control: Rgba,
    /// Pressed / active control fill.
    pub interact: Rgba,
    /// Neutral filled surface (progress troughs, secondary badges).
    pub fill: Rgba,
    pub fill_hover: Rgba,

    // ---- Brand -------------------------------------------------------------
    pub brand: Rgba,
    pub brand_hover: Rgba,

    // ---- Strokes and focus -------------------------------------------------
    /// The default 1px ring around cards and controls.
    pub line: Rgba,
    /// A fainter divider than `line` — table rules, tab-bar underlines.
    pub hairline: Rgba,
    /// The focus ring color. Kumo draws it at 50% alpha.
    pub focus: Rgba,
    pub shadow_edge: Rgba,
    pub shadow_drop: Rgba,
    pub tip_shadow: Rgba,
    pub tip_stroke: Rgba,

    // ---- Status ------------------------------------------------------------
    pub info: Rgba,
    pub info_tint: Rgba,
    pub warning: Rgba,
    pub warning_tint: Rgba,
    pub danger: Rgba,
    pub danger_tint: Rgba,
    pub success: Rgba,
    pub success_tint: Rgba,
    pub banner_info: Rgba,
    pub banner_warning: Rgba,

    // ---- Badge scale -------------------------------------------------------
    pub badge_red: Rgba,
    pub badge_green: Rgba,
    pub badge_orange: Rgba,
    pub badge_purple: Rgba,
    pub badge_teal: Rgba,
    pub badge_blue: Rgba,
    pub badge_neutral: Rgba,
    pub badge_inverted: Rgba,
}

impl KumoTokens {
    /// The same color at a new alpha. Kumo writes this as `color/50`; GPUI has
    /// no such modifier, so the components call this instead.
    pub fn alpha(color: Rgba, a: f32) -> Rgba {
        Rgba { a, ..color }
    }

    /// The focus ring Kumo paints on `:focus` — `ring-kumo-focus/50`.
    pub fn focus_ring(&self) -> Rgba {
        Self::alpha(self.focus, 0.5)
    }
}

/// `[data-mode="dark"]` in `theme-kumo.css`, converted from oklch to sRGB.
pub fn kumo_dark() -> KumoTokens {
    KumoTokens {
        is_light: false,
        text_default: rgb(0xF5F5F5),
        text_inverse: rgb(0x171717),
        text_strong: rgb(0xFAFAFA),
        text_subtle: rgb(0xA1A1A1),
        text_inactive: rgb(0x525252),
        text_placeholder: rgb(0x737373),
        text_brand: rgb(0xF6821F),
        text_link: rgb(0x51A2FF),
        text_info: rgb(0x51A2FF),
        text_success: rgb(0xA4F4CF),
        text_danger: rgb(0xFF6467),
        text_warning: rgb(0xFF8904),
        text_badge_orange_subtle: rgb(0xFFD6A7),
        text_badge_teal_subtle: rgb(0x96F7E4),
        text_badge_neutral_subtle: rgb(0xE5E5E5),
        text_badge_inverted: rgb(0x000000),

        canvas: rgb(0x030303),
        elevated: rgb(0x060606),
        recessed: rgb(0x0B0B0B),
        base: rgb(0x0F0F0F),
        tint: rgb(0x262626),
        contrast: rgb(0xFAFAFA),
        overlay: rgb(0x262626),
        control: rgb(0x18181B),
        interact: rgb(0x404040),
        fill: rgb(0x262626),
        fill_hover: rgb(0x404040),

        brand: rgb(0x056DFF),
        brand_hover: rgb(0x1447E6),

        line: rgb(0x333333),
        hairline: rgb(0x262626),
        focus: rgb(0xE9E9E9),
        shadow_edge: rgba(0xFFFFFF1A),
        shadow_drop: rgba(0x0000004D),
        tip_shadow: rgba(0x00000000),
        tip_stroke: rgb(0x262626),

        info: rgb(0x00A6F4),
        info_tint: rgba(0x1C398E38),
        warning: rgb(0xDB6809),
        warning_tint: rgba(0x5731005E),
        danger: rgb(0xE7000B),
        danger_tint: rgba(0x9900032B),
        success: rgb(0x00D492),
        success_tint: rgba(0x0C542B33),
        banner_info: rgba(0x1C398E80),
        banner_warning: rgba(0xA65F0080),

        badge_red: rgb(0xC10007),
        badge_green: rgb(0x007A55),
        badge_orange: rgb(0xFFAC00),
        badge_purple: rgb(0x8200DB),
        badge_teal: rgb(0x00786F),
        badge_blue: rgb(0x1447E6),
        badge_neutral: rgb(0x525252),
        badge_inverted: rgb(0xFFFFFF),
    }
}

/// `:root` (light mode) in `theme-kumo.css`, converted from oklch to sRGB.
pub fn kumo_light() -> KumoTokens {
    KumoTokens {
        is_light: true,
        text_default: rgb(0x18181B),
        text_inverse: rgb(0xF5F5F5),
        text_strong: rgb(0x0A0A0A),
        text_subtle: rgb(0x737373),
        text_inactive: rgb(0xD4D4D4),
        text_placeholder: rgb(0xA1A1A1),
        text_brand: rgb(0xF6821F),
        text_link: rgb(0x193CB8),
        text_info: rgb(0x193CB8),
        text_success: rgb(0x006045),
        text_danger: rgb(0xC10007),
        text_warning: rgb(0xBD6500),
        text_badge_orange_subtle: rgb(0x9F2D00),
        text_badge_teal_subtle: rgb(0x005F5A),
        text_badge_neutral_subtle: rgb(0x262626),
        text_badge_inverted: rgb(0xFFFFFF),

        canvas: rgb(0xFBFBFB),
        elevated: rgb(0xF8F8F8),
        recessed: rgb(0xF2F2F2),
        base: rgb(0xFFFFFF),
        tint: rgb(0xF5F5F5),
        contrast: rgb(0x020202),
        overlay: rgb(0xF7F7F7),
        control: rgb(0xFFFFFF),
        interact: rgb(0xD4D4D4),
        fill: rgb(0xE5E5E5),
        fill_hover: rgb(0xF3F3F3),

        brand: rgb(0x056DFF),
        brand_hover: rgb(0x1447E6),

        line: rgba(0x0A0A0A1A),
        hairline: rgb(0xE9E9E9),
        focus: rgb(0x0B0B0B),
        shadow_edge: rgba(0x0000001F),
        shadow_drop: rgba(0x00000014),
        tip_shadow: rgb(0xE5E7EB),
        tip_stroke: rgba(0x00000000),

        info: rgb(0x00A6F4),
        info_tint: rgba(0xDBEAFE73),
        warning: rgb(0xFA8900),
        warning_tint: rgba(0xFFE89433),
        danger: rgb(0xFB2C36),
        danger_tint: rgba(0xFFE2E26B),
        success: rgb(0x009966),
        success_tint: rgba(0xDCFCE791),
        banner_info: rgba(0xDBEAFEB3),
        banner_warning: rgb(0xFEF9C2),

        badge_red: rgb(0xE7000B),
        badge_green: rgb(0x009966),
        badge_orange: rgb(0xFFAC00),
        badge_purple: rgb(0x9810FA),
        badge_teal: rgb(0x058378),
        badge_blue: rgb(0x155DFC),
        badge_neutral: rgb(0x737373),
        badge_inverted: rgb(0x0A0A0A),
    }
}

// ---------------------------------------------------------------------------
// Jade palette
// ---------------------------------------------------------------------------
//
// Jade's own colors, expressed as Kumo tokens. Nothing about the palette
// changes here — `canvas` is the same `#1E1F22` charcoal the editor always
// used, `brand` is the same `#56B389` emerald — but routing them through the
// token names lets every ported component style itself, which is what carries
// the density and restraint of the Kumo layout onto Jade's colors.

/// Jade emerald — the accent, and the brand slot for every Kumo component.
pub const JADE_ACCENT: u32 = 0x56B389;
/// Jade emerald for a cream canvas, where `JADE_ACCENT` fails contrast.
pub const JADE_ACCENT_ON_LIGHT: u32 = 0x2E7D5B;

/// jade-dark — the JetBrains New UI charcoal.
pub fn jade_dark() -> KumoTokens {
    KumoTokens {
        is_light: false,
        text_default: rgb(0xDFE1E5),
        text_inverse: rgb(0x1E1F22),
        text_strong: rgb(0xF0F2F5),
        text_subtle: rgb(0x6B7A72),
        text_inactive: rgb(0x4A524E),
        text_placeholder: rgb(0x5C6560),
        text_brand: rgb(JADE_ACCENT),
        text_link: rgb(0x8DB2FF),
        text_info: rgb(0x8DB2FF),
        text_success: rgb(JADE_ACCENT),
        text_danger: rgb(0xCF6B6B),
        text_warning: rgb(0xD4A76A),
        text_badge_orange_subtle: rgb(0xE0BE8E),
        text_badge_teal_subtle: rgb(0x8FCFC4),
        text_badge_neutral_subtle: rgb(0xC4C8CC),
        text_badge_inverted: rgb(0x1E1F22),

        // The surface ladder, lightening as it lifts:
        //
        //   canvas 1E1F22 < recessed 252629 < elevated 2B2D30 < base 323438
        //
        // Every step must be distinct. `base` is what a button, a card, and an
        // active tab pill paint themselves; they sit on `elevated`, so if the
        // two match, the fill disappears and only the 1px ring renders — a
        // stray edge floating next to the control instead of the control.
        canvas: rgb(0x1E1F22),
        recessed: rgb(0x252629),
        elevated: rgb(0x2B2D30),
        base: rgb(0x323438),
        // Hover wash. Distinct from `line` (0x35373B) so hovering a bordered
        // control does not paint its fill the same color as its own border.
        tint: rgb(0x3B3E43),
        contrast: rgb(0xDFE1E5),
        overlay: rgb(0x323438),
        // A form field reads as a well cut into the surface it sits on, so it
        // drops below `elevated` rather than lifting above it.
        control: rgb(0x26282B),
        interact: rgb(0x4A4D52),
        fill: rgb(0x35373B),
        fill_hover: rgb(0x42454A),

        brand: rgb(JADE_ACCENT),
        brand_hover: rgb(0x47996F),

        line: rgb(0x35373B),
        hairline: rgb(0x2E3034),
        focus: rgb(JADE_ACCENT),
        shadow_edge: rgba(0xFFFFFF14),
        shadow_drop: rgba(0x00000040),
        tip_shadow: rgba(0x00000000),
        tip_stroke: rgb(0x35373B),

        info: rgb(0x8DB2FF),
        info_tint: rgba(0x8DB2FF1F),
        warning: rgb(0xD4A76A),
        warning_tint: rgba(0xD4A76A1F),
        danger: rgb(0xCF6B6B),
        danger_tint: rgba(0xCF6B6B1F),
        success: rgb(JADE_ACCENT),
        success_tint: rgba(0x56B3891F),
        banner_info: rgba(0x8DB2FF2E),
        banner_warning: rgba(0xD4A76A2E),

        badge_red: rgb(0xCF6B6B),
        badge_green: rgb(0x47996F),
        badge_orange: rgb(0xD4A76A),
        badge_purple: rgb(0x9B8DD1),
        badge_teal: rgb(0x5AA9A0),
        badge_blue: rgb(0x6E8FD6),
        badge_neutral: rgb(0x4A4D52),
        badge_inverted: rgb(0xDFE1E5),
    }
}

/// jade-light — the warm cream research palette.
pub fn jade_light() -> KumoTokens {
    KumoTokens {
        is_light: true,
        text_default: rgb(0x373528),
        text_inverse: rgb(0xF4EFE2),
        text_strong: rgb(0x1F1E16),
        text_subtle: rgb(0x8A8674),
        text_inactive: rgb(0xBDB9A6),
        text_placeholder: rgb(0xA5A18E),
        text_brand: rgb(JADE_ACCENT_ON_LIGHT),
        text_link: rgb(0x2F5FD0),
        text_info: rgb(0x2F5FD0),
        text_success: rgb(JADE_ACCENT_ON_LIGHT),
        text_danger: rgb(0xB3403F),
        text_warning: rgb(0x9A6700),
        text_badge_orange_subtle: rgb(0x8A5A00),
        text_badge_teal_subtle: rgb(0x0F5F58),
        text_badge_neutral_subtle: rgb(0x4A4838),
        text_badge_inverted: rgb(0xF4EFE2),

        // The same ladder on cream, lightening as it lifts:
        //
        //   recessed E2DBC7 < elevated EAE4D3 < canvas F4EFE2 < base FBF8F0
        canvas: rgb(0xF4EFE2),
        recessed: rgb(0xE2DBC7),
        elevated: rgb(0xEAE4D3),
        base: rgb(0xFBF8F0),
        tint: rgb(0xEFE9DA),
        contrast: rgb(0x373528),
        overlay: rgb(0xFBF8F0),
        control: rgb(0xFBF8F0),
        interact: rgb(0xCFC8B2),
        fill: rgb(0xDED7C2),
        fill_hover: rgb(0xE7E1CE),

        brand: rgb(JADE_ACCENT_ON_LIGHT),
        brand_hover: rgb(0x246147),

        // The panels draw this token at ~8% alpha, so it packs to black.
        line: rgba(0x00000014),
        hairline: rgba(0x0000000F),
        focus: rgb(JADE_ACCENT_ON_LIGHT),
        shadow_edge: rgba(0x0000001F),
        shadow_drop: rgba(0x00000014),
        tip_shadow: rgb(0xDED7C2),
        tip_stroke: rgba(0x00000000),

        info: rgb(0x2F5FD0),
        info_tint: rgba(0x2F5FD01F),
        warning: rgb(0x9A6700),
        warning_tint: rgba(0x9A67001F),
        danger: rgb(0xB3403F),
        danger_tint: rgba(0xB3403F1F),
        success: rgb(JADE_ACCENT_ON_LIGHT),
        success_tint: rgba(0x2E7D5B1F),
        banner_info: rgba(0x2F5FD02E),
        banner_warning: rgba(0x9A67002E),

        badge_red: rgb(0xB3403F),
        badge_green: rgb(JADE_ACCENT_ON_LIGHT),
        badge_orange: rgb(0x9A6700),
        badge_purple: rgb(0x6E5494),
        badge_teal: rgb(0x0F6B62),
        badge_blue: rgb(0x2F5FD0),
        badge_neutral: rgb(0x8A8674),
        badge_inverted: rgb(0x373528),
    }
}
