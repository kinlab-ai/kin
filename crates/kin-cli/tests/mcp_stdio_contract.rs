// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The MCP stdio contract with no daemon anywhere (FIR-2316).
//!
//! `kin mcp start` used to await its repo daemon binding before reading a
//! byte of stdio, so a cold flagship-scale start meant minutes of complete
//! silence against handshake probes budgeted in seconds, without even the
//! `initialize` response. These tests drive the real binary with the update
//! watchdog's exact four-frame session and no daemon at all (`KIN_NO_DAEMON`
//! plus an isolated registry guarantee nothing can be spawned or found), and
//! require every frame answered under a tight bound. The slow-bind case, a
//! binding that stays pending while frames arrive, is covered
//! deterministically in `kin-mcp`'s server tests; what this binary-level test
//! pins is that the served transport never needs a daemon to hold the
//! handshake contract.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

mod common;

/// Wall-clock bound for the whole four-frame session. Orders of magnitude
/// above the millisecond-scale expectation, and far below the minutes a
/// daemon-coupled handshake stalls for: a regression to the pre-loop await
/// shape fails this bound, it does not merely slow the suite.
const FOUR_FRAME_BOUND: Duration = Duration::from_secs(30);

/// The update watchdog's probe session: initialize, the initialized
/// notification, tools/list, and one graph-backed tools/call.
fn probe_frames() -> String {
    [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"kin-mcp-contract-test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"kin_graph_status","arguments":{}}}"#,
    ]
    .join("\n")
        + "\n"
}

struct McpRun {
    responses: Vec<serde_json::Value>,
    stderr: String,
    elapsed: Duration,
}

/// Run `kin mcp start` in `dir` with a fully controlled environment, feed it
/// the probe session, and collect every stdout response under the bound.
///
/// The environment is cleared and rebuilt rather than scrubbed: an inherited
/// `KIN_DAEMON_URL` or `KIN_MCP_REPO` from the developer's shell would change
/// what this test proves, and `KIN_NO_DAEMON=1` plus an isolated registry and
/// HOME guarantee no daemon can be spawned or discovered.
fn run_probe_session(dir: &Path, home: &Path) -> McpRun {
    run_configured_probe_session(dir, home, &[], &[("KIN_NO_DAEMON", "1".into())])
}

/// [`run_probe_session`] with the no-spawn contract carried explicitly by the
/// caller (as `--no-spawn`, as `KIN_NO_DAEMON`, or deliberately as neither),
/// plus whatever extra environment the fixture needs (`KIN_DAEMON_URL`,
/// `KIN_DAEMON_BIN`). The spawn-control tests need all three shapes, and a
/// helper that always injected the env var could never run its own positive
/// control.
fn run_configured_probe_session(
    dir: &Path,
    home: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, std::ffi::OsString)],
) -> McpRun {
    let started = Instant::now();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kin"));
    command
        .args(["mcp", "start"])
        .args(extra_args)
        .current_dir(dir)
        .env_clear()
        // Set this after clearing the environment: assertions read machine codes.
        .env("KIN_MCP_MACHINE_REPLIES", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TMPDIR", std::env::temp_dir())
        .env("KIN_VFS_DISABLE", "1")
        .env("KIN_REGISTRY_PATH", home.join("registry.toml"));
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kin mcp start");

    // Write the whole session and close stdin, exactly as the watchdog's
    // heredoc does; the server answers each frame and exits at EOF.
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(probe_frames().as_bytes())
        .expect("write probe frames");

    // Drain both pipes concurrently: tools/list alone can exceed the pipe
    // buffer, and a reader that waits for exit first deadlocks against a
    // writer blocked on a full pipe.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let stdout_reader = std::thread::spawn(move || {
        let mut collected = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut collected);
        collected
    });
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut collected = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut collected);
        collected
    });

    let status = wait_bounded(&mut child, started + FOUR_FRAME_BOUND);
    let stdout = stdout_reader.join().expect("join stdout reader");
    let stderr =
        String::from_utf8_lossy(&stderr_reader.join().expect("join stderr reader")).into_owned();
    let elapsed = started.elapsed();

    let status = status.unwrap_or_else(|| {
        panic!(
            "kin mcp start answered nothing within {FOUR_FRAME_BOUND:?}: the stdio loop is \
             blocked behind the daemon binding again (FIR-2316); stderr:\n{stderr}"
        )
    });
    assert!(
        status.success(),
        "kin mcp start exited with {status}; stderr:\n{stderr}"
    );

    let responses = String::from_utf8(stdout)
        .expect("stdout is UTF-8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("stdout lines are JSON-RPC"))
        .collect();
    McpRun {
        responses,
        stderr,
        elapsed,
    }
}

