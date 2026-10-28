use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::env::*;

pub(crate) fn integration_target_label(
    target: crate::api::schema::IntegrationTarget,
) -> &'static str {
    match target {
        crate::api::schema::IntegrationTarget::Pi => "pi",
        crate::api::schema::IntegrationTarget::Omp => "omp",
        crate::api::schema::IntegrationTarget::Claude => "claude",
        crate::api::schema::IntegrationTarget::Codex => "codex",
        crate::api::schema::IntegrationTarget::Copilot => "copilot",
        crate::api::schema::IntegrationTarget::Devin => "devin",
        crate::api::schema::IntegrationTarget::Droid => "droid",
        crate::api::schema::IntegrationTarget::Kimi => "kimi",
        crate::api::schema::IntegrationTarget::Opencode => "opencode",
        crate::api::schema::IntegrationTarget::Kilo => "kilo",
        crate::api::schema::IntegrationTarget::Hermes => "hermes",
        crate::api::schema::IntegrationTarget::Qodercli => "qodercli",
        crate::api::schema::IntegrationTarget::Qwen => "qwen",
        crate::api::schema::IntegrationTarget::Cursor => "cursor",
        crate::api::schema::IntegrationTarget::Mastracode => "mastracode",
        crate::api::schema::IntegrationTarget::AntigravityCli => "antigravity-cli",
        crate::api::schema::IntegrationTarget::Grok => "grok",
    }
}

pub(crate) fn integration_target_command(
    target: crate::api::schema::IntegrationTarget,
) -> &'static str {
    integration_target_command_names(target)[0]
}

pub(crate) fn integration_target_command_names(
    target: crate::api::schema::IntegrationTarget,
) -> &'static [&'static str] {
    match target {
        crate::api::schema::IntegrationTarget::Pi => &["pi"],
        crate::api::schema::IntegrationTarget::Omp => &["omp"],
        crate::api::schema::IntegrationTarget::Claude => &["claude"],
        crate::api::schema::IntegrationTarget::Codex => &["codex"],
        crate::api::schema::IntegrationTarget::Copilot => &["copilot"],
        crate::api::schema::IntegrationTarget::Devin => &["devin"],
        crate::api::schema::IntegrationTarget::Droid => &["droid"],
        crate::api::schema::IntegrationTarget::Kimi => &["kimi"],
        crate::api::schema::IntegrationTarget::Opencode => &["opencode"],
        crate::api::schema::IntegrationTarget::Kilo => &["kilo", "kilo-code"],
        crate::api::schema::IntegrationTarget::Hermes => &["hermes"],
        crate::api::schema::IntegrationTarget::Qodercli => qodercli_command_names(),
        crate::api::schema::IntegrationTarget::Qwen => &["qwen"],
        crate::api::schema::IntegrationTarget::Cursor => cursor_command_names(),
        crate::api::schema::IntegrationTarget::Mastracode => &["mastracode"],
        crate::api::schema::IntegrationTarget::AntigravityCli => &["agy"],
        crate::api::schema::IntegrationTarget::Grok => &["grok"],
    }
}

pub(crate) fn cursor_command_names() -> &'static [&'static str] {
    &["cursor-agent"]
}

pub(crate) fn integration_target_supported(target: crate::api::schema::IntegrationTarget) -> bool {
    #[cfg(windows)]
    {
        matches!(
            target,
            crate::api::schema::IntegrationTarget::Pi
                | crate::api::schema::IntegrationTarget::Omp
                | crate::api::schema::IntegrationTarget::Claude
                | crate::api::schema::IntegrationTarget::Codex
                | crate::api::schema::IntegrationTarget::Copilot
                | crate::api::schema::IntegrationTarget::Opencode
                | crate::api::schema::IntegrationTarget::Kilo
                | crate::api::schema::IntegrationTarget::Droid
                | crate::api::schema::IntegrationTarget::Kimi
                | crate::api::schema::IntegrationTarget::Qodercli
                | crate::api::schema::IntegrationTarget::Qwen
                | crate::api::schema::IntegrationTarget::AntigravityCli
                | crate::api::schema::IntegrationTarget::Devin
                | crate::api::schema::IntegrationTarget::Hermes
                | crate::api::schema::IntegrationTarget::Cursor
                | crate::api::schema::IntegrationTarget::Mastracode
                | crate::api::schema::IntegrationTarget::Grok
        )
    }

    #[cfg(not(windows))]
    {
        let _ = target;
        true
    }
}

