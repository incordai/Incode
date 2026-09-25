//! What a session is, on disk.
//!
//! # Why this holds almost nothing
//!
//! Memory captures the work live — every message and every edit — through the
//! memory engine's hooks, which incode fires itself on every route (see
//! [`crate::hooks::memory`]), under incode's own session id rather than the
//! agent's. Writing a second copy here would be a file
//! that drifts from the one that actually gets replayed, and the harness has no
//! way to replay its own copy anyway: `--resume` hands the id back to the agent
//! and the AGENT restores the conversation.
//!
//! So a session record is a pointer, not a transcript: which agent conversation
//! to reopen, and which memory entries summarise it. A few hundred bytes.
//!
//! # Why it is worth writing at all
//!
//! Measured on one real turn: `cache_creation 33,476` against
//! `cache_read 407,830`. Resuming reads that prefix; starting fresh RE-CREATES
//! it at roughly ten times the price. The id in this file is what stands
//! between a continued conversation and paying 33k again.
//!
//! # Subagents
//!
//! Background work is handed to subagents, and each is its own agent
//! conversation with its own id. They are recorded as [`Child`] entries under
//! the session that spawned them, so a run that fanned out to four workers can
//! be reopened as the five conversations it actually was.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a workspace's session records live.
const DIR: &str = "sessions";
/// How many records are kept before the oldest are dropped.
///
/// Bounded so a machine used daily for a year does not accumulate an unbounded
/// directory nobody prunes.
const MAX_RECORDS: usize = 200;
/// How many other workspaces `/sessions` names when this one has no records.
///
/// A hint about where to look, not an inventory — the point is to end the
/// impression that nothing was saved, which the first few lines already do.
const MAX_ELSEWHERE: usize = 10;

/// One agent conversation, and where its summary lives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The agent's own conversation id — what `--resume` takes.
    pub agent_session: String,
    /// Which workspace it was opened on.
    pub workspace: String,
    /// The model asked for, for the operator picking between records.
    #[serde(default)]
    pub model: String,
    // A `memory_ids` list used to sit here, "written by the hooks". Nothing ever
    // wrote it, and nothing could: measured against the running store, a
    // conversation's `conv_id` IS the agent session id — so `agent_session`
    // above already points at the memory this field was going to name. It is
    // gone rather than left empty, because a field that is always empty reads as
    // a thing that failed rather than a thing that was never needed. Old files
    // still load: serde ignores what it does not recognise.
    /// Subagent conversations this session spawned.
    #[serde(default)]
    pub children: Vec<Child>,
    /// Unix seconds when the session opened.
    #[serde(default)]
    pub opened: u64,
    /// Unix seconds of the last turn.
    #[serde(default)]
    pub touched: u64,
    /// The opening instruction, trimmed — so a list of records is readable.
    #[serde(default)]
    pub title: String,
}

/// A subagent conversation spawned by a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Child {
    /// The subagent's own conversation id.
    pub agent_session: String,
    /// What it was asked to do, trimmed.
    #[serde(default)]
    pub title: String,
}

impl Record {
    /// Open a record for a workspace.
    #[must_use]
    pub fn new(agent_session: &str, workspace: &Path, model: &str) -> Self {
        let now = unix_now();
        Self {
            agent_session: agent_session.to_string(),
            workspace: workspace.display().to_string(),
            model: model.to_string(),
            children: Vec::new(),
            opened: now,
            touched: now,
            title: String::new(),
        }
    }

    /// Give this session a name the operator chose.
    ///
    /// Unlike [`Self::name_from`], which takes whatever was asked first, this
    /// replaces the name — that is the point of asking for one.
    pub fn rename(&mut self, title: &str) {
        let wanted = trim_title(title);
        if wanted.is_empty() {
            return;
        }
        self.title = wanted;
        self.touched = unix_now();
    }

    /// Note a subagent this session handed work to.
    pub fn spawned(&mut self, agent_session: &str, title: &str) {
        let id = agent_session.trim();
        if id.is_empty() || self.children.iter().any(|c| c.agent_session == id) {
            return;
        }
        self.children.push(Child {
            agent_session: id.to_string(),
            title: trim_title(title),
        });
        self.touched = unix_now();
    }

    /// Give the session a name, from the first thing asked of it.
    pub fn name_from(&mut self, instruction: &str) {
        if self.title.is_empty() {
            self.title = trim_title(instruction);
        }
    }
}

