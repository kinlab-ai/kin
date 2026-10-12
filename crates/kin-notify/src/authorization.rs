// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Asking macOS whether Kin may post notifications, without ever waiting
//! unbounded.
//!
//! macOS shows the permission request on the Mac's own screen, in the signed-in
//! user's graphical session. A process outside that session, such as one in an
//! SSH login, can still raise the request, and then nobody it can reach is able
//! to answer: the first `kin setup` on a fresh Mac over SSH printed that macOS
//! would ask and waited forever, because the request sat on a screen nobody was
//! looking at and Enter in the SSH terminal answers nothing.
//!
//! So asking takes three decisions, each stated rather than assumed:
//!
//! * Whether the question is still open. A decision macOS already holds is
//!   never asked again; re-running setup must not nag.
//! * Whether a person can see the request. Outside a graphical session the
//!   request is not raised at all, and the result says notifications are not
//!   decided yet and names the command that asks again from the Mac's screen.
//! * How long to wait. With a screen, setup waits a bounded time for the answer
//!   and then goes on, reporting the same "not decided yet" if none arrived.
//!
//! The requester that raised the prompt is not killed when the wait runs out.
//! It runs in its own process group, outside the terminal's reach, and exits
//! once the request is answered. A process that dies while its request is on
//! screen risks that dismissal being recorded as a decision, and a decision
//! macOS records cannot be asked about again, so the prompt is left answerable
//! rather than torn down to make the wait look finished.
//!
//! Every platform call sits behind [`AuthorizationHost`], so the decisions
//! above are tested without a Mac, a screen, or a notification daemon.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long setup waits for an answer before going on.
///
/// Long enough for a person who was just told to look for the request to find
/// it and click, short enough that a run nobody is watching still finishes.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(45);

/// How long a status query may take before it is abandoned.
///
/// The notifier bounds its own settings read at ten seconds; this is the outer
/// bound for a notifier that never gets that far. A status query raises no
/// prompt, so stopping one costs nothing.
pub const STATUS_WAIT: Duration = Duration::from_secs(15);

/// The command that asks again, from the Mac's own screen.
pub const ASK_AGAIN_COMMAND: &str = "kin notify authorize";

/// How often a bounded wait checks whether its child has finished.
const POLL: Duration = Duration::from_millis(50);

/// What the notifier's `--request-authorization` exit codes mean. They are the
/// bundle's contract; see `kin-notifier-macos`.
const EXIT_GRANTED: i32 = 0;
const EXIT_DENIED: i32 = 4;
const EXIT_NO_SCREEN: i32 = 8;
const EXIT_UNANSWERED: i32 = 9;

/// What macOS has recorded about Kin's notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationState {
    /// Nobody has answered yet.
    NotDetermined,
    Denied,
    Authorized,
    /// Delivered quietly to Notification Center without having asked.
    Provisional,
    Ephemeral,
    /// The notifier answered with a state this build does not know.
    Unknown,
}

impl AuthorizationState {
    /// The notifier's spelling, as `--status` prints it.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "not_determined" => Self::NotDetermined,
            "denied" => Self::Denied,
            "authorized" => Self::Authorized,
            "provisional" => Self::Provisional,
            "ephemeral" => Self::Ephemeral,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotDetermined => "not_determined",
            Self::Denied => "denied",
            Self::Authorized => "authorized",
            Self::Provisional => "provisional",
            Self::Ephemeral => "ephemeral",
            Self::Unknown => "unknown",
        }
    }

    /// Whether notifications post as Kin in this state.
    pub fn allows(self) -> bool {
        matches!(self, Self::Authorized | Self::Provisional | Self::Ephemeral)
    }
}

/// Whether a request raised from this process would reach a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptReach {
    /// This process runs in the signed-in user's graphical session, so the
    /// request appears on the screen in front of them.
    Screen,
    /// This is an SSH login. The request would appear on the Mac's own screen,
    /// which nobody at this terminal can see.
    Ssh,
    /// This process has no access to the Mac's screen at all: a background
    /// job, a session started without one, or a session whose kind could not
    /// be read.
    NoScreen,
}

