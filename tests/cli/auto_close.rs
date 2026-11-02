use super::harness::*;
use serde_json::{json, Value};

fn spawn_isolated(base: &Path, socket: &Path, config: &str) -> SpawnedHerdr {
    let directory = toml::Value::String(base.join("worktrees").display().to_string());
    spawn_herdr_with_config_and_home(
        &base.join("config"), &base.join("runtime"), socket, None,
        &format!("onboarding = false\n[update]\nversion_check = false\nmanifest_check = false\n[worktrees]\ndirectory = {directory}\n{config}"),
        Some(&base.join("home")),
    )
}

struct Session {
    base: PathBuf,
    socket: PathBuf,
    server: Option<SpawnedHerdr>,
    pane: String,
}

impl Session {
    fn new(config: &str) -> Self {
        let base = unique_test_dir();
        let socket = base.join("runtime/herdr.sock");
        let server = spawn_isolated(&base, &socket, config);
        wait_for_socket(&socket, Duration::from_secs(10));
        let created = run_cli_json(
            &socket,
            &["workspace", "create", "--cwd", base.to_str().unwrap()],
        );
        Self {
            pane: created["result"]["root_pane"]["pane_id"]
                .as_str()
                .unwrap()
                .into(),
            base,
            socket,
            server: Some(server),
        }
    }

    fn plan(&self) -> Value {
        run_cli_json(&self.socket, &["auto-close", "plan"])["result"]["plan"].clone()
    }

    fn request(&self, method: &str, params: Value) -> Value {
        let response = send_request(
            &self.socket,
            &json!({"id":"auto-close", "method":method, "params":params}).to_string(),
        );
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn reports(&self) -> Vec<Value> {
        fs::read_to_string(
            self.base
                .join("config")
                .join(app_dir_name())
                .join("logbook.jsonl"),
        )
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["type"] == "would_close")
        .collect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.server.take());
        cleanup_test_base(&self.base);
    }
}

#[test]
fn auto_close_reports_at_the_shell_age_without_closing_or_repeating() {
    let session = Session::new(
        "[auto_close]\nmode = 'report'\nplain_shell_age_secs = 3\npass_floor_secs = 1\n",
    );
    let mut plan = Value::Null;
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
            plan = session.plan();
            plan["spared"][0]["exemption"] == "too-young"
        }),
        "{plan}"
    );
    assert!(session.reports().is_empty());
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(100),
        || !session.reports().is_empty()
    ));
    let plan = session.plan();
    let candidate = &plan["candidates"][0];
    assert_eq!(candidate["pane_id"], session.pane);
    assert_eq!(candidate["kind"], "plain-shell");
    assert_eq!(candidate["reason"], "plain-shell");
    assert_eq!(candidate["action"], "close");
    assert!(candidate["age_secs"].as_u64().unwrap() >= 3);
    thread::sleep(Duration::from_secs(3));
    let lines = session.reports();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["by"], "desk");
    assert_eq!(lines[0]["reason"], "plain-shell");
    assert_eq!(
        session.request("pane.get", json!({"pane_id":session.pane}))["pane"]["pane_id"],
        session.pane
    );
    assert_eq!(
        session.request("ping", json!({}))["auto_close"],
        json!({"mode":"report","waiting":0,"closed_24h":1})
    );
}

#[test]
fn auto_close_off_is_silent_and_config_reloads() {
    let session = Session::new("[auto_close]\nplain_shell_age_secs = 0\npass_floor_secs = 1\n");
    assert!(session
        .request("ping", json!({}))
        .get("auto_close")
        .is_none());
    thread::sleep(Duration::from_secs(2));
    assert!(session.reports().is_empty());
    let config = session
        .base
        .join("config")
        .join(app_dir_name())
        .join("config.toml");
    fs::write(&config, "onboarding = false\n[auto_close]\nmode = 'report'\nplain_shell_age_secs = 0\npass_floor_secs = 1\n").unwrap();
    session.request("server.reload_config", json!({}));
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["mode"],
        "report"
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.reports().len() == 1
    ));
    // Invalid durations retain the previous section, rather than silently
    // disabling the rule or interpreting a fractional age.
    fs::write(
        &config,
        "[auto_close]\nmode = 'off'\nplain_shell_age_secs = 1.5\n",
    )
    .unwrap();
    session.request("server.reload_config", json!({}));
    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["mode"],
        "report"
    );
    fs::write(&config, "[auto_close]\nmode = 'off'\n").unwrap();
    session.request("server.reload_config", json!({}));
    assert!(session
        .request("ping", json!({}))
        .get("auto_close")
        .is_none());
}