/// Wait for the child under a deadline; kill it at expiry and return `None`.
fn wait_bounded(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().expect("poll kin mcp start") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn response_with_id(run: &McpRun, id: u64) -> &serde_json::Value {
    run.responses
        .iter()
        .find(|value| value.get("id").and_then(|v| v.as_u64()) == Some(id))
        .unwrap_or_else(|| {
            panic!(
                "no response with id {id}; got {:#?}; stderr:\n{}",
                run.responses, run.stderr
            )
        })
}

fn assert_four_frame_protocol(run: &McpRun) {
    assert!(
        run.elapsed < FOUR_FRAME_BOUND,
        "the probe session took {:?} against a {FOUR_FRAME_BOUND:?} bound",
        run.elapsed
    );
    let initialize = response_with_id(run, 1);
    assert!(
        initialize.pointer("/result/serverInfo/name").is_some(),
        "initialize must answer with server info: {initialize:#?}"
    );
    let tools = response_with_id(run, 2);
    let tool_count = tools
        .pointer("/result/tools")
        .and_then(|tools| tools.as_array())
        .map(|tools| tools.len())
        .unwrap_or(0);
    assert!(
        tool_count > 0,
        "tools/list must serve the tool surface: {tools:#?}"
    );
}

fn assert_four_frame_contract(run: &McpRun) {
    assert_four_frame_protocol(run);

    // The graph-backed call is answered with a structured error naming the
    // situation, never with silence or a dead process.
    let call = response_with_id(run, 3);
    let content = call
        .pointer("/result/content/0/text")
        .and_then(|text| text.as_str())
        .unwrap_or_else(|| panic!("tools/call must carry a content block: {call:#?}"));
    assert_eq!(
        call.pointer("/result/isError").and_then(|v| v.as_bool()),
        Some(true),
        "with no daemon anywhere the graph call is an error result: {call:#?}; stderr:\n{}",
        run.stderr
    );
    assert!(
        content.contains("daemon") || content.contains("repository"),
        "the error must name what is missing: {content}"
    );
}

/// The global-entry shape: launched from a directory that is no repository.
#[test]
fn the_four_frame_probe_is_answered_with_no_repository_and_no_daemon() {
    let root = tempfile::tempdir().expect("temp root");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let plain = root.path().join("not-a-repo");
    std::fs::create_dir_all(&plain).expect("create launch dir");

    let run = run_probe_session(&plain, &home);
    assert_four_frame_contract(&run);
}

/// The watchdog-probe shape: launched inside a real Kin repository whose
/// daemon does not exist and may not be spawned.
#[test]
fn the_four_frame_probe_is_answered_inside_a_repository_whose_daemon_is_absent() {
    let root = tempfile::tempdir().expect("temp root");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    // An empty directory starts an unborn Kin-native repository with
    // nothing to take in, so no daemon is needed to finish it.
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let init = common::Command::new(env!("CARGO_BIN_EXE_kin"))
        .arg("init")
        .arg(&repo)
        .env("HOME", &home)
        .env("KIN_NO_DAEMON", "1")
        .env("KIN_REGISTRY_PATH", home.join("registry.toml"))
        .output()
        .expect("run kin init");
    assert!(
        init.status.success(),
        "kin init failed: stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );

    let run = run_probe_session(&repo, &home);
    assert_four_frame_contract(&run);
}

/// The boot-incident shape (FIR-2341), replayed: a repository whose daemon
/// died with its pid/port record left behind, probed by a session bound to the
/// dead daemon's URL. Before the no-spawn contract covered revival, this exact
/// session started a fresh daemon from inside the probe, the mechanism behind
/// the 2026-08-15 boot spawn storm.
///
/// The negative control preserves the dead endpoint and leaves actors absent.
/// The unrestricted control must revive the admitted real daemon and answer
/// for the repository UUID emitted by initialization. Both sessions run inside
/// the existing owned runtime containment, including cleanup on assertion failure.
#[cfg(unix)]
#[test]
fn a_no_spawn_probe_over_a_dead_daemon_record_spawns_nothing() {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::PermissionsExt as _;

    struct DeadPortReservation {
        _socket: std::fs::File,
        port: u16,
    }

    fn reserve_dead_port() -> DeadPortReservation {
        // SAFETY: socket has no pointer arguments; the returned descriptor is
        // immediately transferred to the RAII owner below.
        let descriptor = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(
            descriptor >= 0,
            "create reserved socket: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a successful socket call returned this owned descriptor.
        let socket = unsafe { std::fs::File::from_raw_fd(descriptor) };
        // SAFETY: the retained file owns a live descriptor for both fcntl calls.
        let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "inspect reserved socket flags");
        // SAFETY: this sets descriptor flags without transferring ownership.
        assert_eq!(
            unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) },
            0
        );
        // SAFETY: sockaddr_in is a plain C data structure with valid zero fields.
        let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        address.sin_family = libc::AF_INET as libc::sa_family_t;
        address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        #[cfg(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        {
            address.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        }
        // SAFETY: address is aligned and remains live for the supplied length.
        assert_eq!(
            unsafe {
                libc::bind(
                    socket.as_raw_fd(),
                    (&address as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            },
            0,
            "reserve dead loopback port: {}",
            std::io::Error::last_os_error()
        );
        let mut length = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: address and length are writable buffers of the declared sizes.
        assert_eq!(
            unsafe {
                libc::getsockname(
                    socket.as_raw_fd(),
                    (&mut address as *mut libc::sockaddr_in).cast(),
                    &mut length,
                )
            },
            0,
            "read reserved port"
        );
        DeadPortReservation {
            _socket: socket,
            port: u16::from_be(address.sin_port),
        }
    }

    fn run_owned_probe_session(
        runtime: &common::IsolatedDaemonRuntime,
        repository: &Path,
        captures: &Path,
        label: &str,
        extra_args: &[&str],
        dead_port: u16,
    ) -> McpRun {
        let input_path = captures.join(format!("{label}.stdin"));
        let stdout_path = captures.join(format!("{label}.stdout"));
        let stderr_path = captures.join(format!("{label}.stderr"));
        std::fs::write(&input_path, probe_frames()).expect("write owned probe frames");
        let started = Instant::now();
        let mut child = runtime
            .kin_command()
            .args(["mcp", "start"])
            .args(extra_args)
            .current_dir(repository)
            .env("KIN_MCP_MACHINE_REPLIES", "1")
            .env("KIN_RUNTIME_MODE", "development")
            .env_remove("KIN_NO_DAEMON")
            .env("KIN_DAEMON_URL", format!("http://127.0.0.1:{dead_port}"))
            .stdin(Stdio::from(
                std::fs::File::open(&input_path).expect("open probe frames"),
            ))
            .stdout(Stdio::from(
                std::fs::File::create(&stdout_path).expect("capture probe stdout"),
            ))
            .stderr(Stdio::from(
                std::fs::File::create(&stderr_path).expect("capture probe stderr"),
            ))
            .spawn_owned()
            .expect("spawn contained MCP probe");
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll owned MCP probe") {
                break status;
            }
            assert!(
                started.elapsed() < FOUR_FRAME_BOUND,
                "contained MCP probe {label} exceeded {FOUR_FRAME_BOUND:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let stderr = std::fs::read_to_string(&stderr_path).expect("read probe stderr");
        assert!(
            status.success(),
            "contained MCP probe {label} exited with {status}; stderr:\n{stderr}"
        );
        let responses = std::fs::read_to_string(&stdout_path)
            .expect("read probe stdout")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("stdout lines are JSON-RPC"))
            .collect();
        McpRun {
            responses,
            stderr,
            elapsed: started.elapsed(),
        }
    }

    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    let environment = runtime.kin_command();
    let home = std::path::PathBuf::from(
        environment
            .configured_env_for_test(std::ffi::OsStr::new("HOME"))
            .flatten()
            .expect("owned HOME"),
    );
    let kin_home = std::path::PathBuf::from(
        environment
            .configured_env_for_test(std::ffi::OsStr::new("KIN_HOME"))
            .flatten()
            .expect("owned Kin home"),
    );
    drop(environment);
    std::fs::create_dir_all(&home).expect("create owned configuration home");
    std::fs::set_permissions(&kin_home, std::fs::Permissions::from_mode(0o700))
        .expect("secure isolated Kin home");

    let init = runtime
        .kin_command()
        .args(["init", "--no-enrich", "--json"])
        .arg(&repo)
        .env("KIN_RUNTIME_MODE", "development")
        .env("KIN_NO_DAEMON", "1")
        .output()
        .expect("run kin init");
    assert!(
        init.status.success(),
        "kin init failed: stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    let initialized: serde_json::Value =
        serde_json::from_slice(&init.stdout).unwrap_or_else(|error| {
            panic!(
                "init JSON failed: {error}; status={} stdout={} stderr={}",
                init.status,
                String::from_utf8_lossy(&init.stdout),
                String::from_utf8_lossy(&init.stderr)
            )
        });
    assert_eq!(initialized["schema"], "kin.init-result.v6");
    let intended_repository = initialized["repository_id"]
        .as_str()
        .expect("init repository UUID")
        .to_string();
    uuid::Uuid::parse_str(&intended_repository).expect("init names a repository UUID");

    let dead_pid = {
        let mut child = runtime
            .process_command_for_test("/usr/bin/true")
            .spawn_owned()
            .or_else(|_| runtime.process_command_for_test("/bin/true").spawn_owned())
            .expect("spawn short-lived owned child");
        let pid = child.id();
        child.wait().expect("reap short-lived owned child");
        pid
    };
    let dead_endpoint = reserve_dead_port();
    let dead_port = dead_endpoint.port;
    let kin_dir = runtime.layout().root().to_path_buf();
    let supervisor_services = runtime
        .registry_path()
        .parent()
        .expect("owned registry parent")
        .join("services");
    assert!(
        !kin_dir.join(kin_daemon_spawn::PID_FILE_NAME).exists(),
        "preparation must not leave a daemon PID before injecting the dead record"
    );
    assert!(
        !kin_dir.join(kin_daemon_spawn::PORT_FILE_NAME).exists(),
        "preparation must not leave a daemon port before injecting the dead record"
    );
    assert!(
        !kin_home.join("services").exists(),
        "preparation must not start services in the owned Kin home"
    );
    assert!(
        !supervisor_services.exists(),
        "preparation must not start supervisor services beside the owned registry"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", dead_port)).is_err(),
        "the retained non-listening socket must not accept connections"
    );
    let stale_pid = dead_pid.to_string();
    let stale_port = dead_port.to_string();
    std::fs::write(kin_dir.join(kin_daemon_spawn::PID_FILE_NAME), &stale_pid)
        .expect("plant stale PID record");
    std::fs::write(kin_dir.join(kin_daemon_spawn::PORT_FILE_NAME), &stale_port)
        .expect("plant stale port record");

    let no_spawn = run_owned_probe_session(
        &runtime,
        &repo,
        root.path(),
        "no-spawn",
        &["--no-spawn"],
        dead_port,
    );
    assert_four_frame_contract(&no_spawn);
    let refusal = response_with_id(&no_spawn, 3)
        .pointer("/result/content/0/text")
        .and_then(|text| text.as_str())
        .expect("tools/call carries a content block");
    assert!(
        refusal.contains("KIN_NO_DAEMON"),
        "the no-spawn refusal must reach the tool answer: {refusal}"
    );
    assert_eq!(
        std::fs::read_to_string(kin_dir.join(kin_daemon_spawn::PID_FILE_NAME)).unwrap(),
        stale_pid,
        "no-spawn must preserve the dead PID record"
    );
    assert_eq!(
        std::fs::read_to_string(kin_dir.join(kin_daemon_spawn::PORT_FILE_NAME)).unwrap(),
        stale_port,
        "no-spawn must preserve the dead port record"
    );
    assert!(!kin_home.join("services").exists());
    assert!(
        !supervisor_services.exists(),
        "no-spawn must leave supervisor actors absent"
    );
    assert!(std::net::TcpStream::connect(("127.0.0.1", dead_port)).is_err());

    // The unrestricted control starts from absence after the no-spawn control
    // proved these deliberately incomplete records were preserved.
    let fabricated_pid = kin_dir.join(kin_daemon_spawn::PID_FILE_NAME);
    let fabricated_port = kin_dir.join(kin_daemon_spawn::PORT_FILE_NAME);
    assert_eq!(
        std::fs::read_to_string(&fabricated_pid).unwrap(),
        stale_pid,
        "discard only the test's unchanged fabricated PID record"
    );
    assert_eq!(
        std::fs::read_to_string(&fabricated_port).unwrap(),
        stale_port,
        "discard only the test's unchanged fabricated port record"
    );
    std::fs::remove_file(&fabricated_pid).expect("discard fabricated PID record");
    std::fs::remove_file(&fabricated_port).expect("discard fabricated port record");
    assert!(!fabricated_pid.exists());
    assert!(!fabricated_port.exists());

    let unrestricted =
        run_owned_probe_session(&runtime, &repo, root.path(), "unrestricted", &[], dead_port);
    assert_four_frame_protocol(&unrestricted);
    let call = response_with_id(&unrestricted, 3);
    assert_ne!(
        call.pointer("/result/isError")
            .and_then(|value| value.as_bool()),
        Some(true),
        "unrestricted graph call must succeed: {call}; stderr:\n{}",
        unrestricted.stderr
    );
    let graph_text = call
        .pointer("/result/content/0/text")
        .and_then(|text| text.as_str())
        .expect("successful graph reply carries JSON text");
    let graph: serde_json::Value = serde_json::from_str(graph_text).expect("graph reply is JSON");
    assert_eq!(
        graph
            .pointer("/repository_base/context/repository_id")
            .and_then(|value| value.as_str()),
        Some(intended_repository.as_str()),
        "unrestricted graph reply must identify the initialized repository: {graph}"
    );

    let pid: u32 = std::fs::read_to_string(kin_dir.join(kin_daemon_spawn::PID_FILE_NAME))
        .expect("real daemon PID published")
        .trim()
        .parse()
        .expect("real daemon PID");
    let port: u16 = std::fs::read_to_string(kin_dir.join(kin_daemon_spawn::PORT_FILE_NAME))
        .expect("real daemon port published")
        .trim()
        .parse()
        .expect("real daemon port");
    assert_ne!(
        port, dead_port,
        "a real daemon cannot listen on the retained non-listening reservation"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "the admitted real daemon must listen on its published port"
    );
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("peer metadata runtime");
    let peer = async_runtime
        .block_on(kin_cli::daemon_client::read_runtime_endpoint(&format!(
            "http://127.0.0.1:{port}"
        )))
        .expect("real daemon runtime identity");
    assert_eq!(peer.schema, "kin.runtime-service.v1");
    assert_eq!(peer.component, "daemon");
    assert_eq!(peer.pid, pid);
    assert_eq!(
        peer.mode,
        kin_core::runtime_identity::RuntimeMode::Development
    );
    assert_eq!(
        peer.identity,
        kin_core::runtime_identity::RuntimeIdentity::current()
    );
    let expected_namespace = kin_core::runtime_identity::RuntimeNamespace::capture(&kin_home)
        .expect("owned runtime namespace");
    assert_eq!(peer.namespace.as_deref(), Some(expected_namespace.token()));
    let incarnation = kin_cli::daemon_client::process_identity(pid)
        .expect("capture real daemon incarnation")
        .expect("real daemon is alive");
    drop(async_runtime);
    // The fixture's existing Drop uses product stop and owned containment on
    // both success and unwinding; it never signals an arbitrary recorded PID.
    drop(runtime);
    assert!(
        !kin_cli::daemon_client::process_identity_is_current(&incarnation)
            .expect("confirm owned daemon cleanup")
    );
}
