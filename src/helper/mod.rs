//! A small Java program run by `app_process`, for the few things only Android's framework
//! can do: the system clipboard, touches where no touchscreen can be read, and telling the
//! system the user is there. It is carried inside the binary, started when first needed, and
//! done without when it does not come up.

use std::fs::Permissions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use crate::input::{Clipboard, Motion};
use crate::{Result, err, utils};

/// Empty in a build without the `helper` feature, which then never starts one.
#[cfg(feature = "helper")]
const DEX: &[u8] = include_bytes!("helper.dex");
#[cfg(not(feature = "helper"))]
const DEX: &[u8] = &[];
const WATCHING: &str = "watching";
const SHELL_ID: u32 = 2000;

type Touches = Box<dyn FnMut(Motion) + Send>;

/// Whom a helper runs as, which decides what Android lets it do.
#[derive(Clone, Copy, PartialEq)]
pub enum User {
    Root,
    /// The one user outside an app that the clipboard answers.
    Shell,
}

/// A helper process, started when it is first asked for something.
pub struct Helper {
    user: User,
    process: Option<Process>,
    failed: bool,
    watching: bool,
}

struct Process {
    child: Child,
    dex: String,
    clips: Receiver<String>,
    status: Receiver<String>,
    up: Arc<AtomicBool>,
    started: Instant,
    used: Instant,
    asked: u32,
    answered: bool,
}

impl Helper {
    pub fn new(user: User) -> Self {
        Self { user, process: None, failed: false, watching: false }
    }

    /// Starts a helper now that reports every touch on the screen to `touches`, and waits for
    /// the system to let it.
    pub fn watch(touches: Touches) -> Result<Self> {
        let process = Process::start(User::Root, Some(touches))?;
        // Where touches come from nowhere else, the helper is given longer to say it has them.
        match process.status.recv_timeout(Duration::from_secs(10)) {
            Ok(status) if status == WATCHING => Ok(Self { user: User::Root, process: Some(process), failed: false, watching: true }),
            Ok(refusal) => Err(err!("{refusal}")),
            Err(_) => Err(err!("the helper did not come up")),
        }
    }

    /// Tells the system the user is active, which keeps the screen from timing out.
    pub fn activity(&mut self) {
        self.send("activity");
    }

    /// Lets go of a helper that has not been needed for a while. It comes back when asked again.
    pub fn rest(&mut self) {
        // A helper nobody has asked anything for half a minute is let go; it holds tens of megabytes.
        if !self.watching && self.process.as_ref().is_some_and(|process| process.used.elapsed() >= Duration::from_secs(30)) {
            // Its leaving takes a moment that the caller, drawing frames, should not wait out.
            let process = self.process.take();
            let _ = utils::spawn("helper", move || drop(process));
        }
    }

    fn process(&mut self) -> Option<&mut Process> {
        if self.process.is_none() && !self.failed {
            self.process = Process::start(self.user, None).inspect_err(|error| log::warn!("{error}")).ok();
            self.failed = self.process.is_none();
        }
        self.process.as_mut()
    }

    /// A helper that went away is started again by the next command, unless it never came up.
    fn send(&mut self, command: &str) -> bool {
        let Some(process) = self.process() else { return false };
        let line = format!("{command}\n");
        let sent = process.child.stdin.as_mut().is_some_and(|commands| commands.write_all(line.as_bytes()).is_ok());
        process.used = Instant::now();
        if !sent {
            self.failed = !process.up.load(Ordering::Relaxed);
            self.process = None;
        }
        sent
    }
}

impl Clipboard for Helper {
    fn prepare(&mut self) {
        if let Some(process) = self.process() {
            process.used = Instant::now();
        }
    }

    fn rest(&mut self) {
        Helper::rest(self);
    }