impl Session {
    fn agent(&self, kind: &str) -> crate::support::InPaneReporter {
        let agent = crate::support::InPaneReporter::write(&self.base, kind, &self.socket);
        let output = run_cli(&self.socket, &["pane", "run", &self.pane, &agent.command()]);
        assert!(output.status.success(), "{output:?}");
        agent.wait_until_started(Duration::from_secs(10));
        assert!(wait_until(
            Duration::from_secs(10),
            Duration::from_millis(50),
            || self.request("pane.get", json!({"pane_id":self.pane}))["pane"]["agent"] == kind
        ));
        self.request(
            "pane.report_agent",
            json!({"pane_id":self.pane,"source":"custom:test","agent":kind,"state":"idle","seq":1}),
        );
        assert!(wait_until(
            Duration::from_secs(10),
            Duration::from_millis(50),
            || {
                let pane = self.request("pane.get", json!({"pane_id":self.pane}));
                matches!(pane["pane"]["agent_status"].as_str(), Some("done" | "idle"))
            }
        ));
        agent
    }
}

#[test]
fn auto_close_dormant_waits_for_a_missing_skill_and_counts_waiting() {
    let session = Session::new("[auto_close]\nmode = 'report'\ndormant_age_secs = 2\npass_floor_secs = 1\ndebrief_skill_path = '/nonexistent/debrief/SKILL.md'\n");
    let agent = session.agent("pi");
    assert_eq!(session.plan()["spared"][0]["exemption"], "too-young");
    let mut candidate = Value::Null;
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(100),
        || {
            candidate = session.plan()["candidates"][0].clone();
            candidate["kind"] == "dormant"
        }
    ));
    assert_eq!(candidate["action"], "wait");
    assert_eq!(candidate["wait_reason"], "skill-missing");
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(100),
        || session.request("ping", json!({}))["auto_close"]["waiting"] == 1
    ));
    assert_eq!(
        session.request("pane.get", json!({"pane_id":session.pane}))["pane"]["agent"],
        "pi"
    );
    agent.stop();
}

#[test]
fn auto_close_gate_exit_status_and_timeout_control_the_plan() {
    let session =
        Session::new("[auto_close]\nmode = 'report'\ndormant_age_secs = 0\npass_floor_secs = 1\n");
    let skill = session.base.join("debrief.md");
    fs::write(&skill, "Record lessons.").unwrap();
    let agent = session.agent("pi");
    let config_path = session
        .base
        .join("config")
        .join(app_dir_name())
        .join("config.toml");
    for (command, action, reason) in [
        ("exit 0", "debrief", None),
        ("exit 1", "wait", Some("gate-closed")),
        ("exec sleep 30", "wait", Some("gate-timeout")),
        ("command-that-does-not-exist", "wait", Some("gate-closed")),
    ] {
        fs::write(&config_path, format!("[auto_close]\nmode = 'report'\ndormant_age_secs = 0\npass_floor_secs = 1\ndebrief_skill_path = '{}'\ngate_timeout_secs = 1\n[auto_close.pi]\ngate_command = '{}'\ninvocation = '/skill:debrief'\nlimit_secs = 600\n", skill.display(), command)).unwrap();
        session.request("server.reload_config", json!({}));
        let plan = session.plan();
        let candidate = &plan["candidates"][0];
        assert_eq!(candidate["action"], action, "{plan}");
        assert_eq!(candidate["wait_reason"].as_str(), reason, "{plan}");
        assert_eq!(
            session.request("pane.get", json!({"pane_id":session.pane}))["pane"]["agent"],
            "pi"
        );
    }
    agent.stop();
}

#[test]
fn auto_close_ended_age_starts_when_the_agent_leaves() {
    let session = Session::new("[auto_close]\nmode = 'report'\nended_age_secs = 3\ndormant_age_secs = 100\npass_floor_secs = 1\n");
    let agent = session.agent("pi");
    agent.stop();
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || session.request("pane.get", json!({"pane_id":session.pane}))["pane"]["ended_agent"]
            == "pi"
    ));
    assert_eq!(session.plan()["spared"][0]["exemption"], "too-young");
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(100),
        || session.plan()["candidates"][0]["kind"] == "ended"
    ));
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(100),
        || session
            .reports()
            .iter()
            .any(|line| line["reason"] == "ended")
    ));
}

