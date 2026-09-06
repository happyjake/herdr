//! Place lookup — turning a name someone types into directories this server
//! has already been to.
//!
//! The answer comes out of memory only: the server's own workspaces, the z
//! frecency database, the Claude Code project list, live tmux pane
//! directories, and the working directories recent agent sessions started
//! in. Nothing here browses the disk, so a directory nobody has ever opened
//! is not a place this server can name. Every source is optional and a
//! missing or unreadable one is skipped without a word.
//!
//! Comparison folds both sides to NFC and then lowercases them, which is
//! the same rule the phone-side matcher applies, so the two agree on a
//! decomposed accent. Full Unicode case folding is deliberately not applied
//! — `ß` and `ss` stay different here as they do there — and that is the
//! known residual.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

use crate::api::schema::PlaceSource;

/// Most places one lookup answers with.
pub(crate) const MAX_PLACES: usize = 8;

/// Newest session transcripts whose first line is read, per agent.
const SESSION_FILES_READ: usize = 40;

/// Transcripts one walk examines before it stops descending. The walk
/// enters bucket directories newest first, so the bound cuts off the oldest
/// history rather than the newest sessions, and the newest transcripts are
/// then chosen by modification time across every bucket it did reach.
const SESSION_FILES_SCANNED: usize = 400;

/// Largest source file this reads, so a pathological dotfile is skipped
/// rather than parsed.
const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// What this server remembers, in the order the sources are trusted.
///
/// `workspaces` and `tmux` are collected by the caller because they come
/// from live state rather than the home directory; everything else is read
/// from `home`, which is what makes the whole lookup testable against a
/// fixture home.
#[derive(Debug, Default, Clone)]
pub(crate) struct Desk {
    /// Directories the server's own workspaces stand in — live first, then
    /// the persisted layout.
    pub workspaces: Vec<PathBuf>,
    /// Home directory carrying the remaining sources.
    pub home: Option<PathBuf>,
    /// Working directories of live tmux panes. Empty when tmux is absent.
    pub tmux: Vec<PathBuf>,
}

/// One answered directory and the memory that held it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place {
    pub path: String,
    pub source: PlaceSource,
}

#[derive(Debug, Clone)]
struct Candidate {
    path: String,
    source: PlaceSource,
}

/// Answer up to `limit` directories for `query`.
///
/// A query starting with `/` or `~` is a literal path: it is answered as
/// itself when it exists, and otherwise its folder name is run through the
/// matcher so a near miss still comes back as a candidate. Anything else is
/// a place name, matched case-insensitively against the remembered
/// directories.
pub(crate) fn lookup(desk: &Desk, query: &str, limit: usize) -> Vec<Place> {
    let candidates = collect(desk);

    if let Some(literal) = literal_path(query, desk.home.as_deref()) {
        if Path::new(&literal).is_dir() {
            // A typed path that exists is its own answer. It is reported
            // under the memory that already held it; a directory this
            // server has never opened is still real, and carries the
            // server's own tier rather than borrowing another source's.
            let source = candidates
                .iter()
                .find(|candidate| candidate.path == literal)
                .map_or(PlaceSource::Workspace, |candidate| candidate.source);
            return vec![Place {
                path: literal,
                source,
            }];
        }
        let name = folder_name(&literal);
        return rank(&name, candidates, limit);
    }

    rank(query, candidates, limit)
}

