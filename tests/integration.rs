//! Integration tests that run against a Docker SSH server.
//!
//! These tests are `#[ignore]`d so that `cargo test` skips them.
//! Run with: `cargo test -- --ignored` (requires Docker container to be running).
//!
//! Start the container:
//!   cd tests/docker && docker compose up -d --build --wait
//! Stop it:
//!   cd tests/docker && docker compose down

use std::io::Write;
use std::path::PathBuf;
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

use sshmux::browser::sftp::SftpState;
use sshmux::browser::ssh::SshBrowserState;
use sshmux::browser::{FileBrowser, SshBrowser};
use sshmux::terminal::EmbeddedTerminal;

// ---------------------------------------------------------------------------
// Test connection parameters
// ---------------------------------------------------------------------------

const SSH_PORT: &str = "2222";
const SSH_USER: &str = "testuser";
const SSH_HOST: &str = "localhost";

fn test_key_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("docker")
        .join("test_key")
}

/// SSH args that use key-based auth and skip host-key checks.
fn ssh_key_args() -> String {
    format!(
        "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {} -p {} {}@{}",
        test_key_path().display(),
        SSH_PORT,
        SSH_USER,
        SSH_HOST
    )
}

/// Host string for sftp/ssh commands using key auth.
/// Format: -o options -i key -P port user@host
fn sftp_key_args() -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-i".into(),
        test_key_path().display().to_string(),
        "-P".into(),
        SSH_PORT.into(),
        format!("{}@{}", SSH_USER, SSH_HOST),
    ]
}

fn ssh_shell_key_args() -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-i".into(),
        test_key_path().display().to_string(),
        "-p".into(),
        SSH_PORT.into(),
        format!("{}@{}", SSH_USER, SSH_HOST),
    ]
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Poll a condition with a timeout. Returns true if the condition was met.
fn wait_for(timeout: Duration, interval: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        thread::sleep(interval);
    }
    false
}

/// Poll browser.tick() until the given state predicate is true, or timeout.
fn wait_sftp_state(
    browser: &mut FileBrowser,
    timeout: Duration,
    pred: impl Fn(&FileBrowser) -> bool,
) -> bool {
    let start = Instant::now();
    let interval = Duration::from_millis(50);
    while start.elapsed() < timeout {
        browser.tick();
        if pred(browser) {
            return true;
        }
        thread::sleep(interval);
    }
    false
}

fn wait_ssh_state(
    browser: &mut SshBrowser,
    timeout: Duration,
    pred: impl Fn(&SshBrowser) -> bool,
) -> bool {
    let start = Instant::now();
    let interval = Duration::from_millis(50);
    while start.elapsed() < timeout {
        browser.tick();
        if pred(browser) {
            return true;
        }
        thread::sleep(interval);
    }
    false
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// SSH config host alias used by SCP tests. The alias is written to
/// `~/.ssh/config` once per process so that `scp sshmux-docker-test:…`
/// resolves to the Docker container with the correct key and port.
const SSH_ALIAS: &str = "sshmux-docker-test";

static INSTALL_SSH_CONFIG: Once = Once::new();

/// Ensure `~/.ssh/config` contains a `Host sshmux-docker-test` entry pointing
/// to the Docker container. The block is appended idempotently (guarded by
/// `Once`) and removed by an atexit handler.
fn ensure_ssh_config_alias() {
    INSTALL_SSH_CONFIG.call_once(|| {
        let ssh_dir = dirs::home_dir().expect("no home dir").join(".ssh");
        let _ = std::fs::create_dir_all(&ssh_dir);
        let config_path = ssh_dir.join("config");

        // Check if alias already present (e.g. from a previous aborted run)
        if let Ok(content) = std::fs::read_to_string(&config_path)
            && content.contains(&format!("Host {SSH_ALIAS}"))
        {
            return;
        }

        let block = format!(
            "\n# --- sshmux integration test (auto-generated, safe to delete) ---\n\
             Host {SSH_ALIAS}\n\
             \x20   HostName {SSH_HOST}\n\
             \x20   Port {SSH_PORT}\n\
             \x20   User {SSH_USER}\n\
             \x20   IdentityFile {key}\n\
             \x20   StrictHostKeyChecking no\n\
             \x20   UserKnownHostsFile /dev/null\n\
             # --- end sshmux integration test ---\n",
            key = test_key_path().display(),
        );

        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config_path)
            .expect("failed to open ~/.ssh/config for append");
        f.write_all(block.as_bytes())
            .expect("failed to write SSH config alias");
    });
}

