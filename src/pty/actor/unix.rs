use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::{mpsc as std_mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TryRecvError as DataTryRecvError};
use tracing::{debug, warn};

use crate::pty::fd;

// Actor handle methods must call wake_actor() after queuing work. The idle
// timeout is only a fallback for missed wakes; PTY and wake readiness drive
// normal responsiveness.
const ACTOR_IDLE_POLL_MS: i32 = 1000;
const ACTOR_COMMAND_BUFFER: usize = 1024;
const HANDOFF_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorState {
    Running,
    Quiesced,
    Released,
}

pub(crate) struct PtyReadResult {
    pub terminal_responses: Vec<Bytes>,
}

impl PtyReadResult {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            terminal_responses: Vec::new(),
        }
    }
}

type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
type ReaderExitCallback = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PtyResize {
    rows: u16,
    cols: u16,
    cell_width_px: u32,
    cell_height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PtyResizeRequest {
    resize: PtyResize,
    terminal_responses: Vec<Bytes>,
}

#[derive(Default)]
struct SharedPtyControls {
    resize: Option<PtyResizeRequest>,
    terminal_responses: Vec<Bytes>,
}

pub(crate) struct PtyIoActorConfig {
    pub pane_id: u32,
    pub master_fd: OwnedFd,
    pub initially_quiesced: bool,
    pub on_read: ReadCallback,
    pub on_reader_exit: Option<ReaderExitCallback>,
}

enum PtyIoDataCommand {
    WriteUserInput(Bytes),
    SubmitUserInput {
        prefix: Bytes,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        guard: Option<super::PtySubmissionGuard>,
        reply: std_mpsc::Sender<std::io::Result<()>>,
    },
}

enum PtyIoControlCommand {
    BeginHandoff(std_mpsc::Sender<std::io::Result<()>>),
    DuplicateForHandoff(std_mpsc::Sender<std::io::Result<RawFd>>),
    ForegroundProcessGroup(std_mpsc::Sender<Option<u32>>),
    RollbackHandoff(std_mpsc::Sender<std::io::Result<()>>),
    ReleaseAfterCommit(std_mpsc::Sender<std::io::Result<()>>),
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct PtyIoActorHandle {
    data_tx: mpsc::Sender<PtyIoDataCommand>,
    control_tx: std_mpsc::Sender<PtyIoControlCommand>,
    wake: fd::WakeWriter,
    user_writes: Arc<Mutex<UserWriteGate>>,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
    input_guard: crate::platform::PtyInputGuard,
}

#[derive(Debug)]
struct UserWriteGate {
    accepting: bool,
}

impl PtyIoActorHandle {
    pub(crate) fn validate_input_submission(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.input_guard.validate(bytes)
    }

    pub(crate) fn try_write_user_input(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        let user_writes = self
            .user_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !user_writes.accepting {
            return Err(mpsc::error::TrySendError::Closed(bytes));
        }
        match self
            .data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(bytes))
        {
            Ok(()) => {
                self.wake_actor();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(command)) => {
                let PtyIoDataCommand::WriteUserInput(bytes) = command else {
                    unreachable!("queued write returned another command")
                };
                Err(mpsc::error::TrySendError::Full(bytes))
            }
            Err(mpsc::error::TrySendError::Closed(command)) => {
                let PtyIoDataCommand::WriteUserInput(bytes) = command else {
                    unreachable!("queued write returned another command")
                };
                Err(mpsc::error::TrySendError::Closed(bytes))
            }
        }
    }

    pub(crate) fn queue_user_input_submission(
        &self,
        prefix: Bytes,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        guard: Option<super::PtySubmissionGuard>,
    ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
        let user_writes = self
            .user_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !user_writes.accepting {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty actor closed",
            ));
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.data_tx
            .try_send(PtyIoDataCommand::SubmitUserInput {
                prefix,
                text,
                enter,
                delay,
                guard,
                reply: reply_tx,
            })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "pty input queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
                }
            })?;
        self.wake_actor();
        Ok(reply_rx)
    }

    pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
        let _order = self
            .response_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(bytes) = response() else {
            return;
        };
        if !bytes.is_empty() {
            self.controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .terminal_responses
                .push(bytes);
            self.wake_actor();
        }
    }

    pub(crate) fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    ) {
        {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            controls.resize = Some(PtyResizeRequest {
                resize: PtyResize {
                    rows,
                    cols,
                    cell_width_px,
                    cell_height_px,
                },
                terminal_responses,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn begin_handoff(&self, timeout: Duration) -> std::io::Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !user_writes.accepting {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "PTY handoff is already in progress",
                ));
            }
            user_writes.accepting = false;
            if self
                .control_tx
                .send(PtyIoControlCommand::BeginHandoff(reply_tx))
                .is_err()
            {
                user_writes.accepting = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ));
            }
            self.wake_actor();
        }
        match reply_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                let _ = self.rollback_handoff();
                Err(err)
            }
            Err(_) => {
                let _ = self.rollback_handoff();
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for PTY actor to quiesce",
                ))
            }
        }
    }

    pub(crate) fn duplicate_for_handoff(&self) -> std::io::Result<RawFd> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::DuplicateForHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff duplicate",
            )
        })?
    }

    pub(crate) fn foreground_process_group_id(&self) -> Option<u32> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::ForegroundProcessGroup(reply_tx))
            .ok()?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).ok()?
    }

    pub(crate) fn rollback_handoff(&self) -> std::io::Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::RollbackHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        let result = reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff rollback",
            )
        })?;
        if result.is_ok() {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = true;
        }
        result
    }

    pub(crate) fn release_after_commit(&self) -> std::io::Result<()> {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::ReleaseAfterCommit(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY actor release",
            )
        })?
    }

    pub(crate) fn shutdown(&self) {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        if self.control_tx.send(PtyIoControlCommand::Shutdown).is_ok() {
            self.wake_actor();
        }
    }

    fn wake_actor(&self) {
        if let Err(err) = self.wake.wake() {
            debug!(err = %err, "failed to wake PTY actor");
        }
    }
}

pub(crate) struct PtyIoActor;