/// How long tmux gets to answer before the lookup goes on without it.
pub(crate) const TMUX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Working directories of live tmux panes, or nothing at all when tmux is
/// not installed, not running, unhappy, or simply too slow. A wedged tmux
/// server is a source that is missing, never a request that hangs, so the
/// process is killed when the deadline passes.
pub(crate) async fn tmux_pane_dirs() -> Vec<PathBuf> {
    let mut command = tokio::process::Command::new("tmux");
    command
        .args(["list-panes", "-a", "-F", "#{pane_current_path}"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(Ok(output)) = tokio::time::timeout(TMUX_TIMEOUT, command.output()).await else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(PathBuf::from)
        .collect()
}

fn collect(desk: &Desk) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    push_all(
        &mut candidates,
        &mut seen,
        desk.workspaces
            .iter()
            .map(PathBuf::as_path)
            .filter_map(path_string),
        PlaceSource::Workspace,
    );
    let home = desk.home.as_deref();
    push_all(
        &mut candidates,
        &mut seen,
        z_dirs(home).into_iter(),
        PlaceSource::Z,
    );
    push_all(
        &mut candidates,
        &mut seen,
        claude_dirs(home).into_iter(),
        PlaceSource::Claude,
    );
    push_all(
        &mut candidates,
        &mut seen,
        desk.tmux
            .iter()
            .map(PathBuf::as_path)
            .filter_map(path_string),
        PlaceSource::Tmux,
    );
    push_all(
        &mut candidates,
        &mut seen,
        session_dirs(home).into_iter(),
        PlaceSource::Session,
    );

    candidates
}

fn push_all(
    candidates: &mut Vec<Candidate>,
    seen: &mut HashSet<String>,
    paths: impl Iterator<Item = String>,
    source: PlaceSource,
) {
    for path in paths {
        let Some(path) = normalize(&path) else {
            continue;
        };
        if seen.insert(path.clone()) {
            candidates.push(Candidate { path, source });
        }
    }
}

fn path_string(path: &Path) -> Option<String> {
    path.to_str().map(str::to_string)
}

/// Trim a remembered path down to the form answers are compared and
/// reported in, or drop it when it could never name a place.
fn normalize(path: &str) -> Option<String> {
    let trimmed = path.trim();
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() || !Path::new(trimmed).is_absolute() {
        return None;
    }
    Some(trimmed.to_string())
}

fn literal_path(query: &str, home: Option<&Path>) -> Option<String> {
    if !(query.starts_with('/') || query.starts_with('~')) {
        return None;
    }
    let expanded = expand_tilde(query, home);
    let trimmed = expanded.trim_end_matches('/');
    if trimmed.is_empty() {
        return Some(expanded);
    }
    Some(trimmed.to_string())
}

/// Expand a leading `~` exactly as the server already expands a configured
/// new-terminal directory, against the home this lookup was handed.
fn expand_tilde(query: &str, home: Option<&Path>) -> String {
    let expanded = match home {
        Some(home) => {
            if query == "~" {
                home.to_path_buf()
            } else if let Some(rest) = query.strip_prefix("~/") {
                home.join(rest)
            } else {
                return query.to_string();
            }
        }
        None => crate::worktree::expand_tilde_path(query),
    };
    expanded
        .to_str()
        .map_or_else(|| query.to_string(), str::to_string)
}

/// The last component of a path, read the way the platform reads paths, so
/// a Windows source keeps its folder tier instead of matching whole drive
/// strings.
fn folder_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |name| name.to_string_lossy().into())
}

/// Order the matches.
///
/// The folder-name tier stands above the path tier. Inside a tier a
/// subsequence match stands above a match that only survived the edit
/// distance, closer edit distances first, and everything still level is
/// left in source order — which is where z's frecency and a session's
/// recency already put it. Only directories that still exist are answered,
/// and the check happens after the sort so a lookup stats what it reports
/// rather than everything it remembers.
fn rank(query: &str, candidates: Vec<Candidate>, limit: usize) -> Vec<Place> {
    let query = folded(query);
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }
    let tolerance = tolerance_for(&query);

    let mut matched: Vec<(u8, u8, usize, usize, Candidate)> = Vec::new();
    for (order, candidate) in candidates.into_iter().enumerate() {
        if let Some((tier, kind, distance)) = match_key(&query, tolerance, &candidate.path) {
            matched.push((tier, kind, distance, order, candidate));
        }
    }

    matched.sort_by_key(|(tier, kind, distance, order, _)| (*tier, *kind, *distance, *order));

    let mut answered = HashSet::new();
    let mut places = Vec::new();
    for (_, _, _, _, candidate) in matched {
        if places.len() == limit {
            break;
        }
        let path = Path::new(&candidate.path);
        if !path.is_dir() {
            continue;
        }
        // Two spellings of one directory — a symlink and what it points at,
        // say — are one place. The canonical form is only the key; the
        // answer keeps the spelling that ranked first, which is the one the
        // server actually remembers.
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(&candidate.path));
        if !answered.insert(key) {
            continue;
        }
        places.push(Place {
            path: candidate.path,
            source: candidate.source,
        });
    }
    places
}

