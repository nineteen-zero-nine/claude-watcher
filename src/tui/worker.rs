//! The background thread that keeps the interface's data fresh. Reading a
//! long transcript takes a noticeable fraction of a second, so none of it
//! happens on the thread that draws and reads keys.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use crate::hooks::{self, HookReport};
use crate::live::{self, LiveSession, Snapshot, TotalsCache};
use crate::paths::{self, Chosen};

/// How often the followed transcript is checked for changes.
const TICK: Duration = Duration::from_secs(1);
/// Sessions and hooks are refreshed every this many ticks.
const SLOW_EVERY: u32 = 3;

/// The session whose context is shown.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// Whichever session of the project was active most recently.
    Latest,
    Transcript(PathBuf),
}

pub enum Command {
    Follow(Target),
    /// Reload everything now.
    Refresh,
}

pub enum Update {
    Context(Result<Box<Snapshot>, String>),
    Sessions(Vec<LiveSession>),
    Hooks(Box<HookReport>),
}

/// Start the worker. It stops when either end of its channels is dropped.
pub fn spawn(project: PathBuf, target: Target) -> (Sender<Command>, Receiver<Update>) {
    let (commands, inbox) = mpsc::channel();
    let (outbox, updates) = mpsc::channel();
    thread::spawn(move || run(project, target, inbox, outbox));
    (commands, updates)
}

fn run(project: PathBuf, mut target: Target, inbox: Receiver<Command>, outbox: Sender<Update>) {
    let mut cache = TotalsCache::default();
    // What the last context update was made from, to send one only on change.
    let mut shown = None;
    let mut failure: Option<String> = None;
    let mut force = true;
    let mut tick = 0;

    loop {
        let chosen = match &target {
            Target::Latest => paths::resolve_session(None, &project),
            Target::Transcript(path) => Ok(Chosen { path: path.clone(), note: None }),
        };
        let update = match chosen {
            Ok(chosen) => {
                let now = live::fingerprint(&chosen.path).map(|print| (chosen.path.clone(), print));
                (force || now != shown).then(|| {
                    shown = now;
                    failure = None;
                    Snapshot::take(chosen, &mut cache).map(Box::new).map_err(|error| format!("{error:#}"))
                })
            }
            Err(error) => {
                let error = format!("{error:#}");
                (force || failure.as_ref() != Some(&error)).then(|| {
                    shown = None;
                    failure = Some(error.clone());
                    Err(error)
                })
            }
        };
        let mut delivered = update.is_none_or(|update| outbox.send(Update::Context(update)).is_ok());

        if force || tick % SLOW_EVERY == 0 {
            delivered &= outbox.send(Update::Hooks(Box::new(hooks::collect(&project)))).is_ok();
            delivered &= outbox.send(Update::Sessions(live::sessions(&mut cache))).is_ok();
        }
        if !delivered {
            return;
        }

        force = false;
        tick += 1;
        match inbox.recv_timeout(TICK) {
            Ok(Command::Follow(next)) => {
                target = next;
                force = true;
            }
            Ok(Command::Refresh) => force = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
