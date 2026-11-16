//! Profile picture for the lock screen.
//!
//! The picture is whatever the system has for the account: AccountsService
//! (`org.freedesktop.Accounts`, property `IconFile`), the same store the
//! desktop settings, DMS, SDDM and GDM read and write. If that has nothing
//! usable, `~/.face` then `~/.face.icon`.
//!
//! Anything missing, unreadable or undecodable is skipped: the lock screen
//! falls back to the initial-in-a-circle badge. A bad picture, or a slow or
//! absent accounts daemon, must never stop the screen from coming up, so the
//! lookup runs on its own thread and the picture appears when it is ready.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use image::imageops::{self, FilterType};
use tiny_skia::{IntSize, Pixmap};

/// The picture is stored square at this many pixels and scaled when drawn.
pub const SIDE: u32 = 256;
/// Larger files are not even opened.
const MAX_FILE_BYTES: u64 = 24 * 1024 * 1024;

/// The traditional per-user locations, used when AccountsService has nothing.
pub fn fallbacks(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".face"), home.join(".face.icon")]
}

/// The picture AccountsService has for `user` (its `IconFile`). Blocks on
/// D-Bus, so call it off the UI thread.
pub fn accounts_icon(user: &str) -> Option<PathBuf> {
    zbus::block_on(accounts_icon_async(user))
}

async fn accounts_icon_async(user: &str) -> Option<PathBuf> {
    use zbus::{Connection, OwnedObjectPath, Proxy};

    let bus = Connection::system().await.ok()?;
    let accounts =
        Proxy::new(&bus, "org.freedesktop.Accounts", "/org/freedesktop/Accounts", "org.freedesktop.Accounts")
            .await
            .ok()?;
    let path: OwnedObjectPath = accounts.call("FindUserByName", &(user,)).await.ok()?;
    let account = Proxy::new(&bus, "org.freedesktop.Accounts", path, "org.freedesktop.Accounts.User").await.ok()?;
    let file: String = account.get_property("IconFile").await.ok()?;
    // An unset icon is the empty string.
    (!file.is_empty()).then(|| PathBuf::from(file))
}

/// First of `paths` that decodes.
pub fn first_decodable(paths: &[PathBuf]) -> Option<Pixmap> {
    paths.iter().find_map(|p| decode(p))
}

/// The account's picture as a premultiplied `SIDE`×`SIDE` pixmap.
pub fn load(user: &str) -> Option<Pixmap> {
    let mut paths: Vec<PathBuf> = accounts_icon(user).into_iter().collect();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.extend(fallbacks(&home));
    }
    first_decodable(&paths)
}

/// Look the picture up on a thread. The receiver yields once, `Some` if one
/// was found.
pub fn spawn_load(user: &str) -> Receiver<Option<Pixmap>> {
    let (tx, rx) = channel();
    let user = user.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(load(&user));
    });
    rx
}

/// Decode `path` to a centred square, scaled to `SIDE`.
pub fn decode(path: &Path) -> Option<Pixmap> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    // A malformed picture must not take the lock screen down with it.
    std::panic::catch_unwind(|| decode_inner(path)).ok().flatten()
}

fn decode_inner(path: &Path) -> Option<Pixmap> {
    let mut reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgba8();

    let (w, h) = img.dimensions();
    let side = w.min(h);
    if side == 0 {
        return None;
    }
    let square = imageops::crop_imm(&img, (w - side) / 2, (h - side) / 2, side, side).to_image();
    let scaled = imageops::resize(&square, SIDE, SIDE, FilterType::Lanczos3);

    // tiny-skia wants premultiplied alpha.
    let mut data = scaled.into_raw();
    for px in data.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u32::from(*c) * a + 127) / 255) as u8;
        }
    }
    Pixmap::from_vec(data, IntSize::from_wh(SIDE, SIDE)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("veil-lock-avatar-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path, w: u32, h: u32, px: Rgba<u8>) {
        RgbaImage::from_pixel(w, h, px).save(path).unwrap();
    }

    /// Run by hand: `cargo test -p veil-lock real_accounts -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the system bus and accounts-daemon"]
    fn real_accountsservice_lookup_finds_this_users_picture() {
        let user = crate::auth::current_user().expect("a user");
        let icon = accounts_icon(&user);
        println!("IconFile for {user}: {icon:?}");
        let pm = load(&user);
        println!("decoded: {}", pm.is_some());
        assert!(icon.is_some(), "AccountsService returned no IconFile");
    }

    #[test]
    fn the_fallbacks_are_the_traditional_home_files_in_order() {
        let f = fallbacks(Path::new("/home/u"));
        assert_eq!(f, vec![PathBuf::from("/home/u/.face"), PathBuf::from("/home/u/.face.icon")]);
    }

    #[test]
    fn the_first_picture_that_decodes_wins_and_bad_ones_are_skipped() {
        let dir = scratch("pick");
        let bad = dir.join("bad.png");
        std::fs::write(&bad, b"nope").unwrap();
        let red = dir.join("red.png");
        write_png(&red, 64, 64, Rgba([200, 0, 0, 255]));
        let blue = dir.join("blue.png");
        write_png(&blue, 64, 64, Rgba([0, 0, 200, 255]));
        let missing = dir.join("missing.png");
        let pm = first_decodable(&[missing, bad, red, blue]).expect("one decodes");
        assert_eq!(pm.pixel(10, 10).unwrap().red(), 200);
        assert!(first_decodable(&[]).is_none());
    }

    #[test]
    fn a_non_square_picture_is_cropped_to_a_centred_square() {
        let dir = scratch("wide");
        let p = dir.join("a.png");
        write_png(&p, 600, 200, Rgba([200, 40, 40, 255]));
        let pm = decode(&p).expect("decodes");
        assert_eq!((pm.width(), pm.height()), (SIDE, SIDE));
        let mid = pm.pixel(SIDE / 2, SIDE / 2).unwrap();
        assert_eq!((mid.red(), mid.alpha()), (200, 255));
    }

    #[test]
    fn transparent_pixels_come_out_premultiplied() {
        let dir = scratch("alpha");
        let p = dir.join("a.png");
        write_png(&p, 64, 64, Rgba([200, 100, 50, 128]));
        let pm = decode(&p).expect("decodes");
        let px = pm.pixel(10, 10).unwrap();
        assert!(px.alpha() >= 127 && px.alpha() <= 129);
        assert!(px.red() <= px.alpha(), "premultiplied colour can never exceed alpha");
    }

    #[test]
    fn missing_empty_and_corrupt_files_are_skipped_not_fatal() {
        let dir = scratch("bad");
        assert!(decode(&dir.join("nope.png")).is_none());
        let empty = dir.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert!(decode(&empty).is_none());
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"this is not an image at all").unwrap();
        assert!(decode(&junk).is_none());
        // A directory is not a picture either.
        assert!(decode(&dir).is_none());
    }

    #[test]
    fn the_format_comes_from_the_content_not_the_name() {
        // `~/.face` has no extension.
        let dir = scratch("noext");
        let p = dir.join(".face");
        RgbaImage::from_pixel(32, 32, Rgba([1, 2, 3, 255])).save_with_format(&p, image::ImageFormat::Png).unwrap();
        assert!(decode(&p).is_some());
    }
}
