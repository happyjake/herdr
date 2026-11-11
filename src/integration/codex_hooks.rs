//! Install edits for codex's hooks.json that keep every hook where it stands.
//!
//! codex records hook trust by position: config.toml holds one
//! `[hooks.state."<hooks.json>:session_start:<group>:<hook>"]` table per hook,
//! with the hash of the hook trusted at that position. A hook that moves no
//! longer matches the hash at its new position: when an install moved herdr's
//! hook from first to last, codex 0.167 asked for a hook review and ran no
//! SessionStart hook at all, other tools' hooks included. So install edits
//! herdr's own entries in place, appends herdr's SessionStart hook only when
//! there is none, and returns the file text unchanged when nothing needs to
//! change. Every other entry keeps its position, its order and its bytes.

use std::io;
use std::path::Path;

use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::{json, Value};

use super::command::hook_command;
use super::config_edit::{hook_command_variants, is_matching_command_hook};

const SESSION_START: &str = "SessionStart";
const SESSION_HOOK_TIMEOUT_SEC: u64 = 10;

/// Actions herdr has installed on SessionStart. The first SessionStart hook
/// that runs one of them is herdr's slot; install rewrites it in place.
const SESSION_START_ACTIONS: &[&str] = &["session", "idle"];

/// Hooks that earlier herdr versions installed on other events.
const LEGACY_HOOKS: &[(&str, &str)] = &[
    ("PermissionRequest", "blocked"),
    ("UserPromptSubmit", "working"),
    ("PreToolUse", "working"),
    ("Stop", "idle"),
    ("Interrupt", "idle"),
];

/// A hook at `hooks[event][group].hooks[hook]`, the position codex trusts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct HookAt {
    group: usize,
    hook: usize,
}

struct EventRemovals {
    event: &'static str,
    /// Ascending document order.
    hooks: Vec<HookAt>,
}

/// The edits install makes, found on the parsed file and then applied both to
/// the parsed value (the expected result) and to the text (the written result).
#[derive(Default)]
struct Plan {
    /// herdr's SessionStart hook, when its type, command or timeout differs.
    rewrite: Option<HookAt>,
    /// herdr hooks to drop: SessionStart duplicates and legacy event hooks.
    removals: Vec<EventRemovals>,
    /// Append herdr's SessionStart hook: the file has none yet.
    append: bool,
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.rewrite.is_none() && self.removals.is_empty() && !self.append
    }
}

/// Returns the hooks.json text with herdr's hooks installed. `content` is
/// `None` when the file does not exist yet. When herdr's hooks are already in
/// place the returned text is `content` itself, so the caller can skip the
/// write.
pub(crate) fn install(
    content: Option<&str>,
    hooks_path: &Path,
    hook_path: &Path,
) -> io::Result<String> {
    let Some(content) = content else {
        let mut desired = json!({});
        apply_to_value(
            &mut desired,
            &Plan {
                append: true,
                ..Plan::default()
            },
            hooks_path,
            hook_path,
        )?;
        return Ok(serde_json::to_string_pretty(&desired)?);
    };

    let original = parse_value(content, hooks_path)?;
    let plan = plan(&original, hooks_path, hook_path)?;
    if plan.is_empty() {
        return Ok(content.to_string());
    }

    let mut desired = original;
    apply_to_value(&mut desired, &plan, hooks_path, hook_path)?;

    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        io::Error::other(format!("failed to parse {}: {err}", hooks_path.display()))
    })?;
    apply_to_text(&root, &plan, hooks_path, hook_path)?;
    let updated = root.to_string();

    if parse_value(&updated, hooks_path)? != desired {
        return Err(io::Error::other(format!(
            "failed to safely update codex hooks file at {}",
            hooks_path.display()
        )));
    }
    Ok(updated)
}

fn plan(original: &Value, hooks_path: &Path, hook_path: &Path) -> io::Result<Plan> {
    let root = original
        .as_object()
        .ok_or_else(|| not_an_object("codex hooks file", hooks_path))?;
    let Some(hooks) = root.get("hooks") else {
        return Ok(Plan {
            append: true,
            ..Plan::default()
        });
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| not_an_object("codex hooks file hooks", hooks_path))?;

    let mut plan = Plan::default();
    let mut session_hooks = herdr_hooks(
        hooks,
        SESSION_START,
        &commands(hook_path, SESSION_START_ACTIONS),
    )?
    .into_iter();
    match session_hooks.next() {
        Some(slot) => {
            let hook = &hooks[SESSION_START][slot.group]["hooks"][slot.hook];
            if !is_canonical_session_hook(hook, hook_path) {
                plan.rewrite = Some(slot);
            }
        }
        None => plan.append = true,
    }
    let duplicates: Vec<HookAt> = session_hooks.collect();
    if !duplicates.is_empty() {
        plan.removals.push(EventRemovals {
            event: SESSION_START,
            hooks: duplicates,
        });
    }

    for &(event, action) in LEGACY_HOOKS {
        let found = herdr_hooks(hooks, event, &commands(hook_path, &[action]))?;
        if !found.is_empty() {
            plan.removals.push(EventRemovals {
                event,
                hooks: found,
            });
        }
    }
    Ok(plan)
}

