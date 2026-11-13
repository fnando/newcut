//! The newcut binary, end to end: a real library, MusicBrainz and
//! ListenBrainz mocked.

mod common;

use std::path::Path;

use assert_cmd::Command;
use common::*;
use predicates::prelude::*;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, p: &str) -> std::path::PathBuf {
        self.dir.path().join(p)
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("newcut").unwrap();
        cmd.env_clear()
            .env("HOME", self.path("home"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CONFIG_HOME", self.path("xdg-config"))
            .env("NEWCUT_CONFIG_DIR", self.path("config"))
            .env("NEWCUT_LOG", "error");
        cmd
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).assert().success();
        String::from_utf8(out.get_output().stdout.clone()).unwrap()
    }

    fn config(&self, library: &Path, server: &str) {
        let text = format!(
            "version = 1\nlibraries = [{:?}]\n\
             [musicbrainz]\nserver = {server:?}\n\
             [listenbrainz]\nserver = {server:?}\n",
            library.display().to_string()
        );
        std::fs::create_dir_all(self.path("config")).unwrap();
        std::fs::write(self.path("config/config.toml"), text).unwrap();
    }
}

fn group(id: &str, title: &str, date: &str, kind: &str) -> serde_json::Value {
    json!({
        "id": id, "title": title, "primary-type": kind, "secondary-types": [],
        "first-release-date": date,
        "artist-credit": [{"name": "Artist A", "joinphrase": "", "artist": {"id": ARTIST_A}}]
    })
}

async fn mock_musicbrainz(server: &MockServer, groups: Vec<serde_json::Value>) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path(format!("/ws/2/artist/{ARTIST_A}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "Artist A", "sort-name": "A, Artist", "relations": []
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/ws/2/release/{RELEASE_2}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"release-group": {"id": GROUP_2}})),
        )
        .mount(server)
        .await;
    let count = groups.len();
    Mock::given(method("GET"))
        .and(path("/ws/2/release-group"))
        .and(query_param("artist", ARTIST_A))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"release-groups": groups, "release-group-count": count})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/1/explore/fresh-releases/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"payload": {"releases": []}})),
        )
        .mount(server)
        .await;
}

