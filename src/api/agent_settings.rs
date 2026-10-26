//! User-level launch defaults, read without starting agents or touching panes.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::schema::{
    AgentDefault, AgentSettings, AgentSettingsReason, PiAgentSettings, ResponseResult,
    SuccessResponse,
};

const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

pub(super) fn handle_settings(id: String) -> String {
    let result = read_settings(crate::worktree::home_dir().as_deref(), |key| {
        std::env::var_os(key)
    });
    serde_json::to_string(&SuccessResponse {
        id: id.clone(),
        result,
    })
    .unwrap_or_else(|_| {
        super::server::error_response_json(
            id,
            "internal_error",
            "failed to encode agent settings".into(),
        )
    })
}

// Deserialize only the allowlisted fields. In particular, never retain or log
// arbitrary settings values or parser diagnostics, which can contain credentials.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiFile {
    default_provider: Option<String>,
    default_model: Option<String>,
    default_thinking_level: Option<String>,
    enabled_models: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeFile {
    model: Option<String>,
    effort_level: Option<String>,
}

#[derive(Deserialize)]
struct CodexFile {
    model: Option<String>,
    model_reasoning_effort: Option<String>,
}

fn read_settings(home: Option<&Path>, env: impl Fn(&str) -> Option<OsString>) -> ResponseResult {
    let pi_dir = pi_config_dir(home, env("PI_CODING_AGENT_DIR"));
    let claude_dir = config_dir(home, env("CLAUDE_CONFIG_DIR"), ".claude");
    let codex_dir = config_dir(home, env("CODEX_HOME"), ".codex");
    let pi = pi_dir
        .and_then(|dir| read_json::<PiFile>(dir.map(|dir| dir.join("settings.json")).as_deref()));
    let (pi, models) = match pi {
        Ok(file) => {
            let model = match (file.default_provider, file.default_model) {
                (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
                (_, model) => model,
            };
            (
                Ok(AgentDefault {
                    model,
                    effort: file.default_thinking_level,
                }),
                file.enabled_models.unwrap_or_default(),
            )
        }
        Err(reason) => (Err(reason), Vec::new()),
    };
    let claude = read_json::<ClaudeFile>(
        claude_dir.map(|dir| dir.join("settings.json")).as_deref(),
    )
    .map(|file| AgentDefault {
        model: file.model,
        effort: file.effort_level,
    });
    let codex = read_file(codex_dir.map(|dir| dir.join("config.toml")).as_deref())
        .and_then(|text| {
            toml::from_str::<CodexFile>(&text).map_err(|_| AgentSettingsReason::Unreadable)
        })
        .map(|file| AgentDefault {
            model: file.model,
            effort: file.model_reasoning_effort,
        });
    ResponseResult::ServerAgentSettings {
        pi: PiAgentSettings {
            settings: settings(pi),
            models,
        },
        claude: settings(claude),
        codex: settings(codex),
    }
}

fn pi_config_dir(
    home: Option<&Path>,
    override_dir: Option<OsString>,
) -> Result<Option<PathBuf>, AgentSettingsReason> {
    let Some(path) = config_dir(home, override_dir, ".pi/agent") else {
        return Ok(None);
    };
    // Pi calls normalizePath with its default options: shell-drive syntax on
    // Windows, tilde expansion, then lowercase file:// URLs. No trimming or @ removal.
    let path = crate::platform::normalize_pi_settings_shell_path(&path);
    if let Ok(rest) = path.strip_prefix("~") {
        return Ok(home.map(|home| {
            // Node's path.join normalizes dot segments before filesystem access.
            let mut result = PathBuf::new();
            for component in home.join(rest).components() {
                match component {
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        result.pop();
                    }
                    _ => result.push(component.as_os_str()),
                }
            }
            result
        }));
    }
    let Some(input) = path.to_str().filter(|input| input.starts_with("file://")) else {
        return Ok(Some(path));
    };
    let url = url::Url::parse(input).map_err(|_| AgentSettingsReason::Unreadable)?;
    let bytes = url.path().as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let hex = |offset| {
                bytes
                    .get(offset)
                    .and_then(|byte| char::from(*byte).to_digit(16))
            };
            let high = hex(index + 1).ok_or(AgentSettingsReason::Unreadable)?;
            let low = hex(index + 2).ok_or(AgentSettingsReason::Unreadable)?;
            let byte = (high * 16 + low) as u8;
            // Node's fileURLToPath rejects encoded separators before decoding.
            if byte == b'/' || byte == std::path::MAIN_SEPARATOR as u8 {
                return Err(AgentSettingsReason::Unreadable);
            }
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).map_err(|_| AgentSettingsReason::Unreadable)?;
    crate::platform::pi_settings_file_url_path(url.host_str(), decoded)
        .map(Some)
        .ok_or(AgentSettingsReason::Unreadable)
}

