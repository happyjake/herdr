use super::harness::*;
use serde_json::{json, Value};

struct Session {
    base: PathBuf,
    socket: PathBuf,
    server: Option<SpawnedHerdr>,
    pane: String,
}

impl Session {
    fn new(kind: &str) -> Self {
        let base = unique_test_dir();
        let socket = base.join("runtime/herdr.sock");
        let server = spawn_herdr_with_config_and_home(
            &base.join("config"),
            &base.join("runtime"),
            &socket,
            None,
            "onboarding = false\n[update]\nversion_check = false\nmanifest_check = false\n",
            Some(&base.join("home")),
        );
        wait_for_socket(&socket, Duration::from_secs(10));
        let created = run_cli_json(
            &socket,
            &["workspace", "create", "--cwd", base.to_str().unwrap()],
        );
        let session = Self {
            pane: created["result"]["root_pane"]["pane_id"]
                .as_str()
                .unwrap()
                .into(),
            base,
            socket,
            server: Some(server),
        };
        session.start_agent(kind);
        session
    }

    fn request(&self, method: &str, params: Value) -> Value {
        let response = send_request(
            &self.socket,
            &json!({"id":"debrief-test","method":method,"params":params}).to_string(),
        );
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn pane(&self) -> Value {
        self.request("pane.get", json!({"pane_id":self.pane}))["pane"].clone()
    }

    fn start_agent(&self, kind: &str) {
        use std::os::unix::fs::PermissionsExt;
        let script = self.base.join(kind);
        // A recognized foreground process exercises the actual guarded prompt
        // delivery path; status reports come from that process's pane.
        let body = r#"#!/bin/sh
export HERDR_AGENT=@KIND@
export HERDR_SOCKET_PATH=@SOCKET@
export DEBRIEF_TEST_DIR=@BASE@
stty -echo
@BIN@ --exact cases::scripted_agent --ignored --nocapture
"#
        .replace("@KIND@", kind)
        .replace("@SOCKET@", &quote(&self.socket.display().to_string()))
        .replace(
            "@BIN@",
            &quote(&std::env::current_exe().unwrap().display().to_string()),
        )
        .replace("@BASE@", &quote(&self.base.display().to_string()));
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let output = run_cli(
            &self.socket,
            &["pane", "run", &self.pane, script.to_str().unwrap()],
        );
        assert!(output.status.success(), "{output:?}");
        crate::support::wait_for_file(&self.base.join("ready"), Duration::from_secs(10));
        self.wait_status("idle");
    }

    fn wait_status(&self, state: &str) {
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(25), || {
                let actual = self.pane()["agent_status"].clone();
                actual == state || (state == "done" && actual == "idle")
            }),
            "pane: {}, reports: {:?}, received: {:?}, logbook: {:?}",
            self.pane(),
            fs::read_to_string(self.base.join("reports")),
            fs::read_to_string(self.base.join("received")),
            self.logbook()
        );
    }

    fn enable(&self, kind: &str, extra: &str) {
        self.configure(&self.config(kind, extra));
    }

    fn config(&self, kind: &str, extra: &str) -> String {
        let skill = self.base.join("debrief.md");
        fs::write(&skill, "Record lessons.").unwrap();
        format!("onboarding = false\n[update]\nversion_check = false\nmanifest_check = false\n[auto_close]\nmode = 'close'\ndormant_age_secs = 0\npass_floor_secs = 1\nuptake_window_secs = 3\nsettle_hold_secs = 2\nquit_grace_secs = 2\ndebrief_skill_path = '{}'\n{extra}\n[auto_close.{kind}]\ngate_command = 'exit 0'\nlimit_secs = 30\n", skill.display())
    }

    fn configure(&self, config: &str) {
        fs::write(
            self.base
                .join("config")
                .join(app_dir_name())
                .join("config.toml"),
            config,
        )
        .unwrap();
        self.request("server.reload_config", json!({}));
    }

    fn logbook(&self) -> Vec<Value> {
        fs::read_to_string(
            self.base
                .join("config")
                .join(app_dir_name())
                .join("logbook.jsonl"),
        )
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
    }

    fn mode(&self, mode: &str) {
        fs::write(self.base.join("mode"), mode).unwrap();
    }

    fn block_input(&self) {
        self.request(
            "agent.prompt",
            json!({"target":self.pane,"text":"fixture:block","preserve_activity":true}),
        );
        crate::support::wait_for_file(&self.base.join("input-blocked"), Duration::from_secs(5));
        for _ in 0..64 {
            self.request(
                "pane.send_text",
                json!({"pane_id":self.pane,"text":"x".repeat(8192),"preserve_activity":true}),
            );
        }
    }

    fn unblock_input(&self) {
        fs::write(self.base.join("unblock-input"), "go").unwrap();
        self.request(
            "pane.send_text",
            json!({"pane_id":self.pane,"text":"\n","preserve_activity":true}),
        );
    }

    fn sent(&self) -> usize {
        self.logbook()
            .iter()
            .filter(|line| line["type"] == "debrief_sent")
            .count()
    }

    fn statuses(&self, status: Option<&str>) -> BufReader<UnixStream> {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(12)))
            .unwrap();
        let mut subscription = json!({"type":"pane.agent_status_changed","pane_id":self.pane});
        if let Some(status) = status {
            subscription["agent_status"] = status.into();
        }
        writeln!(stream, "{}", json!({"id":"status","method":"events.subscribe","params":{"subscriptions":[subscription]}})).unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let ack: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");
        reader
    }

    fn handoff(&mut self) {
        let result = run_cli(&self.socket, &["server", "live-handoff"]);
        assert!(result.status.success(), "{result:?}");
        drop(self.server.take());
        wait_for_socket(&self.socket, Duration::from_secs(10));
    }

    fn another_agent(&self) -> String {
        use std::os::unix::fs::PermissionsExt;
        let directory = self.base.join("second");
        fs::create_dir_all(&directory).unwrap();
        let body = fs::read_to_string(self.base.join("pi")).unwrap().replace(
            &format!(
                "DEBRIEF_TEST_DIR={}",
                quote(&self.base.display().to_string())
            ),
            &format!(
                "DEBRIEF_TEST_DIR={}",
                quote(&directory.display().to_string())
            ),
        );
        let script = directory.join("pi");
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let pane = self.request("tab.create", json!({}))["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        let output = run_cli(
            &self.socket,
            &["pane", "run", &pane, script.to_str().unwrap()],
        );
        assert!(output.status.success(), "{output:?}");
        crate::support::wait_for_file(&directory.join("ready"), Duration::from_secs(10));
        assert!(wait_until(
            Duration::from_secs(10),
            Duration::from_millis(50),
            || self.request("pane.get", json!({"pane_id":pane}))["pane"]["agent_status"] == "idle"
        ));
        pane
    }

    fn restart(&mut self) {
        let config = fs::read_to_string(
            self.base
                .join("config")
                .join(app_dir_name())
                .join("config.toml"),
        )
        .unwrap();
        self.request("server.stop", json!({}));
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(50), || !self
                .socket
                .exists()),
            "test server did not release its endpoint"
        );
        drop(self.server.take());
        self.server = Some(spawn_herdr_with_config_and_home(
            &self.base.join("config"),
            &self.base.join("runtime"),
            &self.socket,
            None,
            &config,
            Some(&self.base.join("home")),
        ));
        wait_for_socket(&self.socket, Duration::from_secs(10));
    }
}