impl PtyIoActor {
    pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, None)
    }

    fn spawn_inner(
        config: PtyIoActorConfig,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> std::io::Result<PtyIoActorHandle> {
        fd::set_cloexec(config.master_fd.as_raw_fd())?;
        fd::set_nonblocking(config.master_fd.as_raw_fd())?;

        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe()?;
        let user_writes = Arc::new(Mutex::new(UserWriteGate {
            accepting: !config.initially_quiesced,
        }));
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let file = Arc::new(std::fs::File::from(config.master_fd));
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
            input_guard: crate::platform::PtyInputGuard::new(&file),
        };

        let mut runner = PtyIoActorRunner {
            pane_id: config.pane_id,
            file,
            data_rx,
            control_rx,
            state: if config.initially_quiesced {
                ActorState::Quiesced
            } else {
                ActorState::Running
            },
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls,
            response_order,
            on_read: config.on_read,
            on_reader_exit: config.on_reader_exit,
            poll_observer,
        };
        std::thread::Builder::new()
            .name(format!("herdr-pty-{}", config.pane_id))
            .spawn(move || runner.run())
            .map_err(|err| std::io::Error::other(err.to_string()))?;

        Ok(handle)
    }

    #[cfg(test)]
    fn spawn_with_poll_observer(
        config: PtyIoActorConfig,
        poll_observer: std_mpsc::Sender<()>,
    ) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, Some(poll_observer))
    }
}

struct PtyIoActorRunner {
    pane_id: u32,
    file: Arc<std::fs::File>,
    data_rx: mpsc::Receiver<PtyIoDataCommand>,
    control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
    state: ActorState,
    pending_writes: VecDeque<PendingWrite>,
    current_write_offset: usize,
    active_submission: Option<ActiveSubmission>,
    pending_handoff: Option<std_mpsc::Sender<std::io::Result<()>>>,
    wake_read_fd: OwnedFd,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
    on_read: ReadCallback,
    on_reader_exit: Option<ReaderExitCallback>,
    poll_observer: Option<std_mpsc::Sender<()>>,
}

struct ActiveSubmission {
    enter: Bytes,
    delay: Duration,
    phase: SubmissionPhase,
    reply: std_mpsc::Sender<std::io::Result<()>>,
}

#[derive(Debug, PartialEq, Eq)]
struct PendingWrite {
    bytes: Bytes,
    boundary: Option<SubmissionBoundary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmissionBoundary {
    Text,
    Enter,
}

enum SubmissionPhase {
    PendingDelivery {
        prefix: Bytes,
        text: Bytes,
        guard: Option<super::PtySubmissionGuard>,
    },
    WritingText {
        uncommitted_guard: Option<super::PtySubmissionGuard>,
    },
    WaitingUntil(Instant),
    WritingEnter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingSubmissionStart {
    NoChange,
    Started,
    Rejected,
}

impl PtyIoActorRunner {
    fn enqueue_write(&mut self, bytes: Bytes) {
        if !bytes.is_empty() {
            self.pending_writes.push_back(PendingWrite {
                bytes,
                boundary: None,
            });
        }
    }

    fn enqueue_submission_write(&mut self, bytes: Bytes, boundary: SubmissionBoundary) {
        if !bytes.is_empty() {
            self.pending_writes.push_back(PendingWrite {
                bytes,
                boundary: Some(boundary),
            });
        }
    }

    fn start_pending_submission_delivery(&mut self) -> PendingSubmissionStart {
        if !self.pending_writes.is_empty() {
            return PendingSubmissionStart::NoChange;
        }
        let Some(mut submission) = self.active_submission.take() else {
            return PendingSubmissionStart::NoChange;
        };
        let SubmissionPhase::PendingDelivery {
            prefix,
            text,
            mut guard,
        } = submission.phase
        else {
            self.active_submission = Some(submission);
            return PendingSubmissionStart::NoChange;
        };
        let phase = match (prefix.is_empty(), text.is_empty()) {
            (true, true) => {
                if let Some(guard) = guard.as_mut() {
                    if let Err(err) = guard() {
                        let _ = submission.reply.send(Err(err));
                        return PendingSubmissionStart::Rejected;
                    }
                }
                SubmissionPhase::WaitingUntil(Instant::now() + submission.delay)
            }
            (false, true) => {
                self.enqueue_submission_write(prefix, SubmissionBoundary::Text);
                SubmissionPhase::WritingText {
                    uncommitted_guard: guard,
                }
            }
            (true, false) => {
                self.enqueue_submission_write(text, SubmissionBoundary::Text);
                SubmissionPhase::WritingText {
                    uncommitted_guard: guard,
                }
            }
            (false, false) => {
                self.enqueue_write(prefix);
                self.enqueue_submission_write(text, SubmissionBoundary::Text);
                SubmissionPhase::WritingText {
                    uncommitted_guard: guard,
                }
            }
        };
        submission.phase = phase;
        self.active_submission = Some(submission);
        PendingSubmissionStart::Started
    }

