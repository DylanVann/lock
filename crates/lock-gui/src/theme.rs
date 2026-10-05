//! Colors and fonts. On Omarchy they come from the current theme's `colors.toml`, so the
//! window matches the terminal, bar and launcher, and it follows theme switches. Elsewhere
//! they follow the system's light or dark appearance, with colors close to AppKit's.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use gpui::{Pixels, Rgba, SharedString, WindowAppearance, px, rgb, rgba};

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Rgba,
    pub hover: Rgba,
    pub selected: Rgba,
    /// Tags, buttons and the progress bar's track.
    pub fill: Rgba,
    pub text: Rgba,
    pub secondary: Rgba,
    pub tertiary: Rgba,
    pub accent: Rgba,
    pub exclusive: Rgba,
    pub ok: Rgba,
    pub warn: Rgba,
    pub bad: Rgba,
}

impl Palette {
    pub fn system(appearance: WindowAppearance) -> Palette {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Palette {
                bg: rgb(0x1e1e1e),
                hover: rgba(0xffffff0f),
                selected: rgba(0x0a84ff4d),
                fill: rgba(0xffffff1a),
                text: rgba(0xffffffd9),
                secondary: rgba(0xffffff8c),
                tertiary: rgba(0xffffff40),
                accent: rgb(0x0a84ff),
                exclusive: rgb(0xbf5af2),
                ok: rgb(0x32d74b),
                warn: rgb(0xff9f0a),
                bad: rgb(0xff453a),
            },
            WindowAppearance::Light | WindowAppearance::VibrantLight => Palette {
                bg: rgb(0xffffff),
                hover: rgba(0x0000000a),
                selected: rgba(0x007aff33),
                fill: rgba(0x0000001a),
                text: rgba(0x000000d9),
                secondary: rgba(0x00000080),
                tertiary: rgba(0x00000040),
                accent: rgb(0x007aff),
                exclusive: rgb(0xaf52de),
                ok: rgb(0x28cd41),
                warn: rgb(0xff9500),
                bad: rgb(0xff3b30),
            },
        }
    }

    /// Maps an Omarchy theme's colors onto the palette. Missing keys fall back to `base`.
    fn omarchy(colors: &HashMap<String, Rgba>, base: Palette) -> Palette {
        let color = |key: &str| colors.get(key).copied();
        let foreground = color("foreground");
        Palette {
            bg: color("background").unwrap_or(base.bg),
            hover: color("lighter_background").unwrap_or(base.hover),
            selected: color("selection").unwrap_or(base.selected),
            fill: color("muted")
                .map(|c| Rgba { a: 0.6, ..c })
                .unwrap_or(base.fill),
            text: color("bright_foreground")
                .or(foreground)
                .unwrap_or(base.text),
            secondary: foreground.unwrap_or(base.secondary),
            tertiary: color("dark_foreground").unwrap_or(base.tertiary),
            accent: color("accent").unwrap_or(base.accent),
            exclusive: color("magenta").unwrap_or(base.exclusive),
            ok: color("green").unwrap_or(base.ok),
            warn: color("yellow").unwrap_or(base.warn),
            bad: color("red").unwrap_or(base.bad),
        }
    }
}

/// `key = "#rrggbb"` lines; that's all a `colors.toml` holds.
fn parse_colors(toml: &str) -> HashMap<String, Rgba> {
    toml.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((
                key.trim().to_string(),
                parse_hex(value.trim().trim_matches('"'))?,
            ))
        })
        .collect()
}

fn parse_hex(s: &str) -> Option<Rgba> {
    let hex = s.strip_prefix('#')?;
    let value = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        6 => Some(rgb(value)),
        8 => Some(rgba(value)),
        _ => None,
    }
}

/// The current Omarchy theme's colors, as last read.
#[derive(Clone, Default, PartialEq)]
pub struct OmarchyColors {
    modified: Option<SystemTime>,
    colors: Option<HashMap<String, Rgba>>,
}

impl OmarchyColors {
    /// Rereads `colors.toml` if it changed since `self` was read. Cheap enough to call on
    /// every refresh: usually it's a single `stat`.
    pub fn reload(&self) -> OmarchyColors {
        let Some(path) = omarchy_colors_path() else {
            return OmarchyColors::default();
        };
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if modified.is_some() && modified == self.modified {
            return self.clone();
        }
        OmarchyColors {
            modified,
            colors: std::fs::read_to_string(&path)
                .ok()
                .map(|toml| parse_colors(&toml)),
        }
    }
}

/// `omarchy-theme-set` stages the current theme in `~/.local/state`; releases before that
/// kept it in `~/.config`.
fn omarchy_colors_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    [
        ".local/state/omarchy/current/theme",
        ".config/omarchy/current/theme",
    ]
    .into_iter()
    .map(|dir| home.join(dir).join("colors.toml"))
    .find(|path| path.exists())
}

#[derive(Clone)]
pub struct Theme {
    pub p: Palette,
    /// `None` for the system UI font.
    pub font: Option<SharedString>,
    pub mono: SharedString,
    pub radius: Pixels,
}

impl Theme {
    /// Omarchy's look: square, with Hyprland's accent-colored borders.
    pub fn is_square(&self) -> bool {
        self.radius == px(0.)
    }
}

impl Theme {
    pub fn new(appearance: WindowAppearance, omarchy: &OmarchyColors, fonts: &Fonts) -> Theme {
        let system = Palette::system(appearance);
        match &omarchy.colors {
            // Omarchy's apps are terminals, Waybar and Walker, all in the monospace font,
            // and Hyprland draws windows with square corners.
            Some(colors) => Theme {
                p: Palette::omarchy(colors, system),
                font: Some(fonts.mono.clone()),
                mono: fonts.mono.clone(),
                radius: px(0.),
            },
            None => Theme {
                p: system,
                font: None,
                mono: fonts.mono.clone(),
                radius: px(6.),
            },
        }
    }

    /// Fully rounded, unless the theme is square.
    pub fn pill(&self) -> Pixels {
        if self.is_square() { px(0.) } else { px(999.) }
    }
}

pub struct Fonts {
    pub mono: SharedString,
}

impl Fonts {
    pub fn detect() -> Fonts {
        let mono = if cfg!(target_os = "macos") {
            "Menlo".to_string()
        } else {
            // fontconfig's monospace, which `omarchy-font-set` changes. The first family
            // in fc-match's alias list is the real name.
            std::process::Command::new("fc-match")
                .args(["monospace", "-f", "%{family}"])
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .and_then(|s| s.split(',').next().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "DejaVu Sans Mono".to_string())
        };
        Fonts { mono: mono.into() }
    }
}