#[test]
#[ignore = "scripted process launched inside a test pane"]
fn scripted_agent() {
    let base = PathBuf::from(std::env::var_os("DEBRIEF_TEST_DIR").unwrap());
    let socket = PathBuf::from(std::env::var_os("HERDR_SOCKET_PATH").unwrap());
    let pane = std::env::var("HERDR_PANE_ID").unwrap();
    let agent = std::env::var("HERDR_AGENT").unwrap();
    let source = if agent == "pi" {
        "herdr:pi"
    } else {
        "custom:debrief-test"
    };
    let mut seq = 0;
    if agent == "pi" {
        let response = send_request(&socket, &json!({"id":"session","method":"pane.report_agent_session","params":{"pane_id":pane,"source":source,"agent":agent,"agent_session_id":"scripted-session","session_start_source":"new","seq":1}}).to_string());
        assert!(response.get("error").is_none(), "{response}");
        seq = 1;
    }
    let mut report = |state: &str| {
        seq += 1;
        let response = send_request(&socket, &json!({"id":"fixture","method":"pane.report_agent","params":{"pane_id":pane,"source":source,"agent":agent,"state":state,"seq":seq,"agent_session_id":"scripted-session"}}).to_string());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join("reports"))
            .unwrap();
        writeln!(file, "{response}").unwrap();
        assert!(response.get("error").is_none(), "{response}");
    };
    println!("debrief v2: 3 memories saved");
    std::io::stdout().flush().unwrap();
    report("idle");
    fs::write(base.join("ready"), "ready").unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let line = line.trim_start_matches('x');
        if line.is_empty() {
            continue;
        }
        if line == "fixture:block" {
            assert!(Command::new("stty")
                .args(["-icanon", "min", "1", "time", "0"])
                .status()
                .unwrap()
                .success());
            fs::write(base.join("input-blocked"), "ready").unwrap();
            while !base.join("unblock-input").exists() {
                thread::sleep(Duration::from_millis(20));
            }
            continue;
        }
        let mut received = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join("received"))
            .unwrap();
        writeln!(received, "{line}").unwrap();
        let mode = fs::read_to_string(base.join("mode")).unwrap_or_default();
        if line == "/quit" {
            if mode == "quit-working" {
                report("working");
                continue;
            }
            if mode == "slow-quit" {
                thread::sleep(Duration::from_secs(11));
            }
            if mode == "quit-ignore" {
                continue;
            }
            break;
        }
        if mode == "no-uptake" {
            continue;
        }
        report("working");
        if mode == "blocked" {
            report("blocked");
            continue;
        }
        if mode == "limit" {
            thread::sleep(Duration::from_secs(3));
            report("idle");
            continue;
        }
        if mode == "hold" {
            while !base.join("release").exists() {
                thread::sleep(Duration::from_millis(100));
            }
        }
        thread::sleep(Duration::from_secs(1));
        if mode == "truncate" {
            for _ in 0..5000 {
                println!("reply output advances the terminal history");
            }
            println!("debrief v2: 3 memories saved\nNOT LANDED: topic — debrief");
        } else {
            println!("debrief v1: 99 memories saved\ndebrief v2: 3 memories saved\nNOT LANDED: topic — debrief");
        }
        std::io::stdout().flush().unwrap();
        report("idle");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.server.is_none() && self.socket.exists() {
            let _ = run_cli(&self.socket, &["server", "stop"]);
        }
        drop(self.server.take());
        cleanup_test_base(&self.base);
    }
}