    fn run(&mut self) {
        let mut should_exit = false;
        while !should_exit {
            should_exit = self.drain_commands();
            if should_exit || self.state == ActorState::Released {
                break;
            }
            let had_active_submission = self.active_submission.is_some();

            self.apply_pending_controls();

            if self.start_pending_submission_delivery() == PendingSubmissionStart::Rejected {
                continue;
            }

            if !self.pending_writes.is_empty() {
                match self.flush_pending_writes_once() {
                    Ok(Some(boundary)) => self.complete_submission_boundary(boundary),
                    Ok(None) => {}
                    Err(err) => {
                        self.fail_active_submission(err);
                        break;
                    }
                }
            }
            if self.start_pending_submission_delivery() != PendingSubmissionStart::NoChange {
                continue;
            }
            self.schedule_submission_enter();
            if (had_active_submission && self.active_submission.is_none())
                || (self.active_submission.is_none() && self.pending_handoff.is_some())
            {
                continue;
            }

            if let Some(poll_observer) = &self.poll_observer {
                let _ = poll_observer.send(());
            }

            match fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                self.state == ActorState::Running,
                !self.pending_writes.is_empty(),
                self.poll_timeout_ms(),
            ) {
                Ok(readiness) => {
                    if readiness.wake_ready {
                        if let Err(err) = fd::drain_wake_fd(self.wake_read_fd.as_raw_fd()) {
                            debug!(pane = self.pane_id, err = %err, "PTY actor wake drain failed");
                            break;
                        }
                        continue;
                    }
                    if self.state == ActorState::Running
                        && readiness.pty_read_ready
                        && !self.read_once()
                    {
                        break;
                    }
                    if readiness.pty_write_ready && !self.pending_writes.is_empty() {
                        match self.flush_pending_writes_once() {
                            Ok(Some(boundary)) => self.complete_submission_boundary(boundary),
                            Ok(None) => {}
                            Err(err) => {
                                self.fail_active_submission(err);
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    debug!(pane = self.pane_id, err = %err, "PTY actor poll failed");
                    break;
                }
            }
        }

        self.close_input_queue();
        if let Some(on_reader_exit) = self.on_reader_exit.take() {
            on_reader_exit();
        }
        debug!(pane = self.pane_id, "PTY actor exiting");
    }

    fn drain_commands(&mut self) -> bool {
        if self.drain_control_commands() {
            return true;
        }
        if self.active_submission.is_some() {
            return false;
        }
        if let Some(reply) = self.pending_handoff.take() {
            self.defer_or_begin_handoff(reply);
            return false;
        }
        self.drain_data_commands()
    }

    fn drain_control_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.control_rx.try_recv() {
                Ok(command) => {
                    if self.handle_control_command(command) {
                        should_exit = true;
                        break;
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn drain_data_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.data_rx.try_recv() {
                Ok(command) => {
                    if self.handle_data_command(command) {
                        should_exit = true;
                        break;
                    }
                    if self.active_submission.is_some() {
                        break;
                    }
                }
                Err(DataTryRecvError::Empty) => break,
                Err(DataTryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn handle_data_command(&mut self, command: PtyIoDataCommand) -> bool {
        match command {
            PtyIoDataCommand::WriteUserInput(bytes) => {
                if self.state == ActorState::Running {
                    self.enqueue_write(bytes);
                }
            }
            PtyIoDataCommand::SubmitUserInput {
                prefix,
                text,
                enter,
                delay,
                guard,
                reply,
            } => {
                if self.state == ActorState::Running {
                    self.active_submission = Some(ActiveSubmission {
                        enter,
                        delay,
                        phase: SubmissionPhase::PendingDelivery {
                            prefix,
                            text,
                            guard,
                        },
                        reply,
                    });
                } else {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "pty actor is not accepting input",
                    )));
                }
            }
        }
        false
    }

    fn handle_control_command(&mut self, command: PtyIoControlCommand) -> bool {
        match command {
            PtyIoControlCommand::BeginHandoff(reply) => {
                self.defer_or_begin_handoff(reply);
            }
            PtyIoControlCommand::DuplicateForHandoff(reply) => {
                let result = if self.state == ActorState::Quiesced {
                    fd::duplicate_cloexec_fd(self.file.as_raw_fd())
                } else {
                    Err(std::io::Error::other(
                        "PTY actor must be quiesced before handoff duplication",
                    ))
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::ForegroundProcessGroup(reply) => {
                let result =
                    crate::platform::foreground_process_group_id_for_tty_fd(self.file.as_raw_fd());
                let _ = reply.send(result);
            }
            PtyIoControlCommand::RollbackHandoff(reply) => {
                self.pending_handoff.take();
                let result = if self.state == ActorState::Released {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "PTY actor was released before handoff rollback",
                    ))
                } else {
                    self.state = ActorState::Running;
                    Ok(())
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::ReleaseAfterCommit(reply) => {
                self.state = ActorState::Released;
                self.pending_writes.clear();
                let _ = reply.send(Ok(()));
                return true;
            }
            PtyIoControlCommand::Shutdown => return true,
        }
        false
    }

    fn defer_or_begin_handoff(&mut self, reply: std_mpsc::Sender<std::io::Result<()>>) {
        if self.active_submission.is_none() {
            self.drain_pre_quiesce_commands();
        }
        if self.active_submission.is_some() {
            self.pending_handoff = Some(reply);
        } else {
            let result = self.begin_handoff();
            let _ = reply.send(result);
        }
    }

    fn begin_handoff(&mut self) -> std::io::Result<()> {
        self.drain_pre_quiesce_commands();
        if self.active_submission.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "PTY input submission is still in progress",
            ));
        }
        self.apply_pending_controls();
        if self.state == ActorState::Released {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "PTY actor was released before handoff quiesce",
            ));
        }
        let deadline = Instant::now() + HANDOFF_DRAIN_TIMEOUT;
        let _ = self.flush_pending_writes_once()?;
        while !self.pending_writes.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out draining PTY writes before handoff",
                ));
            }
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let readiness = fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                true,
                true,
                timeout_ms,
            )?;
            if readiness.wake_ready {
                fd::drain_wake_fd(self.wake_read_fd.as_raw_fd())?;
            }
            if readiness.pty_read_ready && !self.read_once() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "PTY closed while draining writes before handoff",
                ));
            }
            if readiness.pty_write_ready {
                let _ = self.flush_pending_writes_once()?;
            }
        }
        self.state = ActorState::Quiesced;
        Ok(())
    }

    fn drain_pre_quiesce_commands(&mut self) {
        while let Ok(command) = self.data_rx.try_recv() {
            if self.handle_data_command(command) {
                break;
            }
            if self.active_submission.is_some() {
                break;
            }
        }
    }

    fn apply_pending_controls(&mut self) {
        let (resize, terminal_responses) = {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                controls.resize.take(),
                std::mem::take(&mut controls.terminal_responses),
            )
        };
        if self.state == ActorState::Released {
            return;
        }
        if let Some(request) = resize {
            self.resize(request.resize);
            self.enqueue_terminal_responses(request.terminal_responses);
        }
        self.enqueue_terminal_responses(terminal_responses);
    }

    fn read_once(&mut self) -> bool {
        let mut buf = [0u8; 8192];
        match self.file.as_ref().read(&mut buf) {
            Ok(0) => false,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
            Err(err) => {
                debug!(pane = self.pane_id, err = %err, "PTY actor read failed");
                false
            }
            Ok(n) => {
                let response_order = Arc::clone(&self.response_order);
                let _order = response_order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let result = (self.on_read)(&buf[..n]);
                self.controls
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .terminal_responses
                    .extend(result.terminal_responses);
                drop(_order);
                let terminal_responses = std::mem::take(
                    &mut self
                        .controls
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .terminal_responses,
                );
                self.enqueue_terminal_responses(terminal_responses);
                true
            }
        }
    }

    fn enqueue_terminal_responses(&mut self, terminal_responses: Vec<Bytes>) {
        if self.state == ActorState::Released {
            return;
        }
        for bytes in terminal_responses {
            self.enqueue_write(bytes);
        }
    }

    fn complete_submission_boundary(&mut self, boundary: SubmissionBoundary) {
        match boundary {
            SubmissionBoundary::Text => {
                let Some(submission) = self.active_submission.as_mut() else {
                    return;
                };
                debug_assert!(matches!(
                    submission.phase,
                    SubmissionPhase::WritingText {
                        uncommitted_guard: None
                    }
                ));
                submission.phase = SubmissionPhase::WaitingUntil(Instant::now() + submission.delay);
            }
            SubmissionBoundary::Enter => {
                let Some(submission) = self.active_submission.take() else {
                    return;
                };
                debug_assert!(matches!(submission.phase, SubmissionPhase::WritingEnter));
                let _ = submission.reply.send(Ok(()));
            }
        }
    }

    fn schedule_submission_enter(&mut self) {
        let Some(ActiveSubmission {
            enter,
            phase: SubmissionPhase::WaitingUntil(deadline),
            ..
        }) = self.active_submission.as_ref()
        else {
            return;
        };
        if Instant::now() >= *deadline {
            let enter = enter.clone();
            if enter.is_empty() {
                let submission = self.active_submission.take().unwrap();
                let _ = submission.reply.send(Ok(()));
            } else {
                self.active_submission.as_mut().unwrap().phase = SubmissionPhase::WritingEnter;
                self.enqueue_submission_write(enter, SubmissionBoundary::Enter);
            }
        }
    }

    fn poll_timeout_ms(&self) -> i32 {
        let Some(ActiveSubmission {
            phase: SubmissionPhase::WaitingUntil(deadline),
            ..
        }) = self.active_submission.as_ref()
        else {
            return ACTOR_IDLE_POLL_MS;
        };
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1)
            .min(ACTOR_IDLE_POLL_MS as u128) as i32
    }

    fn fail_active_submission(&mut self, err: std::io::Error) {
        if let Some(submission) = self.active_submission.take() {
            let _ = submission.reply.send(Err(err));
        }
    }

    fn check_uncommitted_submission_guard(&mut self) -> bool {
        let Some(ActiveSubmission {
            phase:
                SubmissionPhase::WritingText {
                    uncommitted_guard: Some(guard),
                },
            ..
        }) = self.active_submission.as_mut()
        else {
            return true;
        };
        debug_assert_eq!(self.current_write_offset, 0);
        match guard() {
            Ok(()) => true,
            Err(err) => {
                while let Some(write) = self.pending_writes.pop_front() {
                    if write.boundary == Some(SubmissionBoundary::Text) {
                        break;
                    }
                }
                self.current_write_offset = 0;
                self.fail_active_submission(err);
                false
            }
        }
    }

    fn commit_active_submission_write(&mut self) {
        if let Some(ActiveSubmission {
            phase: SubmissionPhase::WritingText { uncommitted_guard },
            ..
        }) = self.active_submission.as_mut()
        {
            *uncommitted_guard = None;
        }
    }

    fn close_input_queue(&mut self) {
        self.data_rx.close();
        self.fail_active_submission(input_submission_closed_error());
        while let Some(command) = self.data_rx.blocking_recv() {
            if let PtyIoDataCommand::SubmitUserInput { reply, .. } = command {
                let _ = reply.send(Err(input_submission_closed_error()));
            }
        }
    }

    fn flush_pending_writes_once(&mut self) -> std::io::Result<Option<SubmissionBoundary>> {
        while !self.pending_writes.is_empty() {
            if !self.check_uncommitted_submission_guard() {
                continue;
            }
            let (write_len, write_result) = {
                let write = self.pending_writes.front().unwrap();
                let chunk = &write.bytes[self.current_write_offset..];
                (write.bytes.len(), self.file.as_ref().write(chunk))
            };
            match write_result {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "PTY actor write returned zero bytes",
                    ));
                }
                Ok(written) => {
                    self.commit_active_submission_write();
                    self.current_write_offset += written;
                    if self.current_write_offset >= write_len {
                        let completed = self.pending_writes.pop_front().unwrap();
                        self.current_write_offset = 0;
                        if let Some(boundary) = completed.boundary {
                            self.file.as_ref().flush()?;
                            return Ok(Some(boundary));
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
                Err(err) => {
                    warn!(pane = self.pane_id, err = %err, "PTY actor write failed");
                    self.pending_writes.clear();
                    self.current_write_offset = 0;
                    return Err(err);
                }
            }
        }
        self.file.as_ref().flush()?;
        Ok(None)
    }

    fn resize(&self, resize: PtyResize) {
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.rows,
            resize.cols,
            resize.cell_width_px,
            resize.cell_height_px,
        ));
    }

    fn log_resize_result(&self, result: std::io::Result<()>) {
        if let Err(err) = result {
            debug!(pane = self.pane_id, err = %err, "PTY resize failed");
        }
    }
}

