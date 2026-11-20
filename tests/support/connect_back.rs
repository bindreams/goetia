//! Loopback rendezvous with a daemon the service manager started for us.
//!
//! The probes have to distinguish "launchd/SCM really ran the program" from
//! "the command exited 0", and to prove a `Disabled` job did *not* run. Both
//! need the daemon itself to speak: it connects to a listener the test already
//! holds, so the test observes an event rather than the passage of time.
//! Loopback rather than a pipe or a unix socket because it crosses session 0
//! and the LocalSystem boundary and exists on all three platforms.

use std::io;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// The daemon may never start at all — a failure the harness cannot detect any
/// other way. This bound exists to turn that into a message a human can read,
/// not to sequence anything.
const START_DEADLINE: Duration = Duration::from_secs(60);

pub struct ConnectBack {
    listener: TcpListener,
    port: u16,
}

impl ConnectBack {
    pub fn listen() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        Self { listener, port }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// A `sh` command that runs `setup`, reports in to this listener, then runs `rest`. Once
    /// [`Self::accept`] returns, `setup` (a `trap`, say) is in place, so a signal sent after it meets it:
    /// a `Type=exec` or launchd start returns before `sh` has run its first command.
    #[cfg(unix)]
    pub fn sh_after(&self, setup: &str, rest: &str) -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("{setup}; \"$0\" \"$@\"; {rest}"),
            super::current_exe_str(),
            super::sentinel::CONNECT_BACK.to_string(),
            self.port.to_string(),
        ]
    }

    /// Whether the daemon has already reported in.
    ///
    /// Only meaningful after a synchronous barrier — `launchctl bootout`, a
    /// confirmed SCM stop — has made a later connection impossible. Called
    /// before such a barrier it would be a race, not an observation.
    pub fn connected_yet(&self) -> bool {
        self.listener.set_nonblocking(true).expect("set nonblocking");
        let result = self.listener.accept();
        self.listener.set_nonblocking(false).expect("clear nonblocking");
        match result {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("accept: {e}"),
        }
    }

    /// Blocks until the daemon reports in. `what` names the thing being waited
    /// on, because the only way this returns without a connection is a failure
    /// report to whoever reads the CI log.
    pub fn accept(&self, what: &str) {
        self.accept_value(what);
    }

    /// Blocks until the daemon reports in, with no bound of its own: for an event the service manager
    /// causes at a time it chooses (a schedule's next minute, a first logon that builds a profile), where
    /// any number picked here would be a guess. A daemon that never connects is the CI watchdog's to
    /// report. The connection comes back unread: dropping it is how a test releases a daemon that
    /// waits on it (`RUN_UNTIL_RELEASED`).
    pub fn accept_unbounded(&self) -> TcpStream {
        self.listener.accept().expect("accept a daemon's report").0
    }

    /// Like [`Self::accept`], but hands back the connection itself, unread.
    ///
    /// The daemon may be waiting on its end of the stream (it reads until the
    /// test drops this), so closing the stream is how a test releases it
    /// deterministically rather than by timing.
    pub fn accept_stream(&self, what: &str) -> TcpStream {
        let listener = self.listener.try_clone().expect("clone listener");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(listener.accept().map(|(stream, _addr)| stream));
        });
        match rx.recv_timeout(START_DEADLINE) {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => panic!("accept while waiting for {what}: {e}"),
            Err(_) => panic!(
                "waited {}s for {what} on 127.0.0.1:{}; it never connected",
                START_DEADLINE.as_secs(),
                self.port
            ),
        }
    }

    /// Like [`Self::accept`], but returns whatever bytes the daemon wrote
    /// after connecting (as UTF-8), for a sentinel mode that reports more
    /// than just "I ran" — e.g. `--goetia-report-uid`'s decimal uid.
    pub fn accept_value(&self, what: &str) -> String {
        use std::io::Read as _;

        let listener = self.listener.try_clone().expect("clone listener");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = listener.accept().map(|(mut stream, _addr)| {
                let mut buf = String::new();
                let _ = stream.read_to_string(&mut buf);
                buf
            });
            let _ = tx.send(result);
        });
        match rx.recv_timeout(START_DEADLINE) {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => panic!("accept while waiting for {what}: {e}"),
            Err(_) => panic!(
                "waited {}s for {what} on 127.0.0.1:{}; it never connected",
                START_DEADLINE.as_secs(),
                self.port
            ),
        }
    }

    /// Like [`Self::accept`], but returns whatever the daemon wrote before
    /// closing its end — used to carry a value out of the daemon's own
    /// process (its own environment, for instance) rather than a bare
    /// presence signal. Still bounded by [`START_DEADLINE`] for the same
    /// reason `accept` is: the daemon may never connect at all.
    pub fn accept_line(&self, what: &str) -> String {
        let listener = self.listener.try_clone().expect("clone listener");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> io::Result<String> {
                use std::io::Read as _;
                let (mut stream, _) = listener.accept()?;
                let mut buf = String::new();
                stream.read_to_string(&mut buf)?;
                Ok(buf)
            })();
            let _ = tx.send(result);
        });
        match rx.recv_timeout(START_DEADLINE) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => panic!("accept/read while waiting for {what}: {e}"),
            Err(_) => panic!(
                "waited {}s for {what} on 127.0.0.1:{}; it never connected",
                START_DEADLINE.as_secs(),
                self.port
            ),
        }
    }
}
