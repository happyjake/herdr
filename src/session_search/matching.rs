//! Literal matching and ranking over prompt material retained by the catalog.

use std::collections::HashSet;

use super::records::Session;
use crate::api::schema::{ServerSearchSessionsParams, SessionWhy, SessionWhyField};

pub(super) struct Query<'a> {
    params: &'a ServerSearchSessionsParams,
    terms: Vec<(&'a str, String)>,
    place: Option<String>,
}

impl<'a> Query<'a> {
    pub fn new(params: &'a ServerSearchSessionsParams) -> Self {
        let mut terms = Vec::new();
        let mut seen = HashSet::new();
        for term in params
            .terms
            .iter()
            .flatten()
            .filter(|term| !term.is_empty())
        {
            let folded = term.to_lowercase();
            if seen.insert(folded.clone()) {
                terms.push((term.as_str(), folded));
            }
        }
        Self {
            params,
            terms,
            place: params.place.as_ref().map(|place| place.to_lowercase()),
        }
    }

    pub fn accepts(&self, session: &Session) -> bool {
        !(self
            .params
            .agent
            .as_ref()
            .is_some_and(|agent| *agent != session.key.agent)
            || self
                .params
                .since_ms
                .is_some_and(|since| session.last_active_at_ms < since)
            || self
                .params
                .until_ms
                .is_some_and(|until| session.started_at_ms > until)
            || self
                .params
                .since_ms
                .zip(self.params.until_ms)
                .is_some_and(|(since, until)| since > until)
            || self.place.as_ref().is_some_and(|place| {
                !session
                    .cwd
                    .as_ref()
                    .is_some_and(|cwd| cwd.to_lowercase().contains(place))
            }))
    }

    pub fn matches(
        &self,
        session: &Session,
        has_time: &dyn Fn() -> bool,
    ) -> Option<(f64, Option<SessionWhy>)> {
        if self.terms.is_empty() {
            return Some((0.0, None));
        }
        let first_preview = session
            .prompts
            .first()
            .map(|prompt| crate::prompt_trail::cut(&prompt.text, 400));
        let mut found = vec![false; self.terms.len()];
        let mut best: Option<(SessionWhyField, &str, Vec<usize>)> = None;
        let mut consider = |field, text, folded: &str| {
            let mut matching = Vec::new();
            for (index, (_, term)) in self.terms.iter().enumerate() {
                if !has_time() {
                    return;
                }
                if folded.contains(term) {
                    found[index] = true;
                    matching.push(index);
                }
            }
            if !matching.is_empty()
                && best
                    .as_ref()
                    .is_none_or(|(_, _, previous)| matching.len() > previous.len())
            {
                best = Some((field, text, matching));
            }
        };
        for label in &session.labels {
            if !has_time() {
                return None;
            }
            consider(
                SessionWhyField::Label,
                &label.label,
                &label.label.to_lowercase(),
            );
        }
        if let Some(title) = &session.title {
            consider(SessionWhyField::Title, title, &title.to_lowercase());
        }
        if let Some(first) = &first_preview {
            consider(SessionWhyField::FirstPrompt, first, &first.to_lowercase());
        }
        for prompt in &session.prompts {
            if !has_time() {
                return None;
            }
            consider(SessionWhyField::Prompt, &prompt.text, &prompt.folded);
        }
        if let Some(cwd) = &session.cwd {
            consider(SessionWhyField::Cwd, cwd, &cwd.to_lowercase());
        }
        if let Some(branch) = &session.branch {
            consider(SessionWhyField::Branch, branch, &branch.to_lowercase());
        }
        if !has_time() {
            return None;
        }
        let (field, text, matching) = best?;
        let terms: Vec<_> = matching
            .into_iter()
            .map(|index| self.terms[index].0.to_string())
            .collect();
        let line = excerpt(text, &terms);
        let folded = line.to_lowercase();
        let terms = terms
            .into_iter()
            .filter(|term| folded.contains(&term.to_lowercase()))
            .collect();
        Some((
            found.into_iter().filter(|found| *found).count() as f64,
            Some(SessionWhy { field, line, terms }),
        ))
    }
}

/// Keep a hit visible even when it was far beyond the clipped prompt preview.
fn excerpt(text: &str, terms: &[String]) -> String {
    const WIDTH: usize = 400;
    if text.chars().count() <= WIDTH {
        return text.to_string();
    }
    let folded = text.to_lowercase();
    let offset = terms
        .iter()
        .filter_map(|term| folded.find(&term.to_lowercase()))
        .min()
        .unwrap_or(0);
    let mut folded_bytes = 0;
    let first = text
        .chars()
        .position(|character| {
            folded_bytes += character.to_lowercase().map(char::len_utf8).sum::<usize>();
            folded_bytes > offset
        })
        .unwrap_or(0);
    let start = first.saturating_sub(80);
    let width = WIDTH.max(
        terms
            .iter()
            .map(|term| term.chars().count() + 80)
            .max()
            .unwrap_or(0),
    );
    let line: String = text.chars().skip(start).take(width).collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        line,
        if text.chars().count() > start + width {
            "…"
        } else {
            ""
        }
    )
}