/// How waiting on a raised request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestWait {
    /// The person answered.
    Answered { granted: bool },
    /// The wait ran out with the request still on screen. The requester was
    /// left running, so the request can still be answered.
    StillOpen,
    /// The notifier found no screen to ask on and raised nothing.
    NoScreen,
    /// The request could not be raised.
    Failed(String),
}

/// The platform calls asking needs, behind one seam.
pub trait AuthorizationHost {
    /// Whether a request raised now would reach a person.
    fn prompt_reach(&self) -> PromptReach;

    /// What macOS has recorded, or `None` when the notifier could not say
    /// within [`STATUS_WAIT`].
    fn authorization_state(&self, notifier: &Path) -> Option<AuthorizationState>;

    /// Raise the request and wait at most `wait` for the answer. Must return
    /// once `wait` has passed whether or not anyone answered.
    fn request(&self, notifier: &Path, wait: Duration) -> RequestWait;
}

/// Why notifications are not decided yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undecided {
    /// Nothing was asked, because nobody here could have answered.
    NotAsked(PromptReach),
    /// Asked, and no answer arrived within the wait.
    StillOpen { waited: Duration },
    /// Asking failed.
    RequestFailed(String),
}

/// What asking came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationReport {
    /// There is no notifier bundle to ask about. Its absence is reported by
    /// [`crate::Status::degradation`], not here.
    NoBundle,
    /// macOS already holds a decision, so nothing was asked.
    AlreadyDecided(AuthorizationState),
    /// Asked, and allowed.
    Granted,
    /// Asked, and declined.
    Declined,
    /// Not decided yet.
    Undecided(Undecided),
}

impl AuthorizationReport {
    /// Whether notifications post as Kin after this.
    pub fn allows(&self) -> bool {
        match self {
            Self::Granted => true,
            Self::AlreadyDecided(state) => state.allows(),
            _ => false,
        }
    }

    /// A short machine-readable name for the outcome.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoBundle => "no_bundle",
            Self::AlreadyDecided(state) if state.allows() => "allowed",
            Self::AlreadyDecided(AuthorizationState::Denied) => "denied",
            Self::AlreadyDecided(_) => "decided",
            Self::Granted => "granted",
            Self::Declined => "declined",
            Self::Undecided(_) => "not_decided",
        }
    }

    /// Where notifications stand, in whole sentences, and when they are not
    /// decided, exactly what asks again.
    pub fn sentence(&self) -> String {
        match self {
            Self::NoBundle => "There is no KinNotifier.app to ask about, so notifications post \
                               as Script Editor."
                .to_string(),
            Self::AlreadyDecided(state) if state.allows() => {
                "Notifications are allowed and arrive as Kin.".to_string()
            }
            Self::AlreadyDecided(AuthorizationState::Denied) => {
                "Notifications are turned off for Kin. Turn them on in System Settings > \
                 Notifications > Kin."
                    .to_string()
            }
            Self::AlreadyDecided(state) => format!(
                "macOS reports Kin's notification permission as {}.",
                state.as_str()
            ),
            Self::Granted => "Notifications will arrive as Kin.".to_string(),
            Self::Declined => "Notifications declined. Turn them on later in System Settings > \
                               Notifications > Kin."
                .to_string(),
            Self::Undecided(undecided) => undecided.sentence(),
        }
    }
}

impl Undecided {
    /// The "not decided yet" sentences, with the cause and the way back.
    pub fn sentence(&self) -> String {
        match self {
            Self::NotAsked(PromptReach::Ssh) => format!(
                "Notifications are not decided yet. This is an SSH session, and macOS asks on \
                 the Mac's own screen, where nobody here can answer, so Kin did not ask. To be \
                 asked, run `{ASK_AGAIN_COMMAND}` in Terminal on that screen."
            ),
            Self::NotAsked(_) => format!(
                "Notifications are not decided yet. This session has no access to the Mac's \
                 screen, where macOS asks, so Kin did not ask. To be asked, run \
                 `{ASK_AGAIN_COMMAND}` in Terminal on the Mac's own screen."
            ),
            Self::StillOpen { waited } => format!(
                "Notifications are not decided yet. macOS's request had no answer after {} \
                 seconds and is still waiting for one. Answer it, or run `{ASK_AGAIN_COMMAND}` \
                 later to be asked again.",
                waited.as_secs()
            ),
            Self::RequestFailed(reason) => format!(
                "Notifications are not decided yet. macOS's request could not be raised \
                 ({reason}). Run `{ASK_AGAIN_COMMAND}` in Terminal on the Mac's own screen to \
                 try again."
            ),
        }
    }
}

