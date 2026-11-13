//! Small copies of the covers and artist pictures in your folders: pages
//! show them at a fraction of their size, so they get a 250px one (its
//! shorter side), made once and kept under newcut's cache folder. A copy
//! is named after the original's path, size and modification time: change
//! the picture and a new one is made.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use image::ImageReader;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

/// The shorter side of a thumbnail, in pixels.
pub const SIZE: u32 = 250;

const QUALITY: u8 = 85;

/// The picture at `original` (of type `kind`) as pages should get it: its
/// thumbnail from `cache` (made now if there's none yet), or the picture
/// itself when it's small already or can't be read as an image.
///
/// # Errors
///
/// When `original` can't be read.
pub fn get(
    cache: &Path,
    original: &Path,
    kind: &'static str,
) -> std::io::Result<(Vec<u8>, &'static str)> {
    let meta = std::fs::metadata(original)?;
    let thumb = cache_path(cache, original, &meta);
    if let Ok(body) = std::fs::read(&thumb) {
        tracing::debug!("thumbnail of {}: {}", original.display(), thumb.display());
        return Ok((body, "image/jpeg"));
    }
    let body = std::fs::read(original)?;
    match make(&body) {
        Ok(Some(small)) => {
            if let Err(e) = save(&thumb, &small) {
                tracing::warn!("can't keep {}: {e}", thumb.display());
            }
            tracing::debug!(
                "thumbnail of {}: made ({} → {} bytes)",
                original.display(),
                body.len(),
                small.len()
            );
            Ok((small, "image/jpeg"))
        }
        Ok(None) => Ok((body, kind)),
        Err(e) => {
            tracing::debug!("thumbnail of {}: {e}; the original", original.display());
            Ok((body, kind))
        }
    }
}

/// Where `original`'s thumbnail is kept (as it is now).
fn cache_path(cache: &Path, original: &Path, meta: &std::fs::Metadata) -> PathBuf {
    use sha2::Digest;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let key = format!("{SIZE}\0{}\0{}\0{modified}", original.display(), meta.len());
    let hash = crate::hex::encode(&sha2::Sha256::digest(key.as_bytes()));
    cache.join(&hash[..2]).join(format!("{}.jpg", &hash[..32]))
}

/// A JPEG of `body` with its shorter side `SIZE` pixels; `None` when it's
/// that small already.
fn make(body: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let image = ImageReader::new(Cursor::new(body))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let (width, height) = (image.width(), image.height());
    let shorter = width.min(height);
    if shorter <= SIZE {
        return Ok(None);
    }
    let scale = f64::from(SIZE) / f64::from(shorter);
    let size = |side: u32| {
        // Rounded, at least SIZE on the shorter side.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled = (f64::from(side) * scale).round() as u32;
        scaled.max(SIZE)
    };
    let small = image
        .resize_exact(size(width), size(height), FilterType::Triangle)
        .to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, QUALITY)
        .encode_image(&small)
        .map_err(|e| e.to_string())?;
    Ok(Some(out))
}

/// Write `body` to `path` whole (a reader never sees half of it).
fn save(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("part");
    std::fs::write(&partial, body)?;
    std::fs::rename(&partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([
                u8::try_from(x % 256).unwrap(),
                u8::try_from(y % 256).unwrap(),
                90,
            ])
        });
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 95)
            .encode_image(&image)
            .unwrap();
        out
    }

    fn dimensions(body: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory(body).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn makes_a_small_copy_once_and_again_when_the_picture_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache/thumbs");
        let original = dir.path().join("Folder.jpg");
        std::fs::write(&original, jpeg(1000, 800)).unwrap();

        let (body, kind) = get(&cache, &original, "image/jpeg").unwrap();
        assert_eq!(kind, "image/jpeg");
        assert_eq!(dimensions(&body), (313, 250), "the shorter side is 250");
        let kept: Vec<_> = walkdir::WalkDir::new(&cache)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .collect();
        assert_eq!(kept.len(), 1);
        // Kept: served from the cache, even if reading the original would
        // say something else.
        std::fs::write(kept[0].path(), b"kept").unwrap();
        assert_eq!(get(&cache, &original, "image/jpeg").unwrap().0, b"kept");

        // A new picture (another size): a new copy.
        std::fs::write(&original, jpeg(600, 900)).unwrap();
        let (body, _) = get(&cache, &original, "image/jpeg").unwrap();
        assert_eq!(dimensions(&body), (250, 375));
    }

    #[test]
    fn small_and_unreadable_pictures_are_served_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("thumbs");
        let small = dir.path().join("small.jpg");
        let bytes = jpeg(200, 200);
        std::fs::write(&small, &bytes).unwrap();
        assert_eq!(get(&cache, &small, "image/jpeg").unwrap().0, bytes);
        let odd = dir.path().join("cover.png");
        std::fs::write(&odd, b"not an image").unwrap();
        assert_eq!(
            get(&cache, &odd, "image/png").unwrap(),
            (b"not an image".to_vec(), "image/png")
        );
        assert!(!cache.exists(), "nothing kept for either");
        assert!(get(&cache, &dir.path().join("gone.jpg"), "image/jpeg").is_err());
    }
}