fn config_dir(
    home: Option<&Path>,
    override_dir: Option<OsString>,
    fallback: &str,
) -> Option<PathBuf> {
    override_dir
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(fallback)))
}

fn settings(result: Result<AgentDefault, AgentSettingsReason>) -> AgentSettings {
    match result {
        Ok(default) => AgentSettings {
            default: Some(default),
            reason: None,
        },
        Err(reason) => AgentSettings {
            default: None,
            reason: Some(reason),
        },
    }
}

fn read_file(path: Option<&Path>) -> Result<String, AgentSettingsReason> {
    let path = path.ok_or(AgentSettingsReason::NoSettings)?;
    // Reject special files before opening, and bound the actual read too so
    // growth after the metadata check cannot bypass the limit.
    let metadata = std::fs::metadata(path).map_err(read_reason)?;
    if !metadata.is_file() || metadata.len() > MAX_SETTINGS_BYTES {
        return Err(AgentSettingsReason::Unreadable);
    }
    let file = std::fs::File::open(path).map_err(read_reason)?;
    if !file.metadata().map_err(read_reason)?.is_file() {
        return Err(AgentSettingsReason::Unreadable);
    }
    let mut text = String::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(read_reason)?;
    if text.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(AgentSettingsReason::Unreadable);
    }
    Ok(text)
}

fn read_reason(error: std::io::Error) -> AgentSettingsReason {
    match error.kind() {
        std::io::ErrorKind::NotFound => AgentSettingsReason::NoSettings,
        _ => AgentSettingsReason::Unreadable,
    }
}

