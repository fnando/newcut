//! Test helpers: a library of tagged audio files, and an environment for
//! running the newcut binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{ItemKey, Tag};

pub const ARTIST_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
pub const ARTIST_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
pub const VARIOUS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";
pub const GROUP_1: &str = "11111111-0000-4000-8000-000000000001";
pub const GROUP_2: &str = "11111111-0000-4000-8000-000000000002";
pub const GROUP_3: &str = "11111111-0000-4000-8000-000000000003";
pub const GROUP_NEW: &str = "11111111-0000-4000-8000-0000000000ff";
pub const RELEASE_2: &str = "22222222-0000-4000-8000-000000000002";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copy a silent `kind` file (flac, mp3) to `path`, tagged with `tags`.
pub fn audio(path: &Path, kind: &str, tags: &[(ItemKey, &str)]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::copy(fixture(&format!("silence.{kind}")), path).unwrap();
    if tags.is_empty() {
        return;
    }
    let mut file = lofty::read_from_path(path).unwrap();
    let mut tag = Tag::new(file.primary_tag_type());
    for (key, value) in tags {
        tag.insert_text(key.clone(), (*value).to_string());
    }
    file.insert_tag(tag);
    file.save_to_path(path, WriteOptions::default()).unwrap();
}

/// A library: artist A's album (group 1, tagged with its release group), a
/// second album tagged with its release only (release 2 → group 2), a
/// Various Artists compilation, and an untagged folder.
pub fn library(root: &Path) {
    let a_album = |n: u8| -> Vec<(ItemKey, &'static str)> {
        let _ = n;
        vec![
            (ItemKey::AlbumTitle, "First"),
            (ItemKey::AlbumArtist, "Artist A"),
            (ItemKey::MusicBrainzReleaseGroupId, GROUP_1),
            (ItemKey::MusicBrainzReleaseArtistId, ARTIST_A),
        ]
    };
    audio(&root.join("Artist A/First/01.flac"), "flac", &a_album(1));
    audio(&root.join("Artist A/First/02.flac"), "flac", &a_album(2));
    audio(
        &root.join("Artist A/Second/01.mp3"),
        "mp3",
        &[
            (ItemKey::AlbumTitle, "Second"),
            (ItemKey::AlbumArtist, "Artist A"),
            (ItemKey::MusicBrainzReleaseId, RELEASE_2),
            (ItemKey::MusicBrainzReleaseArtistId, ARTIST_A),
        ],
    );
    audio(
        &root.join("Compilations/Hits/01.flac"),
        "flac",
        &[
            (ItemKey::AlbumTitle, "Hits"),
            (ItemKey::AlbumArtist, "Various Artists"),
            (ItemKey::MusicBrainzReleaseArtistId, VARIOUS),
            (ItemKey::MusicBrainzArtistId, ARTIST_B),
        ],
    );
    audio(&root.join("Unknown/track.flac"), "flac", &[]);
    std::fs::write(root.join("Unknown/cover.jpg"), b"not audio").unwrap();
}
