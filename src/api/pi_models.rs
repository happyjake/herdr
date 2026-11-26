//! pi's model list, for the agent settings answer.
//!
//! pi's `enabledModels` setting holds patterns, not models: `provider/*`
//! names every model of a provider, a bare id names that id on every
//! provider that offers it, and a near miss names the closest id. pi expands
//! the patterns against the models it can run each time it starts, and drops
//! a pattern that names none. `--model` takes one model and expands no glob,
//! so the answer lists what the patterns name, each as `provider/id`.
//!
//! Only pi knows which models it can run: its catalog, `models.json`, its
//! credentials and its extensions all decide. So the server reads
//! `pi --list-models` and holds the list (see [super::listing_cache]). A
//! change to pi's settings, models or credentials file lists again at once.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use regex::Regex;

use super::listing_cache::{run_bounded, ListingCache, FIRST_WAIT, LIST_DEADLINE};

/// One model pi can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PiModel {
    pub(super) provider: String,
    pub(super) id: String,
}

/// The files in pi's agent directory that decide which models pi can run.
const WATCHED_FILES: [&str; 3] = ["settings.json", "models.json", "auth.json"];

/// pi's thinking levels, which a pattern may carry as a `:level` suffix.
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Each watched file's modification time and length, or None while absent.
type Stamp = Vec<Option<(SystemTime, u64)>>;

/// Start reading pi's model list in the background, so that the first
/// settings answer after a server start already has it.
pub(crate) fn warm() {
    if let Some(dir) = super::agent_settings::pi_agent_dir() {
        cache().refresh(Instant::now(), &stamp(&dir));
    }
}

/// The models pi can run with the agent directory `dir`, or None while pi
/// has not listed any. Waits briefly for a listing while none is held under
/// the files as they are now.
pub(super) fn listing(dir: &Path) -> Option<Vec<PiModel>> {
    cache().get(Instant::now(), &stamp(dir), FIRST_WAIT)
}

fn cache() -> &'static Arc<ListingCache<PiModel, Stamp>> {
    static CACHE: OnceLock<Arc<ListingCache<PiModel, Stamp>>> = OnceLock::new();
    CACHE.get_or_init(|| ListingCache::new("pi-models", Box::new(list_with_pi)))
}

fn stamp(dir: &Path) -> Stamp {
    WATCHED_FILES
        .iter()
        .map(|name| {
            let metadata = std::fs::metadata(dir.join(name)).ok()?;
            Some((metadata.modified().ok()?, metadata.len()))
        })
        .collect()
}

/// Run `pi --list-models`, or None when pi is missing, fails, runs past its
/// deadline, or lists nothing.
fn list_with_pi() -> Option<Vec<PiModel>> {
    let mut command = Command::new("pi");
    command.arg("--list-models");
    let (status, output) = run_bounded(command, LIST_DEADLINE)?;
    if !status.success() {
        return None;
    }
    let models = parse_listing(&String::from_utf8_lossy(&output));
    (!models.is_empty()).then_some(models)
}

/// `pi --list-models` prints a table: a `provider model context max-out
/// thinking images` header, then one model per line. An id may hold `/`;
/// the four columns after it never hold spaces, and the last two read `yes`
/// or `no`. Lines before the header and lines of any other shape are not
/// models.
fn parse_listing(text: &str) -> Vec<PiModel> {
    let mut lines = text.lines().skip_while(|line| {
        let mut words = line.split_whitespace();
        !(words.next() == Some("provider") && words.next() == Some("model"))
    });
    if lines.next().is_none() {
        return Vec::new();
    }
    let mut models: Vec<PiModel> = Vec::new();
    for model in lines.filter_map(parse_row) {
        if !models.contains(&model) {
            models.push(model);
        }
    }
    models
}