// ---------------------------------------------------------------------------
// SSH terminal tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn ssh_terminal_connect_and_run_command() {
    let args = ssh_key_args();
    let mut term = EmbeddedTerminal::ssh_raw(24, 80, &args).expect("failed to spawn SSH");

    // Wait for shell prompt
    let got_prompt = wait_for(CONNECT_TIMEOUT, Duration::from_millis(100), || {
        if let Ok(p) = term.parser.lock() {
            let text = p.screen().contents();
            text.contains('$') || text.contains('#') || text.contains('%')
        } else {
            false
        }
    });
    assert!(got_prompt, "timed out waiting for shell prompt");

    // Send a command
    term.send_str("echo INTEGRATION_TEST_OK\r");

    // Wait for output
    let got_output = wait_for(CMD_TIMEOUT, Duration::from_millis(100), || {
        if let Ok(p) = term.parser.lock() {
            p.screen().contents().contains("INTEGRATION_TEST_OK")
        } else {
            false
        }
    });
    assert!(got_output, "did not see command output");
}

#[test]
#[ignore]
fn ssh_terminal_exit_detected() {
    let args = ssh_key_args();
    let mut term = EmbeddedTerminal::ssh_raw(24, 80, &args).expect("failed to spawn SSH");

    // Wait for prompt
    let got_prompt = wait_for(CONNECT_TIMEOUT, Duration::from_millis(100), || {
        if let Ok(p) = term.parser.lock() {
            let text = p.screen().contents();
            text.contains('$') || text.contains('#') || text.contains('%')
        } else {
            false
        }
    });
    assert!(got_prompt, "timed out waiting for shell prompt");

    // Send exit
    term.send_str("exit\r");

    // Wait for process exit
    let exited = wait_for(CMD_TIMEOUT, Duration::from_millis(100), || {
        term.process_exited()
    });
    assert!(exited, "process did not exit after 'exit' command");
}

// ---------------------------------------------------------------------------
// SFTP browser tests
// ---------------------------------------------------------------------------

/// Create a FileBrowser that connects via key-based auth to the Docker container.
fn make_sftp_browser() -> FileBrowser {
    let args = sftp_key_args();
    let mut cmd = portable_pty::CommandBuilder::new("sftp");
    for arg in &args {
        cmd.arg(arg);
    }
    cmd.env("TERM", "dumb");
    let term = EmbeddedTerminal::new(200, 220, cmd, true).expect("failed to spawn SFTP");
    FileBrowser {
        core: sshmux::browser::common::BrowserCore::new("test-docker"),
        sftp: Box::new(term),
        sftp_state: SftpState::Connecting,
    }
}

#[test]
#[ignore]
fn sftp_browser_connects_and_lists() {
    let mut browser = make_sftp_browser();

    // Wait for Idle (fully connected, pwd + ls complete)
    let reached_idle = wait_sftp_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(
        reached_idle,
        "SFTP browser did not reach Idle, stuck in {:?}",
        browser.sftp_state
    );

    // Should have remote entries (home directory listing)
    assert!(
        !browser.core.remote.entries.is_empty(),
        "remote entries should not be empty after ls"
    );

    // Should contain known test directories
    let names: Vec<&str> = browser
        .core
        .remote.entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names.contains(&"documents"),
        "expected 'documents' in listing, got: {:?}",
        names
    );
    assert!(
        names.contains(&"photos"),
        "expected 'photos' in listing, got: {:?}",
        names
    );
}