/// The first line of an instruction, short enough to list.
fn trim_title(text: &str) -> String {
    const MAX: usize = 72;
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let flat: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    let kept: String = flat.chars().take(MAX - 1).collect();
    format!("{kept}…")
}

/// Where records are kept.
#[must_use]
pub fn dir_in(root: &Path) -> PathBuf {
    root.join(DIR)
}

/// The file holding one workspace's records.
///
/// # Why a file per workspace, and not one for all of them
///
/// It used to be a single `sessions.json`, and that had two defects that only
/// showed up as data quietly going missing.
///
/// **[`MAX_RECORDS`] was global.** Every save loaded every workspace's records,
/// sorted them by recency and truncated the list — so working hard in one
/// project evicted the pointers of the ones left alone. The pointer is the whole
/// value of this file: it is what `--resume` takes, and losing it means
/// re-creating a cached prefix at roughly ten times the price.
///
/// **One bad byte lost everything.** A parse failure yields an empty list, and
/// the next save wrote over the top of it — so a half-finished write during a
/// crash took every project's history with it, silently. Per file, the blast
/// radius is the one project being written.
#[must_use]
pub fn file_for(root: &Path, workspace: &str) -> PathBuf {
    dir_in(root).join(format!("{}.json", slug(workspace)))
}

/// The longest a slug may be before it is cut.
///
/// A deep path can exceed what a filesystem accepts as one component, and a
/// write that fails for that reason would look exactly like a session that was
/// never recorded.
const MAX_SLUG: usize = 100;

/// A filename for a workspace path.
///
/// **Not required to be unique, and nothing here depends on it being unique.**
/// Two paths can slug alike — `C:\a-b` and `C:\a\b` both become `C--a-b`, and
/// cutting at [`MAX_SLUG`] can make two deep paths agree as well. That is why
/// every read filters on the `workspace` field as well as opening the file: the
/// filename decides which file to look in, the field decides which records are
/// yours. A collision then costs a shared file, never a wrong answer.
fn slug(workspace: &str) -> String {
    let mut out = String::with_capacity(workspace.len().min(MAX_SLUG));
    for ch in workspace.chars().take(MAX_SLUG) {
        out.push(if ch.is_ascii_alphanumeric() { ch } else { '-' });
    }
    if out.is_empty() {
        // A record with no workspace still has to land somewhere nameable.
        return "unknown".to_string();
    }
    out
}

/// Read one file of records, newest first.
fn load_file(path: &Path) -> Vec<Record> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut held: Vec<Record> = serde_json::from_str(&text).unwrap_or_default();
    held.sort_by_key(|r| std::cmp::Reverse(r.touched));
    held
}

/// Write a record, replacing any earlier one for the same agent session.
///
/// Fails soft: a session that cannot be written is not a session that should
/// stop working. The cost of losing it is re-paying a prefix, not losing work —
/// memory still holds what happened.
pub fn save(root: &Path, record: &Record) {
    let dir = dir_in(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    migrate(root);
    let path = file_for(root, &record.workspace);
    let mut held = load_file(&path);
    held.retain(|r| r.agent_session != record.agent_session);
    held.push(record.clone());
    held.sort_by_key(|r| std::cmp::Reverse(r.touched));
    // Per workspace now, so a busy project cannot evict a quiet one.
    held.truncate(MAX_RECORDS);
    if let Ok(text) = serde_json::to_string_pretty(&held) {
        let _ = std::fs::write(path, text);
    }
}

/// Every record from every workspace, newest first.
#[must_use]
pub fn load_all(root: &Path) -> Vec<Record> {
    migrate(root);
    let Ok(entries) = std::fs::read_dir(dir_in(root)) else {
        return Vec::new();
    };
    let mut held: Vec<Record> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            held.extend(load_file(&path));
        }
    }
    held.sort_by_key(|r| std::cmp::Reverse(r.touched));
    held
}

/// The file written before this split existed.
const LEGACY_FILE: &str = "sessions.json";
/// Written once the legacy file has been split, so it is never split twice.
const MIGRATED_MARKER: &str = ".migrated";

