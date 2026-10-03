//! The preset's design tokens, and the palette it draws with.
//!
//! Every size here is in rems, so a larger text setting scales the whole
//! window rather than only its type. Colours come from the GPUI Kit theme; the
//! one colour this preset decides is the accent, and it decides it under a
//! contrast rule rather than taking it on trust.

use std::time::Duration;

use zup_preset_sdk::gpui_kit::component::{Colorize, Theme, ThemeMode};
use zup_preset_sdk::gpui_kit::{App, Hsla, Pixels, Rems, Rgba, Window, px};

use crate::{Accent, Appearance, Settings};

/// Space between things, from touching to unrelated.
pub mod space {
    use super::Rems;

    pub const HAIR: Rems = Rems(0.125);
    pub const XS: Rems = Rems(0.25);
    pub const SM: Rems = Rems(0.5);
    pub const MD: Rems = Rems(0.75);
    pub const LG: Rems = Rems(1.0);
    pub const XL: Rems = Rems(1.5);
    pub const XXL: Rems = Rems(2.0);
    pub const XXXL: Rems = Rems(3.0);
}

/// Type sizes, from fine print to the product's name.
pub mod text {
    use super::Rems;

    pub const CAPTION: Rems = Rems(0.75);
    pub const SMALL: Rems = Rems(0.8125);
    pub const BODY: Rems = Rems(0.875);
    pub const LEAD: Rems = Rems(1.0);
    pub const HEADING: Rems = Rems(1.125);
    pub const TITLE: Rems = Rems(1.5);
}

/// Sizes of the things that are not text.
pub mod size {
    use super::Rems;

    /// The widest a column of reading and choosing gets.
    pub const COLUMN: Rems = Rems(36.0);
    /// The widest the result and progress screens get: they say one thing.
    pub const FOCUS_COLUMN: Rems = Rems(28.0);
    /// The application's mark beside its name.
    pub const MARK: Rems = Rems(3.5);
    /// The mark above a progress or result.
    pub const MARK_FOCUS: Rems = Rems(4.5);
    /// The mark in the title bar.
    pub const MARK_TITLE: Rems = Rems(1.0);
    pub const ICON_SM: Rems = Rems(0.875);
    pub const ICON: Rems = Rems(1.0);
    pub const ICON_LG: Rems = Rems(1.25);
    /// The tile an action's or fact's icon sits in.
    pub const ICON_TILE: Rems = Rems(2.0);
    /// The widest a primary action is allowed to stretch on a narrow window.
    pub const ACTION_MIN: Rems = Rems(7.5);
    /// How many changes a plan group lists before it summarizes the rest.
    pub const PLAN_ROWS: usize = 40;
}

/// How long things take to move.
pub mod motion {
    use super::{Duration, Pixels, px};

    /// A screen arriving.
    pub const ENTER: Duration = Duration::from_millis(220);
    /// How far it travels while it does.
    pub const ENTER_DISTANCE: Pixels = px(6.);
}

/// The window's own proportions.
pub mod window {
    use super::{Pixels, px};

    pub const WIDTH: Pixels = px(720.);
    pub const HEIGHT: Pixels = px(580.);
    pub const MIN_WIDTH: Pixels = px(440.);
    pub const MIN_HEIGHT: Pixels = px(440.);
    /// How wide the plan sheet is, when the window has room for it.
    pub const SHEET: Pixels = px(460.);
}

/// How much room the window has, which decides how things are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layout {
    /// Rows stack: there is not room for a label and its action side by side.
    Compact,
    Regular,
    /// There is room to spare, so the column gets more air rather than more width.
    Wide,
}

impl Layout {
    const COMPACT_BELOW: Pixels = px(580.);
    const WIDE_FROM: Pixels = px(960.);

    pub fn of(window: &Window) -> Self {
        Self::for_width(window.viewport_size().width / (window.rem_size() / px(16.)))
    }

    fn for_width(width: Pixels) -> Self {
        if width < Self::COMPACT_BELOW {
            Self::Compact
        } else if width >= Self::WIDE_FROM {
            Self::Wide
        } else {
            Self::Regular
        }
    }

    pub fn is_compact(self) -> bool {
        self == Self::Compact
    }

    /// The air between the window's edge and its content.
    pub fn gutter(self) -> Rems {
        match self {
            Self::Compact => space::LG,
            Self::Regular => space::XXL,
            Self::Wide => space::XXXL,
        }
    }
}

/// The accent a window uses when the application names none.
const DEFAULT_ACCENT: [u8; 3] = [0x25, 0x63, 0xeb];

/// The least contrast text must have against what it sits on.
const TEXT_CONTRAST: f32 = 4.5;
/// The least contrast a filled control must have against the window.
const FILL_CONTRAST: f32 = 3.0;