pub(crate) fn integration_target_available(target: crate::api::schema::IntegrationTarget) -> bool {
    if !integration_target_supported(target) {
        return false;
    }

    integration_target_command_names(target)
        .iter()
        .any(|command| command_available(command))
        || integration_target_install_layout_available(target)
}

#[cfg(windows)]
pub(crate) fn qodercli_command_names() -> &'static [&'static str] {
    &["qodercli", "qoder", "qoderclicn", "qodercn"]
}

#[cfg(not(windows))]
pub(crate) fn qodercli_command_names() -> &'static [&'static str] {
    &["qodercli"]
}

pub(crate) fn integration_target_install_layout_available(
    target: crate::api::schema::IntegrationTarget,
) -> bool {
    match target {
        crate::api::schema::IntegrationTarget::Codex => codex_standalone_binary_available(),
        crate::api::schema::IntegrationTarget::Hermes => hermes_install_layout_available(),
        _ => false,
    }
}

pub(crate) fn command_available(command: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        command_path_candidates(&dir, command)
            .into_iter()
            .any(|path| executable_file_exists(&path))
    })
}

pub(crate) fn command_path_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    let base = dir.join(command);

    #[cfg(not(windows))]
    {
        vec![base]
    }

    #[cfg(windows)]
    {
        if Path::new(command).extension().is_some() {
            return vec![base];
        }

        let mut candidates = vec![base];
        for extension in [".exe", ".cmd", ".bat", ".ps1"] {
            candidates.push(dir.join(format!("{command}{extension}")));
        }
        candidates
    }
}

pub(crate) fn executable_file_exists(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) fn codex_standalone_binary_available() -> bool {
    let Ok(releases_dir) =
        codex_dir().map(|dir| dir.join("packages").join("standalone").join("releases"))
    else {
        return false;
    };
    let Ok(entries) = fs::read_dir(releases_dir) else {
        return false;
    };

    entries.filter_map(Result::ok).any(|entry| {
        executable_file_exists(&entry.path().join("bin").join(codex_executable_name()))
    })
}

pub(crate) fn codex_executable_name() -> &'static str {
    if cfg!(windows) {
        "codex.exe"
    } else {
        "codex"
    }
}

pub(crate) fn hermes_install_layout_available() -> bool {
    #[cfg(windows)]
    {
        let Ok(dir) = hermes_dir() else {
            return false;
        };
        [
            dir.join("hermes.exe"),
            dir.join("bin").join("hermes.exe"),
            dir.join("Scripts").join("hermes.exe"),
        ]
        .into_iter()
        .any(|path| executable_file_exists(&path))
    }

    #[cfg(not(windows))]
    {
        false
    }
}

pub(crate) fn installed_integration_statuses() -> Vec<super::IntegrationStatus> {
    integration_specs()
        .into_iter()
        .filter_map(|(target, path, expected_version)| {
            if !integration_target_supported(target) {
                return None;
            }
            Some(integration_status_at(target, path.ok()?, expected_version))
        })
        .collect()
}

pub(crate) fn integration_recommendations() -> Vec<super::IntegrationRecommendation> {
    integration_specs()
        .into_iter()
        .filter_map(|(target, path, expected_version)| {
            if !integration_target_supported(target) {
                return None;
            }
            let path = path.ok()?;
            let status = integration_status_at(target, path.clone(), expected_version);
            Some(super::IntegrationRecommendation {
                target,
                label: integration_target_label(target),
                command: integration_target_command(target),
                available: integration_target_available(target)
                    || status.state != super::IntegrationStatusKind::NotInstalled,
                path,
                state: status.state,
            })
        })
        .collect()
}