/// Split a pre-split `sessions.json` into one file per workspace.
///
/// Runs at most once: the marker is what decides, not the absence of the legacy
/// file, so a workspace whose slug happens to be `sessions` cannot be mistaken
/// for the legacy file and re-split into itself.
///
/// The original is KEPT, renamed rather than deleted — it is the only copy of
/// records that existed before this ran.
fn migrate(root: &Path) {
    let dir = dir_in(root);
    let marker = dir.join(MIGRATED_MARKER);
    if marker.exists() {
        return;
    }
    let legacy = dir.join(LEGACY_FILE);
    let held = load_file(&legacy);
    if legacy.exists() {
        for record in &held {
            let path = file_for(root, &record.workspace);
            let mut mine = load_file(&path);
            mine.retain(|r| r.agent_session != record.agent_session);
            mine.push(record.clone());
            mine.sort_by_key(|r| std::cmp::Reverse(r.touched));
            mine.truncate(MAX_RECORDS);
            if let Ok(text) = serde_json::to_string_pretty(&mine) {
                let _ = std::fs::write(&path, text);
            }
        }
        // Kept, not deleted: before this ran it was the only copy.
        let _ = std::fs::rename(&legacy, dir.join("sessions.json.pre-split"));
    }
    // Written even when there was nothing to migrate, so a fresh install does
    // not re-scan for a legacy file on every single save.
    let _ = std::fs::write(marker, "one file per workspace\n");
}

/// The records, as the lines `/sessions` prints.
///
/// Returned rather than printed so what it claims can be asserted without a
/// terminal — the count, the ordering, and the fact that every line carries the
/// id, which is the only part of a record that DOES anything.
#[must_use]
pub fn summary_lines(records: &[Record], now: u64) -> Vec<String> {
    if records.is_empty() {
        return vec!["No sessions recorded for this workspace yet.".to_string()];
    }
    let mut out = Vec::with_capacity(records.len() + 1);
    for record in records {
        let title = if record.title.is_empty() {
            "(unnamed)"
        } else {
            &record.title
        };
        // The id goes on every line because it is what `--resume` takes; a list
        // that reads well and cannot be acted on is a list nobody uses twice.
        let mut line = format!(
            "  {}  {}  {title}",
            ago(now, record.touched),
            record.agent_session
        );
        if !record.children.is_empty() {
            line.push_str(&format!(" (+{} subagent)", record.children.len()));
        }
        out.push(line);
    }
    out.push(format!(
        "Reopen one with `--resume <id>`. {} recorded.",
        records.len()
    ));
    out
}

/// How long ago, in words short enough for a list.
fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        s if s < 90 => "just now".to_string(),
        s if s < 5400 => format!("{}m ago", s / 60),
        s if s < 172_800 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