fn parse_row(line: &str) -> Option<PiModel> {
    let mut rest = line.trim();
    let mut tail = Vec::with_capacity(4);
    for _ in 0..4 {
        let split = rest.rfind(char::is_whitespace)?;
        tail.push(&rest[split..]);
        rest = rest[..split].trim_end();
    }
    // `tail` holds images, thinking, max-out, context: last column first.
    if !tail[..2]
        .iter()
        .all(|flag| matches!(flag.trim(), "yes" | "no"))
    {
        return None;
    }
    let (provider, id) = rest.split_once(char::is_whitespace)?;
    let id = id.trim();
    (!provider.is_empty() && !id.is_empty()).then(|| PiModel {
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

/// What `enabledModels` names, in pattern order, each model once, as
/// `provider/id` with the pattern's `:level` suffix when it has one. A
/// pattern that names no listed model is left out. The models one pattern
/// names keep the listing's order, by provider and then id; pi itself keeps
/// its catalog's order there, which `--list-models` does not print.
///
/// Without a listing, the patterns holding `*` or `?` are left out, since
/// no model id holds those, and the rest are kept as written.
pub(super) fn scope(patterns: &[String], listing: Option<&[PiModel]>) -> Vec<String> {
    let Some(models) = listing else {
        let mut kept: Vec<String> = Vec::new();
        for pattern in patterns {
            if !pattern.trim().is_empty()
                && !pattern.contains(['*', '?'])
                && !kept.contains(pattern)
            {
                kept.push(pattern.clone());
            }
        }
        return kept;
    };
    let mut scoped: Vec<(&PiModel, Option<&str>)> = Vec::new();
    for pattern in patterns {
        let (matches, thinking) = if pattern.contains(['*', '?', '[']) {
            glob_matches(pattern, models)
        } else {
            match resolve(pattern, models, true) {
                Some(resolved) => (resolved.found.into_models(), resolved.thinking),
                None => (Vec::new(), None),
            }
        };
        for model in matches {
            if !scoped.iter().any(|(held, _)| *held == model) {
                scoped.push((model, thinking));
            }
        }
    }
    scoped
        .into_iter()
        .map(|(model, thinking)| match thinking {
            Some(level) => format!("{}/{}:{level}", model.provider, model.id),
            None => format!("{}/{}", model.provider, model.id),
        })
        .collect()
}

/// A glob pattern names the model it spells exactly when there is one (ids
/// such as `model[1m]` are no globs), and otherwise every model whose
/// `provider/id` or id it matches.
fn glob_matches<'a>(
    pattern: &'a str,
    models: &'a [PiModel],
) -> (Vec<&'a PiModel>, Option<&'a str>) {
    let (base, thinking) = match split_thinking(pattern) {
        Some((base, level)) => (base, Some(level)),
        None => (pattern, None),
    };
    if let Some(resolved) = resolve_exact(base, models) {
        return (resolved.into_models(), thinking);
    }
    let Some(glob) = glob_regex(base) else {
        return (Vec::new(), thinking);
    };
    let matches = models
        .iter()
        .filter(|model| {
            glob.is_match(&format!("{}/{}", model.provider, model.id)) || glob.is_match(&model.id)
        })
        .collect();
    (matches, thinking)
}

/// The models one reference names: one, or every model it names equally.
enum Found<'a> {
    One(&'a PiModel),
    Several(Vec<&'a PiModel>),
}

impl<'a> Found<'a> {
    fn into_models(self) -> Vec<&'a PiModel> {
        match self {
            Found::One(model) => vec![model],
            Found::Several(models) => models,
        }
    }
}

struct Resolved<'a> {
    found: Found<'a>,
    thinking: Option<&'a str>,
    /// The reference ended in a `:suffix` that is no thinking level, which
    /// was dropped.
    dropped_suffix: bool,
}

fn resolve_exact<'a>(text: &'a str, models: &'a [PiModel]) -> Option<Found<'a>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    select_reference(text, models, false)
}