/// How far off the folder name a query is allowed to be. A longer query
/// carries more evidence, so it earns the wider tolerance.
fn tolerance_for(query: &[char]) -> usize {
    if query.len() >= 5 {
        2
    } else {
        1
    }
}

/// Score one remembered path against a lowercased query, or refuse it.
///
/// The tuple sorts ascending: tier (folder name, then path), then the kind
/// of match (subsequence, then edit distance), then how far the edit
/// distance ran.
fn match_key(query: &[char], tolerance: usize, path: &str) -> Option<(u8, u8, usize)> {
    let folder = folded(&folder_name(path));
    if is_subsequence(query, &folder) {
        return Some((0, 0, 0));
    }
    if let Some(distance) = edit_distance_within(query, &folder, tolerance) {
        return Some((0, 1, distance));
    }
    if is_subsequence(query, &folded(path)) {
        return Some((1, 0, 0));
    }
    None
}

/// Fold one side of a comparison: NFC first, so a decomposed accent is the
/// same letter as a precomposed one, then lowercase.
fn folded(value: &str) -> Vec<char> {
    value
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .chars()
        .collect()
}

fn is_subsequence(needle: &[char], haystack: &[char]) -> bool {
    let mut wanted = 0;
    for candidate in haystack {
        if wanted == needle.len() {
            return true;
        }
        if needle[wanted] == *candidate {
            wanted += 1;
        }
    }
    wanted == needle.len()
}

/// Levenshtein distance, answered only while it stays within `max`.
fn edit_distance_within(left: &[char], right: &[char], max: usize) -> Option<usize> {
    if left.len().abs_diff(right.len()) > max {
        return None;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (i, l) in left.iter().enumerate() {
        current[0] = i + 1;
        let mut row_best = current[0];
        for (j, r) in right.iter().enumerate() {
            let cost = usize::from(l != r);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            row_best = row_best.min(current[j + 1]);
        }
        if row_best > max {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[right.len()];
    (distance <= max).then_some(distance)
}

fn read_source_file(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The z frecency database: `path|rank|time` a line, highest rank first.
fn z_dirs(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    let Some(contents) = read_source_file(&home.join(".z")) else {
        return Vec::new();
    };

    let mut entries: Vec<(f64, String)> = contents
        .lines()
        .filter_map(|line| {
            // Read the rank off the end so a path holding a separator still
            // parses, which splitting from the front would not manage.
            let (path, rank) = match line.rsplitn(3, '|').collect::<Vec<_>>()[..] {
                [_time, rank, path] => (path, rank),
                [rank, path] => (path, rank),
                _ => return None,
            };
            Some((rank.trim().parse::<f64>().ok()?, path.to_string()))
        })
        .collect();
    entries.sort_by(|left, right| right.0.total_cmp(&left.0));
    entries.into_iter().map(|(_, path)| path).collect()
}

#[derive(Deserialize)]
struct ClaudeProjects {
    #[serde(default)]
    projects: BTreeMap<String, serde::de::IgnoredAny>,
}

/// The Claude Code project list, which keys its projects by directory.
fn claude_dirs(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    let Some(contents) = read_source_file(&home.join(".claude.json")) else {
        return Vec::new();
    };
    serde_json::from_str::<ClaudeProjects>(&contents)
        .map(|parsed| parsed.projects.into_keys().collect())
        .unwrap_or_default()
}

/// Directories the saved layout stands in: each workspace's own, then its
/// panes' in pane order, so one file always reads the same way.
pub(crate) fn persisted_layout_dirs(path: &Path) -> Vec<PathBuf> {
    let Some(snapshot) = crate::persist::load_from_path(path) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for workspace in &snapshot.workspaces {
        dirs.push(workspace.identity_cwd.clone());
        for tab in &workspace.tabs {
            let mut panes: Vec<_> = tab.panes.iter().collect();
            panes.sort_by_key(|(number, _)| **number);
            dirs.extend(panes.into_iter().map(|(_, pane)| pane.cwd.clone()));
        }
    }
    dirs
}

#[derive(Deserialize)]
struct SessionHead {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    payload: Option<SessionPayload>,
}

#[derive(Deserialize)]
struct SessionPayload {
    #[serde(default)]
    cwd: Option<String>,
}

/// Directories recent agent sessions started in, newest first.
fn session_dirs(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    for (root, depth) in [
        (home.join(".pi").join("agent").join("sessions"), 1usize),
        (home.join(".codex").join("sessions"), 3usize),
    ] {
        for transcript in newest_transcripts(&root, depth) {
            if let Some(cwd) = transcript_cwd(&transcript) {
                dirs.push(cwd);
            }
        }
    }
    dirs
}

/// Collect the newest `.jsonl` files sitting exactly `depth` directories
/// under `root`. The walk is a fixed shape, not a search: it never looks
/// wider than the layout an agent writes its sessions in.
///
/// The newest are chosen by modification time across every bucket the walk
/// reached — not per bucket and not in traversal order — so one fresh
/// transcript is never hidden by a crowd of old ones filed elsewhere. The
/// walk enters buckets newest first and stops after
/// [`SESSION_FILES_SCANNED`] transcripts, so the bound cuts the oldest
/// history rather than the newest sessions.
fn newest_transcripts(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk_transcripts(root, depth, &mut found);

    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found
        .into_iter()
        .take(SESSION_FILES_READ)
        .map(|(_, path)| path)
        .collect()
}

fn walk_transcripts(dir: &Path, depth: usize, found: &mut Vec<(SystemTime, PathBuf)>) {
    if found.len() >= SESSION_FILES_SCANNED {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, entry.path()))
        })
        .collect();
    // Newest first, with the name as a stable tiebreak, so the bound below
    // cuts the oldest history rather than whichever bucket sorts last.
    children.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.file_name().cmp(&left.1.file_name()))
    });

    for (modified, path) in children {
        if found.len() >= SESSION_FILES_SCANNED {
            return;
        }
        if depth == 0 {
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                found.push((modified, path));
            }
        } else if path.is_dir() {
            walk_transcripts(&path, depth - 1, found);
        }
    }
}