#[test]
fn auto_close_background_process_spares_a_plain_shell() {
    let session = Session::new(
        "[auto_close]\nmode = 'report'\nplain_shell_age_secs = 0\npass_floor_secs = 1\n",
    );
    session.request("pane.send_input", json!({"pane_id":session.pane,"text":"sleep 30 & printf 'CHILD%s\\n' _STARTED","keys":["Enter"]}));
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || pane_read_recent_contains(&session.socket, &session.pane, "CHILD_STARTED")
    ));
    assert_eq!(session.plan()["spared"][0]["exemption"], "not-bare-prompt");
    thread::sleep(Duration::from_secs(2));
    assert!(session.reports().is_empty());
    session.request("pane.send_input", json!({"pane_id":session.pane,"text":"kill $(jobs -p); wait; printf 'CHILD%s\\n' _STOPPED","keys":["Enter"]}));
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || pane_read_recent_contains(&session.socket, &session.pane, "CHILD_STOPPED")
    ));
    assert_eq!(session.plan()["candidates"][0]["kind"], "plain-shell");
}

#[test]
fn auto_close_pin_flag_and_agent_status_exemptions_are_named() {
    let session = Session::new("[auto_close]\nmode = 'report'\ndormant_age_secs = 0\npass_floor_secs = 1\ndebrief = false\n");
    let agent = session.agent("pi");
    session.request(
        "pane.set_pinned",
        json!({"pane_id":session.pane,"pinned":true}),
    );
    session.request(
        "pane.set_flag",
        json!({"pane_id":session.pane,"note":"review","due_at":1}),
    );
    assert_eq!(session.plan()["spared"][0]["exemption"], "pinned");
    session.request(
        "pane.set_pinned",
        json!({"pane_id":session.pane,"pinned":false}),
    );
    assert_eq!(session.plan()["spared"][0]["exemption"], "flagged");
    session.request("pane.clear_flag", json!({"pane_id":session.pane}));
    for (sequence, state) in ["working", "blocked", "unknown"].into_iter().enumerate() {
        let reported = session.request("pane.report_agent", json!({"pane_id":session.pane,"source":"custom:test","agent":"pi","state":state,"seq":sequence + 2}));
        assert!(
            wait_until(
                Duration::from_secs(5),
                Duration::from_millis(50),
                || session.request("pane.get", json!({"pane_id":session.pane}))["pane"]
                    ["agent_status"]
                    == state
            ),
            "wanted {state}, report {reported}: {}",
            session.request("pane.get", json!({"pane_id":session.pane}))
        );
        assert_eq!(session.plan()["spared"][0]["exemption"], state);
    }
    agent.stop();
}

#[test]
fn auto_close_desk_focus_only_exempts_an_attached_client_target() {
    let session = Session::new("[auto_close]\nplain_shell_age_secs = 0\n");
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.plan()["candidates"][0]["kind"] == "plain-shell"
    ));
    let client_socket = session.socket.with_file_name("herdr-client.sock");
    wait_for_socket(&client_socket, Duration::from_secs(5));
    let mut client = UnixStream::connect(client_socket).unwrap();
    let (_, error) = crate::support::client_shell_handshake(
        &mut client,
        crate::support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        80,
        24,
    )
    .unwrap();
    assert!(error.is_none(), "{error:?}");
    crate::support::wait_for_client_shell_bootstrap(&mut client, Duration::from_secs(5)).unwrap();
    assert_eq!(session.plan()["spared"][0]["exemption"], "desk-focused");
    drop(client);
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.plan()["candidates"][0]["kind"] == "plain-shell"
    ));
}

#[test]
fn auto_close_starter_waits_for_open_kin_including_a_plain_shell() {
    let session = Session::new(
        "[auto_close]\ndormant_age_secs = 0\nplain_shell_age_secs = 0\ndebrief = false\n",
    );
    let agent = session.agent("pi");
    let created: Value = serde_json::from_str(&agent.ok(&["tab", "create"])).unwrap();
    let kin = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.plan()["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pane| pane["pane_id"] == kin)
    ));
    let plan = session.plan();
    assert!(
        plan["spared"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pane| pane["pane_id"] == session.pane && pane["exemption"] == "open-kin"),
        "{plan}"
    );
    // The reported-agent path makes the kin live; its starter remains spared.
    let dir = session.base.join("kin");
    fs::create_dir_all(&dir).unwrap();
    let child = crate::support::InPaneReporter::write(&dir, "pi", &session.socket);
    assert!(
        run_cli(&session.socket, &["pane", "run", kin, &child.command()])
            .status
            .success()
    );
    child.wait_until_started(Duration::from_secs(5));
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.request("pane.get", json!({"pane_id":kin}))["pane"]["agent"] == "pi"
    ));
    let plan = session.plan();
    assert!(
        plan["spared"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pane| pane["pane_id"] == session.pane && pane["exemption"] == "open-kin"),
        "{plan}"
    );
    child.stop();
    session.request("pane.close", json!({"pane_id":kin}));
    assert_eq!(session.plan()["candidates"][0]["pane_id"], session.pane);
    agent.stop();
}