/// A plain reference: an exact `provider/id` or id, else the closest id,
/// with an optional `:level` suffix. A `:suffix` that is no level is
/// dropped.
fn resolve<'a>(text: &'a str, models: &'a [PiModel], fuzzy: bool) -> Option<Resolved<'a>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    resolve_text(text, models, fuzzy)
}

fn resolve_text<'a>(text: &'a str, models: &'a [PiModel], fuzzy: bool) -> Option<Resolved<'a>> {
    let plain = |found| Resolved {
        found,
        thinking: None,
        dropped_suffix: false,
    };
    if let Some(found) = select_reference(text, models, fuzzy) {
        return Some(plain(found));
    }
    if fuzzy {
        if let Some(found) = select_fuzzy(fuzzy_matches(text, models.iter())) {
            return Some(plain(found));
        }
    }
    if let Some((base, level)) = split_thinking(text) {
        let inner = resolve_text(base, models, fuzzy)?;
        return Some(Resolved {
            thinking: (!inner.dropped_suffix).then_some(level),
            ..inner
        });
    }
    let colon = text.rfind(':')?;
    let inner = resolve_text(&text[..colon], models, fuzzy)?;
    Some(Resolved {
        found: inner.found,
        thinking: None,
        dropped_suffix: true,
    })
}

/// An exact id, or an exact id under a named provider. `provider/rest` is
/// only a hint, since `rest` may hold `/` and the whole text may be another
/// provider's id; the named provider wins over others offering the same
/// id. The same id on several providers, none named, names them all.
fn select_reference<'a>(text: &'a str, models: &'a [PiModel], fuzzy: bool) -> Option<Found<'a>> {
    let lower = text.to_lowercase();
    let raw: Vec<&PiModel> = models
        .iter()
        .filter(|model| model.id.to_lowercase() == lower)
        .collect();
    let named = split_provider_prefix(text, models).and_then(|(provider, model_text)| {
        let provider_models: Vec<&PiModel> = models
            .iter()
            .filter(|model| model.provider == provider)
            .collect();
        let lower_model_text = model_text.to_lowercase();
        provider_models
            .iter()
            .copied()
            .find(|model| model.id.to_lowercase() == lower_model_text)
            .or_else(|| raw.iter().copied().find(|model| model.provider == provider))
            .or_else(|| {
                fuzzy
                    .then(|| best_fuzzy(fuzzy_matches(model_text, provider_models.iter().copied())))
                    .flatten()
            })
    });
    let others = raw.iter().copied().filter(|model| Some(*model) != named);
    let candidates: Vec<&PiModel> = named.into_iter().chain(others).collect();
    let first = *candidates.first()?;
    if candidates
        .iter()
        .all(|model| model.provider == first.provider)
    {
        return Some(Found::One(first));
    }
    if let Some(named) = named {
        return Some(Found::One(named));
    }
    Some(Found::Several(candidates))
}

/// The provider a `provider/rest` text names, longest provider first, since
/// a provider name may hold `/` itself.
fn split_provider_prefix<'a>(text: &'a str, models: &'a [PiModel]) -> Option<(&'a str, &'a str)> {
    let mut best: Option<(&str, &str)> = None;
    for model in models {
        let provider = model.provider.as_str();
        let prefix = provider.trim().to_lowercase();
        if prefix.is_empty() || best.is_some_and(|(held, _)| held.len() >= provider.len()) {
            continue;
        }
        let Some(after) = text.get(prefix.len()..) else {
            continue;
        };
        let Some(offset) = after.find('/') else {
            continue;
        };
        let slash = prefix.len() + offset;
        if text[..slash].trim().to_lowercase() != prefix {
            continue;
        }
        let model_text = text[slash + 1..].trim();
        if !model_text.is_empty() {
            best = Some((provider, model_text));
        }
    }
    best
}

/// pi also matches display names, which `--list-models` does not print, so
/// a pattern that only a display name matches names nothing here.
fn fuzzy_matches<'a>(text: &str, models: impl Iterator<Item = &'a PiModel>) -> Vec<&'a PiModel> {
    let lower = text.to_lowercase();
    models
        .filter(|model| model.id.to_lowercase().contains(&lower))
        .collect()
}

