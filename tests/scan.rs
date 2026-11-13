//! Scanning a library of real (silent) audio files.

mod common;

use common::*;
use newcut::scan;
use newcut::state::State;
use newcut::state::library;

fn run(state: &mut State, root: &std::path::Path) -> scan::Report {
    scan::scan(
        &mut state.conn,
        &[root.to_path_buf()],
        &|| false,
        &mut |_| {},
    )
    .unwrap()
}

#[test]
fn reads_tags_and_rereads_only_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("music");
    library(&root);
    let mut state = State::open_in_memory().unwrap();

    let report = run(&mut state, &root);
    assert_eq!(
        (report.files, report.read, report.removed, report.unreadable),
        (5, 5, 0, 0)
    );
    // Various Artists isn't an artist to follow; only A is (B is a track
    // artist on a compilation whose album artist is Various Artists).
    assert_eq!(
        library::artists_to_resolve(&state.conn).unwrap(),
        [ARTIST_A]
    );
    assert_eq!(
        library::releases_to_look_up(&state.conn).unwrap(),
        [RELEASE_2]
    );
    let untagged = library::untagged(&state.conn).unwrap();
    let dirs: Vec<_> = untagged
        .iter()
        .map(|u| u.dir.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(dirs, ["Hits", "Unknown"]);
    let owned = library::owned_albums(&state.conn, ARTIST_A).unwrap();
    assert_eq!(owned.len(), 2);
    assert_eq!(owned[0].album.as_deref(), Some("First"));
    assert_eq!(owned[0].tracks, 2);
    assert_eq!(owned[0].release_group_mbid.as_deref(), Some(GROUP_1));

    // Nothing changed: nothing read.
    let report = run(&mut state, &root);
    assert_eq!((report.files, report.read), (5, 0));

    // A file retagged, one gone.
    audio(
        &root.join("Unknown/track.flac"),
        "flac",
        &[
            (lofty::tag::ItemKey::MusicBrainzReleaseGroupId, GROUP_3),
            (lofty::tag::ItemKey::MusicBrainzReleaseArtistId, ARTIST_B),
        ],
    );
    std::fs::remove_file(root.join("Artist A/First/02.flac")).unwrap();
    let report = run(&mut state, &root);
    assert_eq!((report.files, report.read, report.removed), (4, 1, 1));
    assert_eq!(library::untagged(&state.conn).unwrap().len(), 1);
    assert_eq!(library::artists_to_resolve(&state.conn).unwrap().len(), 2);
}

#[test]
fn a_missing_library_keeps_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("music");
    library(&root);
    let mut state = State::open_in_memory().unwrap();
    run(&mut state, &root);
    std::fs::rename(&root, dir.path().join("unmounted")).unwrap();
    let report = run(&mut state, &root);
    assert_eq!(report.missing.len(), 1);
    assert_eq!(report.removed, 0);
    assert_eq!(
        library::owned_albums(&state.conn, ARTIST_A).unwrap().len(),
        2
    );
}

#[test]
fn scans_only_the_folders_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("music");
    library(&root);
    let mut state = State::open_in_memory().unwrap();
    run(&mut state, &root);

    // A new album in Artist A's folder, one of theirs gone; and changes
    // elsewhere that this scan isn't about.
    audio(
        &root.join("Artist A/Third/01.flac"),
        "flac",
        &[
            (lofty::tag::ItemKey::MusicBrainzReleaseGroupId, GROUP_3),
            (lofty::tag::ItemKey::MusicBrainzReleaseArtistId, ARTIST_A),
        ],
    );
    std::fs::remove_file(root.join("Artist A/First/02.flac")).unwrap();
    std::fs::remove_file(root.join("Unknown/track.flac")).unwrap();
    audio(&root.join("Elsewhere/01.flac"), "flac", &[]);

    let report = scan::scan_dirs(&mut state.conn, &root, &[root.join("Artist A")]).unwrap();
    assert_eq!((report.files, report.read, report.removed), (3, 1, 1));
    let albums = library::owned_albums(&state.conn, ARTIST_A).unwrap();
    assert_eq!(albums.len(), 3);
    assert_eq!(
        library::untagged(&state.conn).unwrap().len(),
        2,
        "Unknown's track is still known; Elsewhere isn't yet"
    );
    // Folders outside the library aren't scanned.
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let report = scan::scan_dirs(&mut state.conn, &root, &[outside]).unwrap();
    assert_eq!(report.files, 0);
}

/// Not a test: `NEWCUT_SAMPLE=/dir cargo test --test scan sample -- --ignored`
/// writes a one-album sample library (Radiohead's OK Computer) for trying
/// newcut against the real services.
#[test]
#[ignore = "writes a sample library where NEWCUT_SAMPLE says"]
fn sample() {
    let Ok(dir) = std::env::var("NEWCUT_SAMPLE") else {
        return;
    };
    let tags = [
        (lofty::tag::ItemKey::AlbumTitle, "OK Computer"),
        (lofty::tag::ItemKey::AlbumArtist, "Radiohead"),
        (
            lofty::tag::ItemKey::MusicBrainzReleaseGroupId,
            "b1392450-e666-3926-a536-22c65f834433",
        ),
        (
            lofty::tag::ItemKey::MusicBrainzReleaseArtistId,
            "a74b1b7f-71a5-4011-9441-d0b5e4122711",
        ),
    ];
    audio(
        &std::path::Path::new(&dir).join("Radiohead/OK Computer/01.flac"),
        "flac",
        &tags,
    );
}

#[test]
fn videos_are_not_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("music");
    library(&root);
    // An artist's backdrop video isn't music.
    std::fs::create_dir_all(root.join("Artist A/backdrops")).unwrap();
    std::fs::write(root.join("Artist A/backdrops/backdrop.mp4"), b"not audio").unwrap();
    let mut state = State::open_in_memory().unwrap();

    let report = run(&mut state, &root);
    assert_eq!((report.files, report.unreadable), (5, 0), "{report:?}");
}