fn next_status(reader: &mut BufReader<UnixStream>, status: &str) -> Value {
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        if value["data"]["agent_status"] == status {
            return value["data"].clone();
        }
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[test]
fn debrief_prompt_path_reaches_reported_agents_without_moving_activity() {
    for kind in ["claude", "codex", "pi"] {
        let session = Session::new(kind);
        let before = session.pane()["input_or_command_at"].clone();
        thread::sleep(Duration::from_secs(1));
        session.request(
            "agent.prompt",
            json!({"target":session.pane,"text":"debrief-test","preserve_activity":true}),
        );
        session.wait_status("working");
        session.wait_status("done");
        assert_eq!(session.pane()["input_or_command_at"], before);
        assert_eq!(
            fs::read_to_string(session.base.join("received")).unwrap(),
            "debrief-test\n"
        );
        assert!(
            session.pane()["completion_seq"].is_number(),
            "ordinary completion is visible"
        );
    }
}

#[test]
fn debrief_finishes_quietly_then_quits_and_records_the_last_sentinel() {
    let session = Session::new("pi");
    let before = session.pane()["input_or_command_at"].clone();
    session.enable("pi", "");
    session.wait_status("working");
    session.wait_status("done");
    let pane = session.pane();
    assert_eq!(pane.get("completion_seq"), Some(&Value::Null));
    assert_eq!(pane["input_or_command_at"], before);
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || session
            .logbook()
            .iter()
            .any(|event| event["type"] == "pane_closed")
    ));
    let lines = session.logbook();
    let closed: Vec<_> = lines
        .iter()
        .filter(|event| event["type"] == "pane_closed")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["by"], "desk");
    assert_eq!(closed[0]["reason"], "dormant");
    assert_eq!(
        closed[0]["debrief"],
        json!({"outcome":"finished","memories":3,"skill_version":"v2"})
    );
    assert_eq!(
        fs::read_to_string(session.base.join("received")).unwrap(),
        "/skill:debrief\n/quit\n"
    );
    assert!(session.request("pane.list", json!({}))["panes"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn debrief_no_uptake_retries_three_times_then_waits() {
    let mut session = Session::new("codex");
    session.mode("no-uptake");
    session.enable("codex", "");
    assert!(wait_until(
        Duration::from_secs(20),
        Duration::from_millis(100),
        || {
            session.sent() == 3
                && session.request("auto_close.plan", json!({}))["plan"]["candidates"][0]
                    ["wait_reason"]
                    == "no-uptake"
        }
    ));
    thread::sleep(Duration::from_secs(4));
    assert_eq!(session.sent(), 3);
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["waiting"],
        1
    );
    assert_eq!(session.pane()["agent"], "codex");
    session.handoff();
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["waiting"],
        1
    );
    assert_eq!(session.sent(), 3);
}