fn select_fuzzy(matches: Vec<&PiModel>) -> Option<Found<'_>> {
    let best = best_fuzzy(matches.iter().copied())?;
    let tied: Vec<&PiModel> = matches
        .into_iter()
        .filter(|model| model.id == best.id)
        .collect();
    if tied.iter().all(|model| model.provider == best.provider) {
        Some(Found::One(best))
    } else {
        Some(Found::Several(tied))
    }
}

/// Aliases (`model`, `model-latest`) over dated ids (`model-20250929`),
/// then the greatest id, the first of equals.
fn best_fuzzy<'a>(matches: impl IntoIterator<Item = &'a PiModel>) -> Option<&'a PiModel> {
    let matches: Vec<&PiModel> = matches.into_iter().collect();
    let aliases: Vec<&PiModel> = matches
        .iter()
        .copied()
        .filter(|model| is_alias(&model.id))
        .collect();
    let pool = if aliases.is_empty() { matches } else { aliases };
    pool.into_iter().min_by(|a, b| b.id.cmp(&a.id))
}

fn is_alias(id: &str) -> bool {
    let bytes = id.as_bytes();
    let dated = bytes.len() >= 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit);
    id.ends_with("-latest") || !dated
}

fn split_thinking(text: &str) -> Option<(&str, &str)> {
    let (base, level) = text.rsplit_once(':')?;
    THINKING_LEVELS.contains(&level).then_some((base, level))
}

/// The glob forms pi's patterns use, case-insensitively: `*` and `?` within
/// one `/`-separated part, `**` as a whole part across parts, `[...]` and
/// `[!...]` classes, and `\` escapes. Any other character matches itself.
fn glob_regex(pattern: &str) -> Option<Regex> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("(?i)^");
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' if index + 1 < chars.len() => {
                out.push_str(&regex::escape(&chars[index + 1].to_string()));
                index += 2;
            }
            '*' => {
                let start = index;
                while chars.get(index) == Some(&'*') {
                    index += 1;
                }
                let whole_part = (start == 0 || chars[start - 1] == '/')
                    && (index == chars.len() || chars[index] == '/');
                if index - start < 2 || !whole_part {
                    out.push_str("[^/]*");
                } else if index == chars.len() {
                    out.push_str(".*");
                } else {
                    // `a/**/b` also matches `a/b`.
                    out.push_str("(?:.*/)?");
                    index += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                index += 1;
            }
            '[' => match glob_class(&chars, index) {
                Some((class, next)) => {
                    out.push_str(&class);
                    index = next;
                }
                None => {
                    out.push_str(r"\[");
                    index += 1;
                }
            },
            other => {
                out.push_str(&regex::escape(&other.to_string()));
                index += 1;
            }
        }
    }
    out.push('$');
    Regex::new(&out).ok()
}

