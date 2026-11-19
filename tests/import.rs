//! Filing a download: real files tagged, named and moved.

mod common;

use common::audio;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, ItemKey};
use newcut::import::{Filing, Replacing, file};
use newcut::musicbrainz::{Credit, ReleaseInfo, TrackInfo};

fn release() -> ReleaseInfo {
    let credit = Credit {
        name: "Spanish Love Songs".into(),
        sort: "Spanish Love Songs".into(),
        ids: vec!["5c9dd772-a38f-4873-baa8-4b5d9dff3990".into()],
        names: vec!["Spanish Love Songs".into()],
    };
    let track = |n: u32, title: &str| TrackInfo {
        mbid: format!("track-{n}"),
        recording_mbid: format!("rec-{n}"),
        disc: 1,
        position: n,
        title: title.into(),
        artist: credit.clone(),
        isrcs: vec![format!("ISRC{n}")],
        length_ms: Some(1000),
    };
    ReleaseInfo {
        mbid: "78f1b8bb-8c4a-4599-a4ec-6882e3ffa468".into(),
        title: "Future Nausea".into(),
        artist: credit.clone(),
        date: Some("2026-11-06".into()),
        status: Some("Official".into()),
        label: Some("Pure Noise Records".into()),
        group_mbid: "bee72d15-d0e8-4629-bf43-73726b6da0b9".into(),
        original_date: Some("2026-11-06".into()),
        types: vec!["album".into()],
        media: vec!["Digital Media".into()],
        track_counts: vec![2],
        tracks: vec![track(1, "Future Nausea"), track(2, "Good American")],
        ..ReleaseInfo::default()
    }
}

#[test]
fn tags_names_and_moves_a_download() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("downloads/newcut/abc");
    // As shared: someone else's tags.
    audio(
        &source.join("02 good american.flac"),
        "flac",
        &[
            (ItemKey::TrackTitle, "wrong"),
            (ItemKey::Comment, "ripped by x"),
        ],
    );
    audio(&source.join("01 future nausea.mp3"), "mp3", &[]);
    let files = vec![
        ("@@x\\Album\\01 future nausea.mp3".to_string(), 1),
        ("@@x\\Album\\02 good american.flac".to_string(), 1),
    ];
    let tracks = vec![Some("track-1".to_string()), Some("track-2".to_string())];
    let template = format!(
        "{}/music/%{{artist}}/%{{album}} (%{{year}})",
        dir.path().display()
    );
    let release = release();
    let filing = Filing {
        source: source.clone(),
        files: &files,
        tracks: &tracks,
        release: &release,
        template: &template,
        partial: false,
        replacing: None,
    };
    let album = file(&filing).unwrap();
    assert_eq!(
        album,
        dir.path()
            .join("music/Spanish Love Songs/Future Nausea (2026)")
    );
    assert!(!source.exists(), "the emptied download folder is gone");

    let flac = album.join("2. Good American.flac");
    let read = newcut::scan::tags::read(&flac).unwrap();
    assert_eq!(read.album.as_deref(), Some("Future Nausea"));
    assert_eq!(
        read.release_group_mbid.as_deref(),
        Some("bee72d15-d0e8-4629-bf43-73726b6da0b9")
    );
    assert_eq!(read.artist_mbids, ["5c9dd772-a38f-4873-baa8-4b5d9dff3990"]);
    let tagged = lofty::read_from_path(&flac).unwrap();
    let tag = tagged.primary_tag().unwrap();
    assert_eq!(tag.title().as_deref(), Some("Good American"));
    assert_eq!(tag.track(), Some(2));
    assert_eq!(tag.track_total(), Some(2));
    assert_eq!(tag.get_string(ItemKey::MusicBrainzTrackId), Some("track-2"));
    assert_eq!(
        tag.get_string(ItemKey::MusicBrainzRecordingId),
        Some("rec-2")
    );
    assert_eq!(tag.get_string(ItemKey::Label), Some("Pure Noise Records"));
    assert_eq!(tag.get_string(ItemKey::Comment), None, "old tags are gone");

    let mp3 = album.join("1. Future Nausea.mp3");
    let read = newcut::scan::tags::read(&mp3).unwrap();
    assert_eq!(
        read.release_mbid.as_deref(),
        Some("78f1b8bb-8c4a-4599-a4ec-6882e3ffa468")
    );

    // Filing it again: its place is taken.
    audio(&source.join("01 future nausea.mp3"), "mp3", &[]);
    audio(&source.join("02 good american.flac"), "flac", &[]);
    let error = file(&filing).unwrap_err();
    assert!(error.contains("is there already"), "{error}");
    assert!(
        source.join("01 future nausea.mp3").exists(),
        "nothing moved"
    );

    // A missing file stops it too.
    std::fs::remove_file(source.join("01 future nausea.mp3")).unwrap();
    let error = file(&filing).unwrap_err();
    assert!(error.contains("isn't there"), "{error}");
}

