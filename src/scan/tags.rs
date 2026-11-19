//! The tags newcut reads from an audio file: the MusicBrainz ids MusicBrainz
//! Picard writes, the album and album artist names, and the audio's
//! quality.

use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub release_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
    /// The album artists (`MUSICBRAINZ_ALBUMARTISTID`), else the track's.
    pub artist_mbids: Vec<String>,
    pub quality: crate::quality::Quality,
}

/// Read a file's tags and audio properties (not its pictures).
///
/// # Errors
///
/// When the file can't be opened or isn't an audio format lofty knows.
pub fn read(path: &Path) -> Result<Tags, String> {
    let file = Probe::open(path)
        .and_then(|p| {
            p.options(
                ParseOptions::new()
                    .read_properties(true)
                    .read_cover_art(false),
            )
            .read()
        })
        .map_err(|e| e.to_string())?;
    let quality = quality(path, file.file_type(), file.properties());
    let tags = file.tags();
    let first = |key: ItemKey| {
        tags.iter()
            .find_map(|t| t.get_string(key).map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_string)
    };
    let ids = |key: ItemKey| tags.iter().map(|t| ids(t, key)).find(|ids| !ids.is_empty());
    Ok(Tags {
        album: first(ItemKey::AlbumTitle),
        album_artist: first(ItemKey::AlbumArtist).or_else(|| first(ItemKey::TrackArtist)),
        release_mbid: ids(ItemKey::MusicBrainzReleaseId).and_then(|i| i.into_iter().next()),
        release_group_mbid: ids(ItemKey::MusicBrainzReleaseGroupId)
            .and_then(|i| i.into_iter().next()),
        artist_mbids: ids(ItemKey::MusicBrainzReleaseArtistId)
            .or_else(|| ids(ItemKey::MusicBrainzArtistId))
            .unwrap_or_default(),
        quality,
    })
}

/// The audio's quality, from its properties.
fn quality(
    path: &Path,
    kind: FileType,
    properties: &lofty::properties::FileProperties,
) -> crate::quality::Quality {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let lossless = match kind {
        FileType::Flac | FileType::Wav | FileType::Aiff | FileType::WavPack | FileType::Ape => true,
        // ALAC says its bit depth; AAC doesn't.
        FileType::Mp4 => properties.bit_depth().is_some(),
        FileType::Custom(_) => crate::quality::lossless_extension(&extension),
        _ => false,
    };
    let format = match (kind, lossless) {
        (FileType::Mp4, true) => "ALAC".to_string(),
        (FileType::Mp4, false) => "AAC".to_string(),
        _ => extension.to_ascii_uppercase(),
    };
    crate::quality::Quality {
        format,
        lossless,
        bit_depth: properties.bit_depth().map(u32::from),
        sample_rate: properties.sample_rate(),
        bitrate: properties.audio_bitrate().filter(|b| *b > 0),
    }
}

/// The MusicBrainz ids in a tag's values (several values, or one with ids
/// separated by `;`, `/` or NUL, as taggers differ).
fn ids(tag: &Tag, key: ItemKey) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in tag.get_strings(key) {
        for id in mbids(value) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// The UUIDs in `text`, lowercased.
#[must_use]
pub fn mbids(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .filter(|part| is_mbid(part))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Whether `text` is shaped like a MusicBrainz id (a UUID).
#[must_use]
pub fn is_mbid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    text.len() == 36
        && groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ids_however_they_are_separated() {
        let a = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
        let b = "8BFAC288-CCC5-448D-9573-C33EA2AA5C30";
        assert_eq!(mbids(a), [a]);
        assert_eq!(
            mbids(&format!("{a}; {b}")),
            [a.to_string(), b.to_ascii_lowercase()]
        );
        assert_eq!(mbids(&format!("{a}/{b}")).len(), 2);
        assert_eq!(mbids(&format!("{a}\0{b}")).len(), 2);
        assert!(mbids("not an id").is_empty());
        assert!(!is_mbid("a74b1b7f71a540119441d0b5e4122711----"));
    }
}
