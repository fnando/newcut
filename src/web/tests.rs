use super::*;
use axum::body::Body;
use tower::ServiceExt;

use crate::state::library::{NewReleaseGroup, ScannedFile};

const TOKEN: &str = "a-long-enough-test-token";
const GROUP_OWNED: &str = "11111111-0000-4000-8000-000000000001";
const GROUP_MISSING: &str = "11111111-0000-4000-8000-000000000002";
const GROUP_NEW: &str = "11111111-0000-4000-8000-000000000003";
const ARTIST: &str = "aaaaaaaa-0000-4000-8000-000000000001";

struct Setup {
    web: Arc<Web>,
    _dir: tempfile::TempDir,
}

fn setup(token: Option<&str>) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let state = Db::open(&db).unwrap();
    let conn = &state.conn;
    let mut config = format!(
        "version = 1\nlibraries = [{:?}]\n",
        dir.path().display().to_string()
    );
    if let Some(token) = token {
        config.push_str(&format!("[server]\ntoken = {token:?}\n"));
    }
    revisions::add(conn, &config, "file", jiff::Timestamp::now()).unwrap();
    let baseline: jiff::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
    library::upsert_file(
        conn,
        &ScannedFile {
            path: "/music/a/1.flac".into(),
            library: dir.path().display().to_string(),
            dir: "/music/a".into(),
            album: Some("Owned One".into()),
            album_artist: Some("Artist & Co".into()),
            release_group_mbid: Some(GROUP_OWNED.into()),
            artist_mbids: vec![ARTIST.into()],
            ..ScannedFile::default()
        },
        baseline,
    )
    .unwrap();
    library::set_resolved(conn, ARTIST, "Artist & Co", "Artist & Co", baseline).unwrap();
    let group = |mbid: &str, title: &str, date: &str| NewReleaseGroup {
        mbid: mbid.into(),
        title: title.into(),
        artist_credit: Some("Artist & Co".into()),
        primary_type: Some("Album".into()),
        first_release_date: Some(date.into()),
        artist_mbids: vec![ARTIST.into()],
        ..NewReleaseGroup::default()
    };
    library::upsert_release_group(
        conn,
        &group(GROUP_OWNED, "Owned One", "2001"),
        "musicbrainz",
        baseline,
    )
    .unwrap();
    library::upsert_release_group(
        conn,
        &group(GROUP_MISSING, "Missing <Two>", "2005"),
        "musicbrainz",
        baseline,
    )
    .unwrap();
    library::set_checked(conn, ARTIST, baseline, baseline).unwrap();
    let today = jiff::Zoned::now().date().to_string();
    library::upsert_release_group(
        conn,
        &group(GROUP_NEW, "Fresh Three", &today),
        "musicbrainz",
        jiff::Timestamp::now(),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:55460".parse().unwrap();
    let dirs = crate::paths::Dirs {
        home: dir.path().into(),
        config_dir: dir.path().join("config"),
        data_dir: dir.path().into(),
    };
    let server = ServerConfig {
        token: token.map(Into::into),
        ..ServerConfig::default()
    };
    let web = Arc::new(Web::new(
        db,
        dirs,
        dir.path().join("config/config.toml"),
        &server,
        addr,
        Arc::default(),
        crate::shutdown::Shutdown::default(),
    ));
    Setup { web, _dir: dir }
}

fn get(uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .uri(uri)
        .header(header::HOST, "127.0.0.1:55460")
}

fn post(web: &Web, uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:55460")
        .header("x-csrf-token", &web.csrf)
        .body(Body::empty())
        .unwrap()
}

