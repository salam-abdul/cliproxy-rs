//! `--working-dir` in the real binary: the directory changes before `.env`, the default
//! config and relative paths are read, and `--log-file` opens before the runtime starts
//! and replaces only stdout, so `logging-to-file` still writes `main.log`. Runs a short
//! import command; no network.
use std::process::Command;

fn scratch(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("cliproxy-{name}-{}-{nanos}", std::process::id()))
}

#[test]
fn working_dir_applies_before_dotenv_config_and_log_paths() {
    let root = scratch("wd");
    let work = root.join("work");
    std::fs::create_dir_all(work.join("auth")).unwrap();
    std::fs::write(
        work.join("config.yaml"),
        "config-version: 8\nport: 0\nauth-dir: ./auth\nobservability:\n  logs:\n    logging-to-file: true\n",
    )
    .unwrap();
    std::fs::write(work.join(".env"), "WRITABLE_PATH=from-dotenv\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cliproxy"))
        .args([
            "--working-dir",
            "work",
            "--log-file",
            "process.log",
            "-vertex-import",
            "missing-key.json",
        ])
        .current_dir(&root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &work)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // Relative to the working directory, not the caller's.
    assert!(work.join("process.log").is_file(), "{stderr}");
    assert!(!root.join("process.log").exists());
    // `.env` from the working directory chose the log directory, the default config
    // there turned on `logging-to-file`, and the import error went to main.log.
    let main_log = std::fs::read_to_string(work.join("from-dotenv/logs/main.log")).unwrap_or_default();
    assert!(
        main_log.contains("missing-key.json"),
        "main.log: {main_log}\nstderr: {stderr}"
    );
    let process_log = std::fs::read_to_string(work.join("process.log")).unwrap();
    assert!(!process_log.contains("missing-key.json"), "{process_log}");
    let _ = std::fs::remove_dir_all(&root);
}

/// `--log-file` opens before the runtime starts, so a warning from sizing it lands in
/// the file and not on stdout, which the Windows Run entry hides.
#[test]
fn a_warning_before_the_runtime_starts_goes_to_the_log_file() {
    let root = scratch("early-log");
    std::fs::create_dir_all(root.join("auth")).unwrap();
    std::fs::write(
        root.join("config.yaml"),
        "config-version: 8\nport: 0\nauth-dir: ./auth\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cliproxy"))
        .args(["--log-file", "process.log", "-vertex-import", "missing-key.json"])
        .current_dir(&root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &root)
        .env("TOKIO_WORKER_THREADS", "none")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let process_log = std::fs::read_to_string(root.join("process.log")).unwrap_or_default();
    assert!(
        process_log.contains("ignoring TOKIO_WORKER_THREADS"),
        "process.log: {process_log}\nstdout: {stdout}"
    );
    assert!(!stdout.contains("TOKIO_WORKER_THREADS"), "{stdout}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_working_dir_fails_before_anything_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_cliproxy"))
        .args(["-working-dir=/nonexistent/cliproxy-wd", "-vertex-import", "x.json"])
        .current_dir(std::env::temp_dir())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("working directory"), "{stderr}");
}

/// The server in `root` with a temp `HOME` and `env`, its output piped. The model catalog
/// refresh goes to a dead loopback proxy.
fn serve(root: &std::path::Path, env: &[(&str, &std::path::Path)]) -> std::process::Child {
    std::fs::create_dir_all(root.join("auth")).unwrap();
    std::fs::write(
        root.join("config.yaml"),
        "config-version: 8\nhost: 127.0.0.1\nport: 0\nauth-dir: ./auth\napi-keys: [fake-client]\n",
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cliproxy"));
    cmd.current_dir(root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "localhost,127.0.0.1,::1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.spawn().unwrap()
}

/// Every output line of `child` until one contains `needle` or 20 seconds pass.
#[cfg(unix)]
fn output_until(child: &mut std::process::Child, needle: &str) -> Vec<String> {
    use std::io::BufRead;
    let (tx, rx) = std::sync::mpsc::channel();
    let streams: [Box<dyn std::io::Read + Send>; 2] = [
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut seen = Vec::new();
    while !seen.iter().any(|line: &String| line.contains(needle)) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => break,
        }
    }
    seen
}

/// Waits up to 20 seconds for `path` while `child` runs.
fn wait_for_file(child: &mut std::process::Child, path: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !path.is_file() && std::time::Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    path.is_file()
}

fn stop(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Starts the server with only `env` beyond a temp `HOME`, waits for `store` to appear
/// and stops it.
fn activity_store_opens_at(root: &std::path::Path, env: &[(&str, &std::path::Path)], store: &std::path::Path) {
    let mut child = serve(root, env);
    let opened = wait_for_file(&mut child, store);
    stop(&mut child);
    let _ = std::fs::remove_dir_all(root);
    assert!(opened, "no activity store at {}", store.display());
}

#[test]
fn without_native_routing_the_activity_store_opens_under_writable_path() {
    let root = scratch("activity-writable");
    let state = root.join("state");
    activity_store_opens_at(&root, &[("WRITABLE_PATH", &state)], &state.join("activity.sqlite"));
}

#[cfg(not(windows))]
#[test]
fn without_native_routing_or_writable_path_the_store_opens_in_the_user_config_folder() {
    let root = scratch("activity-config");
    let config = if cfg!(target_os = "macos") {
        root.join("Library/Application Support")
    } else {
        root.join(".config")
    };
    activity_store_opens_at(&root, &[], &config.join("cpa/activity.sqlite"));
}

/// A second server with the same state folder leaves the store to the first, so the two
/// never write one file.
#[cfg(unix)]
#[test]
fn a_second_server_does_not_open_a_store_another_server_records_in() {
    let root = scratch("activity-owner");
    let state = root.join("state");
    let (first_root, second_root) = (root.join("first"), root.join("second"));
    let mut first = serve(&first_root, &[("WRITABLE_PATH", &state)]);
    let opened = wait_for_file(&mut first, &state.join("activity.sqlite"));
    let mut second = serve(&second_root, &[("WRITABLE_PATH", &state)]);
    let output = output_until(&mut second, "another server records activity");
    stop(&mut second);
    stop(&mut first);
    let _ = std::fs::remove_dir_all(&root);
    assert!(opened, "the first server opened no store");
    let refused = format!(
        "another server records activity at {}",
        state.join("activity.sqlite").display()
    );
    assert!(
        output.iter().any(|line| line.contains(&refused)),
        "{}",
        output.join("\n")
    );
}
