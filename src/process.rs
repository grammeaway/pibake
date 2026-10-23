//! Child processes with a deadline and `wait4` resource usage.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub wall: Duration,
    pub user: Duration,
    pub sys: Duration,
    /// Peak resident set of the child and its reaped descendants (`ru_maxrss`).
    pub max_rss_kib: u64,
}

impl Usage {
    pub fn cpu(&self) -> Duration {
        self.user + self.sys
    }
}

/// A child in its own process group, killed with that group when the deadline passes,
/// or when dropped without `wait`.
pub struct Supervised {
    pub child: Child,
    pub started: Instant,
    done: mpsc::Sender<()>,
    watchdog: Option<JoinHandle<bool>>,
    reaped: bool,
}

impl Supervised {
    pub fn spawn(mut command: Command, timeout: Duration) -> Result<Self> {
        command.process_group(0);
        Self::start(command, timeout)
    }

    /// As `spawn`, with `tty` as stdin, stdout and controlling terminal, in a new session
    /// (so also its own process group).
    pub fn spawn_in_pty(mut command: Command, tty: File, timeout: Duration) -> Result<Self> {
        command.stdin(tty.try_clone()?).stdout(tty);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        };
        Self::start(command, timeout)
    }

    fn start(mut command: Command, timeout: Duration) -> Result<Self> {
        let started = Instant::now();
        let child = command
            .spawn()
            .with_context(|| format!("spawning {:?}", command.get_program()))?;
        let pgid = child.id() as libc::pid_t;
        let (done, finished) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            let timed_out = matches!(
                finished.recv_timeout(timeout),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            if timed_out {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
            timed_out
        });
        Ok(Self {
            child,
            started,
            done,
            watchdog: Some(watchdog),
            reaped: false,
        })
    }

    fn pid(&self) -> libc::pid_t {
        self.child.id() as libc::pid_t
    }

    /// Reaps the child. Errors if the deadline killed it.
    pub fn wait(mut self) -> Result<(ExitStatus, Usage)> {
        let pid = self.pid();
        let mut status = 0;
        let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
        loop {
            if unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) } == pid {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err).context("wait4");
            }
        }
        let wall = self.started.elapsed();
        self.reaped = true;
        let _ = self.done.send(());
        let timed_out = self
            .watchdog
            .take()
            .is_some_and(|w| w.join().unwrap_or(false));
        // Grandchildren left behind (e.g. an extension's helper) go with the group.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        if timed_out {
            bail!("timed out after {:.1}s and was killed", wall.as_secs_f64());
        }
        let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
        Ok((
            ExitStatus::from_raw(status),
            Usage {
                wall,
                user: tv(rusage.ru_utime),
                sys: tv(rusage.ru_stime),
                max_rss_kib: rusage.ru_maxrss as u64,
            },
        ))
    }
}

/// A pseudo-terminal of `cols`×`rows`: (master, slave). Both are close-on-exec.
pub fn openpty(cols: u16, rows: u16) -> Result<(File, File)> {
    let (mut master, mut slave) = (0, 0);
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    if opened < 0 {
        return Err(std::io::Error::last_os_error()).context("openpty");
    }
    // SAFETY: openpty returned two fresh descriptors we now own.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // shortcut: a child another thread spawns before this line inherits the pty and delays
        // its EOF; open with O_CLOEXEC (posix_openpt) if ptys and spawns ever run concurrently.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl");
        }
    }
    Ok((master.into(), slave.into()))
}