/// Put the window in the palette the settings and the system ask for.
///
/// Called whenever either changes: switching palettes reloads the theme's own
/// colours, so the accent is laid over them again every time.
pub fn apply(settings: &Settings, window: &mut Window, cx: &mut App) {
    let mode = match settings.appearance {
        Appearance::System => ThemeMode::from(window.appearance()),
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
    };
    if Theme::global(cx).mode != mode {
        Theme::change(mode, Some(window), cx);
    }
    let accent = settings
        .accent
        .as_ref()
        .and_then(Accent::rgb)
        .unwrap_or(DEFAULT_ACCENT);
    Theme::update(cx, |theme| {
        let palette = Palette::new(accent, theme.background, theme.is_dark());
        theme.primary = palette.fill;
        theme.primary_hover = palette.hover;
        theme.primary_active = palette.pressed;
        theme.primary_foreground = palette.on_fill;
        theme.button_primary = palette.fill;
        theme.button_primary_hover = palette.hover;
        theme.button_primary_active = palette.pressed;
        theme.button_primary_foreground = palette.on_fill;
        theme.ring = palette.fill;
        theme.progress_bar = palette.fill;
        theme.link = palette.text;
        theme.link_hover = palette.hover;
        theme.link_active = palette.pressed;
        theme.radius = px(6.);
        theme.radius_lg = px(10.);
    });
}

/// An accent, made safe to use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Filled controls: the main button, a checked box, progress.
    pub fill: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    /// Text drawn on a fill.
    pub on_fill: Hsla,
    /// The accent used as text on the window's background.
    pub text: Hsla,
}

impl Palette {
    pub fn new([r, g, b]: [u8; 3], background: Hsla, dark: bool) -> Self {
        let wanted: Hsla = Rgba {
            r: f32::from(r) / 255.,
            g: f32::from(g) / 255.,
            b: f32::from(b) / 255.,
            a: 1.,
        }
        .into();
        let white = Hsla::white();
        let ink: Hsla = Rgba {
            r: 0.04,
            g: 0.04,
            b: 0.06,
            a: 1.,
        }
        .into();

        let mut fill = wanted;
        if dark {
            // Visible against the window first; text on it follows.
            while contrast(fill, background) < FILL_CONTRAST && fill.l < 0.95 {
                fill.l += 0.02;
            }
        } else {
            while contrast(fill, white) < TEXT_CONTRAST && fill.l > 0.05 {
                fill.l -= 0.02;
            }
        }
        let on_fill = if contrast(fill, white) >= contrast(fill, ink) {
            white
        } else {
            ink
        };

        let mut text = fill;
        if dark {
            while contrast(text, background) < TEXT_CONTRAST && text.l < 0.95 {
                text.l += 0.02;
            }
        }

        let (hover, pressed) = if dark {
            (fill.lighten(0.08), fill.darken(0.06))
        } else {
            (fill.darken(0.08), fill.darken(0.16))
        };
        Self {
            fill,
            hover,
            pressed,
            on_fill,
            text,
        }
    }
}

/// The WCAG contrast ratio between two opaque colours.
pub fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    let (light, dark) = if a > b { (a, b) } else { (b, a) };
    (light + 0.05) / (dark + 0.05)
}

fn luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn background(dark: bool) -> Hsla {
        if dark {
            Rgba {
                r: 0.04,
                g: 0.04,
                b: 0.04,
                a: 1.,
            }
            .into()
        } else {
            Hsla::white()
        }
    }

    /// No accent an application can choose makes the main button unreadable,
    /// in either palette.
    #[test]
    fn every_accent_keeps_its_text_readable() {
        for accent in [
            [0xff, 0xff, 0x00],
            [0xff, 0xff, 0xff],
            [0x00, 0x00, 0x00],
            [0x7f, 0xff, 0xd4],
            [0x25, 0x63, 0xeb],
            [0xe1, 0x1d, 0x48],
            [0x11, 0x11, 0x33],
        ] {
            for dark in [false, true] {
                let palette = Palette::new(accent, background(dark), dark);
                assert!(
                    contrast(palette.fill, palette.on_fill) >= TEXT_CONTRAST,
                    "{accent:?} dark={dark}: text on the button"
                );
                assert!(
                    contrast(palette.fill, background(dark)) >= FILL_CONTRAST - 0.01,
                    "{accent:?} dark={dark}: the button against the window"
                );
                assert!(
                    contrast(palette.text, background(dark)) >= TEXT_CONTRAST - 0.01,
                    "{accent:?} dark={dark}: accent text against the window"
                );
            }
        }
    }

    /// A readable accent is left as the application chose it.
    #[test]
    fn a_readable_accent_is_not_changed() {
        let palette = Palette::new(DEFAULT_ACCENT, background(false), false);
        let chosen: Hsla = Rgba {
            r: f32::from(DEFAULT_ACCENT[0]) / 255.,
            g: f32::from(DEFAULT_ACCENT[1]) / 255.,
            b: f32::from(DEFAULT_ACCENT[2]) / 255.,
            a: 1.,
        }
        .into();
        assert_eq!(palette.fill, chosen);
    }

    #[test]
    fn the_layout_follows_the_room_there_is() {
        assert_eq!(Layout::for_width(px(460.)), Layout::Compact);
        assert_eq!(Layout::for_width(px(720.)), Layout::Regular);
        assert_eq!(Layout::for_width(px(1280.)), Layout::Wide);
    }
}
