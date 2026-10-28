//! Palette tokens and dynamic theme synchronization.

use serde::{Deserialize, Serialize};
use tiny_skia::Color;
use crate::draw::hex_color;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeTokens {
    pub bg: String,
    pub surface: String,
    pub surface_hi: String,
    pub border: String,
    pub text: String,
    pub text_dim: String,
    pub accent: String,
    pub accent_text: String,
    pub ok: String,
    pub warn: String,
    pub err: String,
}

impl Default for ThemeTokens {
    fn default() -> Self {
        Self {
            bg: "#101216".into(),
            surface: "#181b20".into(),
            surface_hi: "#23272f".into(),
            border: "#2e3440".into(),
            text: "#eceff4".into(),
            text_dim: "#9aa5b5".into(),
            accent: "#88c0d0".into(),
            accent_text: "#101216".into(),
            ok: "#a3be8c".into(),
            warn: "#ebcb8b".into(),
            err: "#bf616a".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub tokens: ThemeTokens,
    pub bg: Color,
    pub surface: Color,
    pub surface_hi: Color,
    pub border: Color,
    pub text: Color,
    pub text_dim: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_tokens(ThemeTokens::default())
    }
}

impl Theme {
    /// Build a theme from the shared palette tokens (what the compositor
    /// publishes). Keeps the string `tokens` field in step for code that
    /// still reads hex.
    pub fn from_palette(p: &veil_palette::Tokens) -> Self {
        let entries = p.hex_entries();
        let get = |name: &str| {
            entries.iter().find(|(n, _)| *n == name).map(|(_, v)| v.clone()).unwrap_or_default()
        };
        Self::from_tokens(ThemeTokens {
            bg: get("bg"),
            surface: get("surface"),
            surface_hi: get("surface_hi"),
            border: get("border"),
            text: get("text"),
            text_dim: get("text_dim"),
            accent: get("accent"),
            accent_text: get("accent_text"),
            ok: get("ok"),
            warn: get("warn"),
            err: get("err"),
        })
    }

    pub fn from_tokens(tokens: ThemeTokens) -> Self {
        Self {
            bg: hex_color(&tokens.bg),
            surface: hex_color(&tokens.surface),
            surface_hi: hex_color(&tokens.surface_hi),
            border: hex_color(&tokens.border),
            text: hex_color(&tokens.text),
            text_dim: hex_color(&tokens.text_dim),
            accent: hex_color(&tokens.accent),
            accent_text: hex_color(&tokens.accent_text),
            ok: hex_color(&tokens.ok),
            warn: hex_color(&tokens.warn),
            err: hex_color(&tokens.err),
            tokens,
        }
    }
}