    fn read(&mut self) -> Option<String> {
        let process = self.process()?;
        process.asked += 1;
        let request = format!("paste {}", process.asked);
        let asked = process.asked;
        // A helper found dead is started again at once, so the paste need not be repeated.
        if !self.send(&request) && !self.send(&request) {
            return None;
        }
        let process = self.process.as_mut()?;
        let deadline = process.patience();
        loop {
            let Ok(reply) = process.clips.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
                // One that has answered before is only slow this once; one that never has is not asked again.
                if !process.answered {
                    log::warn!("the helper does not answer, the system clipboard stays out of reach");
                    let _ = process.child.kill();
                    (self.process, self.failed) = (None, true);
                }
                return None;
            };
            let mut parts = reply.split(' ');
            if parts.next().and_then(utils::parse) == Some(asked) {
                process.answered = true;
                return parts.next().and_then(utils::unhex);
            }
        }
    }

    /// Text travels as hexadecimal UTF-8, which keeps line breaks out of the line protocol.
    fn write(&mut self, text: &str) {
        self.send(&format!("copy {}", utils::hex(text)));
    }
}

impl Process {
    fn start(user: User, touches: Option<Touches>) -> Result<Self> {
        if DEX.is_empty() {
            return Err(err!("this build carries no helper"));
        }
        let dex = format!("/data/local/tmp/.android-egui-{}-{}.dex", std::process::id(), user as u8);
        let unpacked = std::fs::write(&dex, DEX).and_then(|()| std::fs::set_permissions(&dex, Permissions::from_mode(0o644)));
        unpacked.map_err(|error| err!("cannot unpack the helper: {error}"))?;
        let mut command = Command::new("app_process");
        command.env("CLASSPATH", &dex).args(["/system/bin", "egui.Helper"]).args(touches.is_some().then_some("touches"));
        if user == User::Shell {
            command.uid(SHELL_ID).gid(SHELL_ID);
        }
        let started = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        let mut child = started.map_err(|error| {
            let _ = std::fs::remove_file(&dex);
            err!("cannot start the helper: {error}")
        })?;
        let replies = child.stdout.take();
        let (found, clips) = channel();
        let (told, status) = channel();
        let up = Arc::new(AtomicBool::new(false));
        let seen = up.clone();
        let unpacked = dex.clone();
        let _ = utils::spawn("helper", move || listen(replies, &unpacked, touches, &found, told, &seen));
        let now = Instant::now();
        Ok(Self { child, dex, clips, status, up, started: now, used: now, asked: 0, answered: false })
    }

    /// Until when to wait for the helper: it is slow only while it starts. It gets five seconds
    /// to come up and a second to answer once it has. Both are far more than it needs: on a busy
    /// system a paste that waits is better than one that brings the wrong text.
    fn patience(&self) -> Instant {
        (self.started + Duration::from_secs(5)).max(Instant::now() + Duration::from_secs(1))
    }
}

impl Drop for Process {
    /// The helper leaves by itself once its input closes, after doing what it was still asked to.
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let deadline = self.patience();
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Long gone if the helper came up; one that did not has left it behind.
        let _ = std::fs::remove_file(&self.dex);
    }
}

fn listen(replies: Option<impl std::io::Read>, dex: &str, mut touches: Option<Touches>, clips: &Sender<String>, status: Sender<String>, up: &AtomicBool) {
    let mut status = Some(status);
    let mut lines = replies.map(|replies| BufReader::new(replies).lines());
    let mut next = || lines.as_mut()?.next()?.ok();
    // The file is only needed until the helper has loaded it, which its first line shows.
    let first = next();
    let _ = std::fs::remove_file(dex);
    up.store(first.is_some(), Ordering::Relaxed);
    for line in first.into_iter().chain(std::iter::from_fn(next)) {
        if let Some(clip) = line.strip_prefix("clip ") {
            let _ = clips.send(clip.to_owned());
        } else if let (Some(report), Some(motion)) = (&mut touches, Motion::parse(&line)) {
            report(motion);
        } else if line == WATCHING || line.starts_with("error") {
            // Whether touches are being watched is settled by the first of these two.
            if let Some(status) = status.take() {
                let _ = status.send(line.clone());
            }
            if line != WATCHING {
                log::warn!("helper: {line}");
            }
        } else {
            log::debug!("helper: {line}");
        }
    }
}