#[test]
fn debrief_blocked_remains_a_normal_need() {
    let session = Session::new("claude");
    session.mode("blocked");
    session.enable("claude", "");
    session.wait_status("blocked");
    // Blocked needs are identified by their status, not a done sequence.
    assert_eq!(session.pane()["agent_status"], "blocked");
    thread::sleep(Duration::from_secs(4));
    assert_eq!(session.sent(), 1);
    assert_eq!(session.pane()["agent_status"], "blocked");
    assert!(
        session
            .logbook()
            .iter()
            .any(|line| line["type"] == "debrief_outcome"
                && line["outcome"]["outcome"] == "abandoned")
    );
}

#[test]
fn debrief_requires_a_durable_sent_record_before_delivering_input() {
    let session = Session::new("pi");
    let path = session
        .base
        .join("config")
        .join(app_dir_name())
        .join("logbook.jsonl");
    if path.exists() {
        fs::rename(&path, path.with_extension("saved")).unwrap();
    }
    fs::create_dir(&path).unwrap();
    session.enable("pi", "");
    thread::sleep(Duration::from_secs(5));
    assert!(
        !session.base.join("received").exists(),
        "a prompt without a durable sent record can be repeated after restart"
    );
    assert_eq!(session.pane()["agent_status"], "idle");
}

#[test]
fn debrief_working_at_the_limit_retries_without_closing_then_waits() {
    let session = Session::new("pi");
    session.mode("limit");
    session.configure(
        &session
            .config("pi", "")
            .replace("limit_secs = 30", "limit_secs = 2"),
    );
    assert!(wait_until(
        Duration::from_secs(25),
        Duration::from_millis(100),
        || {
            session.sent() == 3
                && session
                    .logbook()
                    .iter()
                    .any(|line| line["state"]["phase"]["phase"] == "waiting")
        }
    ));
    thread::sleep(Duration::from_secs(4));
    assert_eq!(session.sent(), 3);
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["waiting"],
        1
    );
    assert_eq!(
        session.request("auto_close.plan", json!({}))["plan"]["candidates"][0]["wait_reason"],
        "limit"
    );
    assert_eq!(session.pane()["agent"], "pi");
}

#[test]
fn debrief_reconnect_after_answer_has_present_null_on_every_projection() {
    let session = Session::new("codex");
    let mut events = session.statuses(None);
    session.configure(
        &session
            .config("codex", "")
            .replace("settle_hold_secs = 2", "settle_hold_secs = 10"),
    );
    let working = next_status(&mut events, "working");
    assert_eq!(working.get("completion_seq"), Some(&Value::Null));
    let idle = next_status(&mut events, "idle");
    assert_eq!(idle.get("completion_seq"), Some(&Value::Null));
    let mut reconnect = session.statuses(Some("idle"));
    assert_eq!(
        next_status(&mut reconnect, "idle").get("completion_seq"),
        Some(&Value::Null)
    );
    assert_eq!(session.pane().get("completion_seq"), Some(&Value::Null));
    assert_eq!(
        session.request("pane.list", json!({}))["panes"][0].get("completion_seq"),
        Some(&Value::Null)
    );
    assert_eq!(
        session.request("session.snapshot", json!({}))["snapshot"]["panes"][0]
            .get("completion_seq"),
        Some(&Value::Null)
    );
    assert_eq!(
        session.request("agent.get", json!({"target":session.pane}))["agent"].get("completion_seq"),
        Some(&Value::Null)
    );
}

#[test]
fn debrief_live_handoff_waits_for_the_original_reply_without_reprompting() {
    let mut session = Session::new("pi");
    session.mode("hold");
    session.enable("pi", "");
    session.wait_status("working");
    session.handoff();
    thread::sleep(Duration::from_secs(4));
    assert_eq!(session.sent(), 1);
    assert_eq!(
        session.pane()["agent_status"],
        "working",
        "{:?}",
        session.logbook()
    );
    assert_eq!(session.pane().get("completion_seq"), Some(&Value::Null));
    fs::write(session.base.join("release"), "go").unwrap();
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            session
                .logbook()
                .iter()
                .any(|line| line["type"] == "pane_closed")
        }),
        "pane: {}, logbook: {:?}, reports: {:?}",
        session.pane(),
        session.logbook(),
        fs::read_to_string(session.base.join("reports"))
    );
    assert_eq!(session.sent(), 1);
    assert_eq!(
        fs::read_to_string(session.base.join("received")).unwrap(),
        "/skill:debrief\n/quit\n"
    );
}