pub(crate) fn outdated_installed_integrations() -> Vec<super::IntegrationStatus> {
    installed_integration_statuses()
        .into_iter()
        .filter(|status| status.state == super::IntegrationStatusKind::Outdated)
        .collect()
}

fn integration_specs() -> [(
    crate::api::schema::IntegrationTarget,
    io::Result<PathBuf>,
    u32,
); 17] {
    [
        (
            crate::api::schema::IntegrationTarget::Pi,
            pi_extension_dir().map(|dir| dir.join(super::PI_EXTENSION_INSTALL_NAME)),
            super::PI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Omp,
            omp_extension_dir().map(|dir| dir.join(super::OMP_EXTENSION_INSTALL_NAME)),
            super::OMP_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Claude,
            claude_dir().map(|dir| dir.join("hooks").join(super::CLAUDE_HOOK_INSTALL_NAME)),
            super::CLAUDE_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Codex,
            codex_dir().map(|dir| dir.join(super::CODEX_HOOK_INSTALL_NAME)),
            super::CODEX_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Copilot,
            copilot_dir().map(|dir| dir.join("hooks").join(super::COPILOT_HOOK_INSTALL_NAME)),
            super::COPILOT_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Devin,
            devin_dir().map(|dir| dir.join(super::DEVIN_HOOK_INSTALL_NAME)),
            super::DEVIN_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Droid,
            droid_dir().map(|dir| dir.join("hooks").join(super::DROID_HOOK_INSTALL_NAME)),
            super::DROID_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Kimi,
            kimi_dir().map(|dir| dir.join("hooks").join(super::KIMI_HOOK_INSTALL_NAME)),
            super::KIMI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Opencode,
            opencode_dir().map(|dir| {
                dir.join("plugins")
                    .join(super::OPENCODE_PLUGIN_INSTALL_NAME)
            }),
            super::OPENCODE_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Kilo,
            kilo_dir().map(|dir| dir.join("plugin").join(super::KILO_PLUGIN_INSTALL_NAME)),
            super::KILO_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Hermes,
            hermes_plugin_dir().map(|dir| dir.join(super::HERMES_PLUGIN_INIT_INSTALL_NAME)),
            super::HERMES_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Qodercli,
            qodercli_dir().map(|dir| dir.join("hooks").join(super::QODERCLI_HOOK_INSTALL_NAME)),
            super::QODERCLI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Qwen,
            qwen_dir().map(|dir| dir.join("hooks").join(super::QWEN_HOOK_INSTALL_NAME)),
            super::QWEN_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Cursor,
            cursor_dir().map(|dir| dir.join(super::CURSOR_HOOK_INSTALL_NAME)),
            super::CURSOR_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Mastracode,
            mastracode_dir().map(|dir| dir.join("hooks").join(super::MASTRACODE_HOOK_INSTALL_NAME)),
            super::MASTRACODE_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::AntigravityCli,
            antigravity_cli_dir().map(|dir| {
                dir.join("hooks")
                    .join(super::ANTIGRAVITY_CLI_HOOK_INSTALL_NAME)
            }),
            super::ANTIGRAVITY_CLI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Grok,
            grok_dir().map(|dir| dir.join("hooks").join(super::GROK_HOOK_INSTALL_NAME)),
            super::GROK_INTEGRATION_VERSION,
        ),
    ]
}

pub(crate) fn integration_update_instructions(
    targets: &[crate::api::schema::IntegrationTarget],
) -> String {
    let commands: Vec<String> = targets
        .iter()
        .map(|target| {
            format!(
                "`herdr integration install {}`",
                integration_target_label(*target)
            )
        })
        .collect();

    match commands.as_slice() {
        [] => String::new(),
        [command] => format!("run {command}"),
        [rest @ .., last] => format!("run {} and {last}", rest.join(", ")),
    }
}