async fn call(web: &Arc<Web>, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = router(web.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn pages_show_whats_owned_missing_and_new() {
    let s = setup(None);
    let (status, _, body) = call(&s.web, get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Fresh Three"), "new on the dashboard");
    assert!(
        !body.contains("Missing &#60;Two&#62;"),
        "old ones aren't new"
    );
    assert!(body.contains("Artist &#38; Co"), "escaped");

    let (_, _, body) = call(&s.web, get("/artists").body(Body::empty()).unwrap()).await;
    assert!(body.contains(&format!("/artists/{ARTIST}")), "{body}");
    assert!(
        body.contains(&format!("hx-post=\"/artists/{ARTIST}/unfollow\"")),
        "unfollow from the list: {body}"
    );

    let (status, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for part in [
        "Owned One",
        "Missing &#60;Two&#62;",
        "Fresh Three",
        "st-owned",
        "st-missing",
        "/music/a",
    ] {
        assert!(body.contains(part), "{part} on the artist page: {body}");
    }
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}?status=owned"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains("Owned One") && !body.contains("Fresh Three"));

    let (status, _, body) = call(&s.web, get("/releases").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("Fresh Three") && body.contains("st-owned"),
        "{body}"
    );
    let (_, _, body) = call(
        &s.web,
        get("/releases?status=new").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(body.contains("Fresh Three"), "{body}");
    assert!(!body.contains("st-owned"), "{body}");
    let (_, _, body) = call(&s.web, get("/missing?q=two").body(Body::empty()).unwrap()).await;
    assert!(
        body.contains("Missing &#60;Two&#62;") && !body.contains("Fresh Three"),
        "{body}"
    );

    let (status, _, _) = call(&s.web, get("/artists/nobody").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, body) = call(&s.web, get("/untagged").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = call(&s.web, get("/config").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Revision 1"));
}

#[tokio::test]
async fn each_artist_has_a_feed() {
    let s = setup(Some(TOKEN));
    let (status, headers, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}/feed.atom"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "open without the token");
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/atom+xml; charset=utf-8"
    );
    assert!(body.contains("new releases by Artist &#38; Co"), "{body}");
    assert!(
        body.contains(&format!("/artists/{ARTIST}/feed.atom")),
        "{body}"
    );
    assert!(body.contains(GROUP_NEW), "{body}");
    let (status, _, _) = call(
        &s.web,
        get("/artists/00000000-0000-0000-0000-000000000000/feed.atom")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ignoring_takes_a_release_out_of_missing_and_the_feed() {
    let s = setup(None);
    let (_, headers, body) = call(&s.web, get("/feed.atom").body(Body::empty()).unwrap()).await;
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/atom+xml; charset=utf-8"
    );
    assert!(
        body.contains(&format!("urn:musicbrainz:release-group:{GROUP_NEW}")),
        "{body}"
    );
    assert!(body.contains("Artist &#38; Co — Fresh Three"), "{body}");
    assert!(!body.contains(GROUP_MISSING), "only new ones");

    let (status, headers, _) = call(
        &s.web,
        post(&s.web, &format!("/release-groups/{GROUP_NEW}/ignore")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["hx-refresh"], "true");
    let (_, _, body) = call(&s.web, get("/feed.atom").body(Body::empty()).unwrap()).await;
    assert!(!body.contains(GROUP_NEW), "{body}");
    let (_, _, body) = call(&s.web, get("/missing").body(Body::empty()).unwrap()).await;
    assert!(!body.contains("Fresh Three"));
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}?status=ignored"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("Fresh Three") && body.contains("Unignore"),
        "{body}"
    );

    // Without the CSRF token: refused.
    let mut request = post(&s.web, &format!("/release-groups/{GROUP_MISSING}/ignore"));
    request.headers_mut().remove("x-csrf-token");
    let (status, _, _) = call(&s.web, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_token_guards_everything_but_the_feed() {
    let s = setup(Some(TOKEN));
    let (status, headers, _) = call(&s.web, get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/login");
    // Feed readers can't sign in: the feed is open.
    let (status, _, _) = call(&s.web, get("/feed.atom").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(
        &s.web,
        get("/")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(&s.web, get("/assets/app.css").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "assets are open");

    let login = Request::builder()
        .method(Method::POST)
        .uri("/login")
        .header(header::HOST, "127.0.0.1:55460")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("token={TOKEN}")))
        .unwrap();
    let (status, headers, _) = call(&s.web, login).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (status, _, _) = call(
        &s.web,
        get("/artists")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn other_hosts_are_refused() {
    let s = setup(None);
    let request = Request::builder()
        .uri("/")
        .header(header::HOST, "evil.example:55460")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = call(&s.web, request).await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn the_config_editor_masks_the_token_and_shows_what_changes() {
    let s = setup(Some(TOKEN));
    let authed = |b: axum::http::request::Builder| {
        b.header(header::HOST, "127.0.0.1:55460")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
    };
    let form = |uri: &str, text: &str, base: i64| {
        authed(Request::builder().method(Method::POST).uri(uri))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("text={}&base={base}", encode(text))))
            .unwrap()
    };
    let page = |uri: &str| {
        authed(Request::builder().uri(uri))
            .body(Body::empty())
            .unwrap()
    };

    let (_, _, body) = call(&s.web, page("/config")).await;
    let masked = crate::config::diff::mask_key(TOKEN);
    assert!(body.contains(&masked) && !body.contains(TOKEN), "{body}");
    let shown = crate::config::diff::mask_toml(
        &crate::config::store::get(&Db::open(&s.web.db).unwrap().conn, 1)
            .unwrap()
            .unwrap()
            .text,
    );

    let (_, _, body) = call(&s.web, form("/config/check", &shown, 1)).await;
    assert!(body.contains("No changes from revision 1"), "{body}");
    let edited = format!("{shown}[schedule]\nartist = \"3d\"\n");
    let (_, _, body) = call(&s.web, form("/config/check", &edited, 1)).await;
    assert!(
        body.contains("schedule.artist: &#34;1w&#34; → &#34;3d&#34;"),
        "{body}"
    );
    assert!(
        body.contains("+artist = &#34;3d&#34;"),
        "a text diff: {body}"
    );
    assert!(!body.contains(TOKEN), "{body}");

    // A stale page doesn't replace a newer revision.
    let (_, _, body) = call(&s.web, form("/config/save", &edited, 0)).await;
    assert!(body.contains("changed since you opened"), "{body}");
    let (status, _, _) = call(&s.web, form("/config/save", &edited, 1)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let written = std::fs::read_to_string(&s.web.config_path).unwrap();
    assert!(
        written.contains(TOKEN) && written.contains("artist = \"3d\""),
        "{written}"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&s.web.config_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let (_, _, body) = call(&s.web, page("/config/revisions/1")).await;
    assert!(
        body.contains("schedule.artist: &#34;3d&#34; → &#34;1w&#34;"),
        "{body}"
    );
    assert!(body.contains(&masked) && !body.contains(TOKEN), "{body}");
}

#[tokio::test]
async fn the_config_editor_checks_and_saves() {
    let s = setup(None);
    let form = |uri: &str, text: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:55460")
            .header("x-csrf-token", &s.web.csrf)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("text={}", encode(text))))
            .unwrap()
    };
    let (_, _, body) = call(&s.web, form("/config/check", "version = 1\nnope = 2\n")).await;
    assert!(
        body.contains("nothing was saved") && body.contains("config:2:1"),
        "{body}"
    );
    let (_, _, body) = call(&s.web, form("/config/save", "version = 3\n")).await;
    assert!(body.contains("nothing was saved"), "{body}");
    let (status, headers, _) = call(
        &s.web,
        form("/config/save", "version = 1\n[server]\nenabled = false\n"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["hx-redirect"], "/config?saved=2");
    let written = std::fs::read_to_string(&s.web.config_path).unwrap();
    assert!(written.contains("enabled = false"));
    let (status, headers, _) = call(&s.web, post(&s.web, "/config/rollback/1")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["hx-redirect"], "/config?saved=3");
    let (_, _, body) = call(
        &s.web,
        get("/config/revisions/3").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(body.contains("rollback to 1"), "{body}");
}

#[tokio::test]
async fn covers_come_from_album_folders_when_asked() {
    let s = setup(None);
    let album = s._dir.path().join("album");
    std::fs::create_dir(&album).unwrap();
    std::fs::write(album.join("Folder.JPG"), b"jpeg bytes").unwrap();
    let caa = format!("https://coverartarchive.org/release-group/{GROUP_OWNED}/front-250");
    {
        let state = Db::open(&s.web.db).unwrap();
        library::upsert_file(
            &state.conn,
            &ScannedFile {
                path: album.join("1.flac").display().to_string(),
                library: s._dir.path().display().to_string(),
                dir: album.display().to_string(),
                release_group_mbid: Some(GROUP_OWNED.into()),
                artist_mbids: vec![ARTIST.into()],
                ..ScannedFile::default()
            },
            jiff::Timestamp::now(),
        )
        .unwrap();
    }
    let uri = format!("/covers/{GROUP_OWNED}");

    // Off by default: the Cover Art Archive.
    let (status, headers, _) = call(&s.web, get(&uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(headers[header::LOCATION], caa.as_str());
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(!body.contains(&uri), "{body}");

    {
        let state = Db::open(&s.web.db).unwrap();
        let config = format!(
            "version = 1\nlibraries = [{:?}]\n[covers]\nlocal = true\n",
            s._dir.path().display().to_string()
        );
        revisions::add(&state.conn, &config, "file", jiff::Timestamp::now()).unwrap();
    }
    let (status, headers, body) = call(&s.web, get(&uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(body, "jpeg bytes");
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains(&uri), "owned albums use it: {body}");
    assert!(
        body.contains(&format!(
            "coverartarchive.org/release-group/{GROUP_MISSING}"
        )),
        "missing ones don't: {body}"
    );

    // No cover file: the Cover Art Archive.
    std::fs::remove_file(album.join("Folder.JPG")).unwrap();
    let (status, headers, _) = call(&s.web, get(&uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(headers[header::LOCATION], caa.as_str());
}

#[tokio::test]
async fn artist_images_come_from_their_folder() {
    let s = setup(None);
    let library_dir = s._dir.path().display().to_string();
    let artist_dir = s._dir.path().join("Artist & Co");
    let album = artist_dir.join("Owned One");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::write(artist_dir.join("Artist.JPG"), b"artist bytes").unwrap();
    // The library's own folder isn't an artist's.
    std::fs::write(s._dir.path().join("artist.jpg"), b"not theirs").unwrap();
    let set_config = |extra: &str| {
        let state = Db::open(&s.web.db).unwrap();
        let config = format!("version = 1\nlibraries = [{library_dir:?}]\n{extra}");
        revisions::add(&state.conn, &config, "file", jiff::Timestamp::now()).unwrap();
    };
    {
        let state = Db::open(&s.web.db).unwrap();
        library::upsert_file(
            &state.conn,
            &ScannedFile {
                path: album.join("1.flac").display().to_string(),
                library: library_dir.clone(),
                dir: album.display().to_string(),
                release_group_mbid: Some(GROUP_OWNED.into()),
                artist_mbids: vec![ARTIST.into()],
                ..ScannedFile::default()
            },
            jiff::Timestamp::now(),
        )
        .unwrap();
        let now = jiff::Timestamp::now();
        library::set_resolved(&state.conn, ARTIST, "Artist & Co", "Artist & Co", now).unwrap();
    }
    let uri = format!("/artists/{ARTIST}/image");

    // Off: their initial.
    let (_, _, body) = call(&s.web, get("/artists").body(Body::empty()).unwrap()).await;
    assert!(!body.contains(&uri), "{body}");
    assert!(body.contains("cover avatar missing-art\">A<"), "{body}");
    let (status, _, _) = call(&s.web, get(&uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    set_config("[covers]\nlocal = true\n");
    let (_, _, body) = call(&s.web, get("/artists").body(Body::empty()).unwrap()).await;
    assert!(body.contains(&uri), "{body}");
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        body.contains(&format!("src=\"{uri}\"")),
        "on their page: {body}"
    );
    let (status, headers, body) = call(&s.web, get(&uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(body, "artist bytes");

    // No image in their folder: none (the page shows their initial).
    std::fs::remove_file(artist_dir.join("Artist.JPG")).unwrap();
    let (status, _, _) = call(&s.web, get(&uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    set_config("");
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("cover avatar large missing-art\">A<"),
        "{body}"
    );
}

#[tokio::test]
async fn the_artist_page_separates_albums_eps_and_singles() {
    let s = setup(None);
    {
        let state = Db::open(&s.web.db).unwrap();
        let config = format!(
            "version = 1\nlibraries = [{:?}]\n[discography]\nprimary_types = [\"Album\", \"EP\", \"Single\"]\n",
            s._dir.path().display().to_string()
        );
        revisions::add(&state.conn, &config, "file", jiff::Timestamp::now()).unwrap();
        let baseline: jiff::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        for (mbid, title, kind) in [
            ("11111111-0000-4000-8000-000000000010", "Small One", "EP"),
            ("11111111-0000-4000-8000-000000000011", "Hit Song", "Single"),
        ] {
            library::upsert_release_group(
                &state.conn,
                &NewReleaseGroup {
                    mbid: mbid.into(),
                    title: title.into(),
                    artist_credit: Some("Artist & Co".into()),
                    primary_type: Some(kind.into()),
                    first_release_date: Some("2010".into()),
                    artist_mbids: vec![ARTIST.into()],
                    ..NewReleaseGroup::default()
                },
                "musicbrainz",
                baseline,
            )
            .unwrap();
        }
    }
    let (status, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let at = |part: &str| body.find(part).unwrap_or_else(|| panic!("{part}: {body}"));
    assert!(at("<h2>Albums · 3</h2>") < at("Owned One"));
    assert!(at("Owned One") < at("<h2>EPs · 1</h2>"));
    assert!(at("<h2>EPs · 1</h2>") < at("Small One"));
    assert!(at("Small One") < at("<h2>Singles · 1</h2>"));
    assert!(at("<h2>Singles · 1</h2>") < at("Hit Song"));
}

#[tokio::test]
async fn joined_artists_are_hidden_and_credited_to_their_members() {
    const JOINED: &str = "bbbbbbbb-0000-4000-8000-000000000001";
    const SOLO: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    const DUO_A: &str = "bbbbbbbb-0000-4000-8000-000000000003";
    const DUO_B: &str = "bbbbbbbb-0000-4000-8000-000000000004";
    const COLLAB: &str = "11111111-0000-4000-8000-000000000020";
    let s = setup(None);
    let solo_dir = s._dir.path().join("Solo");
    let album = solo_dir.join("Collab");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::write(solo_dir.join("artist.jpg"), b"solo's picture").unwrap();
    {
        let state = Db::open(&s.web.db).unwrap();
        let conn = &state.conn;
        let config = format!(
            "version = 1\nlibraries = [{:?}]\n[covers]\nlocal = true\n",
            s._dir.path().display().to_string()
        );
        revisions::add(conn, &config, "file", jiff::Timestamp::now()).unwrap();
        let now = jiff::Timestamp::now();
        library::upsert_file(
            conn,
            &ScannedFile {
                path: album.join("1.flac").display().to_string(),
                library: s._dir.path().display().to_string(),
                dir: album.display().to_string(),
                album: Some("Collab".into()),
                album_artist: Some("Solo, A & B".into()),
                release_group_mbid: Some(COLLAB.into()),
                artist_mbids: vec![SOLO.into(), JOINED.into()],
                ..ScannedFile::default()
            },
            now,
        )
        .unwrap();
        library::set_resolved(conn, SOLO, "Solo", "Solo", now).unwrap();
        library::set_resolved(conn, JOINED, "A & B", "A & B", now).unwrap();
        library::upsert_release_group(
            conn,
            &NewReleaseGroup {
                mbid: COLLAB.into(),
                title: "Collab".into(),
                artist_credit: Some("Solo, A & B".into()),
                primary_type: Some("Album".into()),
                first_release_date: Some("2015".into()),
                artist_mbids: vec![SOLO.into(), JOINED.into()],
                ..NewReleaseGroup::default()
            },
            "musicbrainz",
            now,
        )
        .unwrap();
        library::set_members(
            conn,
            JOINED,
            &[(DUO_A.into(), "A".into()), (DUO_B.into(), "B".into())],
            now,
        )
        .unwrap();
    }
    let page = |uri: String| {
        let web = s.web.clone();
        async move { call(&web, get(&uri).body(Body::empty()).unwrap()).await }
    };

    let (_, _, body) = page("/artists".into()).await;
    assert!(
        !body.contains(&format!("/artists/{JOINED}\"")),
        "hidden: {body}"
    );
    for member in [DUO_A, DUO_B, SOLO] {
        assert!(
            body.contains(&format!("/artists/{member}\"")),
            "{member}: {body}"
        );
    }
    // Each member has the album, as owned.
    let (_, _, body) = page(format!("/artists/{DUO_A}")).await;
    assert!(
        body.contains("Collab") && body.contains("st-owned"),
        "{body}"
    );
    // The joined artist's page says where its releases went.
    let (status, _, body) = page(format!("/artists/{JOINED}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("is a joined artist") && body.contains(&format!("/artists/{DUO_B}")),
        "{body}"
    );
    // A member's picture isn't the folder of someone they appear with.
    let (status, _, _) = page(format!("/artists/{DUO_A}/image")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Untagged files (a backdrop video) don't make it someone else's folder.
    {
        let state = Db::open(&s.web.db).unwrap();
        library::upsert_file(
            &state.conn,
            &ScannedFile {
                path: solo_dir.join("backdrops/theme.mp4").display().to_string(),
                library: s._dir.path().display().to_string(),
                dir: solo_dir.join("backdrops").display().to_string(),
                ..ScannedFile::default()
            },
            jiff::Timestamp::now(),
        )
        .unwrap();
    }
    let (status, _, body) = page(format!("/artists/{SOLO}/image")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "solo's picture");
}

#[test]
fn a_folder_is_whoever_is_on_most_of_every_albums_sampled_tracks() {
    let files = |albums: &[(&str, usize, &[&str])]| -> Vec<(String, Vec<String>)> {
        albums
            .iter()
            .flat_map(|(dir, tracks, artists)| {
                (0..*tracks).map(move |_| {
                    (
                        (*dir).to_string(),
                        artists.iter().map(|a| (*a).to_string()).collect(),
                    )
                })
            })
            .collect()
    };
    // A split with another band is still theirs.
    let have_mercy = files(&[
        ("/m/Have Mercy/A Place of Our Own", 10, &["hm"]),
        ("/m/Have Mercy/Split", 4, &["hm", "somos"]),
    ]);
    assert_eq!(folder_owners(&main_artists(&have_mercy)), ["hm"]);
    // A collab album in someone else's folder doesn't make it theirs.
    let emicida = files(&[
        (
            "/m/Emicida/Doozicabraba",
            12,
            &["emicida", "ksalaam", "beatnick"],
        ),
        ("/m/Emicida/AmarElo", 15, &["emicida"]),
    ]);
    assert_eq!(folder_owners(&main_artists(&emicida)), ["emicida"]);
    // One stray track among the sampled ones is outvoted.
    let mut mostly = files(&[("/m/A/Album", 9, &["a"])]);
    mostly[4].1 = vec!["guest".into()];
    assert_eq!(folder_owners(&main_artists(&mostly)), ["a"]);
    assert!(folder_owners(&main_artists(&[])).is_empty());
}

#[test]
fn a_folder_named_after_an_artist_is_theirs_with_one_of_their_albums_in_it() {
    let artist = |mbid: &str, name: &str| library::Artist {
        mbid: mbid.into(),
        name: name.into(),
        sort_name: None,
        followed: true,
        baseline_at: None,
        last_checked_at: None,
        next_check_at: None,
        last_error: None,
        joined: false,
    };
    let albums = |list: &[&[&str]]| -> Vec<Vec<String>> {
        list.iter()
            .map(|a| a.iter().map(|s| (*s).to_string()).collect())
            .collect()
    };
    // Elvis Costello's albums, with Elvis Costello & The Attractions'.
    let costello = albums(&[&["ec"], &["ec-attractions"], &["ec-attractions"]]);
    let ec = artist("ec", "Elvis Costello");
    let attractions = artist("ec-attractions", "Elvis Costello & The Attractions");
    assert!(is_artist_folder("/m/Elvis Costello", &ec, &costello));
    assert!(!is_artist_folder(
        "/m/Elvis Costello",
        &attractions,
        &costello
    ));
    // Named after them, but none of its albums is theirs.
    let guest = albums(&[&["other"]]);
    assert!(!is_artist_folder("/m/Elvis Costello", &ec, &guest));
    // Not named after anyone: theirs alone.
    let emicida = albums(&[&["emicida", "ksalaam"], &["emicida"]]);
    let ksalaam = artist("ksalaam", "K-Salaam");
    assert!(is_artist_folder(
        "/m/Rapper",
        &artist("emicida", "Emicida"),
        &emicida
    ));
    assert!(!is_artist_folder("/m/Emicida", &ksalaam, &emicida));
}

#[tokio::test]
async fn artists_without_files_can_be_found_and_followed() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    const NEW: &str = "cccccccc-0000-4000-8000-000000000001";
    let mb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ws/2/artist"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mb)
        .await;
    Mock::given(method("GET"))
        .and(path("/ws/js/artist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"gid": NEW, "name": "New <Band>", "sort_name": "New Band", "comment": "from Brazil",
             "typeName": "Group"},
            {"gid": ARTIST, "name": "Artist & Co", "typeName": "Group"},
            {"current": 1, "pages": 1}
        ])))
        .mount(&mb)
        .await;
    let s = setup(None);
    {
        let state = Db::open(&s.web.db).unwrap();
        let config = format!(
            "version = 1\nlibraries = [{:?}]\n[musicbrainz]\nserver = {:?}\n",
            s._dir.path().display().to_string(),
            mb.uri()
        );
        revisions::add(&state.conn, &config, "file", jiff::Timestamp::now()).unwrap();
    }

    let (status, _, body) = call(
        &s.web,
        get("/artists/search?q=band").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("New &#60;Band&#62;") && body.contains("from Brazil"),
        "{body}"
    );
    assert!(
        body.contains("hx-post=\"/artists/add\""),
        "can be followed: {body}"
    );
    assert!(
        body.contains("Following"),
        "the one already followed says so: {body}"
    );

    let add = |form: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/artists/add")
            .header(header::HOST, "127.0.0.1:55460")
            .header("x-csrf-token", &s.web.csrf)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.to_string()))
            .unwrap()
    };
    let (status, headers, _) = call(
        &s.web,
        add(&format!(
            "mbid={NEW}&name=New+%3CBand%3E&sort_name=New+Band"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["hx-redirect"], format!("/artists/{NEW}").as_str());
    let (_, _, body) = call(&s.web, get("/artists").body(Body::empty()).unwrap()).await;
    assert!(
        body.contains(&format!("/artists/{NEW}\"")),
        "listed: {body}"
    );
    {
        let state = Db::open(&s.web.db).unwrap();
        let artist = library::artist(&state.conn, NEW).unwrap().unwrap();
        assert!(artist.followed);
        assert_eq!(library::artists_to_resolve(&state.conn).unwrap(), [NEW]);
    }

    let (status, _, _) = call(&s.web, add("mbid=nope&name=x")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let various = "89ad4ac3-39f7-470e-963a-56509c546377";
    let (status, _, _) = call(&s.web, add(&format!("mbid={various}&name=Various"))).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "special artists aren't followed"
    );
}

#[tokio::test]
async fn missing_albums_can_be_downloaded_through_slskd() {
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let s = setup(None);
    let page = format!("/download/{GROUP_MISSING}");
    let artist_page = || async {
        call(
            &s.web,
            get(&format!("/artists/{ARTIST}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .2
    };

    // Off: titles lead to MusicBrainz, and there's no page.
    assert!(!artist_page().await.contains(&page));
    let (status, _, _) = call(&s.web, get(&page).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let slskd = MockServer::start().await;
    {
        let state = Db::open(&s.web.db).unwrap();
        let config = format!(
            "version = 1\nlibraries = [{:?}]\n[musicbrainz]\nserver = {:?}\n\
             [slskd]\nenabled = true\nserver = {:?}\napi_key = \"k\"\n",
            s._dir.path().display().to_string(),
            slskd.uri(),
            slskd.uri()
        );
        revisions::add(&state.conn, &config, "file", jiff::Timestamp::now()).unwrap();
    }
    let body = artist_page().await;
    assert!(body.contains(&format!("href=\"{page}\"")), "{body}");
    assert!(
        !body.contains(&format!("/download/{GROUP_OWNED}")),
        "owned albums still lead to MusicBrainz"
    );
    let (status, _, body) = call(&s.web, get(&page).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("value=\"Artist Co Missing Two\""), "{body}");

    Mock::given(method("POST"))
        .and(path("/api/v0/searches"))
        .and(body_json(
            serde_json::json!({ "searchText": "Artist Co Missing Two" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "s1" })))
        .mount(&slskd)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v0/searches/s1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "isComplete": true, "responseCount": 1, "fileCount": 2 }),
        ))
        .mount(&slskd)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v0/searches/s1/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "username": "someone", "hasFreeUploadSlot": true, "uploadSpeed": 2000000,
                "files": [
                    { "filename": "music\\Missing Two\\01.flac", "size": 30000000,
                      "bitDepth": 16, "sampleRate": 44100, "length": 200 },
                    { "filename": "music\\Missing Two\\cover.jpg", "size": 1000 }
                ]
            }])),
        )
        .mount(&slskd)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v0/transfers/downloads/someone"))
        .and(body_json(serde_json::json!([
            { "filename": "music\\Missing Two\\01.flac", "size": 30000000 }
        ])))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&slskd)
        .await;

    // The same server stands in for MusicBrainz: one release, one track.
    Mock::given(method("GET"))
        .and(path("/ws/2/release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "release-count": 2,
            "releases": [
                { "id": "rel-digital", "title": "Missing <Two>", "date": "2005", "status": "Official",
                  "media": [{ "position": 1, "format": "Digital Media", "tracks": [
                      { "id": "track-1", "position": 1, "title": "One", "length": 201000,
                        "recording": { "id": "rec-1" } }]}]},
                { "id": "rel-cd", "title": "Missing <Two>", "date": "2005", "status": "Official",
                  "media": [{ "position": 1, "format": "CD", "tracks": [
                      { "id": "t1", "position": 1, "title": "One", "length": 100000 },
                      { "id": "t2", "position": 2, "title": "Two", "length": 100000 }]}]}
            ]
        })))
        .mount(&slskd)
        .await;
    let form = |uri: &str, body: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:55460")
            .header("x-csrf-token", &s.web.csrf)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let (_, _, body) = call(
        &s.web,
        form(&format!("{page}/searches"), "q=Artist+Co+Missing+Two"),
    )
    .await;
    assert!(
        body.contains(&format!("hx-get=\"{page}/searches/s1\"")),
        "{body}"
    );
    let (_, _, body) = call(
        &s.web,
        get(&format!("{page}/searches/s1"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains("1 folder from 1 user"), "{body}");
    assert!(body.contains("FLAC · 16-bit/44.1 kHz"), "{body}");
    assert!(body.contains("1 track<"), "{body}");
    assert!(body.contains("1 exact match"), "{body}");
    assert!(body.contains("Matches Digital Media · 2005"), "{body}");
    assert!(
        body.contains("→ 1. One (3:21)"),
        "the file list says which track: {body}"
    );
    let (_, _, body) = call(
        &s.web,
        form(
            &format!("{page}/searches/s1/queue"),
            "username=someone&dir=music%5CMissing+Two",
        ),
    )
    .await;
    assert!(body.contains("Queued 1 file<"), "{body}");

    // It's remembered, and slskd says how it's going.
    Mock::given(method("GET"))
        .and(path("/api/v0/transfers/downloads/someone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "username": "someone",
            "directories": [{ "directory": "music\\Missing Two", "fileCount": 1, "files": [{
                "filename": "music\\Missing Two\\01.flac", "size": 30000000,
                "state": "InProgress", "bytesTransferred": 15000000,
                "averageSpeed": 1000000.0, "requestedAt": "2026-11-12T10:00:00"
            }]}]
        })))
        .mount(&slskd)
        .await;
    let downloads = || async {
        call(
            &s.web,
            get(&format!("{page}/downloads"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .2
    };
    let body = downloads().await;
    assert!(body.contains("Downloading 0/1 · 50%"), "{body}");
    assert!(body.contains("For Digital Media · 2005"), "{body}");
    {
        let state = Db::open(&s.web.db).unwrap();
        let stored = crate::state::downloads::for_release(&state.conn, GROUP_MISSING).unwrap();
        assert_eq!(
            stored[0].matched.release_mbid.as_deref(),
            Some("rel-digital")
        );
        assert_eq!(stored[0].matched.tracks, [Some("track-1".to_string())]);
    }
    assert!(
        body.contains("hx-trigger=\"load delay:3s\""),
        "polls while it's going: {body}"
    );
    assert!(artist_page().await.contains(">Downloading</a>"));
    // Its row on the artist page, redrawn when the download changes.
    let (_, _, row) = call(
        &s.web,
        get(&format!("/releases/{GROUP_MISSING}/row?artist={ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(row.contains(">Downloading</a>"), "{row}");
    assert!(
        !row.contains("row-sub"),
        "no artist line on their page: {row}"
    );
    assert!(
        row.contains(&format!(
            "hx-get=\"/releases/{GROUP_MISSING}/row?artist={ARTIST}\""
        )),
        "{row}"
    );
    let id: i64 = {
        let state = Db::open(&s.web.db).unwrap();
        crate::state::downloads::for_release(&state.conn, GROUP_MISSING).unwrap()[0].id
    };
    let (status, headers, _) = call(
        &s.web,
        post(&s.web, &format!("{page}/downloads/{id}/forget")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["hx-trigger"], "downloads-changed");
    assert!(!downloads().await.contains("Downloading"));
    assert!(!artist_page().await.contains(">Downloading</a>"));
}

#[tokio::test]
async fn downloads_left_for_you_can_be_tagged_as_a_release() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REL: &str = "22222222-0000-4000-8000-000000000001";
    let s = setup(None);
    let root = s._dir.path();
    let mb = MockServer::start().await;
    {
        let state = Db::open(&s.web.db).unwrap();
        let config = format!(
            "version = 1\nlibraries = [{lib:?}]\n[musicbrainz]\nserver = {mb:?}\n\
             [slskd]\nenabled = true\napi_key = \"k\"\nserver = {mb:?}\n\
             downloads_dir = {dl:?}\noutput_dir = \"{lib}/%{{artist}}/%{{album}}\"\n",
            lib = root.join("music").display().to_string(),
            dl = root.join("slskd").display().to_string(),
            mb = mb.uri(),
        );
        revisions::add(&state.conn, &config, "file", jiff::Timestamp::now()).unwrap();
        crate::state::downloads::add(
            &state.conn,
            &crate::state::downloads::NewDownload {
                release_key: GROUP_MISSING,
                username: "someone",
                dir: "x\\Missing Two",
                files: &[("x\\Missing Two\\01 one.flac".to_string(), 1)],
                matched: &crate::state::downloads::Matched {
                    kind: "none".into(),
                    reason: Some("track 1 is 9:99 here".into()),
                    ..Default::default()
                },
                destination: Some("newcut/abc"),
            },
            jiff::Timestamp::now(),
        )
        .unwrap();
        crate::state::downloads::set_state(
            &state.conn,
            1,
            crate::state::downloads::MANUAL,
            Some("no exact match: track 1 is 9:99 here"),
            None,
            jiff::Timestamp::now(),
        )
        .unwrap();
    }
    let source = root.join("slskd/newcut/abc");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/silence.flac"),
        source.join("01 one.flac"),
    )
    .unwrap();
    let release = serde_json::json!({
        "id": REL, "title": "Missing <Two>", "date": "2005", "status": "Official",
        "artist-credit": [{ "name": "Artist & Co", "joinphrase": "",
                            "artist": { "id": ARTIST, "name": "Artist & Co", "sort-name": "Artist & Co" } }],
        "release-group": { "id": GROUP_MISSING, "primary-type": "Album", "first-release-date": "2005" },
        "media": [{ "position": 1, "format": "CD", "tracks": [
            { "id": "track-1", "position": 1, "title": "One", "length": 1000,
              "recording": { "id": "rec-1" } }]}]
    });
    Mock::given(method("GET"))
        .and(path("/ws/2/release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "release-count": 1, "releases": [release.clone()] }),
        ))
        .mount(&mb)
        .await;
    Mock::given(method("GET"))
        .and(path("/ws/2/release/22222222-0000-4000-8000-000000000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(release))
        .mount(&mb)
        .await;

    let (status, _, body) = call(&s.web, get("/downloads").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Needs you · 1"), "{body}");
    assert!(
        body.contains("no exact match: track 1 is 9:99 here"),
        "{body}"
    );
    let (_, _, body) = call(
        &s.web,
        get("/downloads/1/releases").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(
        body.contains("value=\"22222222-0000-4000-8000-000000000001\""),
        "{body}"
    );
    assert!(body.contains("CD · 2005"), "{body}");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/downloads/1/file")
        .header(header::HOST, "127.0.0.1:55460")
        .header("x-csrf-token", &s.web.csrf)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("release={REL}")))
        .unwrap();
    let (status, headers, _) = call(&s.web, request).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["hx-refresh"], "true");
    let filed = root.join("music/Artist & Co/Missing _Two_/1. One.flac");
    assert!(filed.is_file(), "filed as the release says");
    let tags = crate::scan::tags::read(&filed).unwrap();
    assert_eq!(tags.release_mbid.as_deref(), Some(REL));
    let stored = {
        let state = Db::open(&s.web.db).unwrap();
        crate::state::downloads::get(&state.conn, 1)
            .unwrap()
            .unwrap()
    };
    assert_eq!(stored.state, crate::state::downloads::FILED);
    assert_eq!(stored.matched.kind, "chosen");
    // Its row, redrawn: owned now (no download tag, nothing to follow).
    let (_, _, row) = call(
        &s.web,
        get(&format!("/releases/{GROUP_MISSING}/row"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(row.contains("pill st-owned\">Owned<"), "{row}");
    assert!(!row.contains("download-changed"), "{row}");
    // Its folder was scanned: it's yours now, not at the next full scan.
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}?status=missing"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(!body.contains("Missing &#60;Two&#62;"), "{body}");

    let (status, _, _) = call(&s.web, post(&s.web, "/downloads/1/handled")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, body) = call(&s.web, get("/downloads").body(Body::empty()).unwrap()).await;
    assert!(body.contains("Dealt with · 1"), "{body}");
}

#[tokio::test]
async fn checking_an_artist_scans_their_folders_too() {
    let s = setup(None);
    // An album you just put in their folder (named after them).
    let album = s._dir.path().join("Artist & Co/Missing Two");
    std::fs::create_dir_all(&album).unwrap();
    let file = album.join("01.flac");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/silence.flac"),
        &file,
    )
    .unwrap();
    {
        use lofty::config::WriteOptions;
        use lofty::file::{AudioFile, TaggedFileExt};
        use lofty::tag::{ItemKey, Tag};
        let mut tagged = lofty::read_from_path(&file).unwrap();
        let mut tag = Tag::new(tagged.primary_tag_type());
        tag.insert_text(ItemKey::MusicBrainzReleaseGroupId, GROUP_MISSING.into());
        tag.insert_text(ItemKey::MusicBrainzReleaseArtistId, ARTIST.into());
        tagged.insert_tag(tag);
        tagged.save_to_path(&file, WriteOptions::default()).unwrap();
    }
    let page = || async {
        call(
            &s.web,
            get(&format!("/artists/{ARTIST}?status=missing"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .2
    };
    assert!(page().await.contains("Missing &#60;Two&#62;"));
    let (status, _, _) = call(&s.web, post(&s.web, &format!("/artists/{ARTIST}/refresh"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!page().await.contains("Missing &#60;Two&#62;"), "owned now");
}

#[tokio::test]
async fn ignore_all_missing_is_per_type() {
    let s = setup(None);
    {
        let state = Db::open(&s.web.db).unwrap();
        let baseline: jiff::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        for (mbid, title) in [
            ("11111111-0000-4000-8000-000000000010", "First EP"),
            ("11111111-0000-4000-8000-000000000011", "Second EP"),
        ] {
            let group = NewReleaseGroup {
                mbid: mbid.into(),
                title: title.into(),
                artist_credit: Some("Artist & Co".into()),
                primary_type: Some("EP".into()),
                first_release_date: Some("2003".into()),
                artist_mbids: vec![ARTIST.into()],
                ..NewReleaseGroup::default()
            };
            library::upsert_release_group(&state.conn, &group, "musicbrainz", baseline).unwrap();
        }
    }
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        body.contains(&format!("/artists/{ARTIST}/ignore-missing?type=ep")),
        "{body}"
    );
    assert!(body.contains("Ignore all 2 missing"), "{body}");

    let (status, _, _) = call(
        &s.web,
        post(&s.web, &format!("/artists/{ARTIST}/ignore-missing?type=ep")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, body) = call(
        &s.web,
        get(&format!("/artists/{ARTIST}?status=ignored"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("First EP") && body.contains("Second EP"),
        "{body}"
    );
    assert!(
        !body.contains("Missing &#60;Two&#62;"),
        "albums stay: {body}"
    );
    let (_, _, body) = call(&s.web, get("/missing").body(Body::empty()).unwrap()).await;
    assert!(body.contains("Missing &#60;Two&#62;"), "{body}");
}