#[test]
#[ignore]
fn sftp_browser_navigate_into_directory() {
    let mut browser = make_sftp_browser();

    let reached_idle = wait_sftp_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "SFTP browser did not reach Idle");

    // Find "documents" in the listing and select it
    let doc_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "documents")
        .expect("'documents' not found in listing");
    browser.core.remote.sel.select(Some(doc_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Remote;

    // Enter the directory
    browser.enter();

    // Wait for the ls to complete (Idle again)
    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "did not reach Idle after entering directory");

    // Remote path should now include "documents"
    assert!(
        browser.core.remote.path.contains("documents"),
        "remote_path should contain 'documents', got: {}",
        browser.core.remote.path
    );

    // Should see the test files
    let names: Vec<&str> = browser
        .core
        .remote.entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names.contains(&"readme.txt"),
        "expected 'readme.txt' in documents/, got: {:?}",
        names
    );
}

#[test]
#[ignore]
fn sftp_browser_download_file() {
    let mut browser = make_sftp_browser();

    let reached_idle = wait_sftp_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "SFTP browser did not reach Idle");

    // Navigate into "documents"
    let doc_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "documents")
        .expect("'documents' not found");
    browser.core.remote.sel.select(Some(doc_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Remote;
    browser.enter();

    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "did not reach Idle after cd documents");

    // Select "readme.txt"
    let file_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "readme.txt")
        .expect("'readme.txt' not found");
    browser.core.remote.sel.select(Some(file_idx));

    // Set local path to a temp directory for download
    let tmp = std::env::temp_dir().join("sshmux_test_sftp_dl");
    let _ = std::fs::create_dir_all(&tmp);
    let dest = tmp.join("readme.txt");
    let _ = std::fs::remove_file(&dest); // clean any previous run
    browser.core.local.path = tmp.clone();

    // Download
    browser.download();

    // Wait for transfer to complete (back to Idle)
    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(
        reached_idle,
        "did not reach Idle after download, stuck in {:?}",
        browser.sftp_state
    );

    // Verify the file was downloaded
    assert!(dest.exists(), "downloaded file should exist at {:?}", dest);
    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(
        content.contains("hello world"),
        "downloaded file should contain 'hello world', got: {:?}",
        content
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore]
fn sftp_browser_upload_file() {
    let mut browser = make_sftp_browser();

    let reached_idle = wait_sftp_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "SFTP browser did not reach Idle");

    // Create a temp file to upload
    let tmp = std::env::temp_dir().join("sshmux_test_sftp_ul");
    let _ = std::fs::create_dir_all(&tmp);
    let upload_file = tmp.join("upload_test.txt");
    std::fs::write(&upload_file, "uploaded from integration test").unwrap();

    // Set local path to the temp dir
    browser.core.local.path = tmp.clone();
    browser.core.local.entries = sshmux::browser::parse::read_local_dir(&tmp);

    // Select the upload file in the local panel
    let file_idx = browser
        .core
        .local.entries
        .iter()
        .position(|e| e.name == "upload_test.txt")
        .expect("'upload_test.txt' not found in local entries");
    browser.core.local.sel.select(Some(file_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Local;

    // Upload
    browser.upload();

    // Wait for transfer to complete (back to Idle)
    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(
        reached_idle,
        "did not reach Idle after upload, stuck in {:?}",
        browser.sftp_state
    );

    // Verify: the remote listing should now contain "upload_test.txt"
    let names: Vec<&str> = browser
        .core
        .remote.entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names.contains(&"upload_test.txt"),
        "expected 'upload_test.txt' in remote listing after upload, got: {:?}",
        names
    );

    // Cleanup local
    let _ = std::fs::remove_dir_all(&tmp);

    // Cleanup remote: delete the uploaded file
    browser.sftp.send_str("rm upload_test.txt\r\n");
}

#[test]
#[ignore]
fn sftp_browser_go_up() {
    let mut browser = make_sftp_browser();

    let reached_idle = wait_sftp_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "SFTP browser did not reach Idle");

    let home_path = browser.core.remote.path.clone();

    // Navigate into "documents"
    let doc_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "documents")
        .expect("'documents' not found");
    browser.core.remote.sel.select(Some(doc_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Remote;
    browser.enter();

    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "did not reach Idle after cd");

    assert!(browser.core.remote.path.contains("documents"));

    // Go up
    browser.go_up();

    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "did not reach Idle after go_up");

    assert_eq!(
        browser.core.remote.path, home_path,
        "should be back to home directory"
    );
}

// ---------------------------------------------------------------------------
// SCP (SSH) browser tests
// ---------------------------------------------------------------------------