#[test]
fn auto_close_typing_resets_dormant_age_but_repeated_idle_reports_do_not() {
    let session = Session::new("[auto_close]\ndormant_age_secs = 3\ndebrief = false\n");
    let agent = session.agent("pi");
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(100),
        || session.plan()["candidates"][0]["kind"] == "dormant"
    ));
    let before = session.request("pane.get", json!({"pane_id":session.pane}))["pane"]
        ["input_or_command_at"]
        .clone();
    // A repeated idle report does not start a new dormant interval.
    session.request(
        "pane.report_agent",
        json!({"pane_id":session.pane,"source":"custom:test","agent":"pi","state":"idle","seq":2}),
    );
    assert_eq!(
        session.request("pane.get", json!({"pane_id":session.pane}))["pane"]["input_or_command_at"],
        before
    );
    assert_eq!(session.plan()["candidates"][0]["kind"], "dormant");
    session.request(
        "pane.send_text",
        json!({"pane_id":session.pane,"text":"operator input"}),
    );
    assert!(wait_until(
        Duration::from_secs(2),
        Duration::from_millis(25),
        || session.plan()["spared"][0]["exemption"] == "too-young"
    ));
    agent.stop();
}

#[test]
fn auto_close_plain_shell_age_ignores_output_and_resets_on_input() {
    let session = Session::new("[auto_close]\nplain_shell_age_secs = 3\n");
    let stop = session.base.join("stop-output");
    let command = format!(
        "while [ ! -f '{}' ]; do printf 'OUTPUT%s\\n' _TICK; sleep 0.1; done &",
        stop.display()
    );
    session.request(
        "pane.send_input",
        json!({"pane_id":session.pane,"text":command,"keys":["Enter"]}),
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || pane_read_recent_contains(&session.socket, &session.pane, "OUTPUT_TICK")
    ));
    let before = session.request("pane.get", json!({"pane_id":session.pane}))["pane"]
        ["input_or_command_at"]
        .clone();
    thread::sleep(Duration::from_secs(4));
    assert_eq!(
        session.request("pane.get", json!({"pane_id":session.pane}))["pane"]["input_or_command_at"],
        before
    );
    fs::write(&stop, "stop").unwrap();
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.plan()["candidates"][0]["kind"] == "plain-shell"
    ));
    session.request(
        "pane.send_text",
        json!({"pane_id":session.pane,"text":"typed at the prompt"}),
    );
    assert!(wait_until(
        Duration::from_secs(2),
        Duration::from_millis(25),
        || session.plan()["spared"][0]["exemption"] == "too-young"
    ));
}

impl Session {
    fn stop(&mut self) {
        self.request("server.stop", json!({}));
        if let Some(mut server) = self.server.take() {
            let pid = server.child.process_id();
            assert!(server.child.wait().unwrap().success());
            unregister_spawned_herdr_pid(pid);
        }
    }

    fn start(&mut self, config: &str) {
        self.server = Some(spawn_isolated(&self.base, &self.socket, config));
        wait_for_socket(&self.socket, Duration::from_secs(10));
    }
}

#[test]
fn auto_close_reports_survive_restart_without_duplicates() {
    let config = "[auto_close]\nmode = 'report'\nplain_shell_age_secs = 1\npass_floor_secs = 1\n";
    let mut session = Session::new(config);
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || session.reports().len() == 1
    ));
    session.stop();
    session.start(config);

    assert_eq!(
        session.request("ping", json!({}))["auto_close"]["closed_24h"],
        1
    );
    thread::sleep(Duration::from_secs(2));
    assert_eq!(session.reports().len(), 1);
    assert!(wait_until(
        Duration::from_secs(20),
        Duration::from_millis(100),
        || session.plan()["candidates"][0]["kind"] == "plain-shell"
    ));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        session.reports().len(),
        1,
        "restart must not duplicate an unchanged candidate"
    );
}

