use std::time::Duration;

use zup_sdk::preset::gpui_kit::component::{Colorize, Theme, ThemeMode};
use zup_sdk::preset::gpui_kit::{App, Hsla, Pixels, Rems, Rgba, Window, px};

use crate::{Accent, Appearance, Settings};

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

pub mod text {
    use super::Rems;

    pub const CAPTION: Rems = Rems(0.75);
    pub const SMALL: Rems = Rems(0.8125);
    pub const BODY: Rems = Rems(0.875);
    pub const LEAD: Rems = Rems(1.0);
    pub const HEADING: Rems = Rems(1.125);
    pub const TITLE: Rems = Rems(1.5);
}

pub mod size {
    use super::Rems;

    pub const COLUMN: Rems = Rems(36.0);
    pub const FOCUS_COLUMN: Rems = Rems(28.0);
    pub const MARK: Rems = Rems(3.5);
    pub const MARK_FOCUS: Rems = Rems(4.5);
    pub const MARK_TITLE: Rems = Rems(1.0);
    pub const ICON_SM: Rems = Rems(0.875);
    pub const ICON: Rems = Rems(1.0);
    pub const ICON_LG: Rems = Rems(1.25);
    pub const ICON_TILE: Rems = Rems(2.0);
    pub const ACTION_MIN: Rems = Rems(7.5);
    pub const PLAN_ROWS: usize = 40;
}

pub mod motion {
    use super::{Duration, Pixels, px};

    pub const ENTER: Duration = Duration::from_millis(220);
    pub const ENTER_DISTANCE: Pixels = px(6.);
}

pub mod window {
    use super::{Pixels, px};

    pub const WIDTH: Pixels = px(720.);
    pub const HEIGHT: Pixels = px(580.);
    pub const MIN_WIDTH: Pixels = px(440.);
    pub const MIN_HEIGHT: Pixels = px(440.);
    pub const SHEET: Pixels = px(460.);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layout {
    Compact,
    Regular,
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

    pub fn gutter(self) -> Rems {
        match self {
            Self::Compact => space::LG,
            Self::Regular => space::XXL,
            Self::Wide => space::XXXL,
        }
    }
}

const DEFAULT_ACCENT: [u8; 3] = [0x25, 0x63, 0xeb];

const TEXT_CONTRAST: f32 = 4.5;
const FILL_CONTRAST: f32 = 3.0;

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub fill: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub on_fill: Hsla,
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