fn commands(hook_path: &Path, actions: &[&str]) -> Vec<String> {
    actions
        .iter()
        .flat_map(|action| hook_command_variants(hook_path, Some(action)))
        .collect()
}

/// herdr's hooks under `event`, in document order.
fn herdr_hooks(
    hooks: &serde_json::Map<String, Value>,
    event: &str,
    commands: &[String],
) -> io::Result<Vec<HookAt>> {
    let Some(entries) = hooks.get(event) else {
        return Ok(Vec::new());
    };
    let entries = entries.as_array().ok_or_else(|| not_an_array(event))?;
    let mut found = Vec::new();
    for (group, entry) in entries.iter().enumerate() {
        let Some(group_hooks) = entry.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for (hook, value) in group_hooks.iter().enumerate() {
            if commands
                .iter()
                .any(|command| is_matching_command_hook(value, command))
            {
                found.push(HookAt { group, hook });
            }
        }
    }
    Ok(found)
}

fn is_canonical_session_hook(hook: &Value, hook_path: &Path) -> bool {
    is_matching_command_hook(hook, &session_command(hook_path))
        && hook.get("timeout").and_then(Value::as_u64) == Some(SESSION_HOOK_TIMEOUT_SEC)
}

fn session_command(hook_path: &Path) -> String {
    hook_command(hook_path, Some("session"))
}

fn apply_to_value(
    value: &mut Value,
    plan: &Plan,
    hooks_path: &Path,
    hook_path: &Path,
) -> io::Result<()> {
    let root = value
        .as_object_mut()
        .ok_or_else(|| not_an_object("codex hooks file", hooks_path))?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| not_an_object("codex hooks file hooks", hooks_path))?;

    if let Some(at) = plan.rewrite {
        let hook = hooks
            .get_mut(SESSION_START)
            .and_then(|entries| entries.get_mut(at.group))
            .and_then(|group| group.get_mut("hooks"))
            .and_then(|group_hooks| group_hooks.get_mut(at.hook))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| missing_planned_hook(SESSION_START))?;
        hook.insert("type".to_string(), json!("command"));
        hook.insert("command".to_string(), json!(session_command(hook_path)));
        hook.insert("timeout".to_string(), json!(SESSION_HOOK_TIMEOUT_SEC));
    }

    for removal in &plan.removals {
        let entries = hooks
            .get_mut(removal.event)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| not_an_array(removal.event))?;
        for at in removal.hooks.iter().rev() {
            let group_hooks = entries
                .get_mut(at.group)
                .and_then(|group| group.get_mut("hooks"))
                .and_then(Value::as_array_mut)
                .ok_or_else(|| missing_planned_hook(removal.event))?;
            group_hooks.remove(at.hook);
            if group_hooks.is_empty() {
                entries.remove(at.group);
            }
        }
        if entries.is_empty() {
            hooks.remove(removal.event);
        }
    }

    if plan.append {
        hooks
            .entry(SESSION_START)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| not_an_array(SESSION_START))?
            .push(session_group_value(hook_path));
    }
    Ok(())
}