#[test]
fn debrief_restart_resumes_waiting_without_reprompting_or_closing() {
    let mut session = Session::new("claude");
    session.mode("hold");
    session.enable("claude", "");
    session.wait_status("working");
    session.restart();
    thread::sleep(Duration::from_secs(5));
    assert_eq!(session.sent(), 1);
    assert_eq!(session.pane().get("completion_seq"), Some(&Value::Null));
    assert!(!session
        .logbook()
        .iter()
        .any(|line| line["type"] == "pane_closed"));
    assert_eq!(
        fs::read_to_string(session.base.join("received")).unwrap(),
        "/debrief\n"
    );
}

#[test]
fn debrief_restart_fixture_stops_a_working_reported_agent_without_auto_close() {
    let mut session = Session::new("claude");
    session.mode("hold");
    session.request(
        "agent.prompt",
        json!({"target":session.pane,"text":"fixture-control"}),
    );
    session.wait_status("working");
    session.restart();
    assert_eq!(session.pane()["pane_id"], session.pane);
}

#[test]
fn debrief_waits_for_gates_and_skill_then_closes_when_both_are_available() {
    let session = Session::new("pi");
    let config = session.config("pi", "gate_timeout_secs = 1");
    for (command, reason) in [
        ("exit 1", "gate-closed"),
        ("exec sleep 30", "gate-timeout"),
        ("unavailable-gate-command", "gate-closed"),
    ] {
        session.configure(&config.replace(
            "gate_command = 'exit 0'",
            &format!("gate_command = '{command}'"),
        ));
        let mut plan = Value::Null;
        assert!(
            wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
                plan = session.request("auto_close.plan", json!({}));
                plan["plan"]["candidates"]
                    .as_array()
                    .is_some_and(|candidates| !candidates.is_empty())
            }),
            "candidate did not become eligible: {plan}"
        );
        assert_eq!(
            plan["plan"]["candidates"][0]["wait_reason"], reason,
            "{plan}"
        );
        thread::sleep(Duration::from_secs(2));
        assert_eq!(session.sent(), 0);
        assert!(!session.base.join("received").exists());
        assert_eq!(session.pane()["agent"], "pi");
    }
    fs::remove_file(session.base.join("debrief.md")).unwrap();
    session.configure(&config);
    assert_eq!(
        session.request("auto_close.plan", json!({}))["plan"]["candidates"][0]["wait_reason"],
        "skill-missing"
    );
    thread::sleep(Duration::from_secs(2));
    assert_eq!(session.sent(), 0);
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["waiting"],
        1
    );
    fs::write(session.base.join("debrief.md"), "Record lessons.").unwrap();
    assert!(wait_until(
        Duration::from_secs(12),
        Duration::from_millis(100),
        || session
            .logbook()
            .iter()
            .any(|line| line["type"] == "pane_closed")
    ));
}

#[test]
fn debrief_disabled_and_unsupported_ui_close_with_a_skipped_outcome() {
    for (kind, extra) in [("pi", "debrief = false"), ("agy", "")] {
        let session = Session::new(kind);
        session.enable("pi", extra);
        assert!(wait_until(
            Duration::from_secs(10),
            Duration::from_millis(100),
            || session
                .logbook()
                .iter()
                .any(|line| line["type"] == "pane_closed")
        ));
        let lines = session.logbook();
        let close = lines
            .iter()
            .find(|line| line["type"] == "pane_closed")
            .unwrap();
        assert_eq!(close["debrief"]["outcome"], "skipped");
        assert_eq!(close["reason"], "dormant");
        assert_eq!(session.sent(), 0);
        assert!(!session.base.join("received").exists());
    }
}

#[test]
fn debrief_closes_after_quit_grace_when_the_agent_does_not_exit() {
    let session = Session::new("codex");
    session.mode("quit-ignore");
    let stamp = session.pane()["input_or_command_at"].clone();
    session.configure(
        &session
            .config("codex", "")
            .replace("quit_grace_secs = 2", "quit_grace_secs = 3"),
    );
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || fs::read_to_string(session.base.join("received"))
            .unwrap_or_default()
            .contains("/quit")
    ));
    assert_eq!(session.pane()["input_or_command_at"], stamp);
    assert_eq!(session.pane()["agent"], "codex");
    assert!(wait_until(
        Duration::from_secs(8),
        Duration::from_millis(100),
        || session
            .logbook()
            .iter()
            .any(|line| line["type"] == "pane_closed")
    ));
}