fn normalize_workspace(path: &str) -> String {
    let s = path.replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// Whether two workspace path strings refer to the same directory,
/// taking into account forward/backward slashes and Windows case insensitivity.
#[must_use]
pub fn same_workspace(a: &str, b: &str) -> bool {
    normalize_workspace(a) == normalize_workspace(b)
}

/// The records for one workspace, newest first.
#[must_use]
pub fn for_workspace(root: &Path, workspace: &Path) -> Vec<Record> {
    let want = workspace.display().to_string();
    migrate(root);
    // Open THIS workspace's file, then still filter on the field. The filename
    // narrows the search; the field is what guarantees the answer, because two
    // paths may share a slug. See [`slug`].
    load_file(&file_for(root, &want))
        .into_iter()
        .filter(|r| same_workspace(&r.workspace, &want))
        .collect()
}

/// The OTHER workspaces that have records, newest first, with how many each has.
///
/// # Why `/sessions` needs this
///
/// Records are kept in one flat file for every workspace and read back through
/// [`for_workspace`], so a session opened in another checkout is invisible here
/// and the list says "No sessions recorded for this workspace yet." That
/// sentence is true and reads as *nothing was saved* — which is the one thing it
/// must not imply, because the id in these records is what `--resume` takes and
/// an operator who believes it was lost starts a fresh conversation and re-pays
/// the prefix.
///
/// So the empty case can say where the records actually are.
#[must_use]
pub fn elsewhere(root: &Path, workspace: &Path) -> Vec<(String, usize)> {
    let here = workspace.display().to_string();
    let mut counts: Vec<(String, usize)> = Vec::new();
    // `load_all` is already newest-first, so first sighting of a workspace is its
    // most recent record and the order below is "most recently worked in first".
    for record in load_all(root) {
        if same_workspace(&record.workspace, &here) {
            continue;
        }
        match counts
            .iter_mut()
            .find(|(name, _)| same_workspace(name, &record.workspace))
        {
            Some((_, n)) => *n += 1,
            None => counts.push((record.workspace, 1)),
        }
    }
    counts
}

/// What `/sessions` adds when this workspace has no records of its own.
///
/// Empty when there is genuinely nothing anywhere — then
/// [`summary_lines`]'s "No sessions recorded for this workspace yet." is the
/// whole truth and needs nothing after it.
///
/// Returned rather than printed, like [`summary_lines`], so the sentence a
/// person actually reads is the thing under test.
#[must_use]
pub fn elsewhere_lines(others: &[(String, usize)]) -> Vec<String> {
    if others.is_empty() {
        return Vec::new();
    }
    let mut out =
        vec!["Recorded for other workspaces (open one there, or `/resume <id>`):".to_string()];
    // Bounded: this is a hint about where to look, not an inventory.
    for (place, count) in others.iter().take(MAX_ELSEWHERE) {
        out.push(format!("  {place} — {count} session(s)"));
    }
    if others.len() > MAX_ELSEWHERE {
        out.push(format!("  …and {} more", others.len() - MAX_ELSEWHERE));
    }
    out
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> Record {
        Record::new(id, Path::new("C:/work/proj"), "opus")
    }

    #[test]
    fn a_record_holds_the_id_that_resumes_the_conversation() {
        // The whole point: resuming reads a cached prefix, starting fresh
        // re-creates 33k tokens of it.
        let r = record("sess-1");
        assert_eq!(r.agent_session, "sess-1");
        assert_eq!(r.workspace, "C:/work/proj");
    }

    #[test]
    fn a_record_holds_no_transcript() {
        // Memory owns the messages. A second copy here would drift from the one
        // that actually gets replayed, and could never replay anything itself.
        let r = record("sess-1");
        let json = serde_json::to_string(&r).expect("serialises");
        assert!(
            json.len() < 400,
            "a record must stay small: {} bytes",
            json.len()
        );
        assert!(!json.contains("message"), "{json}");
        assert!(!json.contains("transcript"), "{json}");
    }

    #[test]
    fn every_listed_session_carries_the_id_that_reopens_it() {
        // `/sessions` used to say this harness kept nothing to list. It keeps
        // these. A line that reads well and cannot be acted on is a line nobody
        // uses twice, so the id is on every one.
        let mut newest = record("sess-new");
        newest.touched = 1_000;
        newest.rename("the recent one");
        let mut older = record("sess-old");
        older.touched = 100;
        older.spawned("sub-a", "look something up");

        let lines = summary_lines(&[newest, older], 1_000);
        assert!(lines[0].contains("sess-new"), "{lines:?}");
        assert!(lines[0].contains("the recent one"), "{lines:?}");
        assert!(lines[1].contains("sess-old"), "{lines:?}");
        assert!(
            lines[1].contains("+1 subagent"),
            "a fanned-out run says so: {lines:?}"
        );
        assert!(
            lines.last().is_some_and(|l| l.contains("--resume")),
            "and how to act on them: {lines:?}"
        );
    }

    #[test]
    fn an_unnamed_session_is_still_listed() {
        // A record written before its first instruction has no title, and
        // dropping it from the list would hide a real conversation.
        let lines = summary_lines(&[record("sess-1")], 0);
        assert!(lines[0].contains("sess-1"), "{lines:?}");
        assert!(lines[0].contains("(unnamed)"), "{lines:?}");
    }

    #[test]
    fn nothing_recorded_says_so_rather_than_printing_a_bare_heading() {
        let lines = summary_lines(&[], 0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("No sessions recorded"), "{lines:?}");
    }

    #[test]
    fn a_session_can_be_given_a_name() {
        // `/save nightly-fix` parsed the name and threw it away, so a record
        // stayed titled by whatever was typed first. Renaming REPLACES, unlike
        // `name_from` — being asked for a name is the whole point.
        let mut r = record("sess-1");
        r.name_from("whatever was asked first");
        r.rename("nightly-fix");
        assert_eq!(r.title, "nightly-fix");
    }

    #[test]
    fn an_empty_name_does_not_erase_the_one_there() {
        // `/save` with nothing after it must not blank the record.
        let mut r = record("sess-1");
        r.name_from("the original");
        r.rename("   ");
        assert_eq!(r.title, "the original");
    }

    #[test]
    fn subagents_are_recorded_under_the_session_that_spawned_them() {
        // Background work fans out to subagents, each its own conversation. A
        // run that used four workers must reopen as the five it really was.
        let mut r = record("sess-1");
        r.spawned("sub-a", "review the guards");
        r.spawned("sub-b", "write the tests");
        r.spawned("sub-a", "review the guards");
        assert_eq!(r.children.len(), 2, "the same subagent is recorded once");
        assert_eq!(r.children[0].agent_session, "sub-a");
        assert_eq!(r.children[0].title, "review the guards");
    }

    #[test]
    fn a_session_is_named_by_the_first_thing_asked_of_it() {
        let mut r = record("sess-1");
        r.name_from("fix the footer\nand then the tests");
        r.name_from("something else entirely");
        assert_eq!(r.title, "fix the footer", "the FIRST instruction names it");
    }

    #[test]
    fn a_long_title_is_trimmed_without_splitting_a_character() {
        let mut r = record("sess-1");
        r.name_from(&"an em dash — ".repeat(20));
        assert!(r.title.chars().count() <= 72, "{}", r.title);
        assert!(r.title.ends_with('…'));
    }

    #[test]
    fn saving_replaces_the_record_for_the_same_conversation() {
        // A session is written on every turn; without replacement the file
        // would hold one entry per turn instead of one per conversation.
        let dir = tempfile::tempdir().expect("tmp");
        let mut r = record("sess-1");
        save(dir.path(), &r);
        r.spawned("sub-1", "look something up");
        save(dir.path(), &r);
        let held = load_all(dir.path());
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(
            held[0].children.len(),
            1,
            "the second save replaced the first rather than adding to it"
        );
    }

    #[test]
    fn records_come_back_newest_first() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut old = record("old");
        old.touched = 100;
        let mut new = record("new");
        new.touched = 200;
        save(dir.path(), &old);
        save(dir.path(), &new);
        let held = load_all(dir.path());
        assert_eq!(held[0].agent_session, "new");
    }

    #[test]
    fn the_store_is_bounded() {
        // A machine used daily for a year must not grow a directory nobody
        // prunes.
        let dir = tempfile::tempdir().expect("tmp");
        for n in 0..(MAX_RECORDS + 40) {
            let mut r = record(&format!("sess-{n}"));
            r.touched = n as u64;
            save(dir.path(), &r);
        }
        let held = load_all(dir.path());
        assert_eq!(held.len(), MAX_RECORDS);
        assert_eq!(
            held[0].agent_session,
            format!("sess-{}", MAX_RECORDS + 39),
            "the NEWEST survive"
        );
    }

    #[test]
    fn only_this_workspace_is_listed() {
        let dir = tempfile::tempdir().expect("tmp");
        save(dir.path(), &record("here"));
        let mut elsewhere = Record::new("there", Path::new("D:/other"), "opus");
        elsewhere.touched = 1;
        save(dir.path(), &elsewhere);
        let mine = for_workspace(dir.path(), Path::new("C:/work/proj"));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].agent_session, "here");
    }

    /// **An empty list here must not read as "nothing was saved".** The records
    /// are one flat file for every workspace, so a session opened in another
    /// checkout is filtered out — and the id in it is what `--resume` takes.
    #[test]
    fn the_other_workspaces_holding_records_can_be_named() {
        let dir = tempfile::tempdir().expect("tmp");
        save(dir.path(), &record("here"));
        for (n, id) in ["there-1", "there-2"].iter().enumerate() {
            let mut other = Record::new(id, Path::new("D:/other"), "opus");
            other.touched = 10 + n as u64;
            save(dir.path(), &other);
        }
        let mut third = Record::new("far", Path::new("E:/far"), "opus");
        third.touched = 5;
        save(dir.path(), &third);

        let others = elsewhere(dir.path(), Path::new("C:/work/proj"));
        assert_eq!(
            others,
            vec![("D:/other".to_string(), 2), ("E:/far".to_string(), 1)],
            "each other workspace once, with its count, most recent first"
        );
    }

    /// The workspace in hand is never listed as somewhere else.
    #[test]
    fn the_current_workspace_is_not_reported_as_elsewhere() {
        let dir = tempfile::tempdir().expect("tmp");
        save(dir.path(), &record("here"));
        assert!(elsewhere(dir.path(), Path::new("C:/work/proj")).is_empty());
    }

    /// **The sentence a person reads is the thing under test.** The bug was
    /// never in the filtering — it was in what the screen said about it.
    #[test]
    fn the_empty_case_says_where_the_records_actually_are() {
        let lines = elsewhere_lines(&[("D:/other".to_string(), 2), ("E:/far".to_string(), 1)]);
        assert!(
            lines[0].contains("other workspaces"),
            "it must point elsewhere, not restate the emptiness; got: {lines:?}"
        );
        assert!(lines[1].contains("D:/other") && lines[1].contains('2'));
        assert!(lines[2].contains("E:/far"));
        assert!(
            lines.iter().any(|l| l.contains("/resume")),
            "the way back in must be on screen; got: {lines:?}"
        );
    }

    /// Nothing anywhere means nothing to add — the existing line is the whole
    /// truth, and a dangling header under it would be worse than silence.
    #[test]
    fn nothing_anywhere_adds_no_lines() {
        assert!(elsewhere_lines(&[]).is_empty());
    }

    /// A hint, not an inventory.
    #[test]
    fn the_list_of_other_workspaces_is_bounded() {
        let many: Vec<(String, usize)> = (0..MAX_ELSEWHERE + 5)
            .map(|n| (format!("D:/w{n}"), 1))
            .collect();
        let lines = elsewhere_lines(&many);
        // header + MAX_ELSEWHERE rows + the "and N more" line
        assert_eq!(lines.len(), MAX_ELSEWHERE + 2);
        assert!(
            lines.last().expect("a last line").contains("5 more"),
            "what was cut must be counted, not dropped in silence; got: {:?}",
            lines.last()
        );
    }

    /// **A busy project must not evict a quiet one.** The cap used to be global:
    /// every save loaded every workspace's records, sorted by recency and cut the
    /// list at [`MAX_RECORDS`] — so filling one project past the cap threw away
    /// another project's pointers, and the pointer is the only thing `--resume`
    /// can use.
    #[test]
    fn one_workspace_filling_up_never_evicts_another() {
        let dir = tempfile::tempdir().expect("tmp");
        let quiet = Record::new("quiet-one", Path::new("D:/quiet"), "opus");
        save(dir.path(), &quiet);

        // Well past the cap, all in a DIFFERENT workspace.
        for n in 0..(MAX_RECORDS + 25) {
            let mut busy = Record::new(&format!("busy-{n}"), Path::new("C:/busy"), "opus");
            busy.touched = 1_000 + n as u64;
            save(dir.path(), &busy);
        }

        let survivors = for_workspace(dir.path(), Path::new("D:/quiet"));
        assert_eq!(
            survivors.len(),
            1,
            "the quiet project's only session was evicted by an unrelated one"
        );
        assert_eq!(survivors[0].agent_session, "quiet-one");
        // And the busy one is still bounded — per workspace, not globally.
        assert_eq!(
            for_workspace(dir.path(), Path::new("C:/busy")).len(),
            MAX_RECORDS
        );
    }

    /// **One corrupt file must not take every project with it.** A parse failure
    /// yields an empty list and the next save writes over the top, so when this
    /// was one file a single bad byte lost the lot, silently.
    #[test]
    fn a_corrupt_file_costs_only_its_own_workspace() {
        let dir = tempfile::tempdir().expect("tmp");
        save(
            dir.path(),
            &Record::new("mine", Path::new("C:/mine"), "opus"),
        );
        save(
            dir.path(),
            &Record::new("theirs", Path::new("D:/theirs"), "opus"),
        );

        // Corrupt exactly one project's file.
        std::fs::write(file_for(dir.path(), "C:/mine"), "{ not json").expect("write");

        assert!(
            for_workspace(dir.path(), Path::new("C:/mine")).is_empty(),
            "the damaged project reads as empty, which is honest"
        );
        let theirs = for_workspace(dir.path(), Path::new("D:/theirs"));
        assert_eq!(
            theirs.len(),
            1,
            "the OTHER project must be untouched by its neighbour's damage"
        );
        assert_eq!(theirs[0].agent_session, "theirs");
    }

    /// **A shared slug must never give a wrong answer.** `C:\a-b` and `C:\a\b`
    /// both slug to `C--a-b`, so they land in one file — and the records must
    /// still come back to the right workspace.
    #[test]
    fn two_workspaces_sharing_a_slug_keep_their_own_records() {
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(
            slug(r"C:\a-b"),
            slug(r"C:\a\b"),
            "this test is pointless unless these really do collide"
        );
        save(
            dir.path(),
            &Record::new("dash", Path::new(r"C:\a-b"), "opus"),
        );
        save(
            dir.path(),
            &Record::new("nested", Path::new(r"C:\a\b"), "opus"),
        );

        let dash = for_workspace(dir.path(), Path::new(r"C:\a-b"));
        let nested = for_workspace(dir.path(), Path::new(r"C:\a\b"));
        assert_eq!(dash.len(), 1, "got {dash:?}");
        assert_eq!(dash[0].agent_session, "dash");
        assert_eq!(nested.len(), 1, "got {nested:?}");
        assert_eq!(nested[0].agent_session, "nested");
    }

    /// A file written before the split is read, not lost — and the original is
    /// kept, because until this runs it is the only copy.
    #[test]
    fn records_written_before_the_split_are_migrated_and_the_original_kept() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir_in(dir.path())).expect("dir");
        let legacy = vec![
            Record::new("old-a", Path::new("C:/one"), "opus"),
            Record::new("old-b", Path::new("D:/two"), "opus"),
        ];
        std::fs::write(
            dir_in(dir.path()).join(LEGACY_FILE),
            serde_json::to_string(&legacy).expect("encode"),
        )
        .expect("write");

        assert_eq!(
            for_workspace(dir.path(), Path::new("C:/one"))[0].agent_session,
            "old-a"
        );
        assert_eq!(
            for_workspace(dir.path(), Path::new("D:/two"))[0].agent_session,
            "old-b"
        );
        assert!(
            dir_in(dir.path()).join("sessions.json.pre-split").exists(),
            "the pre-split file must be kept, not deleted"
        );
    }

    /// Migration runs once. A second pass must not resurrect a record that was
    /// deliberately removed after the split.
    #[test]
    fn migration_does_not_run_twice() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir_in(dir.path())).expect("dir");
        std::fs::write(
            dir_in(dir.path()).join(LEGACY_FILE),
            serde_json::to_string(&vec![Record::new("old", Path::new("C:/one"), "opus")])
                .expect("encode"),
        )
        .expect("write");

        assert_eq!(for_workspace(dir.path(), Path::new("C:/one")).len(), 1);
        // Remove it the way a person would, then look again.
        std::fs::remove_file(file_for(dir.path(), "C:/one")).expect("remove");
        assert!(
            for_workspace(dir.path(), Path::new("C:/one")).is_empty(),
            "a second migration re-created a record that was deleted"
        );
    }

    /// The filename is derived from the workspace, so the folder is browsable.
    #[test]
    fn each_workspace_gets_its_own_named_file() {
        let dir = tempfile::tempdir().expect("tmp");
        save(
            dir.path(),
            &Record::new("s", Path::new(r"C:\Projects\incord-code"), "opus"),
        );
        let path = file_for(dir.path(), r"C:\Projects\incord-code");
        assert!(path.exists(), "expected {}", path.display());
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("C--Projects-incord-code.json")
        );
    }

    #[test]
    fn an_unreadable_store_is_not_a_crash() {
        // Losing a record costs a re-paid prefix, not the work: memory still
        // holds what happened. It must never stop the session.
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir_in(dir.path())).expect("dir");
        std::fs::write(dir_in(dir.path()).join("sessions.json"), "{ not json").expect("write");
        assert!(load_all(dir.path()).is_empty());
        save(dir.path(), &record("sess-1"));
        assert_eq!(load_all(dir.path()).len(), 1, "it recovers by overwriting");
    }

    #[test]
    fn workspace_path_slash_differences_do_not_prevent_session_lookup() {
        let dir = tempfile::tempdir().expect("tmp");
        // Saved with backslashes
        save(
            dir.path(),
            &Record::new("sess-1", Path::new(r"C:\Projects\incord-code"), "opus"),
        );

        // Queried with forward slashes
        let found = for_workspace(dir.path(), Path::new("C:/Projects/incord-code"));
        assert_eq!(found.len(), 1, "slash variation should still find session");
        assert_eq!(found[0].agent_session, "sess-1");

        // Elsewhere should also consider them the same workspace
        let others = elsewhere(dir.path(), Path::new("C:/Projects/incord-code"));
        assert!(
            others.is_empty(),
            "same workspace should not appear in elsewhere"
        );
    }
}