fn apply_to_text(
    root: &CstRootNode,
    plan: &Plan,
    hooks_path: &Path,
    hook_path: &Path,
) -> io::Result<()> {
    let root_object = root
        .value()
        .and_then(|value| value.as_object())
        .ok_or_else(|| not_an_object("codex hooks file", hooks_path))?;
    let hooks = match root_object.get("hooks") {
        Some(property) => Some(
            property
                .object_value()
                .ok_or_else(|| not_an_object("codex hooks file hooks", hooks_path))?,
        ),
        None => None,
    };

    if let Some(at) = plan.rewrite {
        let hook = hooks
            .as_ref()
            .and_then(|hooks| hooks.get(SESSION_START))
            .and_then(|property| property.array_value())
            .and_then(|entries| entries.elements().get(at.group).cloned())
            .and_then(|group| group.as_object())
            .and_then(|group| group.get("hooks"))
            .and_then(|property| property.array_value())
            .and_then(|group_hooks| group_hooks.elements().get(at.hook).cloned())
            .and_then(|hook| hook.as_object())
            .ok_or_else(|| missing_planned_hook(SESSION_START))?;
        set_field(&hook, "type", json!("command"));
        set_field(&hook, "command", json!(session_command(hook_path)));
        set_field(&hook, "timeout", json!(SESSION_HOOK_TIMEOUT_SEC));
    }

    for removal in &plan.removals {
        let event_property = hooks
            .as_ref()
            .and_then(|hooks| hooks.get(removal.event))
            .ok_or_else(|| missing_planned_hook(removal.event))?;
        let entries = event_property
            .array_value()
            .ok_or_else(|| not_an_array(removal.event))?;
        for at in removal.hooks.iter().rev() {
            let group = entries
                .elements()
                .get(at.group)
                .cloned()
                .ok_or_else(|| missing_planned_hook(removal.event))?;
            let group_hooks = group
                .as_object()
                .and_then(|group| group.get("hooks"))
                .and_then(|property| property.array_value())
                .ok_or_else(|| missing_planned_hook(removal.event))?;
            group_hooks
                .elements()
                .get(at.hook)
                .cloned()
                .ok_or_else(|| missing_planned_hook(removal.event))?
                .remove();
            if group_hooks.elements().is_empty() {
                group.remove();
            }
        }
        if entries.elements().is_empty() {
            event_property.remove();
        }
    }

    if plan.append {
        let group = session_group_input(hook_path);
        match hooks {
            None => {
                root_object.append(
                    "hooks",
                    CstInputValue::Object(vec![(
                        SESSION_START.to_string(),
                        CstInputValue::Array(vec![group]),
                    )]),
                );
            }
            Some(hooks) => match hooks.get(SESSION_START) {
                None => {
                    hooks.append(SESSION_START, CstInputValue::Array(vec![group]));
                }
                Some(property) => {
                    property
                        .array_value()
                        .ok_or_else(|| not_an_array(SESSION_START))?
                        .append(group);
                }
            },
        }
    }
    Ok(())
}

/// Sets one field of herdr's hook, leaving its text alone when it already
/// holds `value` and leaving the hook's other fields as they are.
fn set_field(hook: &CstObject, name: &str, value: Value) {
    let input = match &value {
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        _ => unreachable!("herdr hook fields are strings and numbers"),
    };
    match hook.get(name) {
        Some(property)
            if property
                .value()
                .and_then(|current| current.to_serde_value())
                .as_ref()
                == Some(&value) => {}
        Some(property) => property.set_value(input),
        None => {
            hook.append(name, input);
        }
    }
}

fn session_group_value(hook_path: &Path) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": session_command(hook_path),
            "timeout": SESSION_HOOK_TIMEOUT_SEC,
        }],
    })
}

fn session_group_input(hook_path: &Path) -> CstInputValue {
    CstInputValue::Object(vec![(
        "hooks".to_string(),
        CstInputValue::Array(vec![CstInputValue::Object(vec![
            ("type".to_string(), CstInputValue::from("command")),
            (
                "command".to_string(),
                CstInputValue::String(session_command(hook_path)),
            ),
            (
                "timeout".to_string(),
                CstInputValue::from(SESSION_HOOK_TIMEOUT_SEC),
            ),
        ])]),
    )])
}

fn parse_value(content: &str, hooks_path: &Path) -> io::Result<Value> {
    serde_json::from_str(content)
        .map_err(|err| io::Error::other(format!("failed to parse {}: {err}", hooks_path.display())))
}

fn strict_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: false,
        allow_loose_object_property_names: false,
        allow_trailing_commas: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn not_an_object(description: &str, hooks_path: &Path) -> io::Error {
    io::Error::other(format!(
        "{description} at {} must be a JSON object",
        hooks_path.display()
    ))
}

fn not_an_array(event: &str) -> io::Error {
    io::Error::other(format!("hook entries for {event} must be an array"))
}