#[test]
fn debrief_handoff_before_uptake_keeps_waiting_without_a_second_prompt() {
    let mut session = Session::new("pi");
    session.mode("no-uptake");
    session.configure(
        &session
            .config("pi", "")
            .replace("limit_secs = 30", "limit_secs = 6"),
    );
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(25),
        || session.base.join("received").exists()
    ));
    session.handoff();
    thread::sleep(Duration::from_secs(9));
    assert_eq!(session.sent(), 1);
    assert!(session
        .logbook()
        .iter()
        .any(|line| line["state"]["phase"]["reason"] == "recovered-limit"));
    assert_eq!(session.pane().get("completion_seq"), Some(&Value::Null));
    assert!(!session
        .logbook()
        .iter()
        .any(|line| line["type"] == "pane_closed"));
}

#[test]
fn debrief_quit_grace_never_closes_a_working_agent_or_repeats_the_skill() {
    let session = Session::new("pi");
    session.mode("quit-working");
    session.enable("pi", "");
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || fs::read_to_string(session.base.join("received"))
            .unwrap_or_default()
            .contains("/quit")
    ));
    thread::sleep(Duration::from_secs(7));
    assert_eq!(session.pane()["agent_status"], "working");
    assert_eq!(session.sent(), 1);
    assert!(!session
        .logbook()
        .iter()
        .any(|line| line["type"] == "pane_closed"));
    assert!(!session
        .logbook()
        .iter()
        .any(|line| line["outcome"]["outcome"] == "abandoned"));
}

#[test]
fn debrief_replay_rearms_the_kind_deadline_before_the_ordinary_pass() {
    let mut session = Session::new("pi");
    session.mode("hold");
    let config = session
        .config("pi", "")
        .replace("limit_secs = 30", "limit_secs = 6");
    session.configure(&config);
    session.wait_status("working");
    session.configure(&config.replace("pass_floor_secs = 1", "pass_floor_secs = 30"));
    session.handoff();
    assert!(wait_until(
        Duration::from_secs(8),
        Duration::from_millis(50),
        || session
            .logbook()
            .iter()
            .any(|line| line["state"]["phase"]["reason"] == "limit")
    ));
    assert_eq!(session.sent(), 1);
}

#[test]
fn debrief_records_a_fresh_identical_sentinel_after_history_truncation() {
    let session = Session::new("pi");
    session.mode("truncate");
    session.enable("pi", "");
    assert!(wait_until(
        Duration::from_secs(15),
        Duration::from_millis(50),
        || session
            .logbook()
            .iter()
            .any(|line| line["type"] == "pane_closed")
    ));
    let lines = session.logbook();
    let close = lines
        .iter()
        .find(|line| line["type"] == "pane_closed")
        .unwrap();
    assert_eq!(
        close["debrief"],
        json!({"outcome":"finished","skill_version":"v2","memories":3})
    );
}

#[test]
fn debrief_slow_quit_preserves_activity_through_the_whole_grace() {
    let session = Session::new("pi");
    session.mode("slow-quit");
    session.configure(
        &session
            .config("pi", "")
            .replace("quit_grace_secs = 2", "quit_grace_secs = 20"),
    );
    assert!(wait_until(
        Duration::from_secs(20),
        Duration::from_millis(50),
        || session
            .logbook()
            .iter()
            .any(|line| line["type"] == "pane_closed")
    ));
    assert_eq!(session.sent(), 1);
}

#[test]
fn debrief_cancelled_backpressured_prompts_cannot_run_after_the_next_agent_starts() {
    let session = Session::new("pi");
    let second = session.another_agent();
    fs::write(session.base.join("second/mode"), "hold").unwrap();
    session.block_input();
    session.enable("pi", "");
    assert!(wait_until(
        Duration::from_secs(20),
        Duration::from_millis(50),
        || session.base.join("second/received").exists()
    ));
    assert_eq!(
        session.request("pane.get", json!({"pane_id":second}))["pane"]["agent_status"],
        "working"
    );
    assert_eq!(
        session
            .logbook()
            .iter()
            .filter(|line| line["type"] == "debrief_sent" && line["pane"] == session.pane)
            .count(),
        3
    );
    session.unblock_input();
    // A marker after the blocked writes proves that the actor drained them.
    session.request(
        "agent.prompt",
        json!({"target":session.pane,"text":"fixture:drained","preserve_activity":true}),
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || fs::read_to_string(session.base.join("received"))
            .unwrap_or_default()
            .contains("fixture:drained")
    ));
    assert!(!fs::read_to_string(session.base.join("received"))
        .unwrap()
        .contains("/skill:debrief"));
}