/// The CPU time (user and system, every thread) a live process has used so far.
pub fn cpu_time(pid: u32) -> Result<Duration> {
    let mut clock = 0;
    let err = unsafe { libc::clock_getcpuclockid(pid as libc::pid_t, &mut clock) };
    if err != 0 {
        return Err(std::io::Error::from_raw_os_error(err)).context("clock_getcpuclockid");
    }
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(clock, &mut ts) } < 0 {
        return Err(std::io::Error::last_os_error()).context("clock_gettime");
    }
    Ok(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

/// The process in `pid`'s tree (itself included) that has used the most CPU: the Pi process,
/// when `pid` is a wrapper such as `perf stat` that waits for it.
pub fn busiest_in_tree(pid: u32) -> Result<u32> {
    let mut best = (cpu_time(pid)?, pid);
    let mut pending = vec![pid];
    while let Some(parent) = pending.pop() {
        for child in children(parent) {
            // A child that exited in between is skipped.
            if let Ok(cpu) = cpu_time(child) {
                best = best.max((cpu, child));
            }
            pending.push(child);
        }
    }
    Ok(best.1)
}

/// A live process's children; none once it is gone.
pub fn children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|c| c.parse().ok())
        .collect()
}

/// A live process's resident set and its peak so far (`VmRSS`, `VmHWM`).
pub fn memory_kib(pid: u32) -> Result<(u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("reading /proc/{pid}/status"))?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse().ok())
            .with_context(|| format!("no {name} in /proc/{pid}/status"))
    };
    Ok((field("VmRSS")?, field("VmHWM")?))
}

impl Drop for Supervised {
    fn drop(&mut self) {
        if !self.reaped {
            unsafe { libc::killpg(self.pid(), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_cpu_and_memory() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done"]);
        let (status, usage) = Supervised::spawn(cmd, Duration::from_secs(60))
            .unwrap()
            .wait()
            .unwrap();
        assert!(status.success());
        assert!(usage.cpu() > Duration::from_millis(10), "{usage:?}");
        assert!(usage.wall >= usage.cpu() / 4, "{usage:?}");
        assert!(usage.max_rss_kib > 1024, "{usage:?}");
    }

    #[test]
    fn runs_in_a_pty_as_its_controlling_terminal() {
        use std::io::Read;
        let (mut master, tty) = openpty(120, 40).unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "test -t 0 && test -t 1 && stty size < /dev/tty"]);
        let run = Supervised::spawn_in_pty(cmd, tty, Duration::from_secs(10)).unwrap();
        let (status, _) = run.wait().unwrap();
        assert!(status.success());
        let mut out = vec![];
        let mut buf = [0; 256];
        // The hung-up pty reads as EIO once the output is drained.
        while let Ok(n @ 1..) = master.read(&mut buf) {
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(String::from_utf8_lossy(&out), "40 120\r\n");
    }

    #[test]
    fn finds_the_busiest_process_of_a_tree_and_its_usage() {
        let mut cmd = Command::new("sh");
        // The shell waits; its child burns CPU, then idles.
        cmd.args([
            "-c",
            "sh -c 'i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done; sleep 30'; true",
        ]);
        let run = Supervised::spawn(cmd, Duration::from_secs(60)).unwrap();
        let shell = run.child.id();
        let deadline = Instant::now() + Duration::from_secs(20);
        let busy = loop {
            let busy = busiest_in_tree(shell).unwrap();
            if busy != shell && cpu_time(busy).unwrap() > Duration::from_millis(50) {
                break busy;
            }
            assert!(Instant::now() < deadline, "the child never got busy");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(cpu_time(busy).unwrap() > cpu_time(shell).unwrap());
        let (rss, peak) = memory_kib(busy).unwrap();
        assert!(rss > 0 && peak >= rss, "{rss} {peak}");
    }

    #[test]
    fn reports_exit_status() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        let (status, _) = Supervised::spawn(cmd, Duration::from_secs(10))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn dropping_unwaited_kills_the_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let run = Supervised::spawn(cmd, Duration::from_secs(60)).unwrap();
        let pid = run.pid();
        let started = Instant::now();
        drop(run);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "child {pid} still exists"
        );
    }

    #[test]
    fn kills_the_group_at_the_deadline() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & sleep 30"]);
        let started = Instant::now();
        let err = Supervised::spawn(cmd, Duration::from_millis(200))
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(format!("{err}").contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