impl fmt::Display for AuthorizationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sentence())
    }
}

/// Settle notification permission for `notifier`, asking only when the
/// question is open and a person can answer it.
///
/// `on_ask` runs just before the request is raised, so a caller can tell the
/// person what is about to appear and how long it will wait. It does not run
/// when nothing is asked.
pub fn request_with(
    host: &dyn AuthorizationHost,
    notifier: Option<&Path>,
    wait: Duration,
    on_ask: impl FnOnce(),
) -> AuthorizationReport {
    let Some(notifier) = notifier else {
        return AuthorizationReport::NoBundle;
    };
    // An unreadable state is treated as open. Asking about a decision macOS
    // already holds shows nothing and returns the stored answer, so the cost of
    // asking is nil, while skipping would leave an open question unasked.
    match host.authorization_state(notifier) {
        Some(AuthorizationState::NotDetermined) | None => {}
        Some(state) => return AuthorizationReport::AlreadyDecided(state),
    }
    match host.prompt_reach() {
        PromptReach::Screen => {}
        reach => return AuthorizationReport::Undecided(Undecided::NotAsked(reach)),
    }
    on_ask();
    match host.request(notifier, wait) {
        RequestWait::Answered { granted: true } => AuthorizationReport::Granted,
        RequestWait::Answered { granted: false } => AuthorizationReport::Declined,
        RequestWait::StillOpen => {
            AuthorizationReport::Undecided(Undecided::StillOpen { waited: wait })
        }
        RequestWait::NoScreen => {
            AuthorizationReport::Undecided(Undecided::NotAsked(PromptReach::NoScreen))
        }
        RequestWait::Failed(reason) => {
            AuthorizationReport::Undecided(Undecided::RequestFailed(reason))
        }
    }
}

/// The host this process actually runs on.
pub struct SystemHost {
    /// Where the requester's error output goes, so a failed request leaves a
    /// trail after the terminal that started it has moved on.
    log: Option<PathBuf>,
    status_wait: Duration,
}

impl SystemHost {
    pub fn new(log: Option<PathBuf>) -> Self {
        Self {
            log,
            status_wait: STATUS_WAIT,
        }
    }

    #[cfg(test)]
    fn with_status_wait(mut self, status_wait: Duration) -> Self {
        self.status_wait = status_wait;
        self
    }

    fn requester_stderr(&self) -> Stdio {
        let Some(log) = self.log.as_deref() else {
            return Stdio::null();
        };
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null())
    }
}

impl AuthorizationHost for SystemHost {
    fn prompt_reach(&self) -> PromptReach {
        classify_reach(
            ssh_session(|name| std::env::var_os(name)),
            session_has_graphic_access(),
        )
    }

    fn authorization_state(&self, notifier: &Path) -> Option<AuthorizationState> {
        let mut command = Command::new(notifier);
        command.arg("--status");
        let output = output_within(command, self.status_wait)?;
        if !output.status.success() {
            return None;
        }
        parse_status(&String::from_utf8_lossy(&output.stdout))
    }

    fn request(&self, notifier: &Path, wait: Duration) -> RequestWait {
        let mut command = Command::new(notifier);
        command
            .arg("--request-authorization")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(self.requester_stderr());
        // Its own process group, so the Ctrl-C a person sends the terminal that
        // started setup does not also take down a request still on screen.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        match command.spawn() {
            Ok(mut child) => wait_for_answer(&mut child, wait),
            Err(error) => RequestWait::Failed(format!("could not start the notifier: {error}")),
        }
    }
}

/// Whether this is an SSH login, from the variables `sshd` sets.
///
/// `SSH_AUTH_SOCK` is deliberately not one of them: launchd sets it in every
/// graphical session on macOS, so it says nothing about where the person is.
pub fn ssh_session(var: impl Fn(&str) -> Option<OsString>) -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| var(name).is_some_and(|value| !value.is_empty()))
}