/// The class opening at `chars[open]` as a regex class, and the index after
/// it; None when it never closes, so the `[` is literal.
fn glob_class(chars: &[char], open: usize) -> Option<(String, usize)> {
    let mut index = open + 1;
    let negated = matches!(chars.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let body_start = index;
    // A `]` first in the class is a member, not its end.
    if chars.get(index) == Some(&']') {
        index += 1;
    }
    while chars.get(index).is_some_and(|c| *c != ']') {
        index += 1;
    }
    if index >= chars.len() {
        return None;
    }
    let body = &chars[body_start..index];
    let mut class = String::from(if negated { "[^/" } else { "[" });
    for (at, c) in body.iter().enumerate() {
        let range = *c == '-'
            && at > 0
            && at + 1 < body.len()
            && body[at - 1] != '-'
            && body[at + 1] != '-';
        if range {
            class.push('-');
        } else {
            class.push_str(&regex::escape(&c.to_string()));
        }
    }
    class.push(']');
    Some((class, index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(rows: &[(&str, &str)]) -> Vec<PiModel> {
        rows.iter()
            .map(|(provider, id)| PiModel {
                provider: (*provider).into(),
                id: (*id).into(),
            })
            .collect()
    }

    fn scoped(patterns: &[&str], listing: &[PiModel]) -> Vec<String> {
        let patterns: Vec<String> = patterns.iter().map(|p| (*p).into()).collect();
        scope(&patterns, Some(listing))
    }

    /// The listing of a machine with local, gateway and subscription
    /// providers, as `pi --list-models` sorts it.
    fn fleet() -> Vec<PiModel> {
        models(&[
            ("gateway", "deepseek-v4"),
            ("gateway", "glm-5.4"),
            ("gateway", "worker"),
            ("gateway", "xiaomi/mimo-v2"),
            ("gateway", "zai/glm-5"),
            ("local", "default"),
            ("local", "hy4-flash"),
            ("openrouter", "anthropic/claude-haiku-5.5"),
            ("zai", "glm-5-turbo"),
            ("zai", "glm-5.3"),
            ("zai", "glm-5.3-flash"),
            ("zai", "glm-5.4"),
            ("zai", "glm-5.4-20260101"),
        ])
    }

    #[test]
    fn parse_reads_provider_and_id_from_the_table_after_its_header() {
        let text = "Warning: No models match pattern \"gone/model\"\n\
            provider    model                       context  max-out  thinking  images\n\
            local       default                     262.1K   32.8K    yes       yes   \n\
            openrouter  anthropic/claude-haiku-5.5  200K     64K      yes       yes\r\n\
            zai         glm 5 spaced                1M       131.1K   yes       no    \n\
            \n\
            local       default                     262.1K   32.8K    yes       yes   \n\
            Update available: run pi update now please\n\
            short line\n";
        assert_eq!(
            parse_listing(text),
            models(&[
                ("local", "default"),
                ("openrouter", "anthropic/claude-haiku-5.5"),
                ("zai", "glm 5 spaced"),
            ])
        );
    }

    #[test]
    fn parse_without_a_header_lists_nothing() {
        let text = "No models available. Run pi auth login to set one up.\n\
            local  default  262.1K  32.8K  yes  yes\n";
        assert!(parse_listing(text).is_empty());
    }

    #[test]
    fn a_provider_glob_names_each_of_its_models_and_never_itself() {
        assert_eq!(
            scoped(&["local/*"], &fleet()),
            ["local/default", "local/hy4-flash"]
        );
        // `*` stays within one part: it does not reach into an id holding `/`.
        assert_eq!(scoped(&["openrouter/*"], &fleet()), Vec::<String>::new());
        assert_eq!(
            scoped(&["openrouter/**"], &fleet()),
            ["openrouter/anthropic/claude-haiku-5.5"]
        );
        // A glob without a provider also matches bare ids, case aside.
        assert_eq!(
            scoped(&["*GLM-5.3*"], &fleet()),
            ["zai/glm-5.3", "zai/glm-5.3-flash"]
        );
        assert_eq!(
            scoped(&["zai/glm-5.?"], &fleet()),
            ["zai/glm-5.3", "zai/glm-5.4"]
        );
        assert_eq!(
            scoped(&["local/[!d]*", "zai/glm-5.[3-3]"], &fleet()),
            ["local/hy4-flash", "zai/glm-5.3"]
        );
        assert_eq!(
            scoped(&["local/*:high"], &fleet()),
            ["local/default:high", "local/hy4-flash:high"]
        );
    }

    #[test]
    fn a_pattern_that_names_no_model_is_left_out() {
        assert_eq!(
            scoped(
                &[
                    "openrouter-zdr/anthropic/claude-haiku-5.5",
                    "gone/*",
                    "",
                    "  "
                ],
                &fleet()
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_plain_pattern_names_the_model_pi_would_pick() {
        // Exact, any case, written back as the listing spells it.
        assert_eq!(scoped(&["ZAI/GLM-5.4"], &fleet()), ["zai/glm-5.4"]);
        // An id on one provider needs no provider.
        assert_eq!(scoped(&["worker"], &fleet()), ["gateway/worker"]);
        // The same id on two providers, none named: both.
        assert_eq!(
            scoped(&["glm-5.4"], &fleet()),
            ["gateway/glm-5.4", "zai/glm-5.4"]
        );
        // A gateway id that looks like `provider/id` for no listed provider.
        assert_eq!(
            scoped(&["xiaomi/mimo-v2"], &fleet()),
            ["gateway/xiaomi/mimo-v2"]
        );
        // A listed provider wins over another provider's id spelled the same,
        // even by its closest id.
        assert_eq!(scoped(&["zai/glm-5"], &fleet()), ["zai/glm-5.4"]);
        // A near miss under a provider: its greatest undated id holding it.
        assert_eq!(scoped(&["zai/glm-5."], &fleet()), ["zai/glm-5.4"]);
        assert_eq!(scoped(&["deepseek"], &fleet()), ["gateway/deepseek-v4"]);
        // Thinking levels ride along; any other suffix is dropped.
        assert_eq!(
            scoped(&["zai/glm-5.4:high"], &fleet()),
            ["zai/glm-5.4:high"]
        );
        assert_eq!(scoped(&["zai/glm-5.4:turbo"], &fleet()), ["zai/glm-5.4"]);
    }

    #[test]
    fn each_model_is_named_once_in_pattern_order() {
        assert_eq!(
            scoped(
                &["zai/glm-5.4", "local/*", "zai/*:high", "local/default"],
                &fleet()
            ),
            [
                "zai/glm-5.4",
                "local/default",
                "local/hy4-flash",
                // `zai/*` also matches the gateway id `zai/glm-5`, as in pi.
                "gateway/zai/glm-5:high",
                "zai/glm-5-turbo:high",
                "zai/glm-5.3:high",
                "zai/glm-5.3-flash:high",
                "zai/glm-5.4-20260101:high",
            ]
        );
    }

    #[test]
    fn an_id_that_looks_like_a_glob_names_itself() {
        let listing = models(&[
            ("anthropic", "claude-sonnet[1m]"),
            ("anthropic", "claude-sonnetx"),
        ]);
        assert_eq!(
            scoped(&["anthropic/claude-sonnet[1m]"], &listing),
            ["anthropic/claude-sonnet[1m]"]
        );
        assert_eq!(
            scoped(&["anthropic/claude-sonnet[x]"], &listing),
            ["anthropic/claude-sonnetx"]
        );
    }

    #[test]
    fn without_a_listing_globs_are_left_out_and_the_rest_kept_as_written() {
        let patterns: Vec<String> = [
            "zai/glm-5.4",
            "local/*",
            "gone/model",
            "zai/glm-5.4",
            "m[1m]",
            "a?",
            "",
        ]
        .iter()
        .map(|p| (*p).into())
        .collect();
        assert_eq!(
            scope(&patterns, None),
            ["zai/glm-5.4", "gone/model", "m[1m]"]
        );
    }

    #[test]
    fn a_class_that_never_closes_is_literal() {
        let listing = models(&[("p", "a[b"), ("p", "ab")]);
        assert_eq!(scoped(&["p/a[b*"], &listing), ["p/a[b"]);
    }

    #[test]
    fn the_stamp_changes_with_each_watched_file() {
        let dir = std::env::temp_dir().join(format!("pi-models-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = stamp(&dir);
        assert_eq!(empty, vec![None, None, None]);
        let mut seen = vec![empty];
        for name in WATCHED_FILES {
            std::fs::write(dir.join(name), "{}").unwrap();
            let now = stamp(&dir);
            assert!(!seen.contains(&now), "{name} did not change the stamp");
            seen.push(now);
        }
        std::fs::write(dir.join("unrelated.json"), "{}").unwrap();
        assert!(seen.contains(&stamp(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