#[test]
fn what_came_of_a_download_can_be_filed_when_you_say_so() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("downloads/newcut/abc");
    audio(&source.join("02 good american.flac"), "flac", &[]);
    let files = vec![
        ("@@x\\Album\\01 future nausea.flac".to_string(), 1),
        ("@@x\\Album\\02 good american.flac".to_string(), 1),
    ];
    let tracks = vec![Some("track-1".to_string()), Some("track-2".to_string())];
    let template = format!("{}/music/%{{album}}", dir.path().display());
    let release = release();
    let mut filing = Filing {
        source: source.clone(),
        files: &files,
        tracks: &tracks,
        release: &release,
        template: &template,
        partial: false,
        replacing: None,
    };
    let error = file(&filing).unwrap_err();
    assert!(error.contains("isn't there"), "{error}");

    filing.partial = true;
    let album = file(&filing).unwrap();
    assert!(album.join("2. Good American.flac").is_file());
    assert!(!album.join("1. Future Nausea.flac").exists());

    // Nothing at all: nothing to file.
    let error = file(&filing).unwrap_err();
    assert!(error.contains("none of its files"), "{error}");
}

#[test]
fn an_upgrade_sets_the_album_it_replaces_aside() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = root.join("downloads/newcut/abc");
    audio(&source.join("01.flac"), "flac", &[]);
    audio(&source.join("02.flac"), "flac", &[]);
    // What you had: MP3s somewhere else, with a cover.
    let old = root.join("music/old/Future Nausea [MP3]");
    audio(&old.join("01 future nausea.mp3"), "mp3", &[]);
    audio(&old.join("02 good american.mp3"), "mp3", &[]);
    std::fs::write(old.join("cover.jpg"), b"jpg").unwrap();
    let files = vec![
        ("@@x\\Album\\01.flac".to_string(), 1),
        ("@@x\\Album\\02.flac".to_string(), 1),
    ];
    let tracks = vec![Some("track-1".to_string()), Some("track-2".to_string())];
    let template = format!("{}/music/%{{album}}", root.display());
    let release = release();
    let replaced = [old.clone()];
    let trash = root.join("replaced/1-rg");
    let filing = Filing {
        source: source.clone(),
        files: &files,
        tracks: &tracks,
        release: &release,
        template: &template,
        partial: false,
        replacing: Some(Replacing {
            dirs: &replaced,
            trash: trash.clone(),
        }),
    };
    let album = file(&filing).unwrap();
    assert!(album.join("1. Future Nausea.flac").is_file());
    assert!(!old.exists(), "the old folder went along");
    let aside = trash.join("Future Nausea [MP3]");
    assert!(aside.join("01 future nausea.mp3").is_file());
    assert!(aside.join("cover.jpg").is_file());
}

#[test]
fn an_upgrade_in_the_same_folder_keeps_the_rest_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = root.join("downloads/newcut/abc");
    audio(&source.join("01.mp3"), "mp3", &[]);
    audio(&source.join("02.mp3"), "mp3", &[]);
    let album = root.join("music/Future Nausea");
    // The same names: a better MP3.
    audio(&album.join("1. Future Nausea.mp3"), "mp3", &[]);
    audio(&album.join("2. Good American.mp3"), "mp3", &[]);
    std::fs::write(album.join("cover.jpg"), b"jpg").unwrap();
    let files = vec![
        ("@@x\\Album\\01.mp3".to_string(), 1),
        ("@@x\\Album\\02.mp3".to_string(), 1),
    ];
    let tracks = vec![Some("track-1".to_string()), Some("track-2".to_string())];
    let template = format!("{}/music/%{{album}}", root.display());
    let release = release();
    let replaced = [album.clone()];
    let trash = root.join("replaced/1-rg");
    let filing = Filing {
        source: source.clone(),
        files: &files,
        tracks: &tracks,
        release: &release,
        template: &template,
        partial: false,
        replacing: Some(Replacing {
            dirs: &replaced,
            trash: trash.clone(),
        }),
    };
    assert_eq!(file(&filing).unwrap(), album);
    assert!(album.join("1. Future Nausea.mp3").is_file());
    assert!(album.join("cover.jpg").is_file(), "the cover stays");
    assert!(trash.join("Future Nausea/1. Future Nausea.mp3").is_file());
    assert!(!source.exists());
}

#[test]
fn an_upgrade_that_cant_be_filed_puts_the_old_files_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = root.join("downloads/newcut/abc");
    audio(&source.join("01.flac"), "flac", &[]);
    audio(&source.join("02.flac"), "flac", &[]);
    let old = root.join("music/old");
    audio(&old.join("1.mp3"), "mp3", &[]);
    // Its place is taken by something that isn't the album replaced.
    let album = root.join("music/Future Nausea");
    audio(&album.join("1. Future Nausea.flac"), "flac", &[]);
    let files = vec![
        ("@@x\\Album\\01.flac".to_string(), 1),
        ("@@x\\Album\\02.flac".to_string(), 1),
    ];
    let tracks = vec![Some("track-1".to_string()), Some("track-2".to_string())];
    let template = format!("{}/music/%{{album}}", root.display());
    let release = release();
    let replaced = [old.clone()];
    let trash = root.join("replaced/1-rg");
    let filing = Filing {
        source: source.clone(),
        files: &files,
        tracks: &tracks,
        release: &release,
        template: &template,
        partial: false,
        replacing: Some(Replacing {
            dirs: &replaced,
            trash: trash.clone(),
        }),
    };
    let error = file(&filing).unwrap_err();
    assert!(error.contains("is there already"), "{error}");
    assert!(old.join("1.mp3").is_file(), "put back");
    assert!(source.join("01.flac").is_file());
    assert!(!trash.exists(), "nothing left in the trash");
}