#[test]
fn init_writes_the_config_into_newcut_config_dir() {
    let env = Env::new();
    let out = env.run(&["init", "--library", "/music/flac"]);
    assert!(
        out.contains(&env.path("config/config.toml").display().to_string()),
        "{out}"
    );
    let text = std::fs::read_to_string(env.path("config/config.toml")).unwrap();
    assert!(text.contains("libraries = [\"/music/flac\"]"), "{text}");
    env.cmd()
        .arg("init")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--force"));
    assert_eq!(
        env.run(&["config", "path"]).trim(),
        env.path("config/config.toml").display().to_string()
    );
    env.cmd()
        .args(["config", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("isn't a folder"));
}

#[test]
fn config_mistakes_name_the_line() {
    let env = Env::new();
    std::fs::create_dir_all(env.path("config")).unwrap();
    std::fs::write(
        env.path("config/config.toml"),
        "version = 1\n[schedule]\nscan = \"often\"\n",
    )
    .unwrap();
    env.cmd()
        .args(["config", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("config.toml:3:8"));
    env.cmd().arg("scan").assert().code(2);
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_sync_list_and_find_whats_new() {
    let env = Env::new();
    let music = env.path("music");
    library(&music);
    let server = MockServer::start().await;
    env.config(&music, &server.uri());
    mock_musicbrainz(
        &server,
        vec![
            group(GROUP_1, "First", "2010-02-01", "Album"),
            group(GROUP_2, "Second", "2012", "Album"),
            group(GROUP_3, "Third", "2015-06-01", "Album"),
            group(
                "11111111-0000-4000-8000-0000000000aa",
                "A Single",
                "2016",
                "Single",
            ),
        ],
    )
    .await;

    let out = env.run(&["scan"]);
    assert!(out.contains("Scanned: 5 files; 5 read, 0 gone"), "{out}");
    assert!(out.contains("MusicBrainz: looked up 2"), "{out}");

    let out = env.run(&["sync"]);
    assert!(out.contains("Discographies: checked 1 artist"), "{out}");

    // Third is missing; the single doesn't count; nothing is new (the first
    // check is the baseline).
    let out = env.run(&["missing"]);
    assert!(out.contains("Third"), "{out}");
    assert!(!out.contains("A Single") && !out.contains("First"), "{out}");
    let out = env.run(&["new"]);
    assert_eq!(out.trim(), "Nothing new.");

    let out = env.run(&["artists"]);
    assert!(out.contains("Artist A") && out.contains("yes"), "{out}");

    // A new album reaches MusicBrainz.
    mock_musicbrainz(
        &server,
        vec![
            group(GROUP_1, "First", "2010-02-01", "Album"),
            group(GROUP_2, "Second", "2012", "Album"),
            group(GROUP_3, "Third", "2015-06-01", "Album"),
            group(GROUP_NEW, "Brand New Thing", "2099-01-01", "Album"),
        ],
    )
    .await;
    let out = env.run(&["sync", "--artist", "Artist A"]);
    assert!(out.contains("1 new"), "{out}");
    let out = env.run(&["new", "--json"]);
    let new: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(new.as_array().unwrap().len(), 1, "{out}");
    assert_eq!(new[0]["release_group"], GROUP_NEW);
    assert_eq!(new[0]["status"], "upcoming");

    env.run(&["ignore", GROUP_NEW, GROUP_3]);
    assert_eq!(env.run(&["new"]).trim(), "Nothing new.");
    assert_eq!(env.run(&["missing"]).trim(), "Nothing missing.");
    env.run(&["unignore", GROUP_3]);
    assert!(env.run(&["missing"]).contains("Third"));
    env.cmd().args(["ignore", "nope"]).assert().code(2);

    let out = env.run(&["daemon", "status"]);
    assert!(out.contains("not running"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications_tell_each_channel_once() {
    use wiremock::matchers::body_string_contains;

    let env = Env::new();
    let music = env.path("music");
    library(&music);
    let server = MockServer::start().await;
    env.config(&music, &server.uri());
    let first = vec![
        group(GROUP_1, "First", "2010-02-01", "Album"),
        group(GROUP_2, "Second", "2012", "Album"),
        group(GROUP_3, "Third", "2015-06-01", "Album"),
    ];
    mock_musicbrainz(&server, first.clone()).await;
    env.run(&["scan"]);
    env.run(&["sync"]);

    // Both channels: Pushover (mocked) and a script keeping what it gets.
    let got = env.path("got.jsonl");
    let script = env.path("notify.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat >> {0}\necho >> {0}\n", got.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut text = std::fs::read_to_string(env.path("config/config.toml")).unwrap();
    text.push_str(&format!(
        "[notification.pushover]\napp_api_token = \"$PUSHOVER_TOKEN\"\nuser_key = \"u-key\"\nserver = {:?}\n\
         [notification.command]\nbin = {:?}\n",
        server.uri(),
        script.display().to_string()
    ));
    std::fs::write(env.path("config/config.toml"), text).unwrap();
    let notify = |test: bool| {
        let mut cmd = env.cmd();
        cmd.env("PUSHOVER_TOKEN", "a-token").arg("notify");
        if test {
            cmd.arg("--test");
        }
        let out = cmd.assert().get_output().clone();
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    };
    let pushover = |status: u16, body: serde_json::Value, has: &[&str]| {
        let mut mock = Mock::given(method("POST"))
            .and(path("/1/messages.json"))
            .and(body_string_contains("token=a-token"))
            .and(body_string_contains("user=u-key"));
        for part in has {
            mock = mock.and(body_string_contains(*part));
        }
        mock.respond_with(ResponseTemplate::new(status).set_body_json(body))
    };

    // The first run starts the clock.
    let (code, out) = notify(false);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("pushover: starting from now; command: starting from now"),
        "{out}"
    );

    // One new album: named.
    let mut groups = first.clone();
    groups.push(group(GROUP_NEW, "Brand New Thing", "2099-01-01", "Album"));
    mock_musicbrainz(&server, groups.clone()).await;
    env.run(&["sync", "--artist", "Artist A"]);
    pushover(
        200,
        json!({"status": 1}),
        &[
            "title=New+release%3A+Artist+A",
            "message=Brand+New+Thing+%28Album%29+is+coming+2099-01-01.",
        ],
    )
    .expect(1)
    .mount(&server)
    .await;
    let (code, out) = notify(false);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("pushover: sent 1 release; command: sent 1 release"),
        "{out}"
    );
    let payload: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&got).unwrap().trim()).unwrap();
    assert_eq!(payload["count"], 1);
    assert_eq!(payload["title"], "New release: Artist A");
    assert_eq!(payload["releases"][0]["release_group_mbid"], GROUP_NEW);
    assert_eq!(payload["releases"][0]["upcoming"], true);

    // Told once.
    let (_, out) = notify(false);
    assert!(
        out.contains("pushover: nothing new; command: nothing new"),
        "{out}"
    );

    // Two more while Pushover fails: the command hears of them, Pushover
    // does next time, in general terms.
    groups.push(group(
        "11111111-0000-4000-8000-0000000000b1",
        "Another",
        "2099-02-01",
        "Album",
    ));
    groups.push(group(
        "11111111-0000-4000-8000-0000000000b2",
        "And Another",
        "2099-03-01",
        "Album",
    ));
    mock_musicbrainz(&server, groups).await;
    env.run(&["sync", "--artist", "Artist A"]);
    pushover(
        400,
        json!({"status": 0, "errors": ["user identifier is invalid"]}),
        &[],
    )
    .expect(1)
    .mount(&server)
    .await;
    let (code, out) = notify(false);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("pushover: failed: Pushover answered 400 Bad Request: user identifier is invalid; command: sent 2 releases"),
        "{out}"
    );
    server.reset().await;
    pushover(200, json!({"status": 1}), &["title=2+new+releases"])
        .expect(1)
        .mount(&server)
        .await;
    let (_, out) = notify(false);
    assert!(
        out.contains("pushover: sent 2 releases; command: nothing new"),
        "{out}"
    );

    // A test notification goes through both.
    server.reset().await;
    pushover(200, json!({"status": 1}), &["newcut+can+reach+you"])
        .expect(1)
        .mount(&server)
        .await;
    let (code, out) = notify(true);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("pushover: sent\ncommand: sent"), "{out}");
    assert_eq!(std::fs::read_to_string(&got).unwrap().lines().count(), 3);
}

#[test]
fn the_env_file_feeds_the_config() {
    let env = Env::new();
    env.run(&["init", "--library", "/music"]);
    let file = env.path("config/env");
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode, 0o600, "private from the start");
    let mut text = std::fs::read_to_string(env.path("config/config.toml")).unwrap();
    text = text.replace(
        "listen = \"127.0.0.1:55460\"",
        "listen = \"0.0.0.0:55460\"\ntoken = \"$NEWCUT_TOKEN\"",
    );
    std::fs::write(env.path("config/config.toml"), text).unwrap();

    // Not set: listening beyond this machine without a token is refused.
    env.cmd()
        .args(["config", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("server.token is required"));
    // The default env file sets it.
    std::fs::write(&file, "NEWCUT_TOKEN=from-the-env-file\n").unwrap();
    env.cmd()
        .args(["config", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Read").and(predicate::str::contains("is fine")));
    // Readable by others: a warning.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    env.cmd()
        .args(["config", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("other users can read it"));
    // Another one, with --env-file; one that isn't there is a mistake.
    let other = env.path("other.env");
    std::fs::write(&other, "# comment\nNEWCUT_TOKEN=\"quoted\"\n").unwrap();
    env.cmd()
        .args(["config", "check", "--env-file"])
        .arg(&other)
        .assert()
        .success();
    env.cmd()
        .args(["--env-file=/nowhere/env", "config", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("the env file /nowhere/env"));
}
