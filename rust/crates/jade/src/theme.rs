//! Jade theme palettes.
//!
//! A [`Theme`] is now a thin projection of a [`KumoTokens`] set. The Kumo tokens
//! are the source of truth for every surface, stroke, and status color (see
//! `crate::kumo::tokens`, ported 1:1 from `@cloudflare/kumo`); the flat `bg` /
//! `panel` / `text` fields below stay only because the panel modules read them
//! directly, and they are derived — never authored — so a palette swap is one
//! constructor change.
//!
//! The palette is unchanged — `jade-dark` is still the JetBrains New UI
//! charcoal and `jade-light` the warm cream research palette. What the token
//! layer buys is the component system on top of them: every control in
//! `crate::kumo` styles itself from these tokens, so the whole window keeps one
//! density and one set of radii, weights, and rules.
//!
//! Colors are plain `0xRRGGBB` so callers can `gpui::rgb(theme.accent)` at paint
//! time. New code should read `theme.kumo` and let the Kumo components style
//! themselves.

use crate::kumo::tokens::{self, KumoTokens};

/// A resolved color palette. One instance per theme; the app holds the active
/// one and re-reads it on every render so a theme toggle is a field swap.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,

    /// The full Kumo token set. Every component in `crate::kumo` reads this.
    pub kumo: KumoTokens,

    // Surfaces
    pub bg: u32,     // editor / window background  — kumo `canvas`
    pub panel: u32,  // sidebars, action bar, status strip — kumo `elevated`
    pub border: u32, // subtle floating-card border — kumo `line`

    // Text
    pub text: u32,  // primary foreground — kumo `text-default`
    pub muted: u32, // comments / secondary labels — kumo `text-subtle`

    // Accent system
    pub accent: u32,     // brand — keywords, cursor, positive
    pub periwinkle: u32, // functions / info / modified
    pub blue_gray: u32,  // types
    pub amber: u32,      // strings / numbers / warnings
    pub red: u32,        // errors / deleted / leaks

    /// 5-color chart series palette (loss/kernel curves cycle through this).
    pub series: [u32; 5],

    /// Chart grid lines are drawn as a translucent overlay of this color…
    pub grid_line: u32,
    /// …at this alpha (white-on-dark vs black-on-light differ).
    pub grid_alpha: f32,

    /// Memory chart accent + its 10%-alpha area fill.
    pub mem_accent: u32,
    pub mem_fill_alpha: f32,

    /// True when the base surface is light (affects heatmap-neutral choices).
    pub is_light: bool,
}

impl Theme {
    /// Projects a Kumo token set onto the flat fields the panels read.
    ///
    /// `periwinkle`, `blue_gray`, `amber`, and `series` have no Kumo token —
    /// they are syntax and chart roles, not UI roles — so they come in
    /// explicitly.
    #[allow(clippy::too_many_arguments)]
    fn from_tokens(
        name: &'static str,
        k: KumoTokens,
        periwinkle: u32,
        blue_gray: u32,
        amber: u32,
        series: [u32; 5],
        grid_line: u32,
        grid_alpha: f32,
    ) -> Theme {
        use crate::kumo::pack;
        Theme {
            name,
            kumo: k,
            bg: pack(k.canvas),
            panel: pack(k.elevated),
            border: pack(k.line),
            text: pack(k.text_default),
            muted: pack(k.text_subtle),
            accent: pack(k.brand),
            periwinkle,
            blue_gray,
            amber,
            red: pack(k.danger),
            series,
            grid_line,
            grid_alpha,
            mem_accent: pack(k.brand),
            mem_fill_alpha: 0.10,
            is_light: k.is_light,
        }
    }

    /// jade-dark — "JetBrains New UI charcoal" (§4.2).
    pub fn jade_dark() -> Theme {
        Theme::from_tokens(
            "jade-dark",
            tokens::jade_dark(),
            0x8DB2FF, // periwinkle — functions / info / modified
            0x9BB5CF, // blue-gray — types
            0xD4A76A, // amber — strings / numbers / warnings
            [0x56B389, 0x8DB2FF, 0xD4A76A, 0xCF6B6B, 0x9BB5CF],
            0xFFFFFF,
            0.04,
        )
    }

