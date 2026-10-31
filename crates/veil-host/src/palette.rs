//! Palette plumbing for the compositor: which tokens the configured theme
//! means, the colours the host's own chrome (launcher, help, overlays) takes
//! from them, and the `palette.json` file other processes can read.
//!
//! The derivation itself lives in `veil-palette`; this file is only glue.

use std::io::Write;
use std::path::PathBuf;

use veil_config::{Theme, ThemeMode, ThemeName};
use veil_palette::{Mode, Tokens};

pub fn mode_of(m: ThemeMode) -> Mode {
    match m {
        ThemeMode::Dark => Mode::Dark,
        ThemeMode::Light => Mode::Light,
        ThemeMode::Auto => Mode::Auto,
    }
}

/// Tokens for a hand-tuned preset. Presets skip wallpaper sampling, so the
/// bar takes their colours as authored (no contrast rewriting), with the
/// status colours and accent-fill text derived from the preset's accent.
pub fn tokens_for_preset(t: &Theme) -> Tokens {
    let rgb = |c: [u8; 4]| [c[0], c[1], c[2]];
    let base = veil_palette::from_accent(rgb(t.accent), Mode::Dark);
    let panel = rgb(t.panel_bg);
    Tokens {
        bg: panel,
        surface: veil_palette::mix_rgb(panel, rgb(t.highlight), 0.5),
        surface_hi: rgb(t.highlight),
        // Presets use a bright border for overlay outlines; a full-strength
        // 1px line along the whole bar would shout, so soften it.
        border: veil_palette::mix_rgb(panel, rgb(t.border), 0.35),
        text: rgb(t.text),
        text_dim: rgb(t.text_dim),
        accent: rgb(t.accent),
        accent_text: base.accent_text,
        ok: base.ok,
        warn: base.warn,
        err: base.err,
    }
}

/// The tokens to show before any wallpaper has been sampled.
pub fn initial_tokens(name: ThemeName, theme: &Theme) -> Tokens {
    match name {
        ThemeName::Wallpaper => Tokens::default(),
        _ => tokens_for_preset(theme),
    }
}

/// The host chrome's colours when the theme comes from the wallpaper.
pub fn chrome_theme(t: &Tokens) -> Theme {
    let a = |c: [u8; 3], alpha: u8| [c[0], c[1], c[2], alpha];
    Theme {
        background: t.bg,
        panel_bg: a(t.surface, 245),
        border: a(t.accent, 255),
        header: a(t.warn, 255),
        text: a(t.text, 255),
        text_dim: a(t.text_dim, 255),
        accent: a(t.accent, 255),
        highlight: a(t.surface_hi, 255),
    }
}

/// The flat `{"bg": "#rrggbb", ...}` object used for palette.json, the
/// `palette` event and the `hello` reply.
pub fn to_json(t: &Tokens) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (name, hex) in t.hex_entries() {
        m.insert(name.to_string(), serde_json::Value::String(hex));
    }
    serde_json::Value::Object(m)
}

/// `$XDG_RUNTIME_DIR/veil/palette.json`.
pub fn file_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(dir).join("veil").join("palette.json"))
}

/// Write palette.json atomically (temp file, then rename) so a reader never
/// sees half a file.
pub fn write_file(t: &Tokens) -> std::io::Result<()> {
    let Some(path) = file_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(serde_json::to_string_pretty(&to_json(t))?.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_keep_their_authored_text_and_accent() {
        for name in [ThemeName::Nord, ThemeName::Dracula, ThemeName::DeepSage, ThemeName::Default] {
            let t = Theme::for_name(name);
            let tok = tokens_for_preset(&t);
            assert_eq!(tok.text, [t.text[0], t.text[1], t.text[2]], "{name:?}");
            assert_eq!(tok.accent, [t.accent[0], t.accent[1], t.accent[2]], "{name:?}");
            assert_eq!(tok.bg, [t.panel_bg[0], t.panel_bg[1], t.panel_bg[2]], "{name:?}");
        }
    }

    #[test]
    fn wallpaper_theme_starts_from_the_default_tokens() {
        let t = Theme::for_name(ThemeName::Wallpaper);
        assert_eq!(initial_tokens(ThemeName::Wallpaper, &t), Tokens::default());
        assert_ne!(initial_tokens(ThemeName::Nord, &Theme::for_name(ThemeName::Nord)), Tokens::default());
    }

    #[test]
    fn chrome_follows_the_tokens() {
        let tok = veil_palette::from_accent([200, 90, 60], Mode::Dark);
        let c = chrome_theme(&tok);
        assert_eq!(&c.accent[..3], &tok.accent);
        assert_eq!(&c.text[..3], &tok.text);
        assert_eq!(&c.panel_bg[..3], &tok.surface);
        assert_eq!(c.background, tok.bg);
    }

    #[test]
    fn json_has_every_token_as_hex() {
        let j = to_json(&Tokens::default());
        for name in ["bg", "surface", "surface_hi", "border", "text", "text_dim", "accent", "accent_text", "ok", "warn", "err"] {
            let v = j.get(name).and_then(|v| v.as_str()).unwrap_or_else(|| panic!("missing {name}"));
            assert!(veil_palette::parse_hex(v).is_some(), "{name} = {v}");
        }
    }
}
