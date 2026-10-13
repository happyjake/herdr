//! Fresh running-session evidence, kept separate from the cached record catalog.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;

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
}

impl OutsideSessions {
    pub fn get(&self, session: &Session) -> Option<SessionOutside> {
        let agent = session.key.agent.as_str();
        let pid = if agent == "claude" && session.key.kind == SearchSessionKeyKind::Id {
            *self.claude.get(&session.key.value)?
        } else {
            let arguments = self.arguments.get(agent)?;
            *arguments
                .get(&session.key.value)
                .or_else(|| arguments.get(session.record_path.as_ref()?.to_str()?))
                .or_else(|| arguments.get(session.record_id.as_ref()?))?
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

pub(super) fn read(
    home: Option<&Path>,
    processes: &[SessionProcess],
) -> io::Result<OutsideSessions> {
    let mut outside = OutsideSessions::default();
    for process in processes.iter().filter(|process| process.pid > 0) {
        let Some(agent) = process_agent(process) else {
            continue;
        };
        let arguments = outside.arguments.entry(agent).or_default();
        for argument in process.argv.iter().skip(1) {
            let value = argument
                .strip_prefix("--session=")
                .or_else(|| argument.strip_prefix("--session-id="))
                .unwrap_or(argument);
            if !value.starts_with('-') {
                arguments
                    .entry(value.to_string())
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
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 {
            continue;
        }
        let Ok(registry) = serde_json::from_slice::<ClaudeSession>(&bytes) else {
            continue;
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
        Agent::Codex => Some("codex"),
        Agent::Pi => Some("pi"),
        _ => None,
    }
}