fn missing_planned_hook(event: &str) -> io::Error {
    io::Error::other(format!(
        "codex hook entries for {event} changed while editing"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (&'static Path, &'static Path) {
        (
            Path::new("/home/test/.codex/hooks.json"),
            Path::new("/home/test/.codex/herdr-agent-state.sh"),
        )
    }

    fn quoted(command: &str) -> String {
        serde_json::to_string(command).unwrap()
    }

    #[test]
    fn install_returns_a_canonical_file_unchanged() {
        let (hooks_path, hook_path) = paths();
        let session = quoted(&session_command(hook_path));
        // Unsorted keys, CRLF, an extra field on herdr's hook: any rewrite shows.
        let input = format!(
            "{{\r\n  \"hooks\" : {{\"SessionStart\": [\r\n    {{\"hooks\": [{{\"timeout\": 10, \"statusMessage\": \"herdr\", \"command\": {session}, \"type\": \"command\"}}]}},\r\n    {{\"matcher\": \"startup\", \"hooks\": [{{\"type\": \"command\", \"command\": \"echo keep\"}}]}}\r\n  ]}}\r\n}}\r\n"
        );

        assert_eq!(install(Some(&input), hooks_path, hook_path).unwrap(), input);
    }

    #[test]
    fn install_rewrites_only_herdr_text_in_place() {
        let (hooks_path, hook_path) = paths();
        let idle = quoted(&hook_command(hook_path, Some("idle")));
        let session = quoted(&session_command(hook_path));
        let input = format!(
            concat!(
                "{{\n",
                "  \"hooks\": {{\n",
                "    \"SessionStart\": [\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo a\"}}]}},\n",
                "      {{\n",
                "        \"hooks\": [\n",
                "          {{\n",
                "            \"command\": {idle},\n",
                "            \"timeout\": 5,\n",
                "            \"type\": \"command\"\n",
                "          }}\n",
                "        ]\n",
                "      }},\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo b\"}}]}}\n",
                "    ],\n",
                "    \"Stop\": [\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo z\"}}, {{\"type\": \"command\", \"command\": {idle}}}, {{\"type\": \"command\", \"command\": \"echo w\"}}]}},\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": {idle}}}]}},\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo y\"}}]}}\n",
                "    ]\n",
                "  }}\n",
                "}}\n",
            ),
            idle = idle,
        );
        let expected = format!(
            concat!(
                "{{\n",
                "  \"hooks\": {{\n",
                "    \"SessionStart\": [\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo a\"}}]}},\n",
                "      {{\n",
                "        \"hooks\": [\n",
                "          {{\n",
                "            \"command\": {session},\n",
                "            \"timeout\": 10,\n",
                "            \"type\": \"command\"\n",
                "          }}\n",
                "        ]\n",
                "      }},\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo b\"}}]}}\n",
                "    ],\n",
                "    \"Stop\": [\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo z\"}}, {{\"type\": \"command\", \"command\": \"echo w\"}}]}},\n",
                "      {{\"hooks\": [{{\"type\": \"command\", \"command\": \"echo y\"}}]}}\n",
                "    ]\n",
                "  }}\n",
                "}}\n",
            ),
            session = session,
        );

        assert_eq!(
            install(Some(&input), hooks_path, hook_path).unwrap(),
            expected
        );
    }

    #[test]
    fn install_appends_in_the_file_layout() {
        let (hooks_path, hook_path) = paths();
        let session = quoted(&session_command(hook_path));
        let foreign = concat!(
            "      {\n",
            "        \"hooks\": [\n",
            "          {\n",
            "            \"type\": \"command\",\n",
            "            \"command\": \"echo keep\"\n",
            "          }\n",
            "        ]\n",
            "      }",
        );
        let input =
            format!("{{\n  \"hooks\": {{\n    \"SessionStart\": [\n{foreign}\n    ]\n  }}\n}}\n");
        let expected = format!(
            concat!(
                "{{\n  \"hooks\": {{\n    \"SessionStart\": [\n{foreign},\n",
                "      {{\n",
                "        \"hooks\": [\n",
                "          {{\n",
                "            \"type\": \"command\",\n",
                "            \"command\": {session},\n",
                "            \"timeout\": 10\n",
                "          }}\n",
                "        ]\n",
                "      }}\n",
                "    ]\n  }}\n}}\n",
            ),
            foreign = foreign,
            session = session,
        );

        assert_eq!(
            install(Some(&input), hooks_path, hook_path).unwrap(),
            expected
        );
    }

    #[test]
    fn install_rejects_files_it_cannot_edit() {
        let (hooks_path, hook_path) = paths();
        for input in ["[]", r#"{"hooks": []}"#, r#"{"hooks":{"SessionStart":{}}}"#] {
            assert!(
                install(Some(input), hooks_path, hook_path).is_err(),
                "{input}"
            );
        }
    }
}