/// Create an SshBrowser that connects via key-based auth to the Docker container.
/// Uses the `sshmux-docker-test` SSH config alias so that SCP transfers also
/// resolve the correct host/port/key.
fn make_ssh_browser() -> SshBrowser {
    ensure_ssh_config_alias();

    let args = ssh_shell_key_args();
    let mut cmd = portable_pty::CommandBuilder::new("ssh");
    cmd.arg("-t");
    for arg in &args {
        cmd.arg(arg);
    }
    cmd.env("TERM", "dumb");
    let term = EmbeddedTerminal::new(200, 220, cmd, true).expect("failed to spawn SSH shell");
    SshBrowser {
        core: sshmux::browser::common::BrowserCore::new(SSH_ALIAS),
        ssh: Box::new(term),
        scp_pty: None,
        ssh_state: SshBrowserState::Connecting,
        saved_password: None,
        password_buf: String::new(),
        waiting_password: false,
        password_prompts_seen: 0,
    }
}

#[test]
#[ignore]
fn ssh_browser_connects_and_lists() {
    let mut browser = make_ssh_browser();

    let reached_idle = wait_ssh_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(
        reached_idle,
        "SSH browser did not reach Idle, stuck in {:?}, status: {}",
        browser.ssh_state, browser.core.status_msg
    );

    assert!(
        !browser.core.remote.entries.is_empty(),
        "remote entries should not be empty after ls"
    );

    let names: Vec<&str> = browser
        .core
        .remote.entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names.contains(&"documents"),
        "expected 'documents' in listing, got: {:?}",
        names
    );
}

#[test]
#[ignore]
fn ssh_browser_navigate_into_directory() {
    let mut browser = make_ssh_browser();

    let reached_idle = wait_ssh_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "SSH browser did not reach Idle");

    // Find "documents" and navigate into it
    let doc_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "documents")
        .expect("'documents' not found");
    browser.core.remote.sel.select(Some(doc_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Remote;
    browser.enter();

    let reached_idle = wait_ssh_state(&mut browser, CMD_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "did not reach Idle after entering directory");

    assert!(
        browser.core.remote.path.contains("documents"),
        "remote_path should contain 'documents', got: {}",
        browser.core.remote.path
    );

    let names: Vec<&str> = browser
        .core
        .remote.entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names.contains(&"readme.txt"),
        "expected 'readme.txt' in documents/, got: {:?}",
        names
    );
}

