//! Fresh running-session evidence, kept separate from the cached record catalog.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::agent_resume::AgentSessionRef;
use crate::api::schema::{SearchSessionKeyKind, SessionOutside};
use crate::detect::{self, Agent};
use crate::platform::{ForegroundJob, ForegroundProcess, SessionProcess};

use super::records::Session;

#[derive(Default)]
pub(super) struct OutsideSessions {
    claude: HashMap<String, u32>,
    arguments: HashMap<&'static str, HashMap<String, u32>>,
    paths: HashMap<&'static str, HashMap<PathBuf, u32>>,
}

impl OutsideSessions {
    pub fn get(&self, session: &Session) -> Option<SessionOutside> {
        let agent = session.key.agent.as_str();
        let pid = if agent == "claude" && session.key.kind == SearchSessionKeyKind::Id {
            *self.claude.get(&session.key.value)?
        } else {
            let arguments = self.arguments.get(agent)?;
            let id = match session.key.kind {
                SearchSessionKeyKind::Id => arguments.get(&session.key.value),
                _ => None,
            };
            *id.or_else(|| arguments.get(session.record_id.as_ref()?))
                .or_else(|| {
                    self.paths
                        .get(agent)?
                        .get(&normalize_path(session.record_path.as_ref()?, None)?)
                })?
        };
        Some(SessionOutside {
            pid,
            detail: format!("{agent} pid {pid}"),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSession {
    pid: u32,
    session_id: String,
    proc_start: String,
}

enum RegistryRead {
    Complete(ClaudeSession),
    Torn,
    Invalid,
}

fn read_registry(path: &Path, retry_sleep: &dyn Fn(Duration)) -> io::Result<RegistryRead> {
    for attempt in 0..3 {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RegistryRead::Invalid)
            }
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 {
            return Ok(RegistryRead::Invalid);
        }
        match serde_json::from_slice::<ClaudeSession>(&bytes) {
            Ok(registry) => return Ok(RegistryRead::Complete(registry)),
            // A complete JSON value with the wrong schema is not a torn write.
            Err(error) if error.is_data() => return Ok(RegistryRead::Invalid),
            Err(_) => {}
        }
        if attempt < 2 {
            retry_sleep(Duration::from_millis(20));
        }
    }
    Ok(RegistryRead::Torn)
}

pub(super) fn read(
    home: Option<&Path>,
    processes: &[SessionProcess],
    retry_sleep: &dyn Fn(Duration),
) -> io::Result<OutsideSessions> {
    let mut outside = OutsideSessions::default();
    for process in processes.iter().filter(|process| process.pid > 0) {
        let Some(agent) = process_agent(process) else {
            continue;
        };
        let Some((value, accepts_path)) = owning_argument(process, agent) else {
            continue;
        };
        let arguments = outside.arguments.entry(agent).or_default();
        arguments
            .entry(value.to_string())
            .and_modify(|pid| *pid = (*pid).min(process.pid))
            .or_insert(process.pid);
        let path = Path::new(value);
        if accepts_path
            && (path.is_absolute()
                || path.components().count() > 1
                || path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl"))
        {
            if let Some(path) = normalize_path(path, process.cwd.as_deref()) {
                outside
                    .paths
                    .entry(agent)
                    .or_default()
                    .entry(path)
                    .and_modify(|pid| *pid = (*pid).min(process.pid))
                    .or_insert(process.pid);
            }
        }
    }
    let Some(home) = home else { return Ok(outside) };
    let entries = match std::fs::read_dir(home.join(".claude/sessions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(outside),
        Err(error) => return Err(error),
    };
    let processes: HashMap<_, _> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json")
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let Some(process) = processes.get(&pid) else {
            continue;
        };
        let registry = match read_registry(&path, retry_sleep)? {
            RegistryRead::Complete(registry) => registry,
            RegistryRead::Invalid => continue,
            RegistryRead::Torn => {
                let reference = (process_agent(process) == Some("claude"))
                    .then(|| {
                        option_identity(&process.argv[1..], &["--resume", "-r", "--session-id"])
                    })
                    .flatten()
                    .map(|(_, value)| value)
                    .and_then(AgentSessionRef::id);
                if let Some(reference) = reference {
                    outside
                        .claude
                        .entry(reference.value)
                        .and_modify(|previous| *previous = (*previous).min(pid))
                        .or_insert(pid);
                } else {
                    // Emit one event after all retries, not one for each torn read.
                    tracing::debug!(
                        pid,
                        "torn running-session registry has no argv session identity"
                    );
                }
                continue;
            }
        };
        if registry.pid != pid
            || registry.proc_start.trim().is_empty()
            || !registry
                .proc_start
                .split_ascii_whitespace()
                .eq(process.started_at.split_ascii_whitespace())
        {
            continue;
        }
        let Some(reference) = AgentSessionRef::id(registry.session_id) else {
            continue;
        };
        outside
            .claude
            .entry(reference.value)
            .and_modify(|previous| *previous = (*previous).min(pid))
            .or_insert(pid);
    }
    Ok(outside)
}

fn normalize_path(path: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd?.join(path)
    };
    if !absolute.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Some(normalized)
}

fn owning_argument<'a>(process: &'a SessionProcess, agent: &str) -> Option<(&'a str, bool)> {
    let args = process.argv.get(1..)?;
    match agent {
        "pi" => {
            if args
                .iter()
                .take_while(|arg| *arg != "--")
                .any(|arg| arg == "--fork" || arg.starts_with("--fork=") || arg == "--no-session")
            {
                return None;
            }
            // --resume/-r select a session interactively; their following text
            // is not a session identity. Only explicit session operands count.
            option_identity(args, &["--session", "--session-id"])
                .map(|(flag, value)| (value, flag == "--session"))
        }
        "codex" => {
            let executable = Path::new(process.argv.first()?).file_name()?.to_str()?;
            let args = if matches!(executable, "node" | "bun") {
                args.get(1..)?
            } else {
                args
            };
            let [command, identity, ..] = args else {
                return None;
            };
            (command == "resume" && !identity.starts_with('-')).then_some((identity.as_str(), true))
        }
        _ => None,
    }
}

fn option_identity<'a>(args: &'a [String], flags: &[&str]) -> Option<(&'a str, &'a str)> {
    let mut args = args.iter().peekable();
    let mut identity = None;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if flags.contains(&arg.as_str()) {
            if args.peek().is_some_and(|value| !value.starts_with('-')) {
                identity = args.next().map(|value| (arg.as_str(), value.as_str()));
            }
        } else if let Some((flag, value)) = arg.split_once('=') {
            if flags.contains(&flag) && !value.is_empty() {
                identity = Some((flag, value));
            }
        }
    }
    identity
}

fn process_agent(process: &SessionProcess) -> Option<&'static str> {
    let argv0 = process.argv.first()?;
    let job = ForegroundJob {
        process_group_id: process.pid,
        processes: vec![ForegroundProcess {
            pid: process.pid,
            name: Path::new(argv0).file_name()?.to_str()?.to_string(),
            argv0: Some(argv0.clone()),
            argv: Some(process.argv.clone()),
            cmdline: None,
        }],
    };
    match detect::identify_agent_in_job(&job)?.0 {
        Agent::Claude => Some("claude"),
        Agent::Codex => Some("codex"),
        Agent::Pi => Some("pi"),
        _ => None,
    }
}