fn input_submission_closed_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "PTY actor closed during input submission",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd, IntoRawFd},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
    };

    fn test_wake_pair() -> (fd::WakeWriter, OwnedFd) {
        let pipe = fd::create_wake_pipe().expect("wake pipe");
        (pipe.writer, pipe.read_fd)
    }

    fn actor_with_socket_pair(
        initially_quiesced: bool,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        actor_with_socket_pair_and_poll_observer(initially_quiesced, None)
    }

    fn actor_with_socket_pair_and_poll_observer(
        initially_quiesced: bool,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        };
        let handle = if let Some(poll_observer) = poll_observer {
            PtyIoActor::spawn_with_poll_observer(config, poll_observer)
        } else {
            PtyIoActor::spawn(config)
        }
        .expect("actor spawn");
        (handle, peer, read_rx)
    }

    fn actor_runner_for_unit_test() -> (PtyIoActorRunner, UnixStream) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (_data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let runner = PtyIoActorRunner {
            pane_id: 1,
            file: Arc::new(std::fs::File::from(owned)),
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            poll_observer: None,
        };
        (runner, peer)
    }

    #[test]
    fn actor_ignores_empty_user_input_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();

        assert!(!runner.handle_data_command(PtyIoDataCommand::WriteUserInput(Bytes::new())));

        assert!(runner.pending_writes.is_empty());
    }

    #[test]
    fn submission_boundary_does_not_wait_for_following_protocol_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();
        runner.enqueue_submission_write(Bytes::from_static(b"prompt"), SubmissionBoundary::Text);
        runner.enqueue_write(Bytes::from_static(b"response"));

        assert_eq!(
            runner.flush_pending_writes_once().unwrap(),
            Some(SubmissionBoundary::Text)
        );
        assert_eq!(
            runner.pending_writes[0].bytes,
            Bytes::from_static(b"response")
        );
    }

    #[test]
    fn actor_writes_user_input_to_owned_fd() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);

        handle
            .try_write_user_input(Bytes::from_static(b"hello"))
            .expect("write command accepted");

        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives write");
        assert_eq!(&buf, b"hello");
        handle.shutdown();
    }

    #[test]
    fn actor_delays_enter_from_completed_prompt_write() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let text = Bytes::from(vec![b'x'; 4 * 1024 * 1024]);
        let text_len = text.len();
        let delay = Duration::from_millis(200);
        let reader = std::thread::spawn(move || {
            std::thread::sleep(delay);
            let mut received = vec![0; text_len];
            peer.read_exact(&mut received)
                .expect("peer receives prompt");
            let prompt_completed = Instant::now();
            let mut enter = [0; 1];
            peer.read_exact(&mut enter).expect("peer receives enter");
            let enter_received = Instant::now();
            let mut user = [0; 4];
            peer.read_exact(&mut user)
                .expect("peer receives queued input");
            (prompt_completed, enter_received, enter, user)
        });

        let completion = handle
            .queue_user_input_submission(Bytes::new(), text, Bytes::from_static(b"\r"), delay, None)
            .expect("submission queues");
        handle
            .try_write_user_input(Bytes::from_static(b"user"))
            .expect("ordinary input queues behind submission");
        completion
            .recv()
            .expect("actor reports submission")
            .expect("submission completes");
        let (prompt_completed, enter_received, enter, user) = reader.join().expect("reader joins");

        assert_eq!(enter, *b"\r");
        assert_eq!(user, *b"user");
        assert!(enter_received.duration_since(prompt_completed) >= delay / 2);

        let err = match handle.queue_user_input_submission(
            Bytes::new(),
            Bytes::from_static(b"prompt"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
            None,
        ) {
            Ok(completion) => completion
                .recv()
                .expect("actor reports submission")
                .expect_err("closed PTY rejects submission"),
            Err(err) => err,
        };

        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::WriteZero
        ));
    }

    #[test]
    fn actor_completes_empty_submission_parts() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let completion = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::new(),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                None,
            )
            .expect("empty prompt submission queues");
        let mut enter = [0; 1];
        peer.read_exact(&mut enter)
            .expect("peer receives enter for empty prompt");
        assert_eq!(enter, *b"\r");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty prompt submission")
            .expect("empty prompt submission completes");

        let completion = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"prompt"),
                Bytes::new(),
                Duration::from_millis(40),
                None,
            )
            .expect("empty enter submission queues");
        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_millis(250)));
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt)
            .expect("peer receives prompt before empty enter");
        assert_eq!(&prompt, b"prompt");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty enter submission")
            .expect("empty enter submission completes");
        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff resumes without an idle poll after submission");
        handle.shutdown();
    }

    #[test]
    fn actor_reports_peer_closure_during_submission_delay() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let completion = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
                None,
            )
            .expect("submission queues");
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        drop(peer);

        let err = completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports peer closure")
            .expect_err("peer closure fails the active submission");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_fails_buffered_submissions_on_exit() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let active = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"first"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
                None,
            )
            .expect("first submission queues");
        let mut prompt = [0; 5];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        let buffered = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"second"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                None,
            )
            .expect("second submission queues");

        drop(peer);
        let active_err = active
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports active submission")
            .expect_err("peer closure fails active submission");
        let buffered_err = buffered
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports buffered submission")
            .expect_err("peer closure fails buffered submission");

        assert_eq!(active_err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(buffered_err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_evaluates_submission_guard_at_deferred_delivery_boundary() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let first = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"first"),
                Bytes::from_static(b"\r"),
                Duration::from_millis(80),
                None,
            )
            .expect("first submission queues");

        let allow_second = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard_flag = Arc::clone(&allow_second);
        let second = handle
            .queue_user_input_submission(
                Bytes::from_static(b"\x1b[I"),
                Bytes::from_static(b"second"),
                Bytes::from_static(b"\r"),
                Duration::from_millis(80),
                Some(Box::new(move || {
                    if guard_flag.load(std::sync::atomic::Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(super::super::submission_rejected_error(
                            r#"{"id":"req","error":{"code":"agent_busy","message":"busy"}}"#.into(),
                        ))
                    }
                })),
            )
            .expect("second submission queues while first is in flight");

        // Flip the condition while the first submission is still holding the actor.
        allow_second.store(false, std::sync::atomic::Ordering::SeqCst);

        let third = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"third"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                None,
            )
            .expect("third submission queues behind second");

        first
            .recv_timeout(Duration::from_secs(1))
            .expect("first submission reports")
            .expect("first submission succeeds");
        let second_err = second
            .recv_timeout(Duration::from_secs(1))
            .expect("second submission reports")
            .expect_err("second submission is rejected by guard");
        assert!(super::super::submission_rejected_response(&second_err)
            .unwrap()
            .contains("agent_busy"));
        third
            .recv_timeout(Duration::from_secs(1))
            .expect("third submission reports")
            .expect("third submission succeeds");

        let mut received = [0u8; 12];
        peer.read_exact(&mut received)
            .expect("peer receives only first and third submissions");
        assert_eq!(&received, b"first\rthird\r");
        handle.shutdown();
    }

    #[test]
    fn actor_revalidates_submission_guard_after_initial_write_would_block() {
        let (mut actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let fill = [0xAA; 8192];
        let mut prefilled = 0;
        loop {
            match actor_socket.write(&fill) {
                Ok(written) => prefilled += written,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("failed to fill actor write buffer: {err}"),
            }
        }
        assert!(prefilled > 0, "actor write buffer should accept some bytes");

        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn_with_poll_observer(
            PtyIoActorConfig {
                pane_id: 1,
                master_fd: owned,
                initially_quiesced: false,
                on_read: Box::new(|_| PtyReadResult::empty()),
                on_reader_exit: None,
            },
            poll_tx,
        )
        .expect("actor spawn");

        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered initial poll");

        let allow = Arc::new(AtomicBool::new(true));
        let guard_allow = Arc::clone(&allow);
        let guard_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let guard_calls_counter = Arc::clone(&guard_calls);
        let (guard_entered_tx, guard_entered_rx) = std_mpsc::channel();
        let guarded = handle
            .queue_user_input_submission(
                Bytes::from_static(b"\x1b[I"),
                Bytes::from_static(b"guarded"),
                Bytes::from_static(b"\r"),
                Duration::from_millis(20),
                Some(Box::new(move || {
                    guard_calls_counter.fetch_add(1, Ordering::SeqCst);
                    let _ = guard_entered_tx.send(());
                    if guard_allow.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(super::super::submission_rejected_error(
                            r#"{"id":"req","error":{"code":"agent_busy","message":"busy"}}"#.into(),
                        ))
                    }
                })),
            )
            .expect("guarded submission queues");

        guard_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("guard runs on initial write attempt");
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor returns to poll after initial WouldBlock");
        assert_eq!(guard_calls.load(Ordering::SeqCst), 1);

        allow.store(false, Ordering::SeqCst);

        let mut drained = vec![0u8; prefilled];
        peer.read_exact(&mut drained)
            .expect("peer drains prefill buffer");
        assert!(drained.iter().all(|byte| *byte == 0xAA));

        let err = guarded
            .recv_timeout(Duration::from_secs(1))
            .expect("guarded submission reports")
            .expect_err("guarded submission is rejected after initial WouldBlock");
        assert!(super::super::submission_rejected_response(&err)
            .unwrap()
            .contains("agent_busy"));
        assert_eq!(guard_calls.load(Ordering::SeqCst), 2);

        let sentinel = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"sentinel"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                None,
            )
            .expect("sentinel submission queues after guarded rejection");
        sentinel
            .recv_timeout(Duration::from_secs(1))
            .expect("sentinel submission reports")
            .expect("sentinel submission completes");

        let mut after_drain = [0u8; 9];
        peer.read_exact(&mut after_drain)
            .expect("peer receives only sentinel submission");
        assert_eq!(&after_drain, b"sentinel\r");
        handle.shutdown();
    }

    #[test]
    fn actor_delivers_guarded_submission_when_condition_still_holds_after_initial_would_block() {
        let (mut actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let fill = [0xAA; 8192];
        let mut prefilled = 0;
        loop {
            match actor_socket.write(&fill) {
                Ok(written) => prefilled += written,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("failed to fill actor write buffer: {err}"),
            }
        }
        assert!(prefilled > 0, "actor write buffer should accept some bytes");

        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn_with_poll_observer(
            PtyIoActorConfig {
                pane_id: 1,
                master_fd: owned,
                initially_quiesced: false,
                on_read: Box::new(|_| PtyReadResult::empty()),
                on_reader_exit: None,
            },
            poll_tx,
        )
        .expect("actor spawn");

        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered initial poll");

        let allow = Arc::new(AtomicBool::new(true));
        let guard_allow = Arc::clone(&allow);
        let guard_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let guard_calls_counter = Arc::clone(&guard_calls);
        let (guard_entered_tx, guard_entered_rx) = std_mpsc::channel();
        let guarded = handle
            .queue_user_input_submission(
                Bytes::from_static(b"\x1b[I"),
                Bytes::from_static(b"guarded"),
                Bytes::from_static(b"\r"),
                Duration::from_millis(60),
                Some(Box::new(move || {
                    guard_calls_counter.fetch_add(1, Ordering::SeqCst);
                    let _ = guard_entered_tx.send(());
                    if guard_allow.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(super::super::submission_rejected_error(
                            r#"{"id":"req","error":{"code":"agent_busy","message":"busy"}}"#.into(),
                        ))
                    }
                })),
            )
            .expect("guarded submission queues");

        guard_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("guard runs on initial write attempt");
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor returns to poll after initial WouldBlock");
        assert_eq!(guard_calls.load(Ordering::SeqCst), 1);

        let mut drained = vec![0u8; prefilled];
        peer.read_exact(&mut drained)
            .expect("peer drains prefill buffer");
        assert!(drained.iter().all(|byte| *byte == 0xAA));

        let mut prefix_and_text = [0u8; 10];
        peer.read_exact(&mut prefix_and_text)
            .expect("peer receives prefix and text once socket drains");
        assert_eq!(&prefix_and_text, b"\x1b[Iguarded");
        assert_eq!(guard_calls.load(Ordering::SeqCst), 2);

        // Once the first byte has been written, the submission is committed and
        // flipping the guard condition must not abort the remaining bytes or Enter.
        allow.store(false, Ordering::SeqCst);

        let mut enter = [0u8; 1];
        peer.read_exact(&mut enter)
            .expect("peer receives delayed enter for committed submission");
        assert_eq!(&enter, b"\r");

        guarded
            .recv_timeout(Duration::from_secs(1))
            .expect("guarded submission reports")
            .expect("guarded submission completes once committed");
        assert_eq!(guard_calls.load(Ordering::SeqCst), 2);

        let unguarded = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from_static(b"unguarded"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                None,
            )
            .expect("unguarded submission queues");
        unguarded
            .recv_timeout(Duration::from_secs(1))
            .expect("unguarded submission reports")
            .expect("unguarded submission completes");

        let mut unguarded_bytes = [0u8; 10];
        peer.read_exact(&mut unguarded_bytes)
            .expect("peer receives unguarded submission");
        assert_eq!(&unguarded_bytes, b"unguarded\r");
        handle.shutdown();
    }

    #[test]
    fn actor_finishes_a_guarded_prompt_after_a_partial_write_commits() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let send_buffer: libc::c_int = 4096;
        // SAFETY: the socket is owned here and the option points to a live integer.
        let configured = unsafe {
            libc::setsockopt(
                actor_socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&send_buffer as *const libc::c_int).cast(),
                std::mem::size_of_val(&send_buffer) as libc::socklen_t,
            )
        };
        assert_eq!(configured, 0, "set bounded socket buffer");
        let mut actual_buffer: libc::c_int = 0;
        let mut option_len = std::mem::size_of_val(&actual_buffer) as libc::socklen_t;
        // SAFETY: both output pointers are writable and have the declared sizes.
        let queried = unsafe {
            libc::getsockopt(
                actor_socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&mut actual_buffer as *mut libc::c_int).cast(),
                &mut option_len,
            )
        };
        assert_eq!(queried, 0, "read bounded socket buffer");
        assert!(actual_buffer > 0 && actual_buffer < 1024 * 1024);
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("peer timeout");
        // SAFETY: into_raw_fd transfers the descriptor's sole ownership.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (poll_tx, _poll_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn_with_poll_observer(
            PtyIoActorConfig {
                pane_id: 1,
                master_fd: owned,
                initially_quiesced: false,
                on_read: Box::new(|_| PtyReadResult::empty()),
                on_reader_exit: None,
            },
            poll_tx,
        )
        .expect("actor spawn");
        let allow = Arc::new(AtomicBool::new(true));
        let guard_allow = Arc::clone(&allow);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let guard_calls = Arc::clone(&calls);
        const TEXT_LEN: usize = 2 * 1024 * 1024;
        let completion = handle
            .queue_user_input_submission(
                Bytes::new(),
                Bytes::from(vec![b'x'; TEXT_LEN]),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                Some(Box::new(move || {
                    guard_calls.fetch_add(1, Ordering::SeqCst);
                    if guard_allow.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(super::super::submission_rejected_error(
                            r#"{"id":"partial","error":{"code":"agent_busy","message":"changed"}}"#
                                .into(),
                        ))
                    }
                })),
            )
            .expect("guarded prompt queues");

        let mut first = [0];
        peer.read_exact(&mut first)
            .expect("first prompt byte commits");
        assert_eq!(first, [b'x']);
        let calls_at_commit = calls.load(Ordering::SeqCst);
        assert!(calls_at_commit > 0, "the delivery guard must have run");
        allow.store(false, Ordering::SeqCst);

        let mut remainder = vec![0; TEXT_LEN];
        peer.read_exact(&mut remainder)
            .expect("a committed partial prompt must finish with Enter");
        assert!(remainder[..TEXT_LEN - 1].iter().all(|byte| *byte == b'x'));
        assert_eq!(remainder[TEXT_LEN - 1], b'\r');
        completion
            .recv_timeout(Duration::from_secs(5))
            .expect("submission reports")
            .expect("committed submission completes");
        assert_eq!(calls.load(Ordering::SeqCst), calls_at_commit);
        handle.shutdown();
    }

    #[test]
    fn actor_rejects_submission_after_io_loop_exits() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let handle_slot = Arc::new(Mutex::new(None::<PtyIoActorHandle>));
        let (attempt_tx, attempt_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced: false,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: Some(Box::new({
                let handle_slot = Arc::clone(&handle_slot);
                move || {
                    let handle = handle_slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_ref()
                        .expect("actor handle installed")
                        .clone();
                    let attempt = handle.queue_user_input_submission(
                        Bytes::new(),
                        Bytes::from_static(b"prompt"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                        None,
                    );
                    attempt_tx.send(attempt).expect("attempt receiver alive");
                }
            })),
        };
        let handle = PtyIoActor::spawn(config).expect("actor spawn");
        *handle_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);

        drop(peer);
        let err = match attempt_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader exit callback attempts submission")
        {
            Ok(completion) => completion
                .recv_timeout(Duration::from_secs(1))
                .expect("actor reports submission")
                .expect_err("closed actor rejects submission"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_wakes_idle_poll_for_user_input() {
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let (handle, mut peer, _read_rx) =
            actor_with_socket_pair_and_poll_observer(false, Some(poll_tx));
        peer.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("peer timeout");
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered idle poll");

        let start = Instant::now();
        handle
            .try_write_user_input(Bytes::from_static(b"x"))
            .expect("write command accepted");

        let mut buf = [0u8; 1];
        peer.read_exact(&mut buf)
            .expect("peer receives write without waiting for actor poll timeout");
        assert_eq!(&buf, b"x");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "actor write should be driven by wake fd, not the idle poll timeout"
        );
        handle.shutdown();
    }

    #[test]
    fn actor_reads_output_while_input_is_backpressured() {
        let (mut actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let fill = [0xAA; 8192];
        let mut prefilled = 0;
        loop {
            match actor_socket.write(&fill) {
                Ok(written) => prefilled += written,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("failed to fill actor write buffer: {err}"),
            }
        }
        assert!(prefilled > 0, "actor write buffer should accept some bytes");

        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced: false,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        })
        .expect("actor spawn");

        let marker = Bytes::from_static(b"queued-input");
        let completion = handle
            .queue_user_input_submission(
                Bytes::new(),
                marker.clone(),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                None,
            )
            .expect("submission accepted");

        const OUTPUT_LEN: usize = 128 * 1024;
        let mut peer_writer = peer.try_clone().expect("clone peer writer");
        let output_writer = std::thread::spawn(move || {
            peer_writer
                .write_all(&vec![0xBB; OUTPUT_LEN])
                .expect("peer writes sustained output");
        });
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut output_len = 0;
        while output_len < OUTPUT_LEN {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "actor did not keep reading blocked peer output"
            );
            let output = read_rx
                .recv_timeout(remaining)
                .expect("actor keeps reading while input remains blocked");
            assert!(output.iter().all(|byte| *byte == 0xBB));
            output_len += output.len();
        }
        assert_eq!(output_len, OUTPUT_LEN);
        output_writer.join().expect("output writer joins");

        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_secs(1)));

        let mut received_input = vec![0; prefilled + marker.len() + 1];
        peer.read_exact(&mut received_input)
            .expect("peer receives prefill and queued input");
        assert!(received_input[..prefilled].iter().all(|byte| *byte == 0xAA));
        assert_eq!(
            &received_input[prefilled..prefilled + marker.len()],
            marker.as_ref()
        );
        assert_eq!(received_input.last(), Some(&b'\r'));
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports submission")
            .expect("submission completes");
        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff waits for submission");
        handle.shutdown();
    }

    #[test]
    fn actor_wakes_idle_poll_for_handoff_control() {
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let (handle, _peer, _read_rx) =
            actor_with_socket_pair_and_poll_observer(false, Some(poll_tx));
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered idle poll");

        let start = Instant::now();
        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_secs(1)));

        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff control should wake idle actor");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "handoff control should be driven by wake fd, not the idle poll timeout"
        );
        handle.shutdown();
    }

    #[test]
    fn poll_ignores_pty_hup_without_pty_interest() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        drop(peer);
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");

        let readiness = fd::poll_pty_and_wake(
            actor_socket.as_raw_fd(),
            wake_pipe.read_fd.as_raw_fd(),
            false,
            false,
            10,
        )
        .expect("poll succeeds");

        assert!(!readiness.pty_read_ready);
        assert!(!readiness.pty_write_ready);
        assert!(!readiness.wake_ready);
    }

    #[test]
    fn actor_delivers_fd_reads_to_callback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        peer.write_all(b"from-peer").expect("peer write");

        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor read callback");
        assert_eq!(read, Bytes::from_static(b"from-peer"));
        handle.shutdown();
    }

    #[test]
    fn begin_handoff_stops_reads_and_rejects_user_writes_until_rollback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        let err = handle
            .begin_handoff(Duration::from_secs(1))
            .expect_err("concurrent handoff rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        peer.write_all(b"held").expect("peer write during quiesce");
        assert!(
            read_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "actor must not read while quiesced"
        );

        handle.rollback_handoff().expect("rollback resumes actor");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reads held bytes after rollback");
        assert_eq!(read, Bytes::from_static(b"held"));

        handle
            .try_write_user_input(Bytes::from_static(b"after"))
            .expect("write accepted after rollback");
        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives after");
        assert_eq!(&buf, b"after");
        handle.shutdown();
    }

    #[test]
    fn duplicate_for_handoff_requires_quiesced_actor() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        assert!(handle.duplicate_for_handoff().is_err());
        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        let duplicate = handle
            .duplicate_for_handoff()
            .expect("handoff duplicate created");
        assert!(duplicate >= 0);
        unsafe {
            libc::close(duplicate);
        }
        handle.rollback_handoff().expect("rollback resumes actor");

        peer.write_all(b"still-live").expect("peer write");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor still reads after duplicate closes");
        assert_eq!(read, Bytes::from_static(b"still-live"));
        handle.shutdown();
    }

    #[tokio::test]
    async fn input_preflight_preserves_handoff_and_shutdown_write_gates() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let runtime = crate::terminal::TerminalRuntime::test_with_actor(handle.clone());
        handle.begin_handoff(Duration::from_secs(1)).unwrap();
        // A successful preflight is not permission to bypass the existing gate.
        runtime.validate_input_submission(b"blocked").unwrap();
        assert!(runtime
            .try_send_bytes(Bytes::from_static(b"blocked"))
            .is_err());
        handle.rollback_handoff().unwrap();
        runtime.validate_input_submission(b"after").unwrap();
        runtime
            .try_send_bytes(Bytes::from_static(b"after"))
            .unwrap();
        let mut bytes = [0; 5];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"after");
        handle.shutdown();
        assert!(runtime
            .try_send_bytes(Bytes::from_static(b"closed"))
            .is_err());
    }

    #[test]
    fn resize_keeps_latest_request_when_command_queue_is_full() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"fill",
            )))
            .expect("fill command queue");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::clone(&controls),
            response_order: Arc::new(Mutex::new(())),
            input_guard: Default::default(),
        };

        handle.resize(20, 80, 8, 16, vec![Bytes::from_static(b"old")]);
        handle.resize(40, 120, 9, 18, vec![Bytes::from_static(b"new")]);
        handle.write_terminal_response(|| Some(Bytes::from_static(b"response")));

        let controls = controls.lock().expect("controls lock");
        assert_eq!(
            controls.resize,
            Some(PtyResizeRequest {
                resize: PtyResize {
                    rows: 40,
                    cols: 120,
                    cell_width_px: 9,
                    cell_height_px: 18,
                },
                terminal_responses: vec![Bytes::from_static(b"new")],
            })
        );
        assert_eq!(
            controls.terminal_responses,
            vec![Bytes::from_static(b"response")]
        );
    }

    #[test]
    fn appearance_transition_report_precedes_query_of_new_scheme() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let light = Arc::new(AtomicBool::new(false));
        let query_light = Arc::clone(&light);
        let runner = PtyIoActorRunner {
            pane_id: 1,
            file: Arc::new(std::fs::File::from(owned)),
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
            on_read: Box::new(move |_| PtyReadResult {
                terminal_responses: vec![if query_light.load(Ordering::Acquire) {
                    Bytes::from_static(b"query-light")
                } else {
                    Bytes::from_static(b"query-dark")
                }],
            }),
            on_reader_exit: None,
            poll_observer: None,
        };
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls,
            response_order,
            input_guard: crate::platform::PtyInputGuard::new(&runner.file),
        };
        let (changed_tx, changed_rx) = std_mpsc::channel();
        let (continue_tx, continue_rx) = std_mpsc::channel();

        let appearance = std::thread::spawn(move || {
            handle.write_terminal_response(|| {
                light.store(true, Ordering::Release);
                changed_tx.send(()).expect("notify appearance change");
                continue_rx.recv().expect("continue appearance report");
                Some(Bytes::from_static(b"live-light"))
            });
        });
        changed_rx.recv().expect("appearance changed");
        peer.write_all(b"query").expect("write query");
        let reader = std::thread::spawn(move || {
            let mut runner = runner;
            assert!(runner.read_once());
            runner
        });
        continue_tx.send(()).expect("release appearance report");
        appearance.join().expect("appearance thread joins");
        let runner = reader.join().expect("reader thread joins");

        assert_eq!(
            runner.pending_writes,
            VecDeque::from([
                PendingWrite {
                    bytes: Bytes::from_static(b"live-light"),
                    boundary: None,
                },
                PendingWrite {
                    bytes: Bytes::from_static(b"query-light"),
                    boundary: None,
                },
            ])
        );
    }

    #[test]
    fn resize_writes_terminal_responses_after_applying_resize() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let response = Bytes::from_static(b"\x1B[48;40;100;720;900t");

        handle.resize(40, 100, 9, 18, vec![response.clone()]);

        let mut buf = vec![0; response.len()];
        peer.read_exact(&mut buf)
            .expect("peer receives resize response");
        assert_eq!(Bytes::from(buf), response);
        handle.shutdown();
    }

    #[test]
    fn handoff_control_is_not_blocked_by_full_data_queue() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"fill",
            )))
            .expect("fill data queue");
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            input_guard: Default::default(),
        };

        let handoff = std::thread::spawn(move || handle.begin_handoff(Duration::from_secs(1)));
        match control_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handoff control command")
        {
            PtyIoControlCommand::BeginHandoff(reply) => {
                reply.send(Ok(())).expect("handoff waiter alive");
            }
            _ => panic!("expected begin handoff command"),
        }

        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff succeeds despite full data queue");
    }

    #[test]
    fn begin_handoff_drains_user_writes_already_in_command_queue() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"queued-before-ack",
            )))
            .expect("queued write");
        let mut runner = PtyIoActorRunner {
            pane_id: 1,
            file: Arc::new(std::fs::File::from(unsafe {
                OwnedFd::from_raw_fd(actor_socket.into_raw_fd())
            })),
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: fd::create_wake_pipe().expect("wake pipe").read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            poll_observer: None,
        };

        runner.begin_handoff().expect("handoff drains queued write");

        let mut buf = [0u8; 17];
        peer.read_exact(&mut buf)
            .expect("queued write reaches peer before quiesce ack");
        assert_eq!(&buf, b"queued-before-ack");
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    #[test]
    fn release_after_commit_prevents_further_io() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        handle.release_after_commit().expect("actor released");
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        let _ = peer.write_all(b"ignored");
        assert!(read_rx.recv_timeout(Duration::from_millis(150)).is_err());
    }
}