fn read_json<T: serde::de::DeserializeOwned>(
    path: Option<&Path>,
) -> Result<T, AgentSettingsReason> {
    let text = read_file(path)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    // Serde structs also accept positional arrays; settings require named keys.
    if !text.trim_start().starts_with('{') {
        return Err(AgentSettingsReason::Unreadable);
    }
    serde_json::from_str(text).map_err(|_| AgentSettingsReason::Unreadable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let home = std::env::temp_dir().join(format!(
                "agent-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&home).unwrap();
            Self { home }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn read(&self) -> serde_json::Value {
            serde_json::to_value(read_settings(Some(&self.home), |_| None)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn agent_settings_reads_only_named_user_fields_in_file_order() {
        let fixture = Fixture::new();
        fixture.write(
            ".pi/agent/settings.json",
            r#"{
            "defaultProvider":"provider", "defaultModel":"model", "defaultThinkingLevel":"high",
            "enabledModels":["provider/second", "provider/first", "provider/second"],
            "apiKey":"pi-secret", "env":{"TOKEN":"env-secret"}
        }"#,
        );
        fixture.write(
            ".claude/settings.json",
            r#"{
            "model":"sonnet", "effortLevel":"max", "env":{"API_KEY":"claude-secret"}
        }"#,
        );
        fixture.write(
            ".codex/config.toml",
            "model = 'model'\nmodel_reasoning_effort = 'high'\n[env]\nTOKEN = 'codex-secret'\n",
        );
        assert_eq!(
            fixture.read(),
            serde_json::json!({
                "type":"server_agent_settings",
                "pi":{"default":{"model":"provider/model", "effort":"high"}, "reason":null, "models":["provider/second", "provider/first", "provider/second"]},
                "claude":{"default":{"model":"sonnet", "effort":"max"}, "reason":null},
                "codex":{"default":{"model":"model", "effort":"high"}, "reason":null}
            })
        );
    }

    #[test]
    fn agent_settings_distinguishes_absent_files_from_absent_fields_and_bad_files() {
        let fixture = Fixture::new();
        let paths = [
            ("pi", ".pi/agent/settings.json", "{}", "{"),
            ("claude", ".claude/settings.json", "{}", "{"),
            ("codex", ".codex/config.toml", "", "model = ["),
        ];
        let missing = fixture.read();
        for (agent, path, empty, malformed) in paths {
            assert_eq!(missing[agent]["default"], serde_json::Value::Null);
            assert_eq!(missing[agent]["reason"], "no_settings");
            fixture.write(path, empty);
            let present = fixture.read();
            assert_eq!(
                present[agent]["default"],
                serde_json::json!({"model":null, "effort":null})
            );
            assert_eq!(present[agent]["reason"], serde_json::Value::Null);
            fixture.write(path, malformed);
            let bad = fixture.read();
            assert_eq!(bad[agent]["default"], serde_json::Value::Null);
            assert_eq!(bad[agent]["reason"], "unreadable");
            fixture.write(path, empty);
        }
        assert_eq!(missing["pi"]["models"], serde_json::json!([]));
        assert_eq!(fixture.read()["pi"]["models"], serde_json::json!([]));
        fixture.write(".pi/agent/settings.json", r#"{"defaultModel":"model"}"#);
        fixture.write(".claude/settings.json", r#"{"effortLevel":"high"}"#);
        fixture.write(".codex/config.toml", "model = 'model'\n");
        assert_eq!(
            fixture.read(),
            serde_json::json!({
                "type":"server_agent_settings",
                "pi":{"default":{"model":"model", "effort":null}, "reason":null, "models":[]},
                "claude":{"default":{"model":null, "effort":"high"}, "reason":null},
                "codex":{"default":{"model":"model", "effort":null}, "reason":null}
            })
        );
        fixture.write(
            ".pi/agent/settings.json",
            r#"{"defaultProvider":"provider", "defaultThinkingLevel":"low"}"#,
        );
        assert_eq!(
            fixture.read()["pi"]["default"],
            serde_json::json!({"model":null, "effort":"low"})
        );
    }

    #[test]
    fn agent_settings_honors_each_agent_config_directory() {
        let fixture = Fixture::new();
        fixture.write("pi-config/settings.json", r#"{"defaultModel":"chosen"}"#);
        fixture.write("claude-config/settings.json", r#"{"model":"chosen"}"#);
        fixture.write("codex-config/config.toml", "model = 'chosen'\n");
        // A configured directory must win even when the conventional file exists.
        fixture.write(".pi/agent/settings.json", r#"{"defaultModel":"ignored"}"#);
        fixture.write(".claude/settings.json", r#"{"model":"ignored"}"#);
        fixture.write(".codex/config.toml", "model = 'ignored'\n");
        let result = serde_json::to_value(read_settings(Some(&fixture.home), |key| match key {
            "PI_CODING_AGENT_DIR" => Some("~/pi-config".into()),
            "CLAUDE_CONFIG_DIR" => Some(fixture.home.join("claude-config").into_os_string()),
            "CODEX_HOME" => Some(fixture.home.join("codex-config").into_os_string()),
            _ => None,
        }))
        .unwrap();
        for agent in ["pi", "claude", "codex"] {
            assert_eq!(result[agent]["default"]["model"], "chosen");
        }
        let missing = serde_json::to_value(read_settings(Some(&fixture.home), |_| {
            Some(fixture.home.join("absent-config").into_os_string())
        }))
        .unwrap();
        for agent in ["pi", "claude", "codex"] {
            assert_eq!(missing[agent]["reason"], "no_settings");
        }
        let empty_overrides =
            serde_json::to_value(read_settings(Some(&fixture.home), |_| Some("".into()))).unwrap();
        for agent in ["pi", "claude", "codex"] {
            assert_eq!(empty_overrides[agent]["default"]["model"], "ignored");
        }
    }

    #[test]
    fn agent_settings_reads_pi_file_url_overrides_with_percent_encoding() {
        let fixture = Fixture::new();
        fixture.write(".pi/agent/settings.json", r#"{"defaultModel":"ignored"}"#);
        for directory in ["pi-config", "pi config", "pi%config"] {
            fixture.write(
                &format!("{directory}/settings.json"),
                r#"{"defaultModel":"chosen"}"#,
            );
            let path = fixture
                .home
                .join(directory)
                .to_str()
                .unwrap()
                .replace('\\', "/");
            let path = path.replace('%', "%25").replace(' ', "%20");
            let slash = if path.starts_with('/') { "" } else { "/" };
            for authority in ["", "localhost", "LOCALHOST"] {
                let url = format!("file://{authority}{slash}{path}");
                for url in [url.clone(), format!("{url}?ignored#fragment")] {
                    let result = serde_json::to_value(read_settings(Some(&fixture.home), |key| {
                        (key == "PI_CODING_AGENT_DIR").then(|| url.clone().into())
                    }))
                    .unwrap();
                    assert_eq!(
                        result["pi"],
                        serde_json::json!({
                            "default":{"model":"chosen", "effort":null}, "reason":null, "models":[]
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn agent_settings_matches_pi_directory_normalization_forms() {
        let fixture = Fixture::new();
        fixture.write("settings.json", r#"{"defaultModel":"chosen"}"#);
        fixture.write("pi-config/settings.json", r#"{"defaultModel":"chosen"}"#);
        let path = fixture.home.to_str().unwrap().replace('\\', "/");
        let slash = if path.starts_with('/') { "" } else { "/" };
        let url = format!("file://{slash}{path}/missing/%2e%2e/pi-config");
        let read = |directory: &str| {
            serde_json::to_value(read_settings(Some(&fixture.home), |key| {
                (key == "PI_CODING_AGENT_DIR").then(|| directory.into())
            }))
            .unwrap()
        };
        let mut directories = vec![
            "~".to_string(),
            "~//pi-config".into(),
            "~/missing/../pi-config".into(),
            url.clone(),
        ];
        if cfg!(windows) {
            let drive = &path[..1];
            let rest = &path[2..];
            for prefix in ["", "mnt/", "cygdrive/", "MNT/", "CYGDRIVE/"] {
                directories.push(format!("/{prefix}{drive}{rest}/pi-config"));
            }
        }
        for directory in directories {
            assert_eq!(read(&directory)["pi"]["default"]["model"], "chosen");
        }
        for directory in [
            url.replacen("file://", "FILE://", 1),
            url.replacen("file://", "file:", 1),
            format!(" {url}"),
            format!("@{url}"),
            "~user/pi-config".into(),
        ] {
            assert_eq!(read(&directory)["pi"]["reason"], "no_settings");
        }
    }

    #[test]
    fn agent_settings_rejects_invalid_pi_file_urls_without_losing_other_agents() {
        let fixture = Fixture::new();
        fixture.write(".claude/settings.json", r#"{"model":"kept"}"#);
        fixture.write(".codex/config.toml", "model = 'kept'\n");
        for directory in [
            "file:///config%2fprivate",
            "file:///config%2Fprivate",
            "file:///config%",
            "file:///config%zz",
            "file:///config%FF",
            "file:///config%C0%AF",
            "file://user:secret@localhost/config",
            "file://localhost:80/config",
        ] {
            let result = serde_json::to_value(read_settings(Some(&fixture.home), |key| {
                (key == "PI_CODING_AGENT_DIR").then(|| directory.into())
            }))
            .unwrap();
            assert_eq!(
                result,
                serde_json::json!({
                    "type":"server_agent_settings",
                    "pi":{"default":null, "reason":"unreadable", "models":[]},
                    "claude":{"default":{"model":"kept", "effort":null}, "reason":null},
                    "codex":{"default":{"model":"kept", "effort":null}, "reason":null}
                })
            );
        }
    }

    #[test]
    fn agent_settings_limits_each_file_without_losing_other_agents() {
        let fixture = Fixture::new();
        let files = [
            (
                "pi",
                ".pi/agent/settings.json",
                r#"{"defaultModel":"model"}"#,
            ),
            ("claude", ".claude/settings.json", r#"{"model":"model"}"#),
            ("codex", ".codex/config.toml", "model = 'model'\n"),
        ];
        for (_, path, content) in files {
            fixture.write(path, content);
        }
        let good = fixture.read();
        for (agent, path, content) in files {
            let at_limit = format!("{content}{}", " ".repeat(1024 * 1024 - content.len()));
            fixture.write(path, &at_limit);
            assert_eq!(fixture.read(), good);
            fixture.write(path, &(at_limit + " "));
            let bad = fixture.read();
            assert_eq!(bad[agent]["reason"], "unreadable");
            assert_eq!(bad[agent]["default"], serde_json::Value::Null);
            assert_eq!(bad["pi"]["models"], serde_json::json!([]));
            for (other, _, _) in files {
                if other != agent {
                    assert_eq!(bad[other], good[other]);
                }
            }
            fixture.write(path, content);
        }
    }

    #[test]
    fn agent_settings_rejects_non_files_invalid_utf8_and_invalid_field_types() {
        let fixture = Fixture::new();
        for (agent, path, invalid) in [
            ("pi", ".pi/agent/settings.json", r#"{"enabledModels":[1]}"#),
            (
                "claude",
                ".claude/settings.json",
                r#"{"model":{"token":"private"}}"#,
            ),
            (
                "codex",
                ".codex/config.toml",
                "model_reasoning_effort = 1\n",
            ),
        ] {
            fixture.write(path, invalid);
            assert_eq!(fixture.read()[agent]["reason"], "unreadable");
            std::fs::write(fixture.home.join(path), [0xff]).unwrap();
            assert_eq!(fixture.read()[agent]["reason"], "unreadable");
            std::fs::remove_file(fixture.home.join(path)).unwrap();
            std::fs::create_dir(fixture.home.join(path)).unwrap();
            assert_eq!(fixture.read()[agent]["reason"], "unreadable");
        }
    }

    #[test]
    fn agent_settings_reads_json_objects_with_a_byte_order_mark_but_never_arrays() {
        let fixture = Fixture::new();
        fixture.write(
            ".pi/agent/settings.json",
            "\u{feff}{\"defaultModel\":\"model\"}",
        );
        assert_eq!(fixture.read()["pi"]["default"]["model"], "model");
        fixture.write(
            ".pi/agent/settings.json",
            r#"["provider","model","high",["model"]]"#,
        );
        fixture.write(".claude/settings.json", r#"["model","high"]"#);
        for agent in ["pi", "claude"] {
            assert_eq!(fixture.read()[agent]["reason"], "unreadable");
        }
    }
}
