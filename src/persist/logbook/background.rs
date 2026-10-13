use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use super::{Event, Writer};

pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

enum Job {
    Append(Vec<Event>),
    Barrier(Sender<()>),
    #[cfg(test)]
    Pause {
        entered: Sender<()>,
        resume: Receiver<()>,
    },
}

/// One ordered writer per app. Ordinary senders never wait for disk I/O.
/// Dropping the last sender drains the queue, with a bounded wait so a stalled
/// filesystem cannot prevent shutdown. A timed-out worker is left detached.
pub(crate) struct BackgroundWriter {
    sender: Sender<Job>,
}

impl BackgroundWriter {
    pub(crate) fn start(
        path: PathBuf,
        seed: Vec<Event>,
        live_panes: HashSet<String>,
    ) -> std::io::Result<Self> {
        let (sender, jobs) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("herdr-logbook".into())
            .spawn(move || {
                run(path, seed, live_panes, jobs);
            })?;
        Ok(Self { sender })
    }

    pub(crate) fn append(&self, events: Vec<Event>) {
        self.send(Job::Append(events));
    }

    fn send(&self, job: Job) -> bool {
        if self.sender.send(job).is_ok() {
            true
        } else {
            tracing::warn!("session logbook writer is unavailable");
            false
        }
    }

    /// Wait only at an explicit persistence boundary, never on an event sender.
    pub(crate) fn drain_timeout(&self, timeout: Duration) -> bool {
        let (done, completion) = mpsc::channel();
        self.send(Job::Barrier(done)) && completion.recv_timeout(timeout).is_ok()
    }

    #[cfg(test)]
    pub(crate) fn pause_for_test(&self) -> Sender<()> {
        let (entered, arrival) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        assert!(self.send(Job::Pause { entered, resume }));
        arrival
            .recv_timeout(Duration::from_secs(5))
            .expect("writer reached pause");
        release
    }
}

impl Drop for BackgroundWriter {
    fn drop(&mut self) {
        // The barrier follows every queued append. Dropping sender afterwards
        // ends the receiver loop, even when the bounded wait timed out.
        if !self.drain_timeout(DRAIN_TIMEOUT) {
            tracing::warn!("session logbook writer did not finish before shutdown deadline");
        }
    }
}

fn open(path: &std::path::Path, seed: &[Event], live_panes: &HashSet<String>) -> Option<Writer> {
    match Writer::open(path, seed, live_panes) {
        Ok(writer) => Some(writer),
        Err(err) => {
            tracing::warn!(%err, "failed to open session logbook");
            None
        }
    }
}

fn run(path: PathBuf, seed: Vec<Event>, live_panes: HashSet<String>, jobs: Receiver<Job>) {
    let mut writer = open(&path, &seed, &live_panes);
    for job in jobs {
        match job {
            Job::Append(events) => {
                if writer.is_none() {
                    writer = open(&path, &seed, &live_panes);
                }
                if let Some(writer) = &mut writer {
                    for event in events {
                        if let Err(err) = writer.append(&event) {
                            tracing::warn!(%err, "failed to append session logbook event");
                        }
                    }
                }
            }
            Job::Barrier(done) => {
                let _ = done.send(());
            }
            #[cfg(test)]
            Job::Pause { entered, resume } => {
                let _ = entered.send(());
                let _ = resume.recv();
            }
        }
    }
}
