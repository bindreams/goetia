//! A service manager's command-line client (`systemctl`, `launchctl`) as the backends reach it: one trait,
//! [`Client`], real over [`bounded`] and `cosca` ([`Real`]) and, in tests, a table of stated replies
//! (`fake::Fake`).
//!
//! A backend decides what a reply means; the client only runs the program and says how it ended.

use crate::backend::bounded::{self, Capture, Finished, Reservation, Role, SpawnError};
use crate::backend::unreaped::release_unreaped;
use crate::manager::budget::Deadline;

/// A service manager's command-line client.
pub(crate) trait Client: Send + Sync {
    /// Run the client with `args`, waiting as `wait` says. What the call's output needs (temp files,
    /// threads to read it) is drawn from `reservation`: see [`bounded::spawn`].
    fn run(&self, args: &[&str], wait: Wait, reservation: &mut Reservation) -> Result<Finished, ClientError>;
}

pub(crate) enum Wait {
    /// To completion: nothing bounds it, so it never expires and draws nothing from the
    /// reservation, and the child's tree is not contained, since there is no deadline to tear it
    /// down on.
    // systemd's alone: `launchctl` is always waited on under a deadline.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ToCompletion,
    /// Under `deadline`, as [`bounded`] does it for `role`.
    Bounded { role: Role, deadline: Deadline },
}

/// A run that did not complete, and what is known of whether the client ran.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ClientError {
    /// Before the client ran: nothing it would have sent was sent.
    #[error("the client was not run")]
    NotRun(#[source] cosca::error::Error),
    /// Perhaps after it ran: see [`SpawnError::MayHaveRun`]. A run to completion that failed after
    /// the spawn is this too, since cosca says its program may have started.
    #[error("the client may have run")]
    MayHaveRun(#[source] cosca::error::Error),
    /// After it ran: waiting for it or reading it failed. `announced` as in [`bounded::Lost`].
    #[error("the client was lost after it started")]
    Lost {
        #[source]
        error: cosca::error::Error,
        announced: bool,
    },
}

impl From<SpawnError> for ClientError {
    fn from(e: SpawnError) -> ClientError {
        match e {
            SpawnError::NotRun(e) => ClientError::NotRun(e),
            SpawnError::MayHaveRun(e) => ClientError::MayHaveRun(e),
        }
    }
}

/// The real client: `program` (`["systemctl"]`, `["launchctl"]`) run with `environment` applied to every child.
pub(crate) struct Real {
    program: Vec<String>,
    environment: fn(&mut cosca::Command),
}

impl Real {
    pub(crate) fn new(program: Vec<String>, environment: fn(&mut cosca::Command)) -> Real {
        debug_assert!(!program.is_empty(), "a program to run");
        Real { program, environment }
    }

    /// The program's argv, then `args`.
    fn argv<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        self.program
            .iter()
            .map(String::as_str)
            .chain(args.iter().copied())
            .collect()
    }

    fn bounded(
        &self,
        args: &[&str],
        role: Role,
        deadline: Deadline,
        reservation: &mut Reservation,
    ) -> Result<Finished, ClientError> {
        let argv = self.argv(args);
        let mut cmd = bounded::command(argv[0], &argv[1..]).map_err(ClientError::NotRun)?;
        (self.environment)(&mut cmd);
        let spawned = bounded::spawn(&mut cmd, role, reservation)?;
        bounded::wait_bounded(spawned, deadline).map_err(|lost| ClientError::Lost {
            error: lost.error,
            announced: lost.announced,
        })
    }

    /// Spawn, then read both streams to their end on the calling thread: goetia makes no thread and
    /// no temp file. cosca's spawn does make one thread (cosca 0.7.0, on Linux: the pidfd handshake
    /// helper, joined before `output` reads anything), which nothing here can reserve: see
    /// `systemd::manager::systemctl::Held::of`, where the rule and its re-measure on a cosca upgrade live.
    fn to_completion(&self, args: &[&str]) -> Result<Finished, ClientError> {
        let mut cmd = cosca::run(self.argv(args));
        (self.environment)(&mut cmd);
        of_output(cmd.output())
    }
}

/// A run to completion's `output`, as [`Finished`], or why it failed: cosca's
/// [`MayHaveStarted`](cosca::error::Error::MayHaveStarted), which it returns for a failure after the
/// spawn, is [`ClientError::MayHaveRun`], and any other failure is [`ClientError::NotRun`].
fn of_output(output: Result<cosca::Output, cosca::error::Error>) -> Result<Finished, ClientError> {
    let output = output.map_err(|e| ClientError::from(SpawnError::of_spawn(release_unreaped(e))))?;
    Ok(Finished::Exited {
        status: output.status,
        capture: Capture {
            stdout: output.stdout,
            stderr: output.stderr,
            complete: true,
        },
    })
}

impl Client for Real {
    fn run(&self, args: &[&str], wait: Wait, reservation: &mut Reservation) -> Result<Finished, ClientError> {
        match wait {
            Wait::ToCompletion => self.to_completion(args),
            Wait::Bounded { role, deadline } => self.bounded(args, role, deadline, reservation),
        }
    }
}