#[test]
fn auto_close_preserves_main_checkout_and_worktree_group_refusals() {
    let session = Session::new(
        "[auto_close]\nmode = 'report'\nplain_shell_age_secs = 0\npass_floor_secs = 1\n",
    );
    let observed_home = session.base.join("observed-home");
    session.request("pane.send_input", json!({"pane_id":session.pane,"text":format!("printf '%s' \"$HOME\" > '{}'", observed_home.display()),"keys":["Enter"]}));
    crate::support::wait_for_file(&observed_home, Duration::from_secs(5));
    assert_eq!(
        fs::read_to_string(&observed_home).unwrap(),
        session.base.join("home").display().to_string()
    );
    let repo = session.base.join("repo");
    create_committed_repo(&repo);
    let root = session.request("workspace.create", json!({"cwd":repo}));
    let root_id = root["root_pane"]["pane_id"].as_str().unwrap();
    let workspace = root["workspace"]["workspace_id"].as_str().unwrap();
    let plan = session.plan();
    assert!(
        plan["spared"].as_array().unwrap().iter().any(
            |pane| pane["pane_id"] == root_id && pane["exemption"] == "last-main-checkout-pane"
        ),
        "{plan}"
    );
    let real_destination =
        PathBuf::from(std::env::var_os("HOME").unwrap()).join(".herdr/worktrees/repo");
    let before_real_home = directory_entries(&real_destination);
    let created = run_cli(
        &session.socket,
        &[
            "worktree",
            "create",
            "--workspace",
            workspace,
            "--branch",
            "test-child",
        ],
    );
    assert!(created.status.success(), "{created:?}");
    assert!(session
        .base
        .join("worktrees/repo/test-child/.git")
        .is_file());
    assert_eq!(
        directory_entries(&real_destination),
        before_real_home,
        "worktree creation must leave the real home unchanged"
    );
    let refusal = send_request(
        &session.socket,
        &json!({"id":"refusal","method":"pane.close","params":{"pane_id":root_id}}).to_string(),
    );
    assert_eq!(
        refusal["error"]["code"], "confirmation_required",
        "{refusal}"
    );
    let plan = session.plan();
    assert!(
        plan["spared"].as_array().unwrap().iter().any(
            |pane| pane["pane_id"] == root_id && pane["exemption"] == "last-main-checkout-pane"
        ),
        "{plan}"
    );
    thread::sleep(Duration::from_secs(2));
    assert!(session.reports().iter().all(|line| line["pane"] != root_id));
    assert_eq!(
        session.request("pane.get", json!({"pane_id":root_id}))["pane"]["pane_id"],
        root_id
    );
    assert_eq!(directory_entries(&real_destination), before_real_home);
}