pub(crate) fn print_outdated_update_notice() -> bool {
    let outdated = outdated_installed_integrations();
    if outdated.is_empty() {
        return false;
    }

    let targets = outdated
        .iter()
        .map(|integration| integration.target)
        .collect::<Vec<_>>();
    eprintln!(
        "installed herdr integrations need updating; {}.",
        integration_update_instructions(&targets).replace('`', "")
    );
    true
}

/// Whether the Herdr-owned Grok hook config exactly matches the installed
/// integration. JSON formatting and object key order do not affect validity.
fn grok_hook_config_is_valid(hook_path: &Path) -> bool {
    let Some(hooks_dir) = hook_path.parent() else {
        return false;
    };
    let config_path = hooks_dir.join(super::GROK_HOOK_CONFIG_INSTALL_NAME);
    fs::read_to_string(config_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .is_some_and(|config| config == super::targets::grok_hook_config(hook_path))
}

fn opencode_tui_integration_is_valid(plugin_path: &Path, expected_version: u32) -> bool {
    let Some(config_dir) = plugin_path.parent().and_then(Path::parent) else {
        return false;
    };
    let tui_plugin_path = config_dir.join(super::OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    let tui_plugin_current = fs::read_to_string(tui_plugin_path)
        .ok()
        .and_then(|content| parse_integration_version(&content))
        .is_some_and(|version| version >= expected_version);
    tui_plugin_current
        && super::opencode_config::tui_plugin_is_configured(
            config_dir,
            super::OPENCODE_TUI_PLUGIN_SPEC,
        )
        && (!config_dir.join("cli.json").exists()
            || (super::opencode_config::cli_plugin_is_configured(
                config_dir,
                super::OPENCODE_V2_TUI_PLUGIN_SPEC,
            ) && fs::read_to_string(
                config_dir
                    .join(super::OPENCODE_V2_TUI_PLUGIN_DIR)
                    .join("tui.js"),
            )
            .ok()
            .and_then(|content| parse_integration_version(&content))
            .is_some_and(|version| version >= expected_version)))
}

fn integration_state_for_path(
    path: &Path,
    expected_version: u32,
) -> (super::IntegrationStatusKind, Option<u32>) {
    if !path.is_file() {
        return (super::IntegrationStatusKind::NotInstalled, None);
    }

    let installed_version = fs::read_to_string(path)
        .ok()
        .and_then(|content| parse_integration_version(&content));
    let state = if installed_version.is_some_and(|version| version >= expected_version) {
        super::IntegrationStatusKind::Current
    } else {
        super::IntegrationStatusKind::Outdated
    };

    (state, installed_version)
}

pub(crate) fn integration_status_at(
    target: crate::api::schema::IntegrationTarget,
    path: PathBuf,
    expected_version: u32,
) -> super::IntegrationStatus {
    let (mut state, installed_version) = integration_state_for_path(&path, expected_version);

    // Grok only invokes the hook when the herdr-owned `hooks/herdr.json`
    // registers it, so a current hook script with a missing or broken config
    // is a nonfunctional install: report it as outdated so `herdr integration
    // status` flags it and a reinstall rewrites both files.
    if target == crate::api::schema::IntegrationTarget::Grok
        && state == super::IntegrationStatusKind::Current
        && !grok_hook_config_is_valid(&path)
    {
        state = super::IntegrationStatusKind::Outdated;
    }
    if target == crate::api::schema::IntegrationTarget::Opencode
        && state == super::IntegrationStatusKind::Current
        && !opencode_tui_integration_is_valid(&path, expected_version)
    {
        state = super::IntegrationStatusKind::Outdated;
    }

    super::IntegrationStatus {
        target,
        path,
        state,
        installed_version,
        expected_version,
    }
}

/// Letta is intentionally kept out of the frozen client endpoint
/// `IntegrationTarget` enum so published generation-1 clients never receive an
/// unknown variant. It is installable and reportable as an experimental
/// CLI-only target until the agent registry replaces the enum-keyed registry.
pub(crate) fn experimental_letta_integration_status() -> Option<super::ExperimentalIntegrationStatus>
{
    let path = letta_dir()
        .ok()?
        .join("hooks")
        .join(super::LETTA_HOOK_INSTALL_NAME);
    let (state, installed_version) =
        integration_state_for_path(&path, super::LETTA_INTEGRATION_VERSION);
    Some(super::ExperimentalIntegrationStatus {
        label: "letta",
        path,
        state,
        installed_version,
        expected_version: super::LETTA_INTEGRATION_VERSION,
    })
}

pub(crate) fn parse_integration_version(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        let marker_line = line
            .trim()
            .trim_start_matches('/')
            .trim_start_matches('#')
            .trim();
        marker_line
            .strip_prefix(super::INTEGRATION_VERSION_MARKER)?
            .trim()
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Versions shipped in v0.9.5, before direct-socket hooks used the report guard.
    macro_rules! released_hook_is_outdated {
        ($test:ident, $released:literal, $version:ident, $asset:ident) => {
            #[test]
            fn $test() {
                let path = std::env::temp_dir().join(format!(
                    "herdr-hook-migration-{}-{}",
                    std::process::id(),
                    stringify!($test),
                ));
                fs::write(
                    &path,
                    concat!("# HERDR_INTEGRATION_VERSION=", $released, "\n"),
                )
                .unwrap();
                let released = integration_state_for_path(&path, super::super::$version);
                fs::write(&path, super::super::$asset).unwrap();
                let upgraded = integration_state_for_path(&path, super::super::$version);
                fs::remove_file(&path).unwrap();

                assert_eq!(
                    released,
                    (
                        super::super::IntegrationStatusKind::Outdated,
                        Some($released)
                    )
                );
                assert_eq!(
                    upgraded,
                    (
                        super::super::IntegrationStatusKind::Current,
                        Some(super::super::$version)
                    )
                );
            }
        };
    }

    released_hook_is_outdated!(
        released_pi_hook_is_outdated,
        9,
        PI_INTEGRATION_VERSION,
        PI_EXTENSION_ASSET
    );
    released_hook_is_outdated!(
        released_omp_hook_is_outdated,
        10,
        OMP_INTEGRATION_VERSION,
        OMP_EXTENSION_ASSET
    );
    released_hook_is_outdated!(
        released_claude_hook_is_outdated,
        11,
        CLAUDE_INTEGRATION_VERSION,
        CLAUDE_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_codex_hook_is_outdated,
        10,
        CODEX_INTEGRATION_VERSION,
        CODEX_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_kimi_hook_is_outdated,
        8,
        KIMI_INTEGRATION_VERSION,
        KIMI_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_copilot_hook_is_outdated,
        4,
        COPILOT_INTEGRATION_VERSION,
        COPILOT_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_devin_hook_is_outdated,
        3,
        DEVIN_INTEGRATION_VERSION,
        DEVIN_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_droid_hook_is_outdated,
        4,
        DROID_INTEGRATION_VERSION,
        DROID_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_opencode_hook_is_outdated,
        13,
        OPENCODE_INTEGRATION_VERSION,
        OPENCODE_PLUGIN_ASSET
    );
    released_hook_is_outdated!(
        released_kilo_hook_is_outdated,
        4,
        KILO_INTEGRATION_VERSION,
        KILO_PLUGIN_ASSET
    );
    released_hook_is_outdated!(
        released_cursor_hook_is_outdated,
        2,
        CURSOR_INTEGRATION_VERSION,
        CURSOR_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_antigravity_cli_hook_is_outdated,
        4,
        ANTIGRAVITY_CLI_INTEGRATION_VERSION,
        ANTIGRAVITY_CLI_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_mastracode_hook_is_outdated,
        3,
        MASTRACODE_INTEGRATION_VERSION,
        MASTRACODE_HOOK_ASSET
    );
    released_hook_is_outdated!(
        released_grok_hook_is_outdated,
        3,
        GROK_INTEGRATION_VERSION,
        GROK_HOOK_ASSET
    );
}