#[test]
fn debrief_failed_quit_retries_only_quit_and_preserves_the_finished_result() {
    let session = Session::new("claude");
    session.configure(
        &session
            .config("claude", "")
            .replace("settle_hold_secs = 2", "settle_hold_secs = 4"),
    );
    session.wait_status("working");
    session.wait_status("done");
    session.block_input();
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(25),
        || session
            .logbook()
            .iter()
            .any(|line| line["outcome"]["outcome"] == "finished")
    ));
    session.request("pane.report_agent", json!({"pane_id":session.pane,"source":"custom:debrief-test","agent":"claude","state":"working","seq":1000}));
    session.unblock_input();
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        || session
            .logbook()
            .iter()
            .any(|line| line["state"]["phase"]["phase"] == "quit-retry")
    ));
    session.request("pane.report_agent", json!({"pane_id":session.pane,"source":"custom:debrief-test","agent":"claude","state":"idle","seq":1001}));
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || session
            .logbook()
            .iter()
            .any(|line| line["type"] == "pane_closed")
    ));
    assert_eq!(session.sent(), 1);
    let lines = session.logbook();
    assert!(!lines
        .iter()
        .any(|line| line["outcome"]["outcome"] == "abandoned"));
    let close = lines
        .iter()
        .find(|line| line["type"] == "pane_closed")
        .unwrap();
    assert_eq!(
        close["debrief"],
        json!({"outcome":"finished","skill_version":"v2","memories":3})
    );
}

#[test]
fn debrief_reply_boundary_survives_handoff_after_the_answer() {
    let mut session = Session::new("pi");
    session.mode("truncate");
    session.configure(
        &session
            .config("pi", "")
            .replace("settle_hold_secs = 2", "settle_hold_secs = 6"),
    );
    session.wait_status("working");
    session.wait_status("done");
    session.handoff();
    assert!(
        wait_until(Duration::from_secs(25), Duration::from_millis(50), || {
            session
                .logbook()
                .iter()
                .any(|line| line["type"] == "pane_closed")
        }),
        "pane: {}, logbook: {:?}",
        session.pane(),
        session.logbook()
    );
    let lines = session.logbook();
    let close = lines
        .iter()
        .find(|line| line["type"] == "pane_closed")
        .unwrap();
    assert_eq!(
        close["debrief"],
        json!({"outcome":"finished","skill_version":"v2","memories":3})
    );
    assert_eq!(session.sent(), 1);
}

#[test]
fn debrief_runs_one_at_a_time_and_releases_the_slot_after_close() {
    let session = Session::new("pi");
    let second = session.another_agent();
    session.mode("hold");
    fs::write(session.base.join("second/mode"), "hold").unwrap();
    session.enable("pi", "");
    session.wait_status("working");
    thread::sleep(Duration::from_secs(3));
    assert_eq!(session.sent(), 1);
    assert!(!session.base.join("second/received").exists());
    fs::write(session.base.join("release"), "go").unwrap();
    assert!(wait_until(
        Duration::from_secs(12),
        Duration::from_millis(100),
        || session.base.join("second/received").exists()
    ));
    assert_eq!(session.sent(), 2);
    let lines = session.logbook();
    let first_close = lines
        .iter()
        .find(|line| line["type"] == "pane_closed" && line["pane"] == session.pane)
        .unwrap();
    let second_sent = lines
        .iter()
        .find(|line| line["type"] == "debrief_sent" && line["pane"] == second)
        .unwrap();
    assert!(second_sent["at"].as_u64().unwrap() >= first_close["at"].as_u64().unwrap());
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["waiting"],
        0
    );
}

