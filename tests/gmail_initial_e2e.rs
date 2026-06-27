#![allow(clippy::unwrap_used)]

//! End-to-end Gmail initial-sync reproduction against the *spawned*
//! binary over real HTTP, mirroring ratatoskr's `gmail-initial` gate:
//!
//!   1. mint an access token via the mock OAuth provider
//!      (`POST /oauth/token`, `grant_type=authorization_code`,
//!      `account_id=account-1`) on the JMAP/admin listener,
//!   2. drive bifrost's Gmail initial-sync endpoint sequence against
//!      the Gmail listener with that bearer: profile -> labels.list ->
//!      messages.list (no `q`) -> per-message `messages.get`
//!      (metadata / raw / full),
//!   3. assert the fixture's two messages come back and every label a
//!      message carries is advertised by `labels.list` (bifrost's
//!      membership-scope resolution invariant).
//!
//! This is the socket-level twin of the `gmail.rs` oneshot replay: it
//! exercises the real listener, the bearer-enforcement middleware, and
//! the real OAuth token mint, which the oneshot path skips.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }
    fn pid(&self) -> u32 {
        self.child.as_ref().expect("child still owned").id()
    }
    fn into_inner(mut self) -> Child {
        self.child.take().expect("child still owned")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_saehrimnir"))
}

fn unique_scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("gmail-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn fixture_path(name: &str) -> PathBuf {
    std::env::current_dir()
        .expect("cwd")
        .join("fixtures")
        .join(name)
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    path.exists()
}

fn parse_port(sentinel: &str, prefix: &str) -> u16 {
    for line in sentinel.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().parse().expect("port parse");
        }
    }
    panic!("no {prefix} line in sentinel:\n{sentinel}");
}

fn send_sigterm(pid: u32) {
    let pid = libc::pid_t::try_from(pid).expect("pid fits in pid_t");
    // SAFETY: SIGTERM to a pid we own.
    unsafe { libc::kill(pid, libc::SIGTERM) };
}

fn get_json(client: &reqwest::blocking::Client, url: &str, bearer: &str) -> (u16, Value) {
    let resp = client
        .get(url)
        .bearer_auth(bearer)
        .timeout(Duration::from_secs(5))
        .send()
        .expect("GET request");
    let status = resp.status().as_u16();
    let json = resp.json().expect("decode JSON");
    (status, json)
}

#[test]
fn gmail_initial_sync_over_http_returns_fixture_mail() {
    let scratch = unique_scratch_dir("initial");
    let ready = scratch.join("ready");

    let child = Command::new(binary())
        .args(["--readiness-file"])
        .arg(&ready)
        .args(["--fixture"])
        .arg(fixture_path("gmail-initial-repro.toml"))
        .args([
            "--jmap-port",
            "0",
            "--imap-port",
            "0",
            "--smtp-port",
            "0",
            "--graph-port",
            "0",
            "--gmail-port",
            "0",
        ])
        .spawn()
        .expect("spawn saehrimnir");
    let guard = ChildGuard::new(child);

    assert!(
        wait_for_file(&ready, Duration::from_secs(5)),
        "sentinel did not appear",
    );
    let sentinel = std::fs::read_to_string(&ready).expect("read sentinel");
    let jmap_port = parse_port(&sentinel, "JMAP ");
    let gmail_port = parse_port(&sentinel, "GMAIL ");

    let client = reqwest::blocking::Client::new();

    // 1. Mint an access token via the mock OAuth provider, exactly as
    //    the gmail-initial gate does (authorization_code + account_id).
    let token_resp = client
        .post(format!("http://127.0.0.1:{jmap_port}/oauth/token"))
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "account_id": "account-1",
            "code": "harness-gmail-initial-account-1",
            "client_id": "ratatoskr-gmail-harness",
            "redirect_uri": "http://127.0.0.1/oauth-callback",
        }))
        .timeout(Duration::from_secs(5))
        .send()
        .expect("POST /oauth/token");
    assert!(token_resp.status().is_success(), "token status");
    let token_json: Value = token_resp.json().expect("token JSON");
    let access_token = token_json["access_token"]
        .as_str()
        .expect("access_token present")
        .to_string();

    let gmail = |path: &str| format!("http://127.0.0.1:{gmail_port}{path}");

    // 2. profile.
    let (status, profile) = get_json(&client, &gmail("/gmail/v1/users/me/profile"), &access_token);
    assert_eq!(status, 200, "profile status");
    assert_eq!(profile["emailAddress"], "test@example.com");
    assert!(
        profile["historyId"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok(),
        "historyId must parse: {:?}",
        profile["historyId"]
    );

    // 3. labels.list -> known membership scopes.
    let (status, labels) = get_json(&client, &gmail("/gmail/v1/users/me/labels"), &access_token);
    assert_eq!(status, 200, "labels status");
    let known: std::collections::BTreeSet<String> = labels["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        known.contains("INBOX") && known.contains("IMPORTANT"),
        "labels: {known:?}"
    );

    // 4. messages.list with no `q` (bifrost's inventory backfill).
    let (status, list) = get_json(
        &client,
        &gmail("/gmail/v1/users/me/messages?maxResults=500"),
        &access_token,
    );
    assert_eq!(status, 200, "messages.list status");
    let stubs = list["messages"].as_array().unwrap();
    assert_eq!(
        stubs.len(),
        2,
        "Gmail initial sync must return the fixture's two messages, got {}: {list}",
        stubs.len()
    );

    // 5. per-message hydration + membership-scope invariant.
    for stub in stubs {
        let id = stub["id"].as_str().unwrap();
        let (status, msg) = get_json(
            &client,
            &gmail(&format!("/gmail/v1/users/me/messages/{id}?format=metadata")),
            &access_token,
        );
        assert_eq!(status, 200, "metadata {id}");
        for label in msg["labelIds"].as_array().unwrap() {
            let label = label.as_str().unwrap();
            assert!(
                known.contains(label),
                "message {id} carries label {label:?} absent from labels.list \
                 (bifrost cannot resolve the membership scope); known: {known:?}"
            );
        }
        let (status, raw) = get_json(
            &client,
            &gmail(&format!("/gmail/v1/users/me/messages/{id}?format=raw")),
            &access_token,
        );
        assert_eq!(status, 200, "raw {id}");
        assert!(raw["raw"].is_string(), "raw bytes for {id}");
    }

    send_sigterm(guard.pid());
    let mut child = guard.into_inner();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}
