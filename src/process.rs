//! Child processes with a deadline and `wait4` resource usage.

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