#[test]
fn auto_close_keeps_one_named_home_per_main_checkout() {
    let session = Session::new(
        "[auto_close]\nmode = 'report'\nplain_shell_age_secs = 0\npass_floor_secs = 1\n",
    );
    let repo = session.base.join("repo");
    create_committed_repo(&repo);
    let mut workspaces = Vec::new();
    for _ in 0..3 {
        let created = session.request("workspace.create", json!({"cwd":repo}));
        workspaces.push((
            created["root_pane"]["pane_id"].as_str().unwrap().to_owned(),
            created["workspace"]["workspace_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        ));
    }
    thread::sleep(Duration::from_millis(1100));
    session.request(
        "pane.send_text",
        json!({"pane_id":workspaces[1].0,"text":"recent activity"}),
    );

    let plan = session.plan();
    let pane_ids = workspaces
        .iter()
        .map(|(pane_id, _)| pane_id.as_str())
        .collect::<Vec<_>>();
    let homes = plan["spared"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|pane| {
            pane_ids.contains(&pane["pane_id"].as_str().unwrap())
                && pane["exemption"] == "last-main-checkout-pane"
        })
        .collect::<Vec<_>>();
    assert_eq!(homes.len(), 1, "{plan}");
    assert_eq!(homes[0]["pane_id"].as_str(), Some(workspaces[1].0.as_str()));
    let checkout = repo.canonicalize().unwrap().display().to_string();
    assert_eq!(homes[0]["main_checkout"].as_str(), Some(checkout.as_str()));
    assert_eq!(
        homes[0]["main_checkout_home"].as_str(),
        Some(workspaces[1].1.as_str())
    );
    for (pane_id, _) in workspaces
        .iter()
        .filter(|(pane_id, _)| pane_id != &workspaces[1].0)
    {
        let candidate = plan["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["pane_id"].as_str() == Some(pane_id.as_str()))
            .unwrap_or_else(|| panic!("missing sibling candidate {pane_id}: {plan}"));
        assert_eq!(candidate["main_checkout"].as_str(), Some(checkout.as_str()));
        assert_eq!(
            candidate["main_checkout_home"].as_str(),
            Some(workspaces[1].1.as_str())
        );
    }
}

#[test]
fn auto_close_live_pane_checkout_survives_restart() {
    let config = "[auto_close]\nmode = 'report'\nplain_shell_age_secs = 0\npass_floor_secs = 1\n";
    let mut session = Session::new(config);
    let repo_a = session.base.join("repo-a");
    let repo_b = session.base.join("repo-b");
    create_committed_repo(&repo_a);
    create_committed_repo(&repo_b);
    let first = session.request("workspace.create", json!({"cwd":repo_a}));
    let moved = session.request("workspace.create", json!({"cwd":repo_a}));
    let first_pane = first["root_pane"]["pane_id"].as_str().unwrap().to_owned();
    let first_workspace = first["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let moved_pane = moved["root_pane"]["pane_id"].as_str().unwrap().to_owned();
    let moved_workspace = moved["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let checkout_a = repo_a.canonicalize().unwrap().display().to_string();
    let checkout_b = repo_b.canonicalize().unwrap().display().to_string();
    thread::sleep(Duration::from_millis(1100));
    session.request(
        "pane.send_input",
        json!({"pane_id":moved_pane,"text":format!("cd '{}'", repo_b.display()),"keys":["Enter"]}),
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.request("pane.get", json!({"pane_id":moved_pane}))["pane"]["cwd"].as_str()
            == Some(checkout_b.as_str())
    ));

    let assert_homes = |session: &Session| {
        let plan = session.plan();
        for (pane_id, checkout, workspace_id) in [
            (&first_pane, &checkout_a, &first_workspace),
            (&moved_pane, &checkout_b, &moved_workspace),
        ] {
            let spared = plan["spared"]
                .as_array()
                .unwrap()
                .iter()
                .find(|pane| pane["pane_id"].as_str() == Some(pane_id.as_str()))
                .unwrap_or_else(|| panic!("missing spared pane {pane_id}: {plan}"));
            assert_eq!(spared["exemption"], "last-main-checkout-pane", "{plan}");
            assert_eq!(spared["main_checkout"].as_str(), Some(checkout.as_str()));
            assert_eq!(
                spared["main_checkout_home"].as_str(),
                Some(workspace_id.as_str())
            );
        }
    };
    assert_homes(&session);

    session.stop();
    session.start(config);
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.request("pane.get", json!({"pane_id":moved_pane}))["pane"]["cwd"].as_str()
            == Some(checkout_b.as_str())
    ));
    assert_homes(&session);
}

#[test]
fn auto_close_split_workspace_siblings_name_the_single_pane_home() {
    let session = Session::new(
        "[auto_close]\nmode = 'report'\nplain_shell_age_secs = 0\npass_floor_secs = 1\n",
    );
    let repo = session.base.join("repo");
    create_committed_repo(&repo);
    let home = session.request("workspace.create", json!({"cwd":repo}));
    let sibling = session.request("workspace.create", json!({"cwd":repo}));
    let home_pane = home["root_pane"]["pane_id"].as_str().unwrap().to_owned();
    let home_workspace = home["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sibling_pane = sibling["root_pane"]["pane_id"].as_str().unwrap().to_owned();
    let split = session.request(
        "pane.split",
        json!({"target_pane_id":sibling_pane,"direction":"right","focus":false}),
    );
    let split_pane = split["pane"]["pane_id"].as_str().unwrap().to_owned();
    thread::sleep(Duration::from_millis(1100));
    session.request(
        "pane.send_text",
        json!({"pane_id":split_pane,"text":"recent activity"}),
    );

    let plan = session.plan();
    let spared = plan["spared"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"].as_str() == Some(home_pane.as_str()))
        .unwrap_or_else(|| panic!("missing single-pane home: {plan}"));
    assert_eq!(spared["exemption"], "last-main-checkout-pane", "{plan}");
    let checkout = repo.canonicalize().unwrap().display().to_string();
    for pane_id in [&sibling_pane, &split_pane] {
        let candidate = plan["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["pane_id"].as_str() == Some(pane_id.as_str()))
            .unwrap_or_else(|| panic!("missing split sibling {pane_id}: {plan}"));
        assert_eq!(candidate["main_checkout"].as_str(), Some(checkout.as_str()));
        assert_eq!(
            candidate["main_checkout_home"].as_str(),
            Some(home_workspace.as_str())
        );
    }
}

#[test]
fn auto_close_unresolved_agent_restore_is_exempt() {
    let config = "[auto_close]\nmode = 'report'\ndormant_age_secs = 0\nplain_shell_age_secs = 0\npass_floor_secs = 1\ndebrief = false\n";
    let mut session = Session::new(config);
    let _agent = session.agent("pi");
    session.stop();
    let before = session.reports().len();
    session.start(config);
    let plan = session.plan();
    assert_eq!(plan["spared"][0]["exemption"], "restore-window", "{plan}");
    assert_eq!(session.reports().len(), before);
}

#[test]
fn auto_close_hello_matches_over_websocket_and_socket_and_reports_hosted_session() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let session = Session::new(&format!("[websocket_api]\nbind = '127.0.0.1:{port}'\ntoken = 'test-token'\n[auto_close]\nmode = 'report'\ndormant_age_secs = 1\npass_floor_secs = 1\ndebrief = false\n"));
    let agent = session.agent("claude");
    let report = agent.ok(&[
        "pane",
        "report-agent-session",
        &session.pane,
        "--source=herdr:claude",
        "--agent=claude",
        "--seq=2",
        "--agent-session-id=report-session",
        "--session-start-source=startup",
    ]);
    let pane = session.request("pane.get", json!({"pane_id":session.pane}));
    assert!(!pane["pane"]["agent_session"].is_null(), "{report}: {pane}");
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || session
            .reports()
            .iter()
            .any(|line| line["session"]["id"] == "report-session")
    ));
    let (mut ws, _) =
        tungstenite::connect(format!("ws://127.0.0.1:{port}/?token=test-token")).unwrap();
    ws.send(tungstenite::Message::Text(
        json!({"id":"hello","method":"ping","params":{}})
            .to_string()
            .into(),
    ))
    .unwrap();
    let response: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(
        response["result"]["auto_close"],
        session.request("ping", json!({}))["auto_close"]
    );
    let report = session
        .reports()
        .into_iter()
        .find(|line| line["session"]["id"] == "report-session")
        .unwrap();
    assert_eq!(
        report["session"],
        json!({"agent":"claude","id":"report-session"})
    );
    assert_eq!(report["reason"], "dormant");
    assert_eq!(report["by"], "desk");
    agent.stop();
}

fn directory_entries(path: &Path) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut entries = Vec::new();
    if path.exists() {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            entries.push((entry.path(), metadata.len(), metadata.modified().ok()));
        }
    }
    entries.sort();
    entries
}

#[test]
fn auto_close_gate_keeps_runtime_responsive_and_rechecks_exemptions() {
    let session = Session::new("");
    let agent = session.agent("pi");
    let skill = session.base.join("debrief.md");
    fs::write(&skill, "Record lessons.").unwrap();
    let started = session.base.join("gate-started");
    let released = session.base.join("gate-released");
    let finished = session.base.join("gate-finished");
    let command = format!(
        "printf ready > '{}'; while [ ! -f '{}' ]; do sleep 0.05; done; printf done > '{}'",
        started.display(),
        released.display(),
        finished.display()
    );
    fs::write(session.base.join("config").join(app_dir_name()).join("config.toml"),format!("[auto_close]\nmode = 'report'\ndormant_age_secs = 0\npass_floor_secs = 1\ndebrief_skill_path = '{}'\ngate_timeout_secs = 5\n[auto_close.pi]\ngate_command = {}\n",skill.display(), toml::Value::String(command))).unwrap();
    session.request("server.reload_config", json!({}));
    crate::support::wait_for_file(&started, Duration::from_secs(10));
    // Only a responsive runtime can apply the pin and then let the test
    // release the command before its timeout. No timing threshold is asserted.
    session.request(
        "pane.set_pinned",
        json!({"pane_id":session.pane,"pinned":true}),
    );
    fs::write(&released, "go").unwrap();
    crate::support::wait_for_file(&finished, Duration::from_secs(2));
    thread::sleep(Duration::from_secs(2));
    assert!(session.reports().is_empty());
    assert_eq!(session.plan()["spared"][0]["exemption"], "pinned");
    agent.stop();
}

#[test]
fn auto_close_unknown_agent_exit_starts_a_fresh_ended_age() {
    let session =
        Session::new("[auto_close]\nmode = 'report'\nended_age_secs = 3\npass_floor_secs = 1\n");
    let reporter =
        crate::support::InPaneReporter::write(&session.base, "custom-runner", &session.socket);
    assert!(run_cli(
        &session.socket,
        &["pane", "run", &session.pane, &reporter.command()]
    )
    .status
    .success());
    reporter.wait_until_started(Duration::from_secs(5));
    session.request("pane.report_agent", json!({"pane_id":session.pane,"source":"custom:test","agent":"custom-kind","state":"unknown"}));
    let active = session.request("pane.get", json!({"pane_id":session.pane}))["pane"].clone();
    assert_eq!(active["agent"], "custom-kind");
    assert_eq!(active["agent_status"], "unknown");
    thread::sleep(Duration::from_secs(4));
    let exiting_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    reporter.stop();
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        || {
            let pane = session.request("pane.get", json!({"pane_id":session.pane}))["pane"].clone();
            pane["agent"].is_null() && pane["ended_agent"] == "custom-kind"
        }
    ));
    let ended = session.request("pane.get", json!({"pane_id":session.pane}))["pane"].clone();
    assert_eq!(
        ended["agent_status_changed_at"],
        active["agent_status_changed_at"]
    );
    let plan = session.plan();
    assert_eq!(plan["spared"][0]["exemption"], "too-young", "{plan}");
    assert!(session.reports().is_empty());
    assert!(wait_until(
        Duration::from_secs(6),
        Duration::from_millis(50),
        || session.plan()["candidates"][0]["kind"] == "ended"
    ));
    assert!(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            >= exiting_at + 3
    );
}