/// A test client: a table of stated replies, with no process behind it.
#[cfg(test)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod fake {
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    use std::sync::{Arc, Mutex};

    use super::{Client, ClientError, Finished, Reservation, Role, Wait};
    use crate::backend::bounded::{Capture, Fate, Left};

    /// Replies consumed in order. Each entry names the argv prefix it expects, and may carry an
    /// action the test runs at the instant of the call, such as moving a file, to state a
    /// concurrent actor. A call with another argv, or past the table's end, panics. Clones share
    /// one table, so a test keeps a handle after giving one away.
    #[derive(Clone)]
    pub(crate) struct Fake {
        state: Arc<Mutex<State>>,
    }

    struct State {
        entries: VecDeque<Entry>,
        calls: Vec<Vec<String>>,
        waits: Vec<Waited>,
    }

    /// How a call was waited on: the kind of its [`Role`], and whether its deadline can expire.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Waited {
        pub(crate) role: RoleKind,
        pub(crate) bounded: bool,
    }

    /// Which [`Role`] a call had, without its [`Reaper`].
    ///
    /// [`Reaper`]: crate::backend::bounded::Reaper
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum RoleKind {
        Query,
        UncountedQuery,
        AnnouncedRequest,
        Request,
        /// [`Wait::ToCompletion`], which has no role.
        ToCompletion,
    }

    impl Waited {
        fn of(wait: &Wait) -> Waited {
            match wait {
                Wait::ToCompletion => Waited {
                    role: RoleKind::ToCompletion,
                    bounded: false,
                },
                Wait::Bounded { role, deadline } => Waited {
                    role: match role {
                        Role::Query => RoleKind::Query,
                        Role::UncountedQuery => RoleKind::UncountedQuery,
                        Role::AnnouncedRequest(_) => RoleKind::AnnouncedRequest,
                        Role::Request(_) => RoleKind::Request,
                    },
                    bounded: deadline.at().is_some(),
                },
            }
        }
    }

    /// One stated reply: see [`reply`] and [`fail`].
    pub(crate) struct Entry {
        prefix: Vec<String>,
        reply: Result<Finished, ClientError>,
        action: Option<Box<dyn FnOnce() + Send>>,
    }

    fn entry(prefix: &[&str], reply: Result<Finished, ClientError>) -> Entry {
        Entry {
            prefix: prefix.iter().map(|arg| arg.to_string()).collect(),
            reply,
            action: None,
        }
    }

    pub(crate) fn reply(prefix: &[&str], finished: Finished) -> Entry {
        entry(prefix, Ok(finished))
    }

    pub(crate) fn fail(prefix: &[&str], error: ClientError) -> Entry {
        entry(prefix, Err(error))
    }

    /// A client that exited with `code`, having written `stdout` and `stderr` whole.
    pub(crate) fn exited(code: i32, stdout: &str, stderr: &str) -> Finished {
        Finished::Exited {
            status: ExitStatus::from_raw(code << 8),
            capture: Capture {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                complete: true,
            },
        }
    }

    /// How a [`lingering`] reply's request ended up: `None` until its [`Left`] is settled.
    #[derive(Clone, Default)]
    pub(crate) struct Ended(Arc<Mutex<Option<Fate>>>);

    impl Ended {
        pub(crate) fn fate(&self) -> Option<Fate> {
            *self.0.lock().unwrap()
        }
    }

    /// A request the deadline left running: the call expires with a [`Left`] that records its
    /// [`Fate`] in the returned [`Ended`].
    pub(crate) fn lingering(prefix: &[&str]) -> (Entry, Ended) {
        let ended = Ended::default();
        let record = ended.0.clone();
        let left = Left::new(move |fate| *record.lock().unwrap() = Some(fate));
        (entry(prefix, Ok(Finished::Left(left))), ended)
    }

    impl Entry {
        /// Run `action` at the call, before its reply is returned.
        pub(crate) fn then(mut self, action: impl FnOnce() + Send + 'static) -> Entry {
            self.action = Some(Box::new(action));
            self
        }
    }

    impl Fake {
        pub(crate) fn new(entries: impl IntoIterator<Item = Entry>) -> Fake {
            Fake {
                state: Arc::new(Mutex::new(State {
                    entries: entries.into_iter().collect(),
                    calls: Vec::new(),
                    waits: Vec::new(),
                })),
            }
        }

        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.state.lock().unwrap().calls.clone()
        }

        /// How every call so far was waited on, in the order of [`Self::calls`].
        pub(crate) fn waits(&self) -> Vec<Waited> {
            self.state.lock().unwrap().waits.clone()
        }

        /// Panics, naming `what`, unless every entry has been called: the path was driven end to end.
        pub(crate) fn assert_spent(&self, what: &str) {
            let state = self.state.lock().unwrap();
            let left: Vec<&[String]> = state.entries.iter().map(|entry| entry.prefix.as_slice()).collect();
            assert!(left.is_empty(), "{what}: never called: {left:?}");
        }
    }

    impl Client for Fake {
        /// Draws the files and listener threads the real client's spawn would, so a verb that
        /// counted too few trips the assertion in every test that drives it.
        fn run(&self, args: &[&str], wait: Wait, reservation: &mut Reservation) -> Result<Finished, ClientError> {
            let waited = Waited::of(&wait);
            let (files, listeners) = match &wait {
                Wait::ToCompletion => (0, 0),
                Wait::Bounded { role, .. } => (role.files(), role.listeners()),
            };
            for _ in 0..files {
                reservation.file().map_err(|e| ClientError::NotRun(e.into()))?;
            }
            for _ in 0..listeners {
                reservation.listener().map_err(|e| ClientError::NotRun(e.into()))?;
            }
            let argv: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            let next = {
                let mut state = self.state.lock().unwrap();
                state.calls.push(argv.clone());
                state.waits.push(waited);
                state.entries.pop_front()
            };
            let Some(entry) = next else {
                panic!("unscripted call {argv:?}: the table is spent");
            };
            assert!(
                argv.starts_with(&entry.prefix),
                "expected a call starting {:?}, got {argv:?}",
                entry.prefix
            );
            if let Some(action) = entry.action {
                action();
            }
            entry.reply
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