/// Where a request would land, from the two facts that decide it.
///
/// SSH wins over a graphical session. A person attached over SSH to a terminal
/// session that was started on the Mac's screen is still not looking at that
/// screen. A session whose kind could not be read (`None`) is treated as having
/// no screen, because guessing a screen is what produced the unbounded wait.
pub fn classify_reach(ssh: bool, graphic_access: Option<bool>) -> PromptReach {
    if ssh {
        return PromptReach::Ssh;
    }
    match graphic_access {
        Some(true) => PromptReach::Screen,
        _ => PromptReach::NoScreen,
    }
}

/// Whether this process's security session can reach the window server.
///
/// The Security framework's own answer: a login in the graphical session
/// carries `sessionHasGraphicAccess`, while an SSH login and a launchd job do
/// not. `None` off macOS and when the session could not be read.
pub fn session_has_graphic_access() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        macos_session::has_graphic_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod macos_session {
    /// `callerSecuritySession` in `AuthSession.h`: the session of the calling
    /// process.
    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    /// `sessionHasGraphicAccess` in `AuthSession.h`.
    const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
    }

    pub(super) fn has_graphic_access() -> Option<bool> {
        let mut session_id = 0u32;
        let mut attributes = 0u32;
        // SAFETY: both out-pointers are valid for the call, and the framework
        // writes nothing else.
        let status =
            unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut session_id, &mut attributes) };
        (status == 0).then_some(attributes & SESSION_HAS_GRAPHIC_ACCESS != 0)
    }
}

/// The `authorization` field of the notifier's `--status` JSON.
fn parse_status(stdout: &str) -> Option<AuthorizationState> {
    #[derive(serde::Deserialize)]
    struct StatusLine {
        authorization: String,
    }
    let line: StatusLine = serde_json::from_str(stdout.trim()).ok()?;
    Some(AuthorizationState::parse(&line.authorization))
}

/// Wait at most `wait` for a requester to finish, and say how it ended.
///
/// The child is never killed here. When the wait runs out it is left running,
/// holding a request a person can still answer; see the module documentation.
fn wait_for_answer(child: &mut Child, wait: Duration) -> RequestWait {
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return classify_request_exit(status.code()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => return RequestWait::StillOpen,
            Err(error) => {
                return RequestWait::Failed(format!("could not wait on the notifier: {error}"))
            }
        }
    }
}

/// What a requester's exit code says.
fn classify_request_exit(code: Option<i32>) -> RequestWait {
    match code {
        Some(EXIT_GRANTED) => RequestWait::Answered { granted: true },
        Some(EXIT_DENIED) => RequestWait::Answered { granted: false },
        Some(EXIT_NO_SCREEN) => RequestWait::NoScreen,
        Some(EXIT_UNANSWERED) => RequestWait::StillOpen,
        Some(code) => RequestWait::Failed(format!("the notifier exited {code}")),
        None => RequestWait::Failed("the notifier was stopped by a signal".to_string()),
    }
}

