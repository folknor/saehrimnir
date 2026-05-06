//! End-to-end lifecycle test: spawn the actual binary, wait for the
//! sentinel, exercise a real network request, send SIGTERM, verify
//! a clean exit. Closes the gap that `scripts/smoke.sh` covers
//! manually but nothing in `cargo test` does.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Drop guard that kill -KILLs the child if the test panics before
/// reaching its own SIGTERM. Without this a failing test leaks a
/// listening saehrimnir.
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

fn scratch_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

fn unique_scratch_dir(name: &str) -> PathBuf {
    let dir = scratch_root().join(format!("lifecycle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/jmap-small.toml")
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

fn parse_jmap_port(sentinel: &str) -> u16 {
    for line in sentinel.lines() {
        if let Some(rest) = line.strip_prefix("JMAP ") {
            return rest.trim().parse().expect("JMAP port parse");
        }
    }
    panic!("no JMAP line in sentinel:\n{sentinel}");
}

fn send_sigterm(pid: u32) {
    // SAFETY: SIGTERM to a pid we own. Returns -1 on error which we
    // tolerate; the Drop guard will clean up if the kill failed.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> std::io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::other("child did not exit within timeout"));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[test]
fn binary_boots_writes_sentinel_serves_jmap_and_exits_on_sigterm() {
    let scratch = unique_scratch_dir("happy");
    let ready = scratch.join("ready");

    let child = Command::new(binary())
        .args([
            "--readiness-file",
        ])
        .arg(&ready)
        .args([
            "--fixture",
        ])
        .arg(fixture_path())
        .args([
            "--jmap-port", "0",
            "--imap-port", "0",
            "--smtp-port", "0",
            "--graph-port", "0",
            "--gmail-port", "0",
        ])
        .spawn()
        .expect("spawn saehrimnir");
    let guard = ChildGuard::new(child);

    // Sentinel must appear within a reasonable budget. 5s is generous;
    // typical local boot is under 100ms.
    assert!(
        wait_for_file(&ready, Duration::from_secs(5)),
        "sentinel did not appear",
    );

    let body = std::fs::read_to_string(&ready).expect("read sentinel");
    let jmap_port = parse_jmap_port(&body);
    // All five protocol lines should be present, in order.
    let lines: Vec<&str> = body.lines().collect();
    assert!(lines[0].starts_with("JMAP "), "wrong line 0: {:?}", lines[0]);
    assert!(lines[1].starts_with("IMAP "), "wrong line 1: {:?}", lines[1]);
    assert!(lines[2].starts_with("SMTP "), "wrong line 2: {:?}", lines[2]);
    assert!(lines[3].starts_with("GRAPH "), "wrong line 3: {:?}", lines[3]);
    assert!(lines[4].starts_with("GMAIL "), "wrong line 4: {:?}", lines[4]);

    // Hit a real network endpoint to confirm the JMAP listener is
    // actually accepting connections (sentinel only proves bind, not
    // serve).
    let url = format!("http://127.0.0.1:{jmap_port}/jmap/session");
    let resp = reqwest::blocking::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .expect("GET /jmap/session");
    assert!(resp.status().is_success(), "status: {}", resp.status());
    let json: serde_json::Value = resp.json().expect("decode JSON");
    let caps = json["capabilities"].as_object().expect("capabilities");
    assert!(caps.contains_key("urn:ietf:params:jmap:core"));
    assert!(caps.contains_key("urn:ietf:params:jmap:mail"));

    // SIGTERM and verify a clean exit within saehrimnir's documented
    // 1s shutdown budget plus a small slack for process teardown.
    let pid = guard.pid();
    send_sigterm(pid);
    let child = guard.into_inner();
    let status = wait_with_timeout(child, Duration::from_secs(3)).expect("wait child");
    assert!(status.success(), "non-zero exit: {status:?}");

    // Cleanup: the scratch dir is under target/tmp so we leave it for
    // post-mortem if the test failed - cargo cleans up target/ on
    // `cargo clean`. The next test invocation gets a fresh dir
    // because unique_scratch_dir starts with rm -rf.
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn binary_serves_lua_fixture_identically() {
    // Sanity: the same lifecycle works against the Lua fixture, not
    // just the TOML one. Catches regressions where the Lua loader
    // path breaks the boot sequence.
    let scratch = unique_scratch_dir("lua");
    let ready = scratch.join("ready");
    let lua_fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/jmap-small.lua");

    let child = Command::new(binary())
        .args(["--readiness-file"])
        .arg(&ready)
        .args(["--fixture"])
        .arg(&lua_fixture)
        .args([
            "--jmap-port", "0",
            "--imap-port", "0",
            "--smtp-port", "0",
            "--graph-port", "0",
            "--gmail-port", "0",
        ])
        .spawn()
        .expect("spawn saehrimnir");
    let guard = ChildGuard::new(child);

    assert!(
        wait_for_file(&ready, Duration::from_secs(5)),
        "sentinel did not appear",
    );
    let body = std::fs::read_to_string(&ready).expect("read sentinel");
    let jmap_port = parse_jmap_port(&body);

    let url = format!("http://127.0.0.1:{jmap_port}/jmap/session");
    let resp = reqwest::blocking::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .expect("GET /jmap/session");
    assert!(resp.status().is_success());

    send_sigterm(guard.pid());
    let child = guard.into_inner();
    let status = wait_with_timeout(child, Duration::from_secs(3)).expect("wait child");
    assert!(status.success(), "non-zero exit: {status:?}");

    let _ = std::fs::remove_dir_all(&scratch);
}