    /// jade-light — the warm cream research palette (§4.2).
    pub fn jade_light() -> Theme {
        Theme::from_tokens(
            "jade-light",
            tokens::jade_light(),
            0x2F5FD0,
            0x4A6A88,
            0x9A6700,
            [0x2E7D5B, 0x2F5FD0, 0x9A6700, 0xB3403F, 0x6E5494],
            0x000000,
            0.07,
        )
    }

    /// Cloudflare's own Kumo palette, unmodified. Kept so the ported components
    /// can be checked against the upstream library by eye.
    pub fn kumo_dark() -> Theme {
        Theme::from_tokens(
            "kumo-dark",
            tokens::kumo_dark(),
            0x8DB2FF,
            0x9BB5CF,
            0xD4A76A,
            [0x00D492, 0x51A2FF, 0xFFAC00, 0xE7000B, 0x8200DB],
            0xFFFFFF,
            0.04,
        )
    }

    /// Cloudflare's own Kumo light palette.
    pub fn kumo_light() -> Theme {
        Theme::from_tokens(
            "kumo-light",
            tokens::kumo_light(),
            0x2F5FD0,
            0x4A6A88,
            0x9A6700,
            [0x009966, 0x155DFC, 0xFFAC00, 0xE7000B, 0x9810FA],
            0x000000,
            0.07,
        )
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::jade_dark()
    }
}

#[cfg(test)]
mod tests {
    use super::Theme;

    /// The token layer must not move the palette. These are the exact values
    /// `jade-dark` shipped before the Kumo tokens went in; a component-library
    /// change is allowed to restyle the app, never to recolor it.
    #[test]
    fn jade_dark_keeps_its_documented_colors() {
        let t = Theme::jade_dark();
        assert_eq!(t.name, "jade-dark");
        assert_eq!(t.bg, 0x1E1F22);
        assert_eq!(t.panel, 0x2B2D30);
        assert_eq!(t.border, 0x35373B);
        assert_eq!(t.text, 0xDFE1E5);
        assert_eq!(t.muted, 0x6B7A72);
        assert_eq!(t.accent, 0x56B389);
        assert_eq!(t.periwinkle, 0x8DB2FF);
        assert_eq!(t.blue_gray, 0x9BB5CF);
        assert_eq!(t.amber, 0xD4A76A);
        assert_eq!(t.red, 0xCF6B6B);
        assert_eq!(t.series, [0x56B389, 0x8DB2FF, 0xD4A76A, 0xCF6B6B, 0x9BB5CF]);
        assert_eq!(t.grid_line, 0xFFFFFF);
        assert_eq!(t.grid_alpha, 0.04);
        assert_eq!(t.mem_accent, 0x56B389);
        assert_eq!(t.mem_fill_alpha, 0.10);
        assert!(!t.is_light);
    }

    #[test]
    fn jade_light_keeps_its_documented_colors() {
        let t = Theme::jade_light();
        assert_eq!(t.name, "jade-light");
        assert_eq!(t.bg, 0xF4EFE2);
        assert_eq!(t.panel, 0xEAE4D3);
        // The panels draw `border` at ~8% alpha, so it must stay pure black.
        assert_eq!(t.border, 0x000000);
        assert_eq!(t.text, 0x373528);
        assert_eq!(t.muted, 0x8A8674);
        assert_eq!(t.accent, 0x2E7D5B);
        assert_eq!(t.periwinkle, 0x2F5FD0);
        assert_eq!(t.blue_gray, 0x4A6A88);
        assert_eq!(t.amber, 0x9A6700);
        assert_eq!(t.red, 0xB3403F);
        assert_eq!(t.series, [0x2E7D5B, 0x2F5FD0, 0x9A6700, 0xB3403F, 0x6E5494]);
        assert_eq!(t.grid_line, 0x000000);
        assert_eq!(t.grid_alpha, 0.07);
        assert!(t.is_light);
    }

    /// The accent is the brand slot every Kumo component paints its primary
    /// fill and focus ring with, so the two must never diverge.
    #[test]
    fn accent_and_the_kumo_brand_token_stay_in_step() {
        for t in [Theme::jade_dark(), Theme::jade_light()] {
            assert_eq!(t.accent, crate::kumo::pack(t.kumo.brand));
            assert_eq!(t.accent, crate::kumo::pack(t.kumo.focus));
            assert_eq!(t.red, crate::kumo::pack(t.kumo.danger));
        }
    }
}