/// Run `command` to completion, or stop it once `bound` has passed.
///
/// For calls that raise no prompt, where stopping the child costs nothing.
/// Output is read after exit, which is safe for the few bytes a status line
/// carries.
pub(crate) fn output_within(mut command: Command, bound: Duration) -> Option<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A host whose every answer is set by the test, and which records what
    /// it was asked to do.
    struct FakeHost {
        reach: PromptReach,
        state: Option<AuthorizationState>,
        answer: RequestWait,
        requests: RefCell<Vec<Duration>>,
        state_reads: RefCell<usize>,
    }

    impl FakeHost {
        fn new(reach: PromptReach, state: Option<AuthorizationState>, answer: RequestWait) -> Self {
            Self {
                reach,
                state,
                answer,
                requests: RefCell::new(Vec::new()),
                state_reads: RefCell::new(0),
            }
        }
    }

    impl AuthorizationHost for FakeHost {
        fn prompt_reach(&self) -> PromptReach {
            self.reach
        }

        fn authorization_state(&self, _notifier: &Path) -> Option<AuthorizationState> {
            *self.state_reads.borrow_mut() += 1;
            self.state
        }

        fn request(&self, _notifier: &Path, wait: Duration) -> RequestWait {
            self.requests.borrow_mut().push(wait);
            self.answer.clone()
        }
    }

    fn bundle() -> PathBuf {
        PathBuf::from("/opt/kin/lib/KinNotifier.app/Contents/MacOS/KinNotifier")
    }

    fn ask(host: &FakeHost) -> (AuthorizationReport, bool) {
        let mut told = false;
        let report = request_with(host, Some(&bundle()), DEFAULT_WAIT, || told = true);
        (report, told)
    }

    /// The defect: over SSH the request went up on a screen nobody could see
    /// and setup waited forever. Now nothing is raised, and the report names
    /// the command that asks from the Mac's own screen.
    #[test]
    fn an_ssh_session_is_never_asked_and_is_told_how_to_be_asked() {
        let host = FakeHost::new(
            PromptReach::Ssh,
            Some(AuthorizationState::NotDetermined),
            RequestWait::Answered { granted: true },
        );
        let (report, told) = ask(&host);
        assert_eq!(
            report,
            AuthorizationReport::Undecided(Undecided::NotAsked(PromptReach::Ssh))
        );
        assert!(
            host.requests.borrow().is_empty(),
            "no request may be raised"
        );
        assert!(!told, "nothing is announced when nothing is asked");
        let sentence = report.sentence();
        assert!(sentence.contains("not decided yet"), "{sentence}");
        assert!(sentence.contains("SSH"), "{sentence}");
        assert!(sentence.contains("`kin notify authorize`"), "{sentence}");
    }

    #[test]
    fn a_session_with_no_screen_is_never_asked() {
        let host = FakeHost::new(
            PromptReach::NoScreen,
            Some(AuthorizationState::NotDetermined),
            RequestWait::Answered { granted: true },
        );
        let (report, told) = ask(&host);
        assert_eq!(
            report,
            AuthorizationReport::Undecided(Undecided::NotAsked(PromptReach::NoScreen))
        );
        assert!(host.requests.borrow().is_empty());
        assert!(!told);
        assert!(report.sentence().contains("`kin notify authorize`"));
        assert!(report.sentence().contains("Mac's own screen"));
    }

    /// With a screen the wait is bounded, and running out of it reports the
    /// same "not decided yet" with the way back.
    #[test]
    fn a_screen_session_waits_a_bounded_time_and_reports_an_unanswered_request() {
        let host = FakeHost::new(
            PromptReach::Screen,
            Some(AuthorizationState::NotDetermined),
            RequestWait::StillOpen,
        );
        let (report, told) = ask(&host);
        assert!(told, "the person is told before the request goes up");
        assert_eq!(*host.requests.borrow(), vec![DEFAULT_WAIT]);
        assert_eq!(
            report,
            AuthorizationReport::Undecided(Undecided::StillOpen {
                waited: DEFAULT_WAIT
            })
        );
        let sentence = report.sentence();
        assert!(sentence.contains("not decided yet"), "{sentence}");
        assert!(sentence.contains("after 45 seconds"), "{sentence}");
        assert!(sentence.contains("`kin notify authorize`"), "{sentence}");
    }

    #[test]
    fn the_default_wait_is_between_thirty_seconds_and_a_minute() {
        assert!(DEFAULT_WAIT >= Duration::from_secs(30));
        assert!(DEFAULT_WAIT <= Duration::from_secs(60));
    }

    #[test]
    fn an_answer_on_screen_is_reported_as_given() {
        for (granted, expected) in [
            (true, AuthorizationReport::Granted),
            (false, AuthorizationReport::Declined),
        ] {
            let host = FakeHost::new(
                PromptReach::Screen,
                Some(AuthorizationState::NotDetermined),
                RequestWait::Answered { granted },
            );
            assert_eq!(ask(&host).0, expected);
        }
        assert!(AuthorizationReport::Granted.allows());
        assert!(!AuthorizationReport::Declined.allows());
    }

    /// A decision macOS already holds is reported, never asked about again,
    /// and that holds over SSH too, where a second run must not claim the
    /// question is still open.
    #[test]
    fn a_held_decision_is_never_asked_again_from_any_session() {
        for reach in [PromptReach::Screen, PromptReach::Ssh, PromptReach::NoScreen] {
            for state in [AuthorizationState::Authorized, AuthorizationState::Denied] {
                let host = FakeHost::new(reach, Some(state), RequestWait::StillOpen);
                let (report, told) = ask(&host);
                assert_eq!(report, AuthorizationReport::AlreadyDecided(state));
                assert!(host.requests.borrow().is_empty());
                assert!(!told);
            }
        }
        assert!(AuthorizationReport::AlreadyDecided(AuthorizationState::Authorized).allows());
        assert!(
            AuthorizationReport::AlreadyDecided(AuthorizationState::Denied)
                .sentence()
                .contains("System Settings > Notifications > Kin")
        );
    }

    #[test]
    fn an_unreadable_state_is_asked_on_screen_and_left_unasked_elsewhere() {
        let host = FakeHost::new(
            PromptReach::Screen,
            None,
            RequestWait::Answered { granted: true },
        );
        assert_eq!(ask(&host).0, AuthorizationReport::Granted);
        assert_eq!(host.requests.borrow().len(), 1);

        let host = FakeHost::new(PromptReach::Ssh, None, RequestWait::StillOpen);
        assert_eq!(
            ask(&host).0,
            AuthorizationReport::Undecided(Undecided::NotAsked(PromptReach::Ssh))
        );
        assert!(host.requests.borrow().is_empty());
    }

    #[test]
    fn a_notifier_that_finds_no_screen_reports_the_same_as_not_asking() {
        let host = FakeHost::new(
            PromptReach::Screen,
            Some(AuthorizationState::NotDetermined),
            RequestWait::NoScreen,
        );
        assert_eq!(
            ask(&host).0,
            AuthorizationReport::Undecided(Undecided::NotAsked(PromptReach::NoScreen))
        );
    }

    #[test]
    fn a_failed_request_names_the_reason_and_the_way_back() {
        let host = FakeHost::new(
            PromptReach::Screen,
            Some(AuthorizationState::NotDetermined),
            RequestWait::Failed("the notifier exited 5".to_string()),
        );
        let sentence = ask(&host).0.sentence();
        assert!(sentence.contains("the notifier exited 5"), "{sentence}");
        assert!(sentence.contains("`kin notify authorize`"), "{sentence}");
    }

    #[test]
    fn no_bundle_asks_nothing_of_the_host() {
        let host = FakeHost::new(
            PromptReach::Screen,
            Some(AuthorizationState::NotDetermined),
            RequestWait::Answered { granted: true },
        );
        let report = request_with(&host, None, DEFAULT_WAIT, || {});
        assert_eq!(report, AuthorizationReport::NoBundle);
        assert_eq!(*host.state_reads.borrow(), 0);
        assert!(host.requests.borrow().is_empty());
    }

    #[test]
    fn ssh_is_read_from_what_sshd_sets_and_not_from_the_agent_socket() {
        let vars = |set: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                set.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert!(ssh_session(vars(&[(
            "SSH_CONNECTION",
            "10.0.0.2 52144 10.0.0.9 22"
        )])));
        assert!(ssh_session(vars(&[("SSH_TTY", "/dev/ttys003")])));
        assert!(ssh_session(vars(&[("SSH_CLIENT", "10.0.0.2 52144 22")])));
        // launchd sets this in every graphical session on macOS.
        assert!(!ssh_session(vars(&[(
            "SSH_AUTH_SOCK",
            "/private/tmp/com.apple.launchd.x/Listeners"
        )])));
        assert!(!ssh_session(vars(&[("SSH_CONNECTION", "")])));
        assert!(!ssh_session(vars(&[])));
    }

    #[test]
    fn reach_takes_ssh_over_a_screen_and_treats_an_unknown_session_as_screenless() {
        assert_eq!(classify_reach(false, Some(true)), PromptReach::Screen);
        assert_eq!(classify_reach(true, Some(true)), PromptReach::Ssh);
        assert_eq!(classify_reach(true, Some(false)), PromptReach::Ssh);
        assert_eq!(classify_reach(false, Some(false)), PromptReach::NoScreen);
        assert_eq!(classify_reach(false, None), PromptReach::NoScreen);
    }

    #[test]
    fn requester_exit_codes_map_onto_the_bundle_contract() {
        assert_eq!(
            classify_request_exit(Some(0)),
            RequestWait::Answered { granted: true }
        );
        assert_eq!(
            classify_request_exit(Some(4)),
            RequestWait::Answered { granted: false }
        );
        assert_eq!(classify_request_exit(Some(8)), RequestWait::NoScreen);
        assert_eq!(classify_request_exit(Some(9)), RequestWait::StillOpen);
        assert!(matches!(
            classify_request_exit(Some(5)),
            RequestWait::Failed(reason) if reason.contains("exited 5")
        ));
        assert!(matches!(
            classify_request_exit(None),
            RequestWait::Failed(_)
        ));
    }

    #[test]
    fn status_lines_parse_into_states() {
        assert_eq!(
            parse_status(r#"{"bundle_id":"ai.kinlab.kin","authorization":"not_determined"}"#),
            Some(AuthorizationState::NotDetermined)
        );
        assert_eq!(
            parse_status("{\"bundle_id\":\"ai.kinlab.kin\",\"authorization\":\"authorized\"}\n"),
            Some(AuthorizationState::Authorized)
        );
        assert_eq!(
            parse_status(r#"{"bundle_id":"ai.kinlab.kin","authorization":"unknown"}"#),
            Some(AuthorizationState::Unknown)
        );
        assert_eq!(parse_status("not json"), None);
    }

    /// The real waiting code, against a requester that never answers: it must
    /// come back once the wait is spent, and leave the requester running in a
    /// process group of its own.
    #[cfg(unix)]
    #[test]
    fn the_real_wait_returns_on_time_and_leaves_the_request_answerable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("requester.pid");
        let notifier = dir.path().join("KinNotifier");
        std::fs::write(
            &notifier,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&notifier, std::fs::Permissions::from_mode(0o755)).unwrap();

        let host = SystemHost::new(Some(dir.path().join("logs/notify.log")));
        let started = Instant::now();
        let outcome = host.request(&notifier, Duration::from_millis(400));
        let elapsed = started.elapsed();
        assert_eq!(outcome, RequestWait::StillOpen);
        assert!(
            elapsed < Duration::from_secs(5),
            "the wait must end on its bound, took {elapsed:?}"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|raw| raw.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the requester never started");
            std::thread::sleep(Duration::from_millis(20));
        };
        let group = Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let alive = group.status.success();
        let pgid = String::from_utf8_lossy(&group.stdout).trim().to_string();
        let _ = Command::new("kill").arg(pid.to_string()).status();
        assert!(alive, "the requester must outlive the wait");
        assert_eq!(
            pgid,
            pid.to_string(),
            "the requester leads its own process group, out of the terminal's reach"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_real_wait_reads_an_answer_that_arrives_in_time() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        for (code, expected) in [
            (0, RequestWait::Answered { granted: true }),
            (4, RequestWait::Answered { granted: false }),
            (8, RequestWait::NoScreen),
        ] {
            let notifier = dir.path().join(format!("KinNotifier-{code}"));
            std::fs::write(&notifier, format!("#!/bin/sh\nexit {code}\n")).unwrap();
            std::fs::set_permissions(&notifier, std::fs::Permissions::from_mode(0o755)).unwrap();
            let host = SystemHost::new(None);
            assert_eq!(host.request(&notifier, Duration::from_secs(10)), expected);
        }
    }

    /// A status query that never returns is abandoned at its bound rather
    /// than holding setup or `kin doctor`.
    #[cfg(unix)]
    #[test]
    fn a_status_query_is_bounded_and_parsed() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let hung = dir.path().join("hung");
        std::fs::write(&hung, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&hung, std::fs::Permissions::from_mode(0o755)).unwrap();
        let host = SystemHost::new(None).with_status_wait(Duration::from_millis(300));
        let started = Instant::now();
        assert_eq!(host.authorization_state(&hung), None);
        assert!(started.elapsed() < Duration::from_secs(5));

        let answering = dir.path().join("answering");
        std::fs::write(
            &answering,
            "#!/bin/sh\necho '{\"bundle_id\":\"ai.kinlab.kin\",\"authorization\":\"denied\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&answering, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A generous bound here: this half grades the parse, and a loaded test
        // host can take longer than the hung half's bound to start a shell.
        let host = SystemHost::new(None).with_status_wait(Duration::from_secs(10));
        assert_eq!(
            host.authorization_state(&answering),
            Some(AuthorizationState::Denied)
        );
    }
}