#[test]
#[ignore]
fn ssh_browser_download_file() {
    let mut browser = make_ssh_browser();

    let reached_idle = wait_ssh_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "SSH browser did not reach Idle");

    // Navigate into documents
    let doc_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "documents")
        .expect("'documents' not found");
    browser.core.remote.sel.select(Some(doc_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Remote;
    browser.enter();

    let reached_idle = wait_ssh_state(&mut browser, CMD_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "did not reach Idle after cd documents");

    // Select readme.txt
    let file_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "readme.txt")
        .expect("'readme.txt' not found");
    browser.core.remote.sel.select(Some(file_idx));

    // Set local download path
    let tmp = std::env::temp_dir().join("sshmux_test_scp_dl");
    let _ = std::fs::create_dir_all(&tmp);
    let dest = tmp.join("readme.txt");
    let _ = std::fs::remove_file(&dest);
    browser.core.local.path = tmp.clone();

    // Download — spawns a separate SCP process that resolves via SSH config alias
    browser.download();

    // Wait for transfer to complete and return to Idle
    let reached_idle = wait_ssh_state(&mut browser, CMD_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(
        reached_idle,
        "did not reach Idle after SCP download, stuck in {:?}, status: {}",
        browser.ssh_state, browser.core.status_msg
    );

    // Verify
    assert!(dest.exists(), "downloaded file should exist at {:?}", dest);
    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(
        content.contains("hello world"),
        "downloaded file content: {:?}",
        content
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore]
fn ssh_browser_upload_file() {
    let mut browser = make_ssh_browser();

    let reached_idle = wait_ssh_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "SSH browser did not reach Idle");

    // Create a temp file to upload
    let tmp = std::env::temp_dir().join("sshmux_test_scp_ul");
    let _ = std::fs::create_dir_all(&tmp);
    let upload_file = tmp.join("scp_upload_test.txt");
    std::fs::write(&upload_file, "uploaded via scp integration test").unwrap();

    browser.core.local.path = tmp.clone();
    browser.core.local.entries = sshmux::browser::parse::read_local_dir(&tmp);

    let file_idx = browser
        .core
        .local.entries
        .iter()
        .position(|e| e.name == "scp_upload_test.txt")
        .expect("'scp_upload_test.txt' not found in local entries");
    browser.core.local.sel.select(Some(file_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Local;

    // Upload — spawns a separate SCP process
    browser.upload();

    // Wait for transfer to complete
    let reached_idle = wait_ssh_state(&mut browser, CMD_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(
        reached_idle,
        "did not reach Idle after SCP upload, stuck in {:?}, status: {}",
        browser.ssh_state, browser.core.status_msg
    );

    // Verify remote listing contains the uploaded file
    let names: Vec<&str> = browser
        .core
        .remote.entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names.contains(&"scp_upload_test.txt"),
        "expected 'scp_upload_test.txt' in remote listing after upload, got: {:?}",
        names
    );

    // Cleanup local
    let _ = std::fs::remove_dir_all(&tmp);

    // Cleanup remote via the SSH shell
    browser.ssh.send_str("rm ~/scp_upload_test.txt\r\n");
}

#[test]
#[ignore]
fn ssh_browser_go_up() {
    let mut browser = make_ssh_browser();

    let reached_idle = wait_ssh_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "SSH browser did not reach Idle");

    let home_path = browser.core.remote.path.clone();

    // Navigate into documents
    let doc_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "documents")
        .expect("'documents' not found");
    browser.core.remote.sel.select(Some(doc_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Remote;
    browser.enter();

    let reached_idle = wait_ssh_state(&mut browser, CMD_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "did not reach Idle after cd");

    // Go up
    browser.go_up();

    let reached_idle = wait_ssh_state(&mut browser, CMD_TIMEOUT, |b| {
        b.ssh_state == SshBrowserState::Idle
    });
    assert!(reached_idle, "did not reach Idle after go_up");

    assert_eq!(
        browser.core.remote.path, home_path,
        "should be back to home directory"
    );
}

// ---------------------------------------------------------------------------
// SFTP delete test
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn sftp_browser_delete_file() {
    let mut browser = make_sftp_browser();

    let reached_idle = wait_sftp_state(&mut browser, CONNECT_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "SFTP browser did not reach Idle");

    // Upload a file to delete
    let tmp = std::env::temp_dir().join("sshmux_test_sftp_del");
    let _ = std::fs::create_dir_all(&tmp);
    let upload_file = tmp.join("to_delete.txt");
    std::fs::write(&upload_file, "delete me").unwrap();

    browser.core.local.path = tmp.clone();
    browser.core.local.entries = sshmux::browser::parse::read_local_dir(&tmp);
    let file_idx = browser
        .core
        .local.entries
        .iter()
        .position(|e| e.name == "to_delete.txt")
        .expect("'to_delete.txt' not found");
    browser.core.local.sel.select(Some(file_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Local;
    browser.upload();

    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "did not reach Idle after upload");

    // Now select the uploaded file on the remote side and delete it
    let file_idx = browser
        .core
        .remote.entries
        .iter()
        .position(|e| e.name == "to_delete.txt")
        .expect("'to_delete.txt' not found on remote after upload");
    browser.core.remote.sel.select(Some(file_idx));
    browser.core.focus = sshmux::browser::common::BrowserFocus::Remote;

    // Initiate delete (sets confirm dialog)
    browser.delete_focused();
    assert!(
        browser.core.delete.confirm.is_some(),
        "confirm dialog should appear"
    );

    // Confirm
    browser.confirm_delete_yes();

    // Wait for delete to complete
    let reached_idle = wait_sftp_state(&mut browser, CMD_TIMEOUT, |b| {
        b.sftp_state == SftpState::Idle
    });
    assert!(reached_idle, "did not reach Idle after delete");

    // Verify it's gone
    let names: Vec<&str> = browser
        .core
        .remote.entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        !names.contains(&"to_delete.txt"),
        "'to_delete.txt' should be gone after delete, got: {:?}",
        names
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