fn transcript_cwd(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let head: SessionHead = serde_json::from_str(first.trim()).ok()?;
    head.cwd
        .or_else(|| head.payload.and_then(|payload| payload.cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A throwaway home directory holding real dotfiles and real
    /// directories, so existence and ordering are answered by a filesystem
    /// rather than by a stub.
    struct FixtureHome {
        root: PathBuf,
    }

    impl FixtureHome {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "herdr-place-lookup-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("fixture home");
            Self { root }
        }

        /// Create a directory under the fixture home and answer its path.
        fn dir(&self, relative: &str) -> String {
            let path = self.root.join(relative);
            std::fs::create_dir_all(&path).expect("fixture directory");
            path.to_str().expect("utf-8 fixture path").to_string()
        }

        /// Name a path under the fixture home without creating it.
        fn absent(&self, relative: &str) -> String {
            self.root
                .join(relative)
                .to_str()
                .expect("utf-8 fixture path")
                .to_string()
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture parent");
            }
            std::fs::write(path, contents).expect("fixture file");
        }

        fn z(&self, entries: &[(&str, f64)]) {
            let contents: String = entries
                .iter()
                .map(|(path, rank)| format!("{path}|{rank}|1700000000\n"))
                .collect();
            self.write(".z", &contents);
        }

        fn claude(&self, projects: &[&str]) {
            let projects: Vec<String> = projects
                .iter()
                .map(|path| format!("{}: {{}}", serde_json::to_string(path).unwrap()))
                .collect();
            self.write(
                ".claude.json",
                &format!("{{\"projects\": {{{}}}}}", projects.join(", ")),
            );
        }

        /// Write a pi session transcript whose first line carries `cwd`.
        fn pi_session(&self, name: &str, cwd: &str) {
            self.write(
                &format!(".pi/agent/sessions/day/{name}.jsonl"),
                &format!(
                    "{{\"cwd\": {}}}\n{{\"role\": \"user\"}}\n",
                    serde_json::to_string(cwd).unwrap()
                ),
            );
        }

        /// Write a codex session transcript, whose cwd sits under `payload`.
        fn codex_session(&self, name: &str, cwd: &str) {
            self.write(
                &format!(".codex/sessions/2026/09/06/{name}.jsonl"),
                &format!(
                    "{{\"payload\": {{\"cwd\": {}}}}}\n",
                    serde_json::to_string(cwd).unwrap()
                ),
            );
        }

        fn desk(&self) -> Desk {
            Desk {
                home: Some(self.root.clone()),
                ..Desk::default()
            }
        }
    }

    impl Drop for FixtureHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn paths(places: &[Place]) -> Vec<&str> {
        places.iter().map(|place| place.path.as_str()).collect()
    }

    fn sources(places: &[Place]) -> Vec<PlaceSource> {
        places.iter().map(|place| place.source).collect()
    }

    #[test]
    fn every_source_answers_and_they_answer_in_order() {
        let home = FixtureHome::new();
        let workspace = home.dir("code/alpha-workspace");
        let z = home.dir("code/alpha-z");
        let claude = home.dir("code/alpha-claude");
        let tmux = home.dir("code/alpha-tmux");
        let pi = home.dir("code/alpha-pi");

        home.z(&[(z.as_str(), 40.0)]);
        home.claude(&[claude.as_str()]);
        home.pi_session("one", &pi);

        let mut desk = home.desk();
        desk.workspaces = vec![PathBuf::from(&workspace)];
        desk.tmux = vec![PathBuf::from(&tmux)];

        let places = lookup(&desk, "alpha", MAX_PLACES);

        assert_eq!(paths(&places), vec![workspace, z, claude, tmux, pi]);
        assert_eq!(
            sources(&places),
            vec![
                PlaceSource::Workspace,
                PlaceSource::Z,
                PlaceSource::Claude,
                PlaceSource::Tmux,
                PlaceSource::Session,
            ]
        );
    }

    #[test]
    fn a_missing_or_unreadable_source_is_skipped_without_a_word() {
        let home = FixtureHome::new();
        let claude = home.dir("code/beta-claude");
        // No `.z`, no sessions, no tmux; `.claude.json` is not even JSON.
        home.write(".claude.json", "{ this is not json");

        let mut desk = home.desk();
        desk.workspaces = vec![PathBuf::from(&claude)];

        let places = lookup(&desk, "beta", MAX_PLACES);
        assert_eq!(paths(&places), vec![claude]);

        // And a desk that remembers nothing at all answers nothing.
        let empty = FixtureHome::new();
        assert!(lookup(&empty.desk(), "beta", MAX_PLACES).is_empty());
    }

    #[test]
    fn z_answers_its_highest_rank_first() {
        let home = FixtureHome::new();
        let cold = home.dir("code/gamma-cold");
        let hot = home.dir("code/gamma-hot");
        home.z(&[(cold.as_str(), 3.0), (hot.as_str(), 120.5)]);

        let places = lookup(&home.desk(), "gamma", MAX_PLACES);
        assert_eq!(paths(&places), vec![hot, cold]);
    }

    #[test]
    fn a_path_two_sources_remember_is_answered_once_by_the_first() {
        let home = FixtureHome::new();
        let shared = home.dir("code/delta");
        home.z(&[(shared.as_str(), 9.0)]);
        home.claude(&[shared.as_str()]);
        home.pi_session("one", &shared);

        let places = lookup(&home.desk(), "delta", MAX_PLACES);
        assert_eq!(paths(&places), vec![shared.as_str()]);
        assert_eq!(sources(&places), vec![PlaceSource::Z]);
    }

    #[test]
    fn a_trailing_slash_does_not_make_a_second_place() {
        let home = FixtureHome::new();
        let shared = home.dir("code/epsilon");
        home.z(&[(shared.as_str(), 9.0)]);
        home.claude(&[&format!("{shared}/")]);

        let places = lookup(&home.desk(), "epsilon", MAX_PLACES);
        assert_eq!(paths(&places), vec![shared.as_str()]);
    }

    #[test]
    fn a_remembered_directory_that_is_gone_is_not_answered() {
        let home = FixtureHome::new();
        let gone = home.absent("code/zeta-gone");
        let here = home.dir("code/zeta-here");
        home.z(&[(gone.as_str(), 90.0), (here.as_str(), 1.0)]);

        let places = lookup(&home.desk(), "zeta", MAX_PLACES);
        assert_eq!(paths(&places), vec![here]);
    }

    #[test]
    fn a_remembered_file_is_not_a_place() {
        let home = FixtureHome::new();
        home.write("code/eta-file", "not a directory");
        let file = home.absent("code/eta-file");
        home.z(&[(file.as_str(), 90.0)]);

        assert!(lookup(&home.desk(), "eta", MAX_PLACES).is_empty());
    }

    #[test]
    fn the_answer_is_capped_at_eight_and_honours_a_smaller_limit() {
        let home = FixtureHome::new();
        let mut dirs = Vec::new();
        for index in 0..12 {
            dirs.push(home.dir(&format!("code/theta-{index}")));
        }
        let entries: Vec<(&str, f64)> = dirs
            .iter()
            .enumerate()
            .map(|(index, path)| (path.as_str(), (100 - index) as f64))
            .collect();
        home.z(&entries);

        assert_eq!(lookup(&home.desk(), "theta", MAX_PLACES).len(), 8);
        assert_eq!(lookup(&home.desk(), "theta", 3).len(), 3);
    }

    #[test]
    fn the_folder_name_tier_stands_above_the_path_tier() {
        let home = FixtureHome::new();
        let in_path = home.dir("iota/nested-elsewhere");
        let in_folder = home.dir("code/iota");
        // The path-tier match is the hotter z entry, and still loses.
        home.z(&[(in_path.as_str(), 500.0), (in_folder.as_str(), 1.0)]);

        let places = lookup(&home.desk(), "iota", MAX_PLACES);
        assert_eq!(
            places.first().map(|place| place.path.as_str()),
            Some(in_folder.as_str())
        );
        assert!(paths(&places).contains(&in_path.as_str()));
    }

    #[test]
    fn a_subsequence_match_stands_above_a_typo_inside_the_same_tier() {
        let home = FixtureHome::new();
        let typo = home.dir("code/kapa");
        let subsequence = home.dir("code/kappa-runtime");
        // The typo is the hotter z entry, and still loses to a folder the
        // query actually spells out.
        home.z(&[(typo.as_str(), 500.0), (subsequence.as_str(), 1.0)]);

        let places = lookup(&home.desk(), "kappa", MAX_PLACES);
        assert_eq!(paths(&places), vec![subsequence, typo]);
    }

    #[test]
    fn a_typo_on_the_folder_name_still_finds_the_place() {
        let home = FixtureHome::new();
        let rocket = home.dir("code/rocket");
        home.z(&[(rocket.as_str(), 10.0)]);

        let places = lookup(&home.desk(), "rocjet", MAX_PLACES);
        assert_eq!(paths(&places), vec![rocket]);
    }

    #[test]
    fn a_name_nothing_remembers_answers_nothing() {
        let home = FixtureHome::new();
        let rocket = home.dir("code/rocket");
        home.z(&[(rocket.as_str(), 10.0)]);

        assert!(lookup(&home.desk(), "qqqqqq", MAX_PLACES).is_empty());
    }

    #[test]
    fn the_tolerance_widens_at_five_characters() {
        assert_eq!(tolerance_for(&chars("rock")), 1);
        assert_eq!(tolerance_for(&chars("rocket")), 2);

        // Four characters, two edits away: refused.
        assert_eq!(match_key(&chars("abcd"), 1, "/x/abef"), None);
        // The same two edits, on a query long enough to earn them.
        assert_eq!(match_key(&chars("abcde"), 2, "/x/abefe"), Some((0, 1, 2)));
    }

    #[test]
    fn the_match_key_ranks_folder_then_kind_then_distance() {
        assert_eq!(match_key(&chars("zeta"), 1, "/code/zeta"), Some((0, 0, 0)));
        assert_eq!(
            match_key(&chars("zeta"), 1, "/code/zeta/nested"),
            Some((1, 0, 0))
        );
        assert_eq!(
            match_key(&chars("rocjet"), 2, "/code/rocket"),
            Some((0, 1, 1))
        );
        assert_eq!(match_key(&chars("rocjet"), 2, "/code/nothing"), None);
    }

    #[test]
    fn matching_ignores_case() {
        let home = FixtureHome::new();
        let shouty = home.dir("code/LAMBDA");
        home.z(&[(shouty.as_str(), 10.0)]);

        assert_eq!(
            paths(&lookup(&home.desk(), "lambda", MAX_PLACES)),
            vec![shouty]
        );
    }

    #[test]
    fn a_literal_path_that_exists_is_its_own_answer() {
        let home = FixtureHome::new();
        let mu = home.dir("code/mu");
        home.z(&[(mu.as_str(), 10.0)]);

        let places = lookup(&home.desk(), &mu, MAX_PLACES);
        assert_eq!(paths(&places), vec![mu.as_str()]);
        assert_eq!(sources(&places), vec![PlaceSource::Z]);
    }

    #[test]
    fn a_literal_path_expands_a_leading_tilde() {
        let home = FixtureHome::new();
        let nu = home.dir("code/nu");

        let places = lookup(&home.desk(), "~/code/nu", MAX_PLACES);
        assert_eq!(paths(&places), vec![nu]);
    }

    #[test]
    fn a_literal_path_that_is_gone_comes_back_as_its_near_miss() {
        let home = FixtureHome::new();
        let real = home.dir("code/rocket-app");
        home.z(&[(real.as_str(), 10.0)]);

        // One wrong character in the folder name of a path that is spelled
        // out in full.
        let typo = home.absent("code/rocket-ap");
        let places = lookup(&home.desk(), &typo, MAX_PLACES);
        assert_eq!(paths(&places), vec![real]);
    }

    #[test]
    fn a_literal_path_nothing_remembers_still_answers_when_it_exists() {
        let home = FixtureHome::new();
        let xi = home.dir("code/xi");

        let places = lookup(&home.desk(), &xi, MAX_PLACES);
        assert_eq!(paths(&places), vec![xi.as_str()]);
        assert_eq!(sources(&places), vec![PlaceSource::Workspace]);
    }

    #[test]
    fn a_codex_transcript_carries_its_cwd_under_payload() {
        let home = FixtureHome::new();
        let omicron = home.dir("code/omicron");
        home.codex_session("one", &omicron);

        assert_eq!(
            paths(&lookup(&home.desk(), "omicron", MAX_PLACES)),
            vec![omicron]
        );
    }

    #[test]
    fn the_newest_transcript_is_read_first() {
        let home = FixtureHome::new();
        let older = home.dir("code/pi-older");
        let newer = home.dir("code/pi-newer");
        home.pi_session("aaa-older", &older);
        home.pi_session("zzz-newer", &newer);
        // Names sort the other way round, so only the timestamps can put
        // the newer transcript first.
        let stamp = std::time::SystemTime::now();
        set_modified(
            &home.root.join(".pi/agent/sessions/day/aaa-older.jsonl"),
            stamp - std::time::Duration::from_secs(3600),
        );
        set_modified(
            &home.root.join(".pi/agent/sessions/day/zzz-newer.jsonl"),
            stamp,
        );

        assert_eq!(
            paths(&lookup(&home.desk(), "pi-", MAX_PLACES)),
            vec![newer, older]
        );
    }

    #[test]
    fn a_relative_or_empty_remembered_path_is_dropped() {
        assert_eq!(normalize("  /code/rho  "), Some("/code/rho".to_string()));
        assert_eq!(normalize("/code/rho/"), Some("/code/rho".to_string()));
        assert_eq!(normalize("code/rho"), None);
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("/"), None);
    }

    #[test]
    fn the_z_rank_is_read_off_the_end_so_a_separator_in_a_path_survives() {
        let home = FixtureHome::new();
        let odd = home.dir("code/sig|ma");
        home.z(&[(odd.as_str(), 10.0)]);

        assert_eq!(paths(&lookup(&home.desk(), "sigma", MAX_PLACES)), vec![odd]);
    }

    #[test]
    fn a_saved_layout_is_a_source() {
        let home = FixtureHome::new();
        let workspace = home.dir("code/upsilon-workspace");
        let pane = home.dir("code/upsilon-pane");
        home.write(
            "session.json",
            &serde_json::json!({
                "version": 3,
                "workspaces": [{
                    "id": "wtest",
                    "identity_cwd": workspace,
                    "tabs": [{
                        "layout": { "Pane": 0 },
                        "panes": {
                            "0": { "cwd": workspace },
                            "1": { "cwd": pane }
                        },
                        "zoomed": false,
                        "focused": 0,
                        "root_pane": 0
                    }],
                    "active_tab": 0
                }],
                "active": 0,
                "selected": 0
            })
            .to_string(),
        );

        let dirs = persisted_layout_dirs(&home.root.join("session.json"));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from(&workspace),
                PathBuf::from(&workspace),
                PathBuf::from(&pane)
            ]
        );

        // And a layout that is not there is simply not a source.
        assert!(persisted_layout_dirs(&home.root.join("nothing.json")).is_empty());
    }

    #[test]
    fn the_newest_transcript_wins_over_a_crowded_older_bucket() {
        let home = FixtureHome::new();
        let crowded = home.dir("code/tau-crowded");
        let newest = home.dir("code/tau-newest");

        // The crowd is written first and named so it sorts first by name:
        // only reading modification times can put the lone newer
        // transcript ahead of it.
        let stamp = std::time::SystemTime::now();
        for index in 0..SESSION_FILES_SCANNED {
            home.write(
                &format!(".pi/agent/sessions/zzz-crowd/{index}.jsonl"),
                &format!(
                    "{{\"cwd\": {}}}\n",
                    serde_json::to_string(&crowded).unwrap()
                ),
            );
            set_modified(
                &home
                    .root
                    .join(format!(".pi/agent/sessions/zzz-crowd/{index}.jsonl")),
                stamp - std::time::Duration::from_secs(3600),
            );
        }
        home.write(
            ".pi/agent/sessions/aaa-fresh/one.jsonl",
            &format!("{{\"cwd\": {}}}\n", serde_json::to_string(&newest).unwrap()),
        );
        set_modified(
            &home.root.join(".pi/agent/sessions/aaa-fresh/one.jsonl"),
            stamp,
        );

        let places = lookup(&home.desk(), "tau", MAX_PLACES);
        assert_eq!(
            places.first().map(|place| place.path.as_str()),
            Some(newest.as_str()),
            "places: {places:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_directory_reached_two_ways_is_one_place() {
        let home = FixtureHome::new();
        let real = home.dir("code/phi");
        let alias = home.absent("code/phi-alias");
        std::os::unix::fs::symlink(&real, &alias).expect("fixture symlink");

        // The alias is the hotter z entry, so it is the spelling that
        // ranks first and the spelling the answer keeps.
        home.z(&[(alias.as_str(), 500.0), (real.as_str(), 1.0)]);

        let places = lookup(&home.desk(), "phi", MAX_PLACES);
        assert_eq!(paths(&places), vec![alias.as_str()]);
    }

    #[test]
    fn a_decomposed_accent_is_the_same_letter_as_a_precomposed_one() {
        // The same word, written both ways, must fold to one thing.
        assert_eq!(folded("cafe\u{301}"), folded("CAF\u{c9}"));
        assert_eq!(
            match_key(&folded("caf\u{e9}"), 1, "/code/Cafe\u{301}"),
            Some((0, 0, 0))
        );

        let home = FixtureHome::new();
        let cafe = home.dir("code/caf\u{e9}");
        home.z(&[(cafe.as_str(), 10.0)]);

        assert_eq!(
            paths(&lookup(&home.desk(), "cafe\u{301}", MAX_PLACES)),
            vec![cafe]
        );
    }

    #[test]
    fn the_folder_name_comes_from_the_platform_s_path_rules() {
        assert_eq!(folder_name("/code/zeta"), "zeta");
        assert_eq!(folder_name("/code/zeta/"), "zeta");
        // A path with no last component is all the name there is.
        assert_eq!(folder_name("/"), "/");
        #[cfg(windows)]
        assert_eq!(folder_name(r"C:\code\zeta"), "zeta");
    }

    fn chars(value: &str) -> Vec<char> {
        value.chars().collect()
    }

    fn set_modified(path: &Path, when: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open transcript");
        file.set_modified(when).expect("set transcript mtime");
    }
}
