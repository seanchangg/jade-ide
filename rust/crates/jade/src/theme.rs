//! Jade theme palettes (docs/jade-feature-inventory.md §4.2).
//!
//! `forge-dark` is implemented now; `forge-light` is structured (the light
//! series/accent colors are the ones the Electron charts used, plus the
//! documented cream/charcoal editor colors) so the runtime can flip themes
//! once Phase-3 wires the theme toggle. Colors are plain `0xRRGGBB` so callers
//! can `gpui::rgb(theme.accent)` at paint time, matching how the TS charts
//! resolved colors on demand.

/// A resolved color palette. One instance per theme; the app holds the active
/// one and re-reads it on every render so a theme toggle is a field swap.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,

    // Surfaces
    pub bg: u32,     // editor / window background
    pub panel: u32,  // sidebars, action bar, status strip
    pub border: u32, // subtle floating-card border

    // Text
    pub text: u32,  // primary foreground
    pub muted: u32, // comments / secondary labels

    // Accent system (§4.2)
    pub accent: u32,     // emerald  — keywords, cursor, positive
    pub periwinkle: u32, // functions / info / modified
    pub blue_gray: u32,  // types
    pub amber: u32,      // strings / numbers / warnings
    pub red: u32,        // errors / deleted / leaks

    /// 5-color chart series palette (loss/kernel curves cycle through this).
    pub series: [u32; 5],

    /// Chart grid lines are drawn as a translucent overlay of this color…
    pub grid_line: u32,
    /// …at this alpha (white-on-dark vs black-on-light differ, §training-view).
    pub grid_alpha: f32,

    /// Memory chart accent + its 10%-alpha area fill.
    pub mem_accent: u32,
    pub mem_fill_alpha: f32,

    /// True when the base surface is light (affects heatmap-neutral choices).
    pub is_light: bool,
}

impl Theme {
    /// forge-dark — "JetBrains New UI charcoal" (§4.2).
    pub fn forge_dark() -> Theme {
        Theme {
            name: "forge-dark",
            bg: 0x1E1F22,
            panel: 0x2B2D30,
            border: 0x35373B,
            text: 0xDFE1E5,
            muted: 0x6B7A72,
            accent: 0x56B389,
            periwinkle: 0x8DB2FF,
            blue_gray: 0x9BB5CF,
            amber: 0xD4A76A,
            red: 0xCF6B6B,
            series: [0x56B389, 0x8DB2FF, 0xD4A76A, 0xCF6B6B, 0x9BB5CF],
            grid_line: 0xFFFFFF,
            grid_alpha: 0.04,
            mem_accent: 0x56B389,
            mem_fill_alpha: 0.10,
            is_light: false,
        }
    }

    /// forge-light — warm cream research palette (§4.2). Structured now; the
    /// theme toggle that selects it lands with Phase-3.
    pub fn forge_light() -> Theme {
        Theme {
            name: "forge-light",
            bg: 0xF4EFE2,
            panel: 0xEAE4D3,
            border: 0x000000, // used at ~8% alpha by callers
            text: 0x373528,
            muted: 0x8A8674,
            accent: 0x2E7D5B,
            periwinkle: 0x2F5FD0,
            blue_gray: 0x4A6A88,
            amber: 0x9A6700,
            red: 0xB3403F,
            series: [0x2E7D5B, 0x2F5FD0, 0x9A6700, 0xB3403F, 0x6E5494],
            grid_line: 0x000000,
            grid_alpha: 0.07,
            mem_accent: 0x2E7D5B,
            mem_fill_alpha: 0.10,
            is_light: true,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::forge_dark()
    }
}