#[test]
fn debrief_disabled_revalidates_the_main_checkout_home_between_closes() {
    let session = Session::new("pi");
    create_committed_repo(&session.base);
    let second = session.another_agent();
    let config = session.config("pi", "debrief = false");
    session.configure(&config.replace("mode = 'close'", "mode = 'report'"));
    let mut plan = Value::Null;
    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
            plan = session.request("auto_close.plan", json!({}))["plan"].clone();
            plan["candidates"].as_array().is_some_and(|candidates| {
                candidates
                    .iter()
                    .filter(|candidate| {
                        (candidate["pane_id"] == session.pane || candidate["pane_id"] == second)
                            && candidate["kind"] == "dormant"
                            && candidate["action"] == "close"
                    })
                    .count()
                    == 2
            })
        }),
        "{plan}"
    );
    let checkout = session.base.canonicalize().unwrap().display().to_string();
    assert!(plan["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|candidate| candidate["main_checkout"] == checkout));
    session.configure(&config);
    assert!(wait_until(
        Duration::from_secs(8),
        Duration::from_millis(50),
        || session
            .logbook()
            .iter()
            .any(|line| line["type"] == "pane_closed")
    ));
    thread::sleep(Duration::from_secs(2));
    let lines = session.logbook();
    let closes: Vec<_> = lines
        .iter()
        .filter(|line| line["type"] == "pane_closed")
        .collect();
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0]["debrief"]["outcome"], "skipped");
    let live = session.request("pane.list", json!({}));
    assert_eq!(live["panes"].as_array().unwrap().len(), 1);
    let survivor = &live["panes"][0];
    assert_eq!(survivor["agent"], "pi");
    assert_ne!(survivor["pane_id"], closes[0]["pane"]);
    let plan = session.request("auto_close.plan", json!({}))["plan"].clone();
    assert_eq!(plan["spared"][0]["pane_id"], survivor["pane_id"]);
    assert_eq!(plan["spared"][0]["exemption"], "last-main-checkout-pane");
    assert_eq!(plan["spared"][0]["main_checkout"], checkout);
    assert_eq!(
        plan["spared"][0]["main_checkout_home"],
        survivor["workspace_id"]
    );
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["closed_24h"],
        1
    );
}

#[test]
fn debrief_finished_close_preserves_a_new_main_checkout_home() {
    let session = Session::new("pi");
    create_committed_repo(&session.base);
    let second = session.another_agent();
    session.mode("hold");
    session.enable("pi", "");
    session.wait_status("working");
    session.request("pane.close", json!({"pane_id":second}));
    fs::write(session.base.join("release"), "go").unwrap();
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || fs::read_to_string(session.base.join("received"))
            .unwrap_or_default()
            .contains("/quit")
    ));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(session.pane()["pane_id"], session.pane);
    assert_eq!(
        session.request("auto_close.plan", json!({}))["plan"]["spared"][0]["exemption"],
        "last-main-checkout-pane"
    );
    assert!(session
        .logbook()
        .iter()
        .all(|line| line["type"] != "pane_closed" || line["pane"] != session.pane));
    // Another pane makes the completed pane closable again. Its saved outcome
    // goes through the same close-time decision, without another skill prompt.
    session.another_agent();
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || session
            .logbook()
            .iter()
            .any(|line| line["type"] == "pane_closed" && line["pane"] == session.pane)
    ));
    let lines = session.logbook();
    let close = lines
        .iter()
        .find(|line| line["type"] == "pane_closed" && line["pane"] == session.pane)
        .unwrap();
    assert_eq!(
        close["debrief"],
        json!({"outcome":"finished","memories":3,"skill_version":"v2"})
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line["type"] == "debrief_sent" && line["pane"] == session.pane)
            .count(),
        1
    );
}

#[test]
fn debrief_disabled_waits_for_metadata_without_blocking_requests() {
    use std::os::unix::fs::OpenOptionsExt;
    let session = Session::new("pi");
    create_committed_repo(&session.base);
    let config = session.base.join(".git/config");
    fs::remove_file(&config).unwrap();
    assert!(Command::new("mkfifo")
        .arg(&config)
        .status()
        .unwrap()
        .success());
    let blocked = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&config)
        .unwrap();
    session.enable("pi", "debrief = false");
    let mut pending_plan = UnixStream::connect(&session.socket).unwrap();
    writeln!(
        pending_plan,
        "{}",
        json!({"id":"pending","method":"auto_close.plan","params":{}})
    )
    .unwrap();
    thread::sleep(Duration::from_secs(2));
    assert_eq!(session.pane()["agent"], "pi");
    assert_eq!(
        session.request("workspace.list", json!({}))["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(session
        .logbook()
        .iter()
        .all(|line| line["type"] != "pane_closed"));
    drop(session);
    drop(blocked);
}