impl Session {
    fn saved_pane(&self) -> Value {
        let snapshot: Value = serde_json::from_slice(
            &fs::read(
                self.base
                    .join("config")
                    .join(app_dir_name())
                    .join("session.json"),
            )
            .unwrap(),
        )
        .unwrap();
        snapshot["workspaces"][0]["tabs"][0]["panes"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone()
    }

    fn wait_for_ended(&self, kind: &str) {
        assert!(wait_until(
            Duration::from_secs(5),
            Duration::from_millis(25),
            || {
                let pane = self.request("pane.get", json!({"pane_id":self.pane}))["pane"].clone();
                pane["agent"].is_null() && pane["ended_agent"] == kind
            }
        ));
    }
}

#[test]
fn auto_close_ended_age_survives_restart_without_restarting_its_clock() {
    let config = "[auto_close]\nmode = 'report'\nended_age_secs = 6\npass_floor_secs = 1\n";
    let mut session = Session::new(config);
    let agent = session.agent("pi");
    let before_exit = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    agent.stop();
    session.wait_for_ended("pi");
    session.stop();
    let ended_at = session.saved_pane()["ended_at"]
        .as_u64()
        .expect("persisted exit time");
    assert!(ended_at >= before_exit);
    session.start(config);
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || session.plan()["spared"][0]["exemption"] == "too-young"
    ));
    assert!(wait_until(
        Duration::from_secs(8),
        Duration::from_millis(50),
        || session.plan()["candidates"][0]["kind"] == "ended"
    ));
    let before_plan = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let age = session.plan()["candidates"][0]["age_secs"]
        .as_u64()
        .unwrap();
    let after_plan = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!((before_plan - ended_at..=after_plan - ended_at).contains(&age));
    session.stop();
    assert_eq!(session.saved_pane()["ended_at"], ended_at);
}

#[test]
fn auto_close_legacy_ended_pane_waits_for_a_known_exit_time() {
    let config = "[auto_close]\nmode = 'report'\nended_age_secs = 0\npass_floor_secs = 1\n";
    let mut session = Session::new(config);
    let agent = session.agent("pi");
    agent.stop();
    session.wait_for_ended("pi");
    session.stop();
    let reports_before = session.reports().len();
    let path = session
        .base
        .join("config")
        .join(app_dir_name())
        .join("session.json");
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for pane in snapshot["workspaces"][0]["tabs"][0]["panes"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        assert!(pane.as_object_mut().unwrap().remove("ended_at").is_some());
    }
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    session.start(config);
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || session.plan()["spared"][0]["exemption"] == "ended-time-unknown"
    ));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(session.reports().len(), reports_before);
    session.stop();
    assert!(session.saved_pane().get("ended_at").is_none());
    session.start(config);
    let next = session.agent("codex");
    next.stop();
    session.wait_for_ended("codex");
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || session.plan()["candidates"][0]["kind"] == "ended"
    ));
    session.stop();
    assert!(session.saved_pane()["ended_at"].as_u64().is_some());
}
