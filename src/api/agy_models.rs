//! agy's model list, for the agent settings answer.
//!
//! Only `agy models` knows which models agy offers, and it asks agy's
//! service over the network, which takes seconds. So the server holds the
//! last list it read and lists again in the background once that list is
//! stale. An answer waits for agy only while the server holds no list at
//! all, and then only briefly.

use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use super::listing_cache::{run_bounded, ListingCache, FIRST_WAIT, LIST_DEADLINE};
use super::schema::AgyModel;

/// Start reading agy's model list in the background, so that the first
/// settings answer after a server start already has it.
pub(crate) fn warm() {
    cache().refresh(Instant::now(), &());
}

/// The held model list. Starts a background listing when the list is
/// stale; waits briefly for one in flight only while no list is held.
pub(super) fn models() -> Vec<AgyModel> {
    cache()
        .get(Instant::now(), &(), FIRST_WAIT)
        .unwrap_or_default()
}

/// agy's list depends on no file the server reads, so every listing runs
/// under the same settings.
fn cache() -> &'static Arc<ListingCache<AgyModel, ()>> {
    static CACHE: OnceLock<Arc<ListingCache<AgyModel, ()>>> = OnceLock::new();
    CACHE.get_or_init(|| ListingCache::new("agy-models", Box::new(list_with_agy)))
}

/// Run `agy models`, or None when agy is missing, fails, runs past its
/// deadline, or lists nothing.
fn list_with_agy() -> Option<Vec<AgyModel>> {
    let mut command = Command::new("agy");
    command.arg("models");
    let (status, output) = run_bounded(command, LIST_DEADLINE)?;
    if !status.success() {
        return None;
    }
    let models = parse_models(&String::from_utf8_lossy(&output));
    (!models.is_empty()).then_some(models)
}

/// One model per `<id>\t<name>` line, the first of any repeated id. Any
/// other line (a progress note, a blank) is not a model.
fn parse_models(text: &str) -> Vec<AgyModel> {
    let mut models: Vec<AgyModel> = Vec::new();
    for line in text.lines() {
        let Some((id, name)) = line.split_once('\t') else {
            continue;
        };
        let (id, name) = (id.trim(), name.trim());
        if id.is_empty()
            || name.is_empty()
            || id.contains(char::is_whitespace)
            || models.iter().any(|model| model.id == id)
        {
            continue;
        }
        models.push(AgyModel {
            id: id.to_string(),
            name: name.to_string(),
        });
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, name: &str) -> AgyModel {
        AgyModel {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn parse_reads_tab_separated_ids_and_names_in_order() {
        let text = "Fetching available models...\n\
            gemini-pro-orchestrator-argon\tOrchestrator (Argon)\n\
            \n\
            gemini-4-argon-high\tGemini 4 Argon (High)\r\n\
            gemini-pro-orchestrator-argon\tA repeat\n\
            spaced id\tNot a model\n\
            \tNo id\n\
            no-name\t\n";
        assert_eq!(
            parse_models(text),
            vec![
                model("gemini-pro-orchestrator-argon", "Orchestrator (Argon)"),
                model("gemini-4-argon-high", "Gemini 4 Argon (High)"),
            ]
        );
    }
}
