//! The project brief, and the intake that completes it before code is written.
//!
//! The proxy decides *as the owner*. It cannot decide well about a project it
//! knows nothing about: "should this dependency be added?" is answerable from a
//! roadmap and unanswerable without one. So the brief is not documentation — it
//! is the context every later verdict is made from, and a project without one is
//! a project where nobody can review what the agent does.
//!
//! So an incomplete brief makes *completing it* the session's first job. It is
//! not a refusal and nothing is blocked: the harness asks, researches, proposes,
//! and saves what is agreed, then carries on into the code. A harness that
//! exited here would teach the operator to work somewhere else, and the brief
//! would never be written at all.
//!
//! **Nothing here stores a brief.** The brief belongs to the memory service,
//! which already holds it, serves it, and has the research path that drafts it.
//! This module reads it, says what is missing, and writes back what the owner
//! agrees — if it ever grows a local copy, a cache, or a "default brief", that
//! is the second-memory defect returning.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where the body listens. Overridable because a port is deployment, not design.
pub const MEMORY_URL_ENV: &str = "INCORD_MEMORY_URL";

/// The default the service is normally reachable on.
pub const DEFAULT_MEMORY_URL: &str = "http://127.0.0.1:9180";

/// The path that serves the proxy's configuration, briefs included.
pub const BRIEF_PATH: &str = "/v1/owner/proxy";

/// Bounded because this runs before the first prompt, with the user waiting.
/// A slow answer here is indistinguishable from a hung start.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// One item of the ordered plan.
///
/// Mirrors the service's shape. Only the fields the gate reads are declared;
/// anything else on the wire is ignored rather than fought with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct RoadmapItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// `todo` | `doing` | `done` — anything else reads as `todo`.
    #[serde(default)]
    pub status: String,
}

/// The allowed status for a roadmap item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoadmapStatus {
    Todo,
    Doing,
    Done,
}

impl RoadmapStatus {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "todo" => Ok(Self::Todo),
            "doing" => Ok(Self::Doing),
            "done" => Ok(Self::Done),
            other => Err(format!(
                "invalid roadmap status `{other}`: must be todo, doing, or done"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Doing => "doing",
            Self::Done => "done",
        }
    }
}

impl RoadmapItem {
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.status.eq_ignore_ascii_case("done")
    }
}

/// What the proxy knows about one project.
///
/// `Serialize` so a complete brief can be cached on disk and survive the
/// memory service being down — see [`check`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct ProjectBrief {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub about: String,
    #[serde(default)]
    pub goals: String,
    #[serde(default)]
    pub synopsis: String,
    #[serde(default)]
    pub prd: String,
    #[serde(default)]
    pub roadmap: Vec<RoadmapItem>,
    /// The project this brief belongs to, as the store's own stable id
    /// (`proj-…`), or empty when the service has not assigned one.
    ///
    /// # What it is NOT
    ///
    /// **Not a unique key for a brief.** Several folders can belong to one
    /// project and share this id — measured live: `proj-memory` covers both
    /// `C:/Projects/Chat/incord-dashboard` and `Memory`. So a lookup that
    /// matched on this would return the wrong brief for a folder that shares a
    /// scope, which is why [`brief_for`] still matches on the workspace PATH.
    ///
    /// It is carried for the things that ARE keyed per project — see
    /// [`crate::recorder`], whose store addresses a project by this id.
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ProxyConfig {
    #[serde(default)]
    briefs: Vec<ProjectBrief>,
}

/// One of the artifacts the gate requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    About,
    Goals,
    Synopsis,
    Prd,
    Roadmap,
}

impl Artifact {
    /// Every artifact a brief needs, in the order a person would write them.
    pub const ALL: [Self; 5] = [
        Self::About,
        Self::Goals,
        Self::Synopsis,
        Self::Prd,
        Self::Roadmap,
    ];

    #[must_use]
    pub fn field(self) -> &'static str {
        match self {
            Self::About => "about",
            Self::Goals => "goals",
            Self::Synopsis => "synopsis",
            Self::Prd => "prd",
            Self::Roadmap => "roadmap",
        }
    }

    /// Why the proxy needs it — printed with the refusal, because a gate that
    /// only says "missing" teaches nobody what to write.
    #[must_use]
    pub fn why(self) -> &'static str {
        match self {
            Self::About => "what this project is",
            Self::Goals => "what the proxy should protect",
            Self::Synopsis => "the PRD in a few lines — this rides every review prompt",
            Self::Prd => "the product document, searched when a decision touches it",
            Self::Roadmap => "the ordered plan, derived from the PRD",
        }
    }
}

impl ProjectBrief {
    /// The artifacts this brief is missing, in the order a person would write them.
    #[must_use]
    pub fn missing(&self) -> Vec<Artifact> {
        let mut missing = Vec::new();
        if self.about.trim().is_empty() {
            missing.push(Artifact::About);
        }
        if self.goals.trim().is_empty() {
            missing.push(Artifact::Goals);
        }
        if self.synopsis.trim().is_empty() {
            missing.push(Artifact::Synopsis);
        }
        if self.prd.trim().is_empty() {
            missing.push(Artifact::Prd);
        }
        if self.roadmap.is_empty() {
            missing.push(Artifact::Roadmap);
        }
        missing
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing().is_empty()
    }

    /// Whether the PRD says what is deliberately NOT being built.
    ///
    /// The most valuable section and the one always omitted. Scope drift is the
    /// top failure mode of an autonomous loop, and an explicit "not building
    /// auth in v1" is the only thing that stops an agent helpfully adding it —
    /// because adding it is genuinely useful, genuinely in the spirit of the
    /// request, and genuinely not what anyone asked for.
    ///
    /// A heading, not a field: the service's brief has no `non_goals` of its own,
    /// and a section inside the document needs nothing from anyone and is already
    /// searched when a decision touches it.
    ///
    /// Checked by [`crate::spec::has_section`], the one check every "this
    /// document must say X" rule shares — the architecture's `## Layout` too.
    #[must_use]
    pub fn states_non_goals(&self) -> bool {
        crate::spec::has_section(&self.prd, |h| {
            h.contains("non-goal") || h.contains("non goal") || h.contains("not building")
        })
    }

    /// Open roadmap items that name no section of the PRD.
    ///
    /// The roadmap is supposed to be *derived* from the document: every item
    /// implements something the PRD argued for. An item with nothing behind it
    /// is scope that arrived without a decision, and it is the cheapest kind of
    /// scope to acquire — someone thinks of it, it gets written down, and by the
    /// time anyone asks why, it looks like it was always the plan.
    ///
    /// The rule is deliberately literal rather than clever: an item is traceable
    /// when its title names a heading that exists in the PRD, or names a section
    /// the PRD numbers (`§7`, `Phase 3`, `R12`). Fuzzy matching would call
    /// everything traceable and the check would mean nothing.
    #[must_use]
    pub fn untraceable_items(&self) -> Vec<String> {
        if self.prd.trim().is_empty() {
            // No document to trace to. That is the PRD artifact's problem, and
            // reporting every item here as well would bury it.
            return Vec::new();
        }
        let headings = prd_headings(&self.prd);
        self.roadmap
            .iter()
            .filter(|item| item.is_open())
            // The scaffold traces to the architecture's `## Layout`, not to a
            // PRD section: it builds the skeleton every section needs.
            .filter(|item| !crate::scaffold::is_scaffold(item))
            .filter(|item| !names_a_section(&item.title, &headings, &self.prd))
            .map(|item| {
                let id = if item.id.trim().is_empty() {
                    "(no id)"
                } else {
                    item.id.trim()
                };
                format!("{id}: {}", item.title.trim())
            })
            .collect()
    }
}

/// The most questions the intake may put to the owner before it starts deciding
/// for itself.
///
/// Seven. Past that, a person stops answering and starts agreeing, and the
/// answers get worse exactly as the questions get more detailed. Everything
/// beyond the cap is decided by the harness and written down as an assumption,
/// which the owner skims in one pass instead of answering one at a time. A
/// correction at that stage is free, because nothing has been built.
pub const MAX_INTAKE_QUESTIONS: usize = 7;

/// How the intake is to be conducted, appended to every gathering task.
///
/// Kept beside the gate rather than in the system prompt because it is only true
/// while the brief is incomplete — a session that already has one should not be
/// carrying instructions about how to ask for it.
#[must_use]
pub fn intake_method() -> String {
    format!(
        "How to gather this:\n\
         - Ask AT MOST {MAX_INTAKE_QUESTIONS} questions, in ONE batch, and only \
         about things you genuinely cannot decide: what this is for, who uses it, \
         what must not break.\n\
         - Every question carries YOUR RECOMMENDED ANSWER, as options with the one \
         you would choose marked `{rec}` (or `recommended: true` in \
         AskUserQuestion). The owner can agree with one keystroke, and correct \
         only what is wrong. If nobody answers — a run with no one at the prompt \
         — your recommendations stand: record each as an assumption and carry on.\n\
         - Decide everything else yourself, and record each decision with its \
         reasoning under an `## Assumptions` heading in the PRD — \"Postgres, \
         because the document implies relational data and no scale requirement \
         was given\". The owner skims one list instead of answering twenty \
         questions, and correcting an assumption is free while nothing is built.\n\
         - Research before proposing. Name exact current versions, confirm each \
         choice is free or self-hostable, and state what you considered instead. \
         Never state a version, a licence or a running cost you have not verified.\n\
         - Include a `## Non-goals` section. It is the section everyone omits and \
         the only thing that stops an agent adding work nobody asked for.",
        rec = crate::ask::RECOMMENDED,
    )
}

/// The document the belief is derived from.
pub const PRD_PATH: &str = "docs/PRD.md";

/// Headings whose sections are the brief's own fields rather than roadmap work.
///
/// A section called "About" describes the project; it is not a thing to build.
/// Matched on the heading's first word so `## 1. About` and `## About the tool`
/// both land, which is how people actually write them.
const FIELD_HEADINGS: [&str; 3] = ["about", "goals", "synopsis"];

/// Headings that describe what is deliberately NOT being built.
///
/// Excluded from the roadmap for the obvious reason: a non-goal turned into a
/// task is the harness building the one thing the document says not to.
const EXCLUDED_HEADINGS: [&str; 5] = ["non-goal", "non goal", "assumption", "risk", "scope"];

/// Build the proxy's belief from the project's own documents.
///
/// # Why this exists
///
/// The brief IS the proxy's belief about a project, and until now the only way
/// to create one was for somebody to write it into the proxy by hand. That is
/// backwards: the project already carries a document that says what it is and
/// what it is for, the spec gate already refuses to let code be written until
/// that document exists, and every heading in it is already a claim about scope.
/// Deriving the belief from the document means the two cannot disagree.
///
/// # What is derived, and from where
///
/// - **prd** — the whole document, verbatim. It is the thing decisions are
///   searched against, so it is carried as written rather than summarised.
/// - **about / goals / synopsis** — the body of the section named by each, when
///   the document has one.
/// - **roadmap** — one item per remaining section, in document order. That is
///   the definition [`ProjectBrief::untraceable_items`] already enforces from
///   the other side: an item is traceable when it names a section, so an item
///   BUILT from a section is traceable by construction.
///
/// Nothing is invented. A field the document does not describe comes back
/// absent, so [`Gate`] still asks for it rather than being handed a guess —
/// which is the whole point of the gate, and a plausible-looking `about` that
/// nobody wrote is worse than none.
#[must_use]
pub fn from_docs(workspace: &Path) -> BriefUpdate {
    let Ok(prd) = std::fs::read_to_string(workspace.join(PRD_PATH)) else {
        return BriefUpdate::default();
    };
    if prd.trim().is_empty() {
        return BriefUpdate::default();
    }

    let sections = sections_of(&prd);
    let field = |name: &str| -> Option<String> {
        sections
            .iter()
            .find(|(heading, _)| first_word(heading) == name)
            .map(|(_, body)| body.clone())
            .filter(|body| !body.trim().is_empty())
    };

    let mut roadmap: Vec<RoadmapItem> = sections
        .iter()
        .filter(|(heading, _)| {
            let first = first_word(heading);
            !FIELD_HEADINGS.contains(&first.as_str())
                && !EXCLUDED_HEADINGS.iter().any(|skip| heading.contains(skip))
        })
        .enumerate()
        .map(|(index, (heading, _))| RoadmapItem {
            id: format!("d{}", index + 1),
            // The heading VERBATIM, because traceability is decided by whether
            // an item names a section — a reworded title is an item that no
            // longer traces to the document it came from.
            title: title_of(heading),
            status: "todo".to_string(),
        })
        .collect();
    // A project with no code yet builds its layout before any section of the
    // PRD — see `crate::scaffold`.
    if crate::scaffold::applies(workspace) {
        crate::scaffold::put_first(&mut roadmap);
    }

    BriefUpdate {
        about: field("about"),
        goals: field("goals"),
        synopsis: field("synopsis"),
        prd: Some(prd.clone()),
        roadmap: (!roadmap.is_empty()).then_some(roadmap),
    }
}

/// Each heading with the text beneath it, in document order.
fn sections_of(prd: &str) -> Vec<(String, String)> {
    // DEPTH TELLS A TITLE FROM A SECTION. The first attempt dropped the first
    // heading whenever there was more than one, and its own test caught the
    // hole: a document with ONLY a title kept it and produced a roadmap of one
    // item named after the project.
    //
    // The shallowest heading level is the title level; anything deeper is a
    // section. That reads both conventions — `#` title with `##` sections, and a
    // document written entirely in `#` where the first is still its name —
    // without counting anything.
    let depth = |line: &str| line.len() - line.trim_start_matches('#').len();
    let mut in_fence = false;
    let mut title_depth = None;
    for line in prd.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && trimmed.starts_with('#') {
            let d = depth(trimmed);
            title_depth = Some(title_depth.map_or(d, |min: usize| min.min(d)));
        }
    }
    let Some(title_depth) = title_depth else {
        return Vec::new();
    };

    let mut out: Vec<(String, String)> = Vec::new();
    let mut body = String::new();
    let mut seen_title = false;
    let mut in_fence = false;

    for line in prd.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            body.push_str(line);
            body.push('\n');
            continue;
        }
        if in_fence {
            body.push_str(line);
            body.push('\n');
            continue;
        }

        if trimmed.starts_with('#') {
            if let Some(last) = out.last_mut() {
                last.1 = body.trim().to_string();
            }
            body.clear();
            // At the title level the FIRST is the document's name; a later one
            // at that level is a section, because some documents are written
            // entirely in `#`.
            if depth(trimmed) == title_depth && !seen_title {
                seen_title = true;
                continue;
            }
            out.push((
                trimmed.trim_start_matches('#').trim().to_lowercase(),
                String::new(),
            ));
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(last) = out.last_mut() {
        last.1 = body.trim().to_string();
    }
    out
}

/// A heading's first meaningful word, with any `1.` or `0b.` numbering removed.
pub(crate) fn first_word(heading: &str) -> String {
    heading
        .split_whitespace()
        .find(|word| word.chars().any(char::is_alphabetic))
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
        .to_lowercase()
}

/// A heading as a roadmap title: its own words, numbering and all.
///
/// Kept as written so the item still NAMES the section — the traceability check
/// looks for exactly that.
fn title_of(heading: &str) -> String {
    heading.trim().to_string()
}

/// Every heading in a markdown document, lowercased, without its `#` markers.
#[must_use]
pub fn prd_headings(prd: &str) -> Vec<String> {
    let mut headings = Vec::new();
    let mut in_fence = false;
    for line in prd.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.starts_with('#') {
            let heading = trimmed.trim_start_matches('#').trim().to_lowercase();
            if !heading.is_empty() {
                headings.push(heading);
            }
        }
    }
    headings
}

/// Whether one roadmap item points at something in the document.
fn names_a_section(title: &str, headings: &[String], prd: &str) -> bool {
    let lower = title.to_lowercase();
    let trimmed = lower.trim();
    if trimmed.is_empty() {
        return false;
    }
    // A heading named outright, in either direction: an item may quote the
    // heading, or a heading may be the item's own words written longer.
    if headings
        .iter()
        .any(|h| lower.contains(h.as_str()) || heading_contains_title(h, trimmed))
    {
        return true;
    }
    // Or a reference the document itself uses. Checked against the PRD text so a
    // marker the document never mentions does not count as a citation.
    let prd_lower = prd.to_lowercase();
    section_markers(&lower)
        .into_iter()
        .any(|marker| prd_lower.contains(&marker))
}

/// Whether a heading contains a title as a distinct word or phrase.
///
/// Prevents false positives where short stop-words or substring fragments
/// (such as "the" inside "theoretical" or "the widget") would otherwise cause
/// an unrelated item to match every heading containing those characters.
fn heading_contains_title(heading: &str, title: &str) -> bool {
    let title = title.trim();
    if title.len() < 3 {
        return false;
    }
    const STOP_WORDS: &[&str] = &[
        "the", "and", "for", "with", "from", "that", "this", "some", "what", "when", "how", "all",
    ];
    if STOP_WORDS.contains(&title) {
        return false;
    }
    let heading_bytes = heading.as_bytes();
    let mut start = 0;
    while let Some(pos) = heading[start..].find(title) {
        let idx = start + pos;
        let before_ok = idx == 0 || !heading_bytes[idx - 1].is_ascii_alphanumeric();
        let after_idx = idx + title.len();
        let after_ok =
            after_idx == heading.len() || !heading_bytes[after_idx].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
        if start >= heading.len() {
            break;
        }
    }
    false
}

/// The section references a roadmap item might carry: `§7`, `phase 3`, `r12`.
///
/// A reference always carries its NUMBER. The word `phase` on its own is not a
/// citation — every PRD contains it, so accepting it would make "deliver phase 9"
/// trace to a document that stops at phase 3, which is worse than no check.
#[must_use]
fn section_markers(lower_title: &str) -> Vec<String> {
    let words: Vec<&str> = lower_title
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '§'))
        .filter(|w| !w.is_empty())
        .collect();

    let mut markers = Vec::new();
    for (i, word) in words.iter().enumerate() {
        // `§7`, or `r12` — the number is attached.
        let attached = word.strip_prefix('§').or_else(|| word.strip_prefix('r'));
        if let Some(rest) = attached {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                markers.push((*word).to_string());
            }
        }
        // `phase 3` — the number is the next word, and without it there is no
        // reference at all.
        if *word == "phase" {
            if let Some(next) = words.get(i + 1) {
                if next.chars().all(|c| c.is_ascii_digit()) {
                    markers.push(format!("phase {next}"));
                }
            }
        }
    }
    markers
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The brief is complete. Coding may start.
    Ready(Box<ProjectBrief>),
    /// A brief exists but is not finished.
    Incomplete {
        project: String,
        missing: Vec<Artifact>,
    },
    /// The service answered, and no brief covers this workspace.
    NoBrief { workspace: PathBuf },
    /// The service could not be reached or did not answer usefully.
    ///
    /// Kept distinct from every other outcome because the operator's next action
    /// is completely different: start the service, rather than write a document.
    Unreachable { url: String, reason: String },
}

impl Gate {
    #[must_use]
    pub fn may_code(&self) -> bool {
        self.intake_task().is_none()
    }

    /// What the session must do first, when the brief is not ready.
    ///
    /// This is an instruction to complete the brief, not a refusal to run.
    /// Nothing is blocked: the session starts, and its first job is gathering
    /// what the proxy needs — asking, researching, proposing — after which
    /// coding continues in the same session. A harness that exited here would
    /// simply teach the operator to work somewhere else.
    #[must_use]
    pub fn intake_task(&self) -> Option<String> {
        let (subject, needed) = match self {
            // A complete brief can still have a roadmap that drifted from the
            // document it was supposed to come from. That is not a reason to
            // block, and it IS a reason to say so before building the items.
            Self::Ready(brief) => {
                let loose = brief.untraceable_items();
                if !brief.states_non_goals() {
                    return Some(format!(
                        "The brief is complete, but the PRD has no non-goals \
                         section.\n\nAdd a `## Non-goals` heading naming what is \
                         deliberately NOT being built in this version. It is the \
                         section everyone omits and the only thing that stops \
                         scope drift: an agent adding authentication nobody asked \
                         for is being helpful, and there is nothing in the \
                         document to say it should not.{}",
                        if loose.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "\n\nAlso: {} open roadmap item(s) name no section \
                                 of the PRD — {}",
                                loose.len(),
                                loose.join("; ")
                            )
                        }
                    ));
                }
                if loose.is_empty() {
                    return None;
                }
                return Some(format!(
                    "The brief is complete, but {} open roadmap item(s) name no \
                     section of the PRD:\n{}\n\nThe roadmap is supposed to be \
                     derived from the document. Before working these, either point \
                     each one at the section it implements, or write the section \
                     that argues for it — an item with nothing behind it is scope \
                     that arrived without a decision. Use `save_brief` to record \
                     the result.",
                    loose.len(),
                    loose
                        .iter()
                        .map(|i| format!("  - {i}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            Self::Incomplete { project, missing } => {
                let list = missing
                    .iter()
                    .map(|a| format!("  - {} — {}", a.field(), a.why()))
                    .collect::<Vec<_>>()
                    .join("\n");
                (format!("The brief for {project} is incomplete."), list)
            }
            Self::NoBrief { workspace } => (
                format!("No brief covers {}.", workspace.display()),
                Artifact::ALL
                    .iter()
                    .map(|a| format!("  - {} — {}", a.field(), a.why()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            // Nothing can be gathered while the store is unreachable — there is
            // nowhere to save it to. Say so plainly instead of collecting work
            // that would be lost.
            Self::Unreachable { url, reason } => {
                return Some(format!(
                    "The brief could not be read from {url}: {reason}\n\n\
                     Start the memory service, or point {MEMORY_URL_ENV} at it. \
                     Until it answers there is nowhere to save a brief to."
                ))
            }
        };

        Some(format!(
            "{subject}\n\nBefore writing code, complete it:\n{needed}\n\n{}\n\n\
             Scalability, security, auth and design are first-version concerns, \
             not things added later. The roadmap follows the PRD, in order.\n\n\
             Save each part with `save_brief` as it is agreed. Coding starts once \
             the brief is complete.",
            intake_method()
        ))
    }

    /// The message shown when coding is refused.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::Ready(brief) => {
                if !brief.states_non_goals() {
                    Some(
                        "The brief is missing a non-goals section in its PRD, so no code will be written.\n\
                         Add a `## Non-goals` section naming what is deliberately NOT being built."
                            .to_string(),
                    )
                } else if !brief.untraceable_items().is_empty() {
                    Some(format!(
                        "The brief has {} open roadmap item(s) that name no section of the PRD, so no code will be written.\n\
                         Point each item at a PRD section or argue for it in the document.",
                        brief.untraceable_items().len()
                    ))
                } else {
                    None
                }
            }
            Self::Incomplete { project, missing } => {
                let mut out = format!(
                    "This project's brief is incomplete, so no code will be written.\n\
                     Project: {project}\n\nMissing:"
                );
                for artifact in missing {
                    out.push_str(&format!("\n  - {} — {}", artifact.field(), artifact.why()));
                }
                out.push_str(
                    "\n\nThe proxy decides as you, and it cannot review work on a project \
                     it has no brief for. Fill these in — by asking, researching and \
                     proposing — before coding starts.",
                );
                Some(out)
            }
            Self::NoBrief { workspace } => Some(format!(
                "No project brief covers {}, so no code will be written.\n\n\
                 Create one for this workspace: about, goals, synopsis, the PRD, and a \
                 roadmap derived from it.",
                workspace.display()
            )),
            Self::Unreachable { url, reason } => Some(format!(
                "The brief could not be read from {url}: {reason}\n\n\
                 The brief is what the proxy reviews against, so the check cannot be \
                 skipped — a gate that opens when its own check fails is not a gate. \
                 Start the memory service, or point {MEMORY_URL_ENV} at it."
            )),
        }
    }
}

/// Where to read the brief from.
///
/// Environment first, then the operator's standing settings, then the local
/// default — the same ladder every other setting climbs, so one answer to
/// "who decided this?" covers all of them.
#[must_use]
pub fn memory_url() -> String {
    if let Some(url) = std::env::var(MEMORY_URL_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        return url;
    }
    static CACHED: std::sync::Mutex<Option<(Option<String>, String)>> = std::sync::Mutex::new(None);
    let current_data_dir = std::env::var(crate::admin::DATA_DIR_ENV).ok();
    if let Ok(guard) = CACHED.lock() {
        if let Some((cached_dir, url)) = &*guard {
            if *cached_dir == current_data_dir {
                return url.clone();
            }
        }
    }
    let resolved = crate::admin::AdminConfig::load_default()
        .memory_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_MEMORY_URL.to_string());
    if let Ok(mut guard) = CACHED.lock() {
        *guard = Some((current_data_dir, resolved.clone()));
    }
    resolved
}

/// Compare two project identifiers the way a filesystem would, not a byte
/// comparer: separators differ by platform and case differs by habit.
fn normalise(value: &str) -> String {
    value
        .trim()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Find the brief covering `workspace`.
///
/// Matches a full path first, then a project named after the workspace's own
/// folder — the two conventions actually present in the store.
///
/// **The authoritative routing lives in the proxy**, which owns a folder map
/// this endpoint does not expose. This is the subset resolvable from the brief
/// list alone; a workspace mapped to a differently-named project through that
/// map will read as having no brief here. Exposing the proxy's own resolution
/// would remove this duplication and should replace it.
#[must_use]
pub fn brief_for<'a>(briefs: &'a [ProjectBrief], workspace: &Path) -> Option<&'a ProjectBrief> {
    let wanted = normalise(&workspace.to_string_lossy());
    if let Some(exact) = briefs.iter().find(|b| normalise(&b.project) == wanted) {
        return Some(exact);
    }
    let folder = workspace
        .file_name()
        .map(|n| normalise(&n.to_string_lossy()))
        .unwrap_or_default();
    if folder.is_empty() {
        return None;
    }
    briefs.iter().find(|b| normalise(&b.project) == folder)
}

/// Resolve the canonical project identifier for `workspace` from existing briefs.
///
/// If an existing brief covers the workspace (by exact normalized path or by folder name),
/// its stored `project` identifier is returned to prevent duplicate entries from being created.
/// Otherwise, returns the workspace path as a string.
#[must_use]
pub fn canonical_project_for(briefs: &[ProjectBrief], workspace: &Path) -> String {
    if let Some(existing) = brief_for(briefs, workspace) {
        return existing.project.clone();
    }
    workspace.display().to_string()
}

/// Parse a proxy-config payload into the briefs it carries.
pub fn parse_briefs(body: &str) -> Result<Vec<ProjectBrief>, String> {
    serde_json::from_str::<ProxyConfig>(body)
        .map(|c| c.briefs)
        .map_err(|e| format!("the brief list could not be read: {e}"))
}

/// Where the last brief this workspace saw is kept.
pub const CACHE_FILE: &str = ".incode/brief.cache.json";

/// Keep a copy of a complete brief, so an outage does not undo the intake.
///
/// Failure is ignored: losing the copy costs a fallback, and failing the run
/// over it would cost the work.
fn remember_locally(workspace: &Path, brief: &ProjectBrief) {
    let path = workspace.join(CACHE_FILE);
    if let Some(parent) = path.parent() {
        let _ = crate::folders::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(brief) {
        let _ = std::fs::write(path, text);
    }
}

/// The last complete brief this workspace saw, if there is one.
#[must_use]
pub fn cached(workspace: &Path) -> Option<ProjectBrief> {
    let text = std::fs::read_to_string(workspace.join(CACHE_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read the brief for `workspace` and decide whether coding may start.
#[must_use]
pub fn check(workspace: &Path) -> Gate {
    let base = memory_url();
    let url = format!("{}{BRIEF_PATH}", base.trim_end_matches('/'));

    // THE SERVICE IS THE ONLY WAY TO CHANGE A BRIEF, WHICH IS WHY A CACHE IS
    // HONEST HERE.
    //
    // With the store down, `intake_task` told every session to complete the
    // brief before writing code — including projects whose brief had been
    // finished for months. An outage in a service that holds a DOCUMENT was
    // undoing the intake of every project on the machine.
    //
    // A cached brief cannot have gone stale in a way that matters: the only
    // route to editing one is `save_brief`, which needs the same service that
    // is not answering. What it cannot see is an edit made from another
    // machine after this one last succeeded, which is the narrow residual
    // risk and the reason the fallback is traced rather than silent.
    let fall_back = |reason: String| match cached(workspace) {
        Some(brief) if brief.missing().is_empty() => {
            crate::trace!(
                "brief",
                "{url} unreachable ({reason}); working from the cached brief for {}",
                brief.project
            );
            Gate::Ready(Box::new(brief))
        }
        // Nothing cached, so there is genuinely nowhere to read a brief from
        // and nowhere to save one to. The original message is the right one.
        _ => Gate::Unreachable {
            url: url.clone(),
            reason,
        },
    };

    match fetch(&url) {
        Err(reason) => fall_back(reason),
        Ok(body) => match parse_briefs(&body) {
            Err(reason) => fall_back(reason),
            Ok(briefs) => match brief_for(&briefs, workspace) {
                None => Gate::NoBrief {
                    workspace: workspace.to_path_buf(),
                },
                Some(brief) => {
                    let missing = brief.missing();
                    if missing.is_empty() {
                        // Kept on every success, so the copy is as fresh as
                        // the last time the service answered. Only a COMPLETE
                        // brief is cached: falling back to a half-finished one
                        // would skip the intake it is in the middle of.
                        remember_locally(workspace, brief);
                        Gate::Ready(Box::new(brief.clone()))
                    } else {
                        Gate::Incomplete {
                            project: brief.project.clone(),
                            missing,
                        }
                    }
                }
            },
        },
    }
}

/// The tool the agent calls to record what it has gathered.
pub const SAVE_TOOL: &str = "save_brief";

/// `save_brief` as advertised to the model.
#[must_use]
pub fn save_tool_definition() -> Value {
    serde_json::json!({
        "name": SAVE_TOOL,
        "description": "Record part of this project's brief once it is agreed. \
            Send only the fields you have settled — anything omitted keeps its \
            current value, so this can be called several times as the brief comes \
            together. The reply lists what is still missing. Do not invent \
            content to fill a field: ask the owner what only they can answer, and \
            research what can be researched.",
        "input_schema": {
            "type": "object",
            "properties": {
                "about": {"type": "string", "description": "What this project is."},
                "goals": {"type": "string", "description": "What the proxy should protect."},
                "synopsis": {"type": "string", "description": "The PRD in a few lines. This rides every review prompt, so keep it short and specific."},
                "prd": {"type": "string", "description": "The product document, markdown."},
                "roadmap": {
                    "type": "array",
                    "description": "The ordered plan, derived from the PRD.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "title": {"type": "string"},
                            "status": {"type": "string", "description": "todo | doing | done"}
                        },
                        "required": ["title"]
                    }
                }
            }
        }
    })
}

/// Parse a `save_brief` call into an update.
pub fn parse_save(input: &str) -> Result<BriefUpdate, String> {
    let parsed: Value = serde_json::from_str(input)
        .map_err(|e| format!("`{SAVE_TOOL}` needs a JSON object: {e}"))?;

    let obj = parsed.as_object().ok_or_else(|| {
        format!(
            "`{SAVE_TOOL}` needs a JSON object, got {}",
            match parsed {
                Value::Array(_) => "an array",
                Value::String(_) => "a string",
                Value::Number(_) => "a number",
                Value::Bool(_) => "a boolean",
                Value::Null => "null",
                _ => "invalid JSON",
            }
        )
    })?;

    const ALLOWED_KEYS: &[&str] = &["about", "goals", "synopsis", "prd", "roadmap"];
    for key in obj.keys() {
        if !ALLOWED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "unknown field `{key}` in `{SAVE_TOOL}` input; allowed fields are: {}",
                ALLOWED_KEYS.join(", ")
            ));
        }
    }

    let extract_text = |key: &str| -> Result<Option<String>, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Some(_) => Err(format!("`{key}` must be a string")),
        }
    };

    let about = extract_text("about")?;
    let goals = extract_text("goals")?;
    let synopsis = extract_text("synopsis")?;
    let prd = extract_text("prd")?;

    let roadmap = match obj.get("roadmap") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut roadmap_items = Vec::with_capacity(items.len());
            let mut seen_ids = std::collections::HashSet::new();

            for (idx, item_val) in items.iter().enumerate() {
                let item_obj = item_val
                    .as_object()
                    .ok_or_else(|| format!("roadmap item at index {idx} must be an object"))?;

                for k in item_obj.keys() {
                    if k != "id" && k != "title" && k != "status" {
                        return Err(format!(
                            "unknown field `{k}` in roadmap item at index {idx}; allowed fields are: id, title, status"
                        ));
                    }
                }

                let title = item_obj
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        format!("roadmap item at index {idx} is missing required non-empty `title`")
                    })?;

                let id = match item_obj.get("id") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.trim().to_string(),
                    Some(_) => {
                        return Err(format!(
                            "`id` in roadmap item at index {idx} must be a string"
                        ))
                    }
                };

                if !id.is_empty() {
                    let id_norm = id.to_ascii_lowercase();
                    if !seen_ids.insert(id_norm) {
                        return Err(format!("duplicate roadmap item id `{id}` at index {idx}"));
                    }
                }

                let status = match item_obj.get("status") {
                    None | Some(Value::Null) => "todo".to_string(),
                    Some(Value::String(s)) => {
                        let parsed_status = RoadmapStatus::parse(s)?;
                        parsed_status.as_str().to_string()
                    }
                    Some(_) => {
                        return Err(format!(
                            "`status` in roadmap item at index {idx} must be a string"
                        ))
                    }
                };

                roadmap_items.push(RoadmapItem {
                    id,
                    title: title.to_string(),
                    status,
                });
            }
            Some(roadmap_items)
        }
        Some(_) => return Err("`roadmap` must be a list of items".to_string()),
    };

    let update = BriefUpdate {
        about,
        goals,
        synopsis,
        prd,
        roadmap,
    };
    if update.is_empty() {
        return Err(format!(
            "`{SAVE_TOOL}` was called with nothing to save. Send at least one of: \
             about, goals, synopsis, prd, roadmap."
        ));
    }
    Ok(update)
}

/// Render what a save left outstanding, for the tool result.
#[must_use]
pub fn saved_report(still_missing: &[Artifact]) -> String {
    if still_missing.is_empty() {
        return "Saved. The brief is complete — coding can start.".to_string();
    }
    let list = still_missing
        .iter()
        .map(|a| format!("  - {} — {}", a.field(), a.why()))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Saved. Still missing:\n{list}")
}

/// The fields of a brief being filled in. `None` leaves a field untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BriefUpdate {
    pub about: Option<String>,
    pub goals: Option<String>,
    pub synopsis: Option<String>,
    pub prd: Option<String>,
    pub roadmap: Option<Vec<RoadmapItem>>,
}

impl BriefUpdate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.about.is_none()
            && self.goals.is_none()
            && self.synopsis.is_none()
            && self.prd.is_none()
            && self.roadmap.is_none()
    }
}

/// The status a roadmap item wears once a command has proven it.
pub const STATUS_DONE: &str = "done";

/// Mark roadmap items done IN THE RAW CONFIG, touching nothing else.
///
/// # Wired to
///
/// - **Called by** [`mark_roadmap_done`], which owns the fetch/write/verify.
/// - **Sibling to** [`patch_config`], which writes whole brief FIELDS. This one
///   reaches into a single item's `status` and is deliberately narrower.
///
/// # Why this edits JSON instead of `Vec<RoadmapItem>`
///
/// [`RoadmapItem`] declares only `id`, `title` and `status` — its own note says
/// anything else on the wire is ignored rather than fought with, which is right
/// for READING and destructive for WRITING. The live service sends `at` and
/// `note` on every item, so round-tripping the roadmap through that struct would
/// silently erase both on every item of every project it touched. Verified on
/// the running service before this was written:
/// `{'at': 0, 'id': 'b1', 'note': '', 'status': 'todo', 'title': '...'}`.
///
/// # PROMOTE ONLY, NEVER DEMOTE
///
/// An item is moved to `done` when a command proved it. Nothing here moves an
/// item BACK, and that asymmetry is deliberate:
///
/// - `Unproven` is not failure. Most roadmap items are settled by a person, and
///   demoting one because no machine can check it would erase the owner's own
///   judgement on every run.
/// - A FAILED check on an item the owner marked done is worth REPORTING, not
///   silently reverting — the owner may know something the check does not, and a
///   machine quietly undoing a person's decision is the failure mode the whole
///   completion law is built against.
///
/// Returns how many items actually changed, so a caller can skip a pointless
/// write.
pub fn mark_done_in_config(config: &mut Value, project: &str, ids: &[String]) -> usize {
    if ids.is_empty() {
        return 0;
    }
    let wanted = normalise(project);
    let Some(briefs) = config.get_mut("briefs").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut changed = 0;
    for brief in briefs.iter_mut() {
        let matches = brief
            .get("project")
            .and_then(Value::as_str)
            .is_some_and(|name| normalise(name) == wanted);
        if !matches {
            continue;
        }
        let Some(roadmap) = brief.get_mut("roadmap").and_then(Value::as_array_mut) else {
            continue;
        };
        for item in roadmap.iter_mut() {
            let is_wanted = item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| ids.iter().any(|w| w.eq_ignore_ascii_case(id)));
            if !is_wanted {
                continue;
            }
            let already = item
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case(STATUS_DONE));
            if already {
                continue;
            }
            if let Some(object) = item.as_object_mut() {
                object.insert("status".into(), Value::String(STATUS_DONE.to_string()));
                changed += 1;
            }
        }
    }
    changed
}

/// Tell the belief which goals a command has proven.
///
/// # Wired to
///
/// - **Called by** `crate::cli::slash_commands::handle_roadmap` and by the run
///   handler once a run ends.
/// - **Calls** the same `/v1/owner/proxy` document [`save`] writes, with the
///   same read-back-and-retry, because the same lost-update race applies: the
///   endpoint replaces the WHOLE document and carries no version.
///
/// Returns how many items were newly marked done. `Ok(0)` means the belief
/// already agreed and nothing was written.
pub fn mark_roadmap_done(project: &str, ids: &[String]) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let base = memory_url();
    let base = base.trim_end_matches('/');
    let url = format!("{base}{BRIEF_PATH}");

    for attempt in 1..=SAVE_ATTEMPTS {
        let (raw_config, etag) = fetch_with_etag(&url)?;
        let mut config: Value =
            serde_json::from_str(&raw_config).map_err(|e| format!("reading the config: {e}"))?;
        let changed = mark_done_in_config(&mut config, project, ids);
        if changed == 0 {
            // Already true. Writing anyway would put the whole document back on
            // the wire for no reason, and every such write is another chance to
            // lose somebody else's.
            return Ok(0);
        }
        let body =
            serde_json::to_string(&config).map_err(|e| format!("encoding the config: {e}"))?;
        let written_ok = post_with_etag(&url, body, etag.as_deref())?;
        if !written_ok {
            crate::trace!(
                "goal",
                "{project}: concurrent write detected (precondition failed), attempt {attempt} of {SAVE_ATTEMPTS}"
            );
            continue;
        }

        // READ BACK. The same reason `save` does: this endpoint has no version,
        // so the only way to know a write survived is to look.
        let (raw_written, _) = fetch_with_etag(&url)?;
        let mut written: Value =
            serde_json::from_str(&raw_written).map_err(|e| format!("reading the config: {e}"))?;
        if mark_done_in_config(&mut written, project, ids) == 0 {
            crate::trace!("goal", "{project}: {changed} item(s) marked done");
            return Ok(changed);
        }
        crate::trace!(
            "goal",
            "{project}: the tick did not survive, attempt {attempt} of {SAVE_ATTEMPTS}"
        );
    }
    Err(format!(
        "the roadmap tick did not survive {SAVE_ATTEMPTS} attempts — another \
         writer is replacing the document"
    ))
}

/// Write the given fields into `project`'s brief inside a whole config document,
/// leaving every other key and every other brief exactly as they were.
///
/// # Why this takes the entire document
///
/// The service's write endpoint replaces the **whole** `ProxyConfig`. It carries
/// the owner's rules, the proxy's mode, its model and its severity ceiling
/// alongside the briefs — none of which this crate models. Serialising a partial
/// struct and posting that would silently delete all of them. So the document is
/// treated as opaque: parsed as JSON, patched at one path, and handed back with
/// everything it arrived with.
pub fn patch_config(config: &mut Value, project: &str, update: &BriefUpdate) -> Result<(), String> {
    let wanted = normalise(project);
    if wanted.is_empty() {
        return Err("a brief needs a project name".to_string());
    }

    let briefs = config
        .get_mut("briefs")
        .ok_or("the config has no `briefs` list")?
        .as_array_mut()
        .ok_or("`briefs` is not a list")?;

    let folder = Path::new(project)
        .file_name()
        .map(|n| normalise(&n.to_string_lossy()))
        .unwrap_or_default();

    let index = briefs.iter().position(|b| {
        b.get("project")
            .and_then(Value::as_str)
            .map(|p| {
                let norm = normalise(p);
                if norm == wanted {
                    return true;
                }
                if !folder.is_empty() && norm == folder {
                    return true;
                }
                let p_folder = Path::new(p)
                    .file_name()
                    .map(|n| normalise(&n.to_string_lossy()))
                    .unwrap_or_default();
                if !p_folder.is_empty() && p_folder == wanted {
                    return true;
                }
                false
            })
            .unwrap_or(false)
    });

    let entry = match index {
        Some(i) => &mut briefs[i],
        None => {
            briefs.push(serde_json::json!({ "project": project }));
            briefs.last_mut().expect("just pushed")
        }
    };
    let object = entry.as_object_mut().ok_or("that brief is not an object")?;

    if let Some(about) = &update.about {
        object.insert("about".into(), Value::String(about.clone()));
    }
    if let Some(goals) = &update.goals {
        object.insert("goals".into(), Value::String(goals.clone()));
    }
    if let Some(synopsis) = &update.synopsis {
        object.insert("synopsis".into(), Value::String(synopsis.clone()));
    }
    if let Some(prd) = &update.prd {
        object.insert("prd".into(), Value::String(prd.clone()));
    }
    if let Some(roadmap) = &update.roadmap {
        object.insert(
            "roadmap".into(),
            serde_json::to_value(roadmap).map_err(|e| format!("roadmap: {e}"))?,
        );
    }
    Ok(())
}

/// How many times a lost write is retried before it is reported.
///
/// Three, then stop and say so. A lost update is a race, so a retry usually
/// wins; a write that loses three times in a row is not unlucky, it is a second
/// writer that is not going away, and retrying past that is thrashing.
const SAVE_ATTEMPTS: u32 = 3;

/// Read the config, patch this project's brief, and write it back.
///
/// Returns the artifacts still missing after the write, so a caller learns what
/// is left without a second round trip.
pub fn save(
    workspace: &Path,
    project: &str,
    update: &BriefUpdate,
) -> Result<Vec<Artifact>, String> {
    if update.is_empty() {
        return Err("nothing to save".to_string());
    }
    let base = memory_url();
    let base = base.trim_end_matches('/');
    let url = format!("{base}{BRIEF_PATH}");

    let mut last_missing: Option<Vec<&'static str>> = None;
    for attempt in 1..=SAVE_ATTEMPTS {
        let (raw_config, etag) = fetch_with_etag(&url)?;
        let mut config: Value =
            serde_json::from_str(&raw_config).map_err(|e| format!("reading the config: {e}"))?;

        let existing_briefs = parse_briefs(&raw_config).unwrap_or_default();
        let canonical_project = canonical_project_for(&existing_briefs, workspace);
        let project_to_patch = if !canonical_project.is_empty() {
            &canonical_project
        } else {
            project
        };

        patch_config(&mut config, project_to_patch, update)?;
        let body =
            serde_json::to_string(&config).map_err(|e| format!("encoding the config: {e}"))?;

        let written_ok = post_with_etag(&url, body, etag.as_deref())?;
        if !written_ok {
            crate::trace!(
                "brief",
                "attempt {attempt} of {SAVE_ATTEMPTS} precondition failed (concurrent write detected) — retrying"
            );
            continue;
        }

        let briefs = parse_briefs(&fetch(&url)?)?;
        let Some(saved) = brief_for(&briefs, workspace).or_else(|| {
            let wanted = normalise(project_to_patch);
            briefs.iter().find(|b| normalise(&b.project) == wanted)
        }) else {
            return Err("the brief was written but could not be read back".to_string());
        };

        // CONTENT, NOT PRESENCE. Every field this update carried has to be in
        // what came back, or the write did not survive.
        let lost = fields_not_stored(update, saved);
        if lost.is_empty() {
            return Ok(saved.missing());
        }
        crate::trace!(
            "brief",
            "attempt {attempt} of {SAVE_ATTEMPTS} lost {lost:?} — retrying"
        );
        last_missing = Some(lost);
    }

    // Loud, not silent. A brief that quietly failed to save is a project the
    // proxy will later be asked to reason about with nothing to reason from.
    Err(format!(
        "the brief did not survive {SAVE_ATTEMPTS} attempts — {} was written and came back \
         empty each time. Another writer is replacing the whole document; the write endpoint \
         carries no version to write against.",
        last_missing.unwrap_or_default().join(", ")
    ))
}

/// Derive the belief from the project's documents and save it.
///
/// # Why this runs before asking anyone
///
/// The spec gate already refuses to let code be written until the project has
/// its documents. So by the time the loop can run at all, the thing the brief
/// describes is written down — and asking a person to type it a second time,
/// into a different store, is asking them to keep two copies of one truth in
/// step by hand. They will not, and the copy the proxy reads is the one that
/// goes stale.
///
/// Returns what is STILL missing after the save, so the caller can fall back to
/// asking for exactly the parts the document does not cover. A document with no
/// `## Goals` section leaves `goals` missing and the gate asks for that one
/// thing rather than the whole brief.
///
/// # What it will not do
///
/// It does not overwrite a field that is already there. `patch_config` only
/// writes the fields an update carries, and [`from_docs`] carries only what the
/// document actually says — so a brief a person wrote is never replaced by one
/// derived from a heading.
pub fn derive_from_docs(workspace: &Path) -> Result<Vec<Artifact>, String> {
    let update = from_docs(workspace);
    if update.is_empty() {
        return Err(format!("there is no {PRD_PATH} to derive a brief from"));
    }
    save(workspace, &workspace.display().to_string(), &update)
}

/// Write the brief's document to disk when the project has none.
///
/// # Why this is the other half of deriving
///
/// [`from_docs`] rebuilds a lost belief from the project's document, which makes
/// a clobbered brief recoverable — but only for projects that HAVE a document. A
/// brief someone typed straight into the proxy has nothing behind it, so losing
/// it loses it for good. That was the honest gap left after the healing fix.
///
/// So the two directions close the loop: a document without a belief gets one,
/// and a belief without a document writes one. After either, the truth exists in
/// two places and neither store is the only copy.
///
/// # The direction of truth, which this must not invert
///
/// The document is the source; the brief is derived from it. So this writes ONLY
/// when there is no document to be the source — an absent or empty file. An
/// existing document is never touched, because a brief that has drifted would
/// otherwise overwrite the thing it was supposed to be derived from, and the
/// drift would become the truth.
///
/// Returns whether it wrote.
pub fn write_prd_to_docs(workspace: &Path, brief: &ProjectBrief) -> Result<bool, String> {
    use std::io::Write;
    if brief.prd.trim().is_empty() {
        return Ok(false);
    }
    let path = workspace.join(PRD_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
    }
    // Attempt exclusive atomic creation first to prevent TOCTOU races.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(brief.prd.as_bytes())
                .map_err(|e| format!("could not write {}: {e}", path.display()))?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("could not read {}: {e}", path.display()))?;
            if !content.trim().is_empty() {
                return Ok(false);
            }
            std::fs::write(&path, &brief.prd)
                .map_err(|e| format!("could not write {}: {e}", path.display()))?;
            Ok(true)
        }
        Err(err) => Err(format!("could not open {}: {err}", path.display())),
    }
}

fn field_lost(written: &Option<String>, stored: &str) -> bool {
    let Some(w) = written.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let w_norm = w.replace("\r\n", "\n");
    let s_norm = stored.trim().replace("\r\n", "\n");
    w_norm != s_norm
}

fn roadmap_lost(written: &Option<Vec<RoadmapItem>>, stored: &[RoadmapItem]) -> bool {
    let Some(items) = written else {
        return false;
    };
    if items.is_empty() {
        return false;
    }
    if stored.is_empty() {
        return true;
    }
    for item in items {
        let found = stored.iter().any(|s| {
            let id_match = if !item.id.trim().is_empty() && !s.id.trim().is_empty() {
                item.id.trim().eq_ignore_ascii_case(s.id.trim())
            } else {
                true
            };
            let title_match = item.title.trim() == s.title.trim();
            let status_match = item.status.trim().eq_ignore_ascii_case(s.status.trim());
            id_match && title_match && status_match
        });
        if !found {
            return true;
        }
    }
    false
}

/// The fields this update carried that are NOT in what came back.
///
/// Compared by actual normalized content rather than mere non-emptiness,
/// so that concurrent overwrites or dropped roadmap items are detected.
fn fields_not_stored(update: &BriefUpdate, saved: &ProjectBrief) -> Vec<&'static str> {
    let mut lost = Vec::new();
    if field_lost(&update.about, &saved.about) {
        lost.push("about");
    }
    if field_lost(&update.goals, &saved.goals) {
        lost.push("goals");
    }
    if field_lost(&update.synopsis, &saved.synopsis) {
        lost.push("synopsis");
    }
    if field_lost(&update.prd, &saved.prd) {
        lost.push("prd");
    }
    if roadmap_lost(&update.roadmap, &saved.roadmap) {
        lost.push("roadmap");
    }
    lost
}
use crate::runtime::http::{client_for, run_async};

/// Fetch a URL reusing the pooled HTTP client and ambient/shared runtime, returning body and optional ETag.
fn fetch_with_etag(url: &str) -> Result<(String, Option<String>), String> {
    let url = url.to_string();
    run_async(async move {
        let response = client_for(&url)
            .get(&url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the service answered {status}"));
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let text = response
            .text()
            .await
            .map_err(|e| format!("reading the response failed: {e}"))?;
        Ok((text, etag))
    })
}

/// Send a document back reusing the pooled HTTP client and ambient/shared runtime, optionally with If-Match.
/// Returns Ok(true) on success, Ok(false) on 412 Precondition Failed / 409 Conflict.
fn post_with_etag(url: &str, body: String, etag: Option<&str>) -> Result<bool, String> {
    let url = url.to_string();
    let etag_owned = etag.map(str::to_string);
    run_async(async move {
        let mut req = client_for(&url)
            .post(&url)
            .timeout(FETCH_TIMEOUT)
            .header("content-type", "application/json")
            .body(body);
        if let Some(etag_val) = etag_owned {
            req = req.header("if-match", etag_val);
        }
        let response = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED
            || status == reqwest::StatusCode::CONFLICT
        {
            return Ok(false);
        }
        if !status.is_success() {
            return Err(format!("the service answered {status}"));
        }
        Ok(true)
    })
}

/// Fetch a URL reusing the pooled HTTP client and ambient/shared runtime.
fn fetch(url: &str) -> Result<String, String> {
    fetch_with_etag(url).map(|(body, _)| body)
}

/// Send a document back reusing the pooled HTTP client and ambient/shared runtime.
#[allow(dead_code)]
fn post(url: &str, body: String) -> Result<(), String> {
    match post_with_etag(url, body, None) {
        Ok(true) => Ok(()),
        Ok(false) => Err("post failed precondition".to_string()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Serialises the tests that point [`MEMORY_URL_ENV`] at a stand-in server.
///
/// The environment is process-wide and Rust runs tests in parallel, so two of
/// them setting this at once would send one test's request to the other's
/// server — intermittently, and only on a busy machine.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    /// The exact shape the running service returns, `at` and `note` included.
    fn live_config() -> Value {
        serde_json::json!({
            "mode": "decide",
            "briefs": [
                {
                    "project": "D:\\ball-game",
                    "about": "a game",
                    "roadmap": [
                        {"at": 0, "id": "b1", "note": "", "status": "todo", "title": "falls and lands"},
                        {"at": 7, "id": "b2", "note": "keep me", "status": "todo", "title": "jumps"},
                        {"at": 0, "id": "b3", "note": "", "status": "done", "title": "playable"}
                    ]
                },
                {
                    "project": "other-project",
                    "roadmap": [{"at": 0, "id": "b1", "note": "", "status": "todo", "title": "theirs"}]
                }
            ]
        })
    }

    /// **THE REASON THIS PATCHES RAW JSON.** `RoadmapItem` declares only id,
    /// title and status; the live service sends `at` and `note` as well. Writing
    /// the roadmap back through that struct would erase both on every item of
    /// every project it touched — silently, and for fields this crate does not
    /// even know the meaning of.
    ///
    /// Verified against the running service before this was written:
    /// `{'at': 0, 'id': 'b1', 'note': '', 'status': 'todo', 'title': '...'}`.
    #[test]
    fn ticking_an_item_preserves_fields_this_crate_does_not_declare() {
        let mut config = live_config();
        let changed = mark_done_in_config(&mut config, "D:\\ball-game", &["b2".to_string()]);
        assert_eq!(changed, 1);

        let item = &config["briefs"][0]["roadmap"][1];
        assert_eq!(item["status"], "done", "the tick landed");
        assert_eq!(item["at"], 7, "`at` survived: {item}");
        assert_eq!(item["note"], "keep me", "`note` survived: {item}");
        assert_eq!(item["title"], "jumps");
    }

    /// A tick reaches ONE project. Roadmap ids are only unique within a brief,
    /// and `b1` exists in two of them here.
    #[test]
    fn a_tick_does_not_reach_another_project_with_the_same_item_id() {
        let mut config = live_config();
        mark_done_in_config(&mut config, "D:\\ball-game", &["b1".to_string()]);

        assert_eq!(config["briefs"][0]["roadmap"][0]["status"], "done");
        assert_eq!(
            config["briefs"][1]["roadmap"][0]["status"], "todo",
            "another project's identically-named item must not move"
        );
    }

    /// Already true is not a change, so the caller can skip the write. Every
    /// avoidable write to this endpoint is another chance to lose someone
    /// else's, because it replaces the whole document with no version.
    #[test]
    fn ticking_what_is_already_done_changes_nothing() {
        let mut config = live_config();
        let changed = mark_done_in_config(&mut config, "D:\\ball-game", &["b3".to_string()]);
        assert_eq!(changed, 0, "b3 was already done");
    }

    /// **PROMOTE ONLY.** Nothing here can move an item back to `todo`: an
    /// unproven item is not a failed one, and a machine that could revert the
    /// owner's own judgement would be undoing a person's decision on every run.
    #[test]
    fn nothing_here_can_move_an_item_back_from_done() {
        let mut config = live_config();
        // Ask for every id, including the one already done.
        mark_done_in_config(
            &mut config,
            "D:\\ball-game",
            &["b1".to_string(), "b2".to_string(), "b3".to_string()],
        );
        let statuses: Vec<&str> = config["briefs"][0]["roadmap"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, vec!["done", "done", "done"]);
        assert!(
            !statuses.contains(&"todo"),
            "no path in this function writes `todo`"
        );
    }

    #[test]
    fn an_empty_list_or_an_unknown_project_writes_nothing() {
        let mut config = live_config();
        assert_eq!(mark_done_in_config(&mut config, "D:\\ball-game", &[]), 0);
        assert_eq!(
            mark_done_in_config(&mut config, "no-such-project", &["b1".to_string()]),
            0
        );
        assert_eq!(config["briefs"][0]["roadmap"][0]["status"], "todo");
    }

    /// A config that is not the shape we expect is left alone rather than
    /// mangled — the service is a separate program on its own release schedule.
    #[test]
    fn a_config_without_a_roadmap_is_left_untouched() {
        let mut config = serde_json::json!({"briefs": [{"project": "x"}]});
        assert_eq!(
            mark_done_in_config(&mut config, "x", &["b1".to_string()]),
            0
        );
        let mut empty = serde_json::json!({});
        assert_eq!(mark_done_in_config(&mut empty, "x", &["b1".to_string()]), 0);
    }

    use super::*;

    /// A project whose document says what it is, in the shape people write.
    fn documented() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("docs")).expect("docs");
        std::fs::write(
            dir.path().join(PRD_PATH),
            "# Ball Game\n\n\
             ## 1. About\nA browser game: one ball, one key, gravity.\n\n\
             ## 2. Goals\nProve the loop works on something small.\n\n\
             ## 3. Synopsis\nA canvas, a ball and a space bar.\n\n\
             ## 4. The ball falls and lands\nGravity pulls it to the floor.\n\n\
             ## 5. The ball jumps\nSpace lifts it, only when grounded.\n\n\
             ## 6. Non-goals\nNo score, no sound, no levels.\n\n\
             ## 7. Assumptions\nCanvas is 480x320.\n",
        )
        .expect("write");
        dir
    }

    /// **A BELIEF WITH NO DOCUMENT BEHIND IT IS ONE CLOBBER FROM GONE.**
    /// Deriving rebuilds a lost brief from the project's document — but only for
    /// projects that have one. This is the other direction, so a brief typed
    /// straight into the proxy also ends up written down.
    #[test]
    fn a_brief_with_no_document_behind_it_gets_one_written() {
        let dir = tempfile::tempdir().expect("tmp");
        let brief = ProjectBrief {
            project: "P".to_string(),
            prd: "# Doc\n\n## A thing\nIt does the thing.\n".to_string(),
            ..ProjectBrief::default()
        };
        assert!(write_prd_to_docs(dir.path(), &brief).expect("writes"));

        let written = std::fs::read_to_string(dir.path().join(PRD_PATH)).expect("read");
        assert_eq!(written, brief.prd, "the document is the brief's own text");
        // And the loop closes: what was written derives straight back.
        assert_eq!(
            from_docs(dir.path()).prd.as_deref(),
            Some(brief.prd.as_str())
        );
    }

    /// **THE DOCUMENT IS THE SOURCE, AND THIS MUST NOT INVERT THAT.** A brief
    /// that has drifted would otherwise overwrite the thing it was derived
    /// from, and the drift would silently become the truth.
    #[test]
    fn an_existing_document_is_never_overwritten_by_the_brief() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("docs")).expect("docs");
        let real = "# The real document\n\n## A thing\nWritten by a person.\n";
        std::fs::write(dir.path().join(PRD_PATH), real).expect("write");

        let stale = ProjectBrief {
            project: "P".to_string(),
            prd: "# A stale copy\n".to_string(),
            ..ProjectBrief::default()
        };
        assert!(
            !write_prd_to_docs(dir.path(), &stale).expect("no error"),
            "it must decline to write"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(PRD_PATH)).expect("read"),
            real,
            "the person's document survived"
        );
    }

    /// An empty file is not a document — it is the shape a failed write leaves,
    /// and refusing to fill it would strand the project forever.
    #[test]
    fn an_empty_document_file_is_filled_rather_than_treated_as_the_source() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("docs")).expect("docs");
        std::fs::write(dir.path().join(PRD_PATH), "   \n\n").expect("write");
        let brief = ProjectBrief {
            project: "P".to_string(),
            prd: "# Doc\n\n## A thing\nreal.\n".to_string(),
            ..ProjectBrief::default()
        };
        assert!(write_prd_to_docs(dir.path(), &brief).expect("writes"));
    }

    /// Nothing to write is not an error — most projects reach here with the
    /// document already in place.
    #[test]
    fn a_brief_with_no_document_text_writes_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(!write_prd_to_docs(dir.path(), &ProjectBrief::default()).expect("no error"));
        assert!(!dir.path().join(PRD_PATH).exists());
    }

    /// **AN EMPTIED BRIEF EXISTS.** This is the exact shape that got past the
    /// old read-back: the write was lost to another writer, the project key
    /// survived, every field came back blank — and a check that asked "is the
    /// brief there?" said yes. The one guard meant to catch a lost write passed
    /// while the write was being lost.
    #[test]
    fn a_write_that_came_back_empty_is_detected_as_lost() {
        let update = BriefUpdate {
            about: Some("what it is".to_string()),
            goals: Some("what it protects".to_string()),
            prd: Some("# Doc".to_string()),
            roadmap: Some(vec![RoadmapItem {
                id: "r1".to_string(),
                title: "a thing".to_string(),
                status: "todo".to_string(),
            }]),
            ..BriefUpdate::default()
        };
        // Present, named, and empty — what a clobbered write leaves behind.
        let clobbered = ProjectBrief {
            project: "P".to_string(),
            ..ProjectBrief::default()
        };
        assert_eq!(
            fields_not_stored(&update, &clobbered),
            vec!["about", "goals", "prd", "roadmap"],
            "every field that was written and came back blank must be named"
        );
    }

    /// A write that survived reports nothing lost, or every save would retry
    /// three times and then fail on success.
    #[test]
    fn a_write_that_survived_reports_nothing_lost() {
        let update = BriefUpdate {
            about: Some("what it is".to_string()),
            roadmap: Some(vec![RoadmapItem {
                id: "r1".to_string(),
                title: "a thing".to_string(),
                status: "todo".to_string(),
            }]),
            ..BriefUpdate::default()
        };
        let stored = ProjectBrief {
            project: "P".to_string(),
            about: "what it is".to_string(),
            roadmap: update.roadmap.clone().unwrap_or_default(),
            ..ProjectBrief::default()
        };
        assert!(fields_not_stored(&update, &stored).is_empty());
    }

    /// A field this update never wrote is not this update's to lose. Reporting
    /// it would make every partial save look like a failed one.
    #[test]
    fn a_field_that_was_not_written_is_not_reported_as_lost() {
        let update = BriefUpdate {
            about: Some("only this".to_string()),
            ..BriefUpdate::default()
        };
        let stored = ProjectBrief {
            project: "P".to_string(),
            about: "only this".to_string(),
            // goals, synopsis, prd and roadmap are empty and always were.
            ..ProjectBrief::default()
        };
        assert!(
            fields_not_stored(&update, &stored).is_empty(),
            "a partial save must not read as a lost one"
        );
    }

    /// **The belief comes from the document, and nothing is invented.**
    #[test]
    fn the_brief_is_derived_from_the_projects_own_document() {
        let dir = documented();
        let update = from_docs(dir.path());

        assert_eq!(
            update.about.as_deref(),
            Some("A browser game: one ball, one key, gravity.")
        );
        assert_eq!(
            update.goals.as_deref(),
            Some("Prove the loop works on something small.")
        );
        assert_eq!(
            update.synopsis.as_deref(),
            Some("A canvas, a ball and a space bar.")
        );
        assert!(
            update
                .prd
                .as_deref()
                .is_some_and(|p| p.contains("## 5. The ball jumps")),
            "the document is carried verbatim, because decisions are searched against it"
        );
    }

    /// **Work sections become the roadmap; the brief's own fields do not.**
    /// "About" describes the project — building it is not a task.
    #[test]
    fn only_the_work_sections_become_roadmap_items() {
        // With code, so the list is the PRD's alone — a new project's
        // scaffold item is the next test's subject.
        let dir = documented();
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src").join("main.rs"), "fn main() {}").expect("code");
        let titles: Vec<String> = from_docs(dir.path())
            .roadmap
            .expect("a documented project has work in it")
            .into_iter()
            .map(|item| item.title)
            .collect();

        assert_eq!(
            titles,
            vec![
                "4. the ball falls and lands".to_string(),
                "5. the ball jumps".to_string()
            ],
            "about/goals/synopsis are fields, and non-goals and assumptions are not work"
        );
    }

    /// **A project with no code builds its layout first.** The derived roadmap
    /// opens with the scaffold, ahead of every PRD section; once there is
    /// code, it does not.
    #[test]
    fn a_new_project_derives_the_scaffold_as_item_one() {
        let dir = documented();
        let roadmap = from_docs(dir.path()).roadmap.expect("a roadmap");
        let ids: Vec<&str> = roadmap.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, [crate::scaffold::ID, "d1", "d2"]);
        assert_eq!(roadmap[0].title, crate::scaffold::TITLE);

        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src").join("lib.rs"), "").expect("code");
        let roadmap = from_docs(dir.path()).roadmap.expect("a roadmap");
        assert_eq!(roadmap[0].id, "d1", "an existing codebase has its layout");
    }

    /// **A non-goal must never become a task.** It is the one section that says
    /// what NOT to build, and an item derived from it is the harness building
    /// precisely the thing the document forbids.
    #[test]
    fn a_non_goal_is_never_turned_into_work() {
        let roadmap = from_docs(documented().path()).roadmap.unwrap_or_default();
        for item in &roadmap {
            assert!(
                !item.title.contains("non-goal") && !item.title.contains("assumption"),
                "`{}` was derived from a section that says what not to build",
                item.title
            );
        }
    }

    /// **Derived items are traceable BY CONSTRUCTION.** An item built from a
    /// section names that section, which is exactly what the gate checks — so a
    /// brief written this way can never produce the "names no section of the
    /// PRD" complaint that hand-written roadmaps do.
    #[test]
    fn every_derived_item_traces_back_to_the_document_it_came_from() {
        let dir = documented();
        let update = from_docs(dir.path());
        let brief = ProjectBrief {
            project: "x".to_string(),
            about: update.about.clone().unwrap_or_default(),
            goals: update.goals.clone().unwrap_or_default(),
            synopsis: update.synopsis.clone().unwrap_or_default(),
            prd: update.prd.clone().unwrap_or_default(),
            roadmap: update.roadmap.clone().unwrap_or_default(),
            scope: String::new(),
        };
        assert!(
            brief.untraceable_items().is_empty(),
            "derived items must trace: {:?}",
            brief.untraceable_items()
        );
        assert!(brief.is_complete(), "and the brief must be complete");
    }

    /// **No document, no guess.** The gate then asks for the brief, which is the
    /// whole point of the gate — a plausible `about` nobody wrote is worse than
    /// an absent one.
    #[test]
    fn a_project_with_no_document_yields_nothing_rather_than_a_guess() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(from_docs(dir.path()).is_empty());

        std::fs::create_dir_all(dir.path().join("docs")).expect("docs");
        std::fs::write(dir.path().join(PRD_PATH), "   \n\n").expect("write");
        assert!(
            from_docs(dir.path()).is_empty(),
            "an empty document is no document"
        );
    }

    /// A document with only a title has nothing to build from — and must not
    /// produce a roadmap of one item named after the project.
    #[test]
    fn a_document_with_only_a_title_yields_no_roadmap() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("docs")).expect("docs");
        std::fs::write(dir.path().join(PRD_PATH), "# Just A Title\n").expect("write");
        assert_eq!(from_docs(dir.path()).roadmap, None);
    }

    fn complete() -> ProjectBrief {
        ProjectBrief {
            project: "C:\\Projects\\demo".to_string(),
            about: "a demo".to_string(),
            goals: "keep it small".to_string(),
            synopsis: "a demo that stays small".to_string(),
            prd: "# Demo\n\n## Non-goals\nNo auth.\n\n## First\nDo first task.".to_string(),
            roadmap: vec![RoadmapItem {
                id: "t-1".to_string(),
                title: "first".to_string(),
                status: "todo".to_string(),
            }],
            scope: String::new(),
        }
    }

    #[test]
    fn a_complete_brief_opens_the_gate() {
        let brief = complete();
        assert!(brief.is_complete());
        assert_eq!(brief.missing(), Vec::new());
    }

    /// The refusal has to name every missing artifact, not just the first. An
    /// operator who fixes one field and is refused again learns nothing about
    /// how much is left.
    #[test]
    fn every_missing_artifact_is_named_at_once() {
        let brief = ProjectBrief {
            project: "demo".to_string(),
            prd: "# Demo".to_string(),
            ..Default::default()
        };
        assert_eq!(
            brief.missing(),
            vec![
                Artifact::About,
                Artifact::Goals,
                Artifact::Synopsis,
                Artifact::Roadmap
            ]
        );
    }

    /// Whitespace is not content. A brief whose fields hold a stray newline
    /// would otherwise pass a non-empty check while telling the proxy nothing.
    #[test]
    fn whitespace_does_not_count_as_a_filled_field() {
        let brief = ProjectBrief {
            about: "   ".to_string(),
            goals: "\n\t".to_string(),
            ..complete()
        };
        assert_eq!(brief.missing(), vec![Artifact::About, Artifact::Goals]);
    }

    #[test]
    fn a_brief_is_found_by_its_full_path_whatever_the_separators() {
        let briefs = vec![ProjectBrief {
            project: "C:\\Projects\\incord-code".to_string(),
            ..Default::default()
        }];
        let found = brief_for(&briefs, Path::new("C:/Projects/incord-code"));
        assert!(found.is_some(), "separators must not decide identity");

        let trailing = brief_for(&briefs, Path::new("C:/Projects/incord-code/"));
        assert!(
            trailing.is_some(),
            "a trailing separator must not decide it"
        );
    }

    #[test]
    fn a_brief_named_after_the_folder_is_found() {
        let briefs = vec![ProjectBrief {
            project: "Memory".to_string(),
            ..Default::default()
        }];
        assert!(brief_for(&briefs, Path::new("D:/work/Memory")).is_some());
    }

    /// Matching must not reach across projects. A brief for something else is
    /// worse than none: it would let coding start under a review context that
    /// describes a different product.
    #[test]
    fn a_brief_for_another_project_does_not_match() {
        let briefs = vec![ProjectBrief {
            project: "C:\\Projects\\other".to_string(),
            ..Default::default()
        }];
        assert!(brief_for(&briefs, Path::new("C:/Projects/incord-code")).is_none());
    }

    /// The exact payload shape the service serves, trimmed to the fields read.
    #[test]
    fn the_services_own_payload_shape_parses() {
        let body = r##"{
            "briefs": [
                {"project":"C:\\Projects\\incord-code","about":"","goals":"",
                 "synopsis":"","prd":"# PRD","roadmap":[]},
                {"project":"Memory","about":"the store","goals":"keep recall fast",
                 "synopsis":"short","prd":"# Memory",
                 "roadmap":[{"id":"r1","title":"ship","status":"done"}]}
            ],
            "mode":"decide","auto_max_severity":4,"rules":["x"]
        }"##;
        let briefs = parse_briefs(body).expect("the payload must parse");
        assert_eq!(briefs.len(), 2);
        assert_eq!(briefs[0].missing().len(), 4, "only the prd is present");
        assert!(briefs[1].is_complete());
        assert!(!briefs[1].roadmap[0].is_open(), "done is not open");
    }

    /// Unknown fields on the wire must not break the read. The service owns this
    /// shape and will add to it; a parser that fails on an addition turns every
    /// upstream improvement into an outage here.
    #[test]
    fn fields_this_crate_does_not_know_about_are_ignored() {
        let body = r#"{"briefs":[{"project":"x","about":"a","goals":"g","synopsis":"s",
            "prd":"p","roadmap":[],"prd_at":123,"handoff_on_end":true}],
            "something_added_next_year": {"nested": 1}}"#;
        let briefs = parse_briefs(body).expect("unknown fields must be ignored");
        assert_eq!(briefs[0].about, "a");
    }

    #[test]
    fn a_malformed_payload_is_reported_rather_than_panicking() {
        let error = parse_briefs("not json").expect_err("must be refused");
        assert!(error.contains("could not be read"), "got: {error}");
    }

    /// Each outcome must give a different next action, and the refusal must say
    /// which one. "It didn't work" sends an operator to the wrong place.
    #[test]
    fn each_refusal_names_what_to_do_next() {
        let incomplete = Gate::Incomplete {
            project: "demo".to_string(),
            missing: vec![Artifact::Goals],
        };
        let text = incomplete.refusal().expect("a refusal");
        assert!(text.contains("goals"), "got: {text}");
        assert!(
            text.contains("what the proxy should protect"),
            "got: {text}"
        );
        assert!(!incomplete.may_code());

        let unreachable = Gate::Unreachable {
            url: "http://127.0.0.1:9180/v1/owner/proxy".to_string(),
            reason: "connection refused".to_string(),
        };
        let text = unreachable.refusal().expect("a refusal");
        assert!(text.contains(MEMORY_URL_ENV), "got: {text}");
        assert!(!unreachable.may_code());

        let ready = Gate::Ready(Box::new(complete()));
        assert!(ready.may_code());
        assert_eq!(ready.refusal(), None);
    }

    /// **`may_code` AND "IS IT READY" MUST STAY THE SAME QUESTION.**
    ///
    /// `/run` used to decide with `matches!(check(..), Gate::Ready(_))` —
    /// true today, and not the rule. It now asks `may_code`, which is the one
    /// place the rule lives. That substitution is only correct while the two
    /// agree, so the agreement is pinned here rather than reasoned about: a
    /// variant added later that is `Ready` but must not code, or vice versa,
    /// fails this instead of silently opening the loop.
    #[test]
    fn may_code_and_being_ready_are_the_same_question() {
        let cases = [
            Gate::Ready(Box::new(complete())),
            Gate::Incomplete {
                project: "p".to_string(),
                missing: vec![],
            },
            Gate::NoBrief {
                workspace: std::path::PathBuf::from("."),
            },
            Gate::Unreachable {
                url: "u".to_string(),
                reason: "r".to_string(),
            },
        ];

        for gate in cases {
            let ready = matches!(gate, Gate::Ready(_));
            assert_eq!(
                gate.may_code(),
                ready,
                "`may_code` and `Gate::Ready` disagree for {gate:?}; \
                 the `/run` gate depends on them agreeing"
            );
        }
    }

    /// An unreachable service must not be treated as a passing check. This is
    /// the failure that would quietly disable the whole gate.
    #[test]
    fn an_unreachable_service_never_opens_the_gate() {
        let gate = Gate::Unreachable {
            url: "http://127.0.0.1:1/v1/owner/proxy".to_string(),
            reason: "refused".to_string(),
        };
        assert!(!gate.may_code(), "a failed check must never read as a pass");
    }

    fn live_shaped_config() -> Value {
        serde_json::json!({
            "briefs": [
                {"project":"Other","about":"keep me","goals":"and me","synopsis":"s",
                 "prd":"p","roadmap":[{"id":"1","title":"t","status":"done"}],"prd_at":99},
                {"project":"C:\\Projects\\incord-code","about":"","goals":"",
                 "synopsis":"","prd":"# PRD","roadmap":[]}
            ],
            "rules": ["rule one","rule two"],
            "mode": "decide",
            "auto_max_severity": 4,
            "model": "",
            "source": "cli",
            "agent": "claude"
        })
    }

    /// THE test for this write path. The endpoint replaces the entire document,
    /// so a patch that drops the owner's rules, the proxy mode or its severity
    /// ceiling would delete settings this crate does not even model — silently,
    /// and on the first save.
    #[test]
    fn patching_a_brief_preserves_every_other_key_in_the_document() {
        let mut config = live_shaped_config();
        let before = config.clone();

        patch_config(
            &mut config,
            "C:/Projects/incord-code",
            &BriefUpdate {
                about: Some("a coding harness".to_string()),
                ..Default::default()
            },
        )
        .expect("the patch applies");

        assert_eq!(
            config["rules"], before["rules"],
            "the owner's rules survive"
        );
        assert_eq!(config["mode"], before["mode"]);
        assert_eq!(config["auto_max_severity"], before["auto_max_severity"]);
        assert_eq!(config["source"], before["source"]);
        assert_eq!(config["agent"], before["agent"]);
        assert_eq!(
            config["briefs"][0], before["briefs"][0],
            "another project's brief must be untouched"
        );
        assert_eq!(config["briefs"][1]["about"], "a coding harness");
        assert_eq!(
            config["briefs"][1]["prd"], "# PRD",
            "a field not being written must keep its value"
        );
        assert_eq!(
            config["briefs"][1]["prd_at"],
            Value::Null,
            "absent keys are not invented"
        );
    }

    /// A field left as `None` is not the same as a field set to empty. Writing
    /// `about` must not blank `goals` the operator wrote a minute earlier.
    #[test]
    fn fields_not_being_written_are_left_alone() {
        let mut config = live_shaped_config();
        patch_config(
            &mut config,
            "Other",
            &BriefUpdate {
                synopsis: Some("new synopsis".to_string()),
                ..Default::default()
            },
        )
        .expect("applies");
        assert_eq!(config["briefs"][0]["about"], "keep me");
        assert_eq!(config["briefs"][0]["goals"], "and me");
        assert_eq!(config["briefs"][0]["synopsis"], "new synopsis");
    }

    #[test]
    fn a_project_with_no_brief_yet_gets_one_created() {
        let mut config = live_shaped_config();
        patch_config(
            &mut config,
            "D:/new/thing",
            &BriefUpdate {
                about: Some("brand new".to_string()),
                roadmap: Some(vec![RoadmapItem {
                    id: "r1".to_string(),
                    title: "first".to_string(),
                    status: "todo".to_string(),
                }]),
                ..Default::default()
            },
        )
        .expect("applies");

        let briefs = config["briefs"].as_array().expect("a list");
        assert_eq!(briefs.len(), 3, "appended, not replaced");
        assert_eq!(briefs[2]["project"], "D:/new/thing");
        assert_eq!(briefs[2]["roadmap"][0]["title"], "first");
    }

    /// Separators and case must not create a duplicate brief for one project.
    #[test]
    fn patching_matches_the_same_project_written_differently() {
        let mut config = live_shaped_config();
        patch_config(
            &mut config,
            "c:\\projects\\INCORD-CODE",
            &BriefUpdate {
                goals: Some("protect the loop".to_string()),
                ..Default::default()
            },
        )
        .expect("applies");

        let briefs = config["briefs"].as_array().expect("a list");
        assert_eq!(briefs.len(), 2, "no duplicate brief was appended");
        assert_eq!(briefs[1]["goals"], "protect the loop");
    }

    #[test]
    fn a_document_without_a_briefs_list_is_refused_rather_than_corrupted() {
        let mut config = serde_json::json!({"rules": ["only rules here"]});
        let error =
            patch_config(&mut config, "x", &BriefUpdate::default()).expect_err("must refuse");
        assert!(error.contains("no `briefs` list"), "got: {error}");
        assert_eq!(config["rules"][0], "only rules here", "nothing was touched");
    }

    /// Proves the real request path, not just the parsing: a served payload
    /// reaches `check` and resolves to a decision.
    /// One connection to a fake server, or `None` after 30 seconds. These
    /// servers are joined while the test holds ENV_LOCK, so a request that
    /// never comes must end the server rather than hold every env test behind
    /// it (seen on Windows CI: four tests stuck for 35 minutes).
    fn accept_within(listener: &std::net::TcpListener) -> Option<std::net::TcpStream> {
        listener.set_nonblocking(true).expect("nonblocking");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(false).expect("blocking");
                    return Some(socket);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
    }

    #[test]
    fn a_served_brief_is_read_over_http() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let Some(mut socket) = accept_within(&listener) else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf);
            let body = r#"{"briefs":[{"project":"served-demo","about":"a","goals":"g",
                "synopsis":"s","prd":"p","roadmap":[{"id":"1","title":"t","status":"todo"}]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.flush();
        });

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(MEMORY_URL_ENV, format!("http://{addr}"));
        // A complete brief is cached under the workspace, so the workspace
        // must be somewhere this test owns. A drive-letter literal is
        // RELATIVE on Linux and macOS, and wrote `D:/anywhere/...` into the
        // crate itself.
        let dir = tempfile::tempdir().expect("temp dir");
        let gate = check(&dir.path().join("served-demo"));
        std::env::remove_var(MEMORY_URL_ENV);
        let _ = server.join();

        match gate {
            Gate::Ready(brief) => assert_eq!(brief.project, "served-demo"),
            other => panic!("expected a complete brief to open the gate, got {other:?}"),
        }
    }

    /// **R18, the part a machine can check.** An item that names a heading is
    /// traceable; one that names nothing is scope that arrived without a
    /// decision, and the gate says so rather than letting it into the ledger
    /// unremarked.
    #[test]
    fn a_roadmap_item_naming_no_prd_section_is_reported() {
        let brief = ProjectBrief {
            project: "p".to_string(),
            about: "a".to_string(),
            goals: "g".to_string(),
            synopsis: "s".to_string(),
            prd: "# Product

## The intake gate

Why it exists.

## Phase 3 — routing
"
            .to_string(),
            roadmap: vec![
                RoadmapItem {
                    id: "r1".to_string(),
                    title: "Build the intake gate".to_string(),
                    status: "todo".to_string(),
                },
                RoadmapItem {
                    id: "r2".to_string(),
                    title: "Add a mascot".to_string(),
                    status: "todo".to_string(),
                },
            ],
            scope: String::new(),
        };

        let loose = brief.untraceable_items();
        assert_eq!(loose.len(), 1, "got: {loose:?}");
        assert!(loose[0].starts_with("r2:"), "got: {loose:?}");
    }

    /// A section reference the document actually uses counts as a citation.
    #[test]
    fn a_numbered_section_reference_is_traceable() {
        let brief = ProjectBrief {
            prd: "# Doc

## Phase 3 — routing
"
            .to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "Deliver phase 3".to_string(),
                status: "todo".to_string(),
            }],
            ..Default::default()
        };
        assert!(brief.untraceable_items().is_empty());
    }

    /// A marker the document never mentions is not a citation — otherwise any
    /// item could name `§99` and pass.
    #[test]
    fn a_reference_to_a_section_that_does_not_exist_is_not_traceable() {
        let brief = ProjectBrief {
            prd: "# Doc

## Phase 3 — routing
"
            .to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "Deliver phase 9".to_string(),
                status: "todo".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(brief.untraceable_items().len(), 1);
    }

    /// Items already done are history; re-litigating them helps nobody.
    #[test]
    fn a_closed_item_is_not_checked_for_traceability() {
        let brief = ProjectBrief {
            prd: "# Doc

## Something
"
            .to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "Add a mascot".to_string(),
                status: "done".to_string(),
            }],
            ..Default::default()
        };
        assert!(brief.untraceable_items().is_empty());
    }

    /// With no PRD there is nothing to trace to, and reporting every item here
    /// would bury the one thing that actually needs writing.
    #[test]
    fn a_missing_prd_is_reported_as_a_missing_prd_and_not_as_loose_items() {
        let brief = ProjectBrief {
            prd: String::new(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "Anything".to_string(),
                status: "todo".to_string(),
            }],
            ..Default::default()
        };
        assert!(brief.untraceable_items().is_empty());
        assert!(brief.missing().contains(&Artifact::Prd));
    }

    /// A complete brief with a loose item still produces work — the gate asks,
    /// it does not block.
    #[test]
    fn a_complete_brief_with_a_loose_item_still_asks_for_the_link() {
        let brief = ProjectBrief {
            project: "p".to_string(),
            about: "a".to_string(),
            goals: "g".to_string(),
            synopsis: "s".to_string(),
            prd: "# Doc

## Something
"
            .to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "Add a mascot".to_string(),
                status: "todo".to_string(),
            }],
            scope: String::new(),
        };
        let task = Gate::Ready(Box::new(brief))
            .intake_task()
            .expect("a loose item is work");
        assert!(task.contains("r1"), "got: {task}");
        assert!(task.contains("no section of the PRD"), "got: {task}");
    }

    /// A short title like "the" must not falsely match headings like "The widget"
    /// or "Theoretical model".
    #[test]
    fn short_titles_and_stop_words_do_not_falsely_match_headings() {
        let brief = ProjectBrief {
            prd: "# Doc\n\n## Theoretical model\n\n## The widget\n".to_string(),
            roadmap: vec![
                RoadmapItem {
                    id: "r1".to_string(),
                    title: "the".to_string(),
                    status: "todo".to_string(),
                },
                RoadmapItem {
                    id: "r2".to_string(),
                    title: "a".to_string(),
                    status: "todo".to_string(),
                },
            ],
            ..Default::default()
        };
        let loose = brief.untraceable_items();
        assert_eq!(
            loose.len(),
            2,
            "stop words/short titles must be loose: {loose:?}"
        );
    }

    /// `intake_task` on a complete brief with untraceable items and missing non-goals
    /// returns an instruction to complete those sections before coding.
    #[test]
    fn intake_task_exercises_loose_items_and_non_goals_on_ready_gate() {
        let mut brief = complete();
        brief.prd = "# Demo\n...".to_string();
        let ready = Gate::Ready(Box::new(brief));
        let task = ready.intake_task();
        assert!(
            task.is_some(),
            "expected intake task for loose item / missing non-goals"
        );
        let text = task.unwrap();
        assert!(
            text.contains("Non-goals"),
            "should name missing non-goals: {text}"
        );
        assert!(
            text.contains("first"),
            "should name untraceable item 'first': {text}"
        );
    }

    /// **The section everyone omits.** Without it there is nothing in the
    /// document that says "no" to work that is genuinely useful and genuinely
    /// nobody asked for.
    #[test]
    fn a_prd_with_no_non_goals_section_becomes_work() {
        let brief = ProjectBrief {
            project: "p".to_string(),
            about: "a".to_string(),
            goals: "g".to_string(),
            synopsis: "s".to_string(),
            prd: "# Doc

## Goals

Build the thing.
"
            .to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "Build the goals".to_string(),
                status: "todo".to_string(),
            }],
            scope: String::new(),
        };
        assert!(!brief.states_non_goals());
        let task = Gate::Ready(Box::new(brief))
            .intake_task()
            .expect("a missing non-goals section is work");
        assert!(task.contains("non-goals"), "got: {task}");
        assert!(task.contains("scope drift"), "say why: {task}");
    }

    /// However it is spelled. A gate that only accepted one wording would send
    /// people to write a second section they already had.
    #[test]
    fn any_reasonable_wording_of_the_heading_counts() {
        for heading in ["## Non-goals", "## Non Goals", "### NOT BUILDING in v1"] {
            let brief = ProjectBrief {
                prd: format!(
                    "# Doc

{heading}

No auth in v1.
"
                ),
                ..Default::default()
            };
            assert!(
                brief.states_non_goals(),
                "`{heading}` must satisfy the check"
            );
        }
    }

    /// The words in a paragraph are not a section. A document that mentions
    /// non-goals in passing has not decided any.
    #[test]
    fn a_passing_mention_is_not_a_non_goals_section() {
        let brief = ProjectBrief {
            prd: "# Doc

We should write non-goals one day.
"
            .to_string(),
            ..Default::default()
        };
        assert!(!brief.states_non_goals());
    }

    /// With both in place the gate is quiet — it must not nag a finished brief.
    #[test]
    fn a_brief_with_non_goals_and_traceable_items_asks_for_nothing() {
        let brief = ProjectBrief {
            project: "p".to_string(),
            about: "a".to_string(),
            goals: "g".to_string(),
            synopsis: "s".to_string(),
            prd: "# Doc

## Non-goals

No auth in v1.

## The widget
"
            .to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "Build the widget".to_string(),
                status: "todo".to_string(),
            }],
            scope: String::new(),
        };
        assert!(Gate::Ready(Box::new(brief)).intake_task().is_none());
    }

    /// **The pressure valve on the questioning phase.** Past seven questions a
    /// person stops answering and starts agreeing, so everything else is decided
    /// and written down for one skim.
    #[test]
    fn the_intake_caps_questions_and_says_what_to_do_with_the_rest() {
        let method = intake_method();
        assert!(
            method.contains(&MAX_INTAKE_QUESTIONS.to_string()),
            "the cap must be stated as a number: {method}"
        );
        assert!(method.contains("ONE batch"), "got: {method}");
        assert!(
            method.contains("## Assumptions"),
            "the overflow needs somewhere to go: {method}"
        );
        assert!(
            method.contains("with its reasoning"),
            "an assumption without its reasoning cannot be corrected: {method}"
        );
    }

    /// **Every question arrives with an answer to agree to.** Still one batch
    /// and still capped; the recommendation is marked the way the question
    /// parser reads it, and it is what stands when nobody is there to answer.
    #[test]
    fn every_intake_question_carries_a_recommended_answer() {
        let method = intake_method();
        assert!(method.contains("RECOMMENDED ANSWER"), "{method}");
        assert!(
            method.contains(crate::ask::RECOMMENDED),
            "the mark must be the one `ask::parse` reads: {method}"
        );
        assert!(method.contains("recommended: true"), "{method}");
        assert!(method.contains("your recommendations stand"), "{method}");
        assert!(method.contains("ONE batch"), "{method}");
    }

    /// The research standards and the non-goals section ride the same task, so a
    /// session gathering a brief is told all of it at once rather than being
    /// corrected one rule at a time.
    #[test]
    fn the_gathering_task_carries_the_standards_it_will_be_held_to() {
        let task = Gate::NoBrief {
            workspace: PathBuf::from("/tmp/x"),
        }
        .intake_task()
        .expect("no brief is work");
        assert!(task.contains("Non-goals"), "got: {task}");
        assert!(task.contains("free or self-hostable"), "got: {task}");
        assert!(
            task.contains("never state a version") || task.contains("Never state a version"),
            "the standard that forbids invented answers must be present: {task}"
        );
    }

    #[test]
    fn parse_save_validates_strict_schema_and_roadmap_rules() {
        // Valid save with all fields
        let valid_json = serde_json::json!({
            "about": "An awesome project",
            "goals": "Deliver high quality code",
            "synopsis": "A quick summary",
            "prd": "# PRD\n\n## Non-goals\nNone",
            "roadmap": [
                {
                    "id": "item-1",
                    "title": "First feature",
                    "status": "todo"
                },
                {
                    "id": "item-2",
                    "title": "Second feature",
                    "status": "doing"
                },
                {
                    "title": "Third feature without id",
                    "status": "done"
                }
            ]
        });
        let update = parse_save(&valid_json.to_string()).expect("valid save payload");
        assert_eq!(update.about.as_deref(), Some("An awesome project"));
        assert_eq!(update.goals.as_deref(), Some("Deliver high quality code"));
        assert_eq!(update.synopsis.as_deref(), Some("A quick summary"));
        assert_eq!(update.prd.as_deref(), Some("# PRD\n\n## Non-goals\nNone"));
        assert_eq!(update.roadmap.as_ref().map(|r| r.len()), Some(3));

        // Rejection: top-level non-object
        let err_array = parse_save("[]").expect_err("array should be rejected");
        assert!(err_array.contains("needs a JSON object"));

        // Rejection: unknown top-level field
        let unknown_top = serde_json::json!({
            "about": "valid",
            "extra_field": "not allowed"
        });
        let err_top = parse_save(&unknown_top.to_string()).expect_err("unknown top-level field");
        assert!(err_top.contains("unknown field `extra_field`"));

        // Rejection: non-string value for text field
        let non_str = serde_json::json!({
            "about": 123
        });
        let err_str = parse_save(&non_str.to_string()).expect_err("non-string field");
        assert!(err_str.contains("`about` must be a string"));

        // Rejection: duplicate roadmap item IDs
        let dup_ids = serde_json::json!({
            "roadmap": [
                {"id": "task-1", "title": "First", "status": "todo"},
                {"id": "TASK-1", "title": "Second", "status": "doing"}
            ]
        });
        let err_dup = parse_save(&dup_ids.to_string()).expect_err("duplicate roadmap id");
        assert!(err_dup.contains("duplicate roadmap item id"));

        // Rejection: invalid roadmap status
        let bad_status = serde_json::json!({
            "roadmap": [
                {"id": "task-1", "title": "First", "status": "in_progress"}
            ]
        });
        let err_status = parse_save(&bad_status.to_string()).expect_err("invalid roadmap status");
        assert!(err_status.contains("invalid roadmap status `in_progress`"));

        // Rejection: missing or empty title in roadmap item
        let empty_title = serde_json::json!({
            "roadmap": [
                {"id": "task-1", "title": "   ", "status": "todo"}
            ]
        });
        let err_title = parse_save(&empty_title.to_string()).expect_err("empty title");
        assert!(err_title.contains("missing required non-empty `title`"));

        // Rejection: unknown field in roadmap item
        let unknown_item_field = serde_json::json!({
            "roadmap": [
                {"id": "task-1", "title": "First", "status": "todo", "priority": "high"}
            ]
        });
        let err_item_k =
            parse_save(&unknown_item_field.to_string()).expect_err("unknown item field");
        assert!(err_item_k.contains("unknown field `priority` in roadmap item"));

        // Rejection: empty update
        let empty_obj = serde_json::json!({});
        let err_empty = parse_save(&empty_obj.to_string()).expect_err("empty payload");
        assert!(err_empty.contains("nothing to save"));
    }

    #[test]
    fn canonical_project_for_resolves_path_or_folder() {
        let briefs = vec![
            ProjectBrief {
                project: "C:/Projects/incord-code".to_string(),
                ..Default::default()
            },
            ProjectBrief {
                project: "my-service".to_string(),
                ..Default::default()
            },
        ];

        // Matches exact path (even with different separators / case)
        assert_eq!(
            canonical_project_for(&briefs, Path::new("c:\\projects\\INCORD-CODE")),
            "C:/Projects/incord-code"
        );

        // Matches folder name
        assert_eq!(
            canonical_project_for(&briefs, Path::new("D:/somewhere/my-service")),
            "my-service"
        );

        // Fallback to workspace path display when not found
        assert_eq!(
            canonical_project_for(&briefs, Path::new("C:/Projects/unknown")),
            "C:/Projects/unknown"
        );
    }

    #[test]
    fn fields_not_stored_detects_value_overwrites_and_missing_roadmap_items() {
        let update = BriefUpdate {
            about: Some("new about".to_string()),
            roadmap: Some(vec![RoadmapItem {
                id: "r1".to_string(),
                title: "item one".to_string(),
                status: "todo".to_string(),
            }]),
            ..Default::default()
        };

        // Saved has different text for `about`: must report lost!
        let overwritten = ProjectBrief {
            project: "p".to_string(),
            about: "old about that was not updated".to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "item one".to_string(),
                status: "todo".to_string(),
            }],
            ..Default::default()
        };
        let lost = fields_not_stored(&update, &overwritten);
        assert_eq!(lost, vec!["about"]);

        // Saved has different status for roadmap item: must report lost!
        let altered_roadmap = ProjectBrief {
            project: "p".to_string(),
            about: "new about".to_string(),
            roadmap: vec![RoadmapItem {
                id: "r1".to_string(),
                title: "item one".to_string(),
                status: "done".to_string(),
            }],
            ..Default::default()
        };
        let lost_rm = fields_not_stored(&update, &altered_roadmap);
        assert_eq!(lost_rm, vec!["roadmap"]);
    }

    #[test]
    fn patch_config_resolves_project_by_folder_name_to_avoid_duplicates() {
        let mut config = serde_json::json!({
            "briefs": [
                {
                    "project": "my-tool",
                    "about": "original about"
                }
            ]
        });

        // Patching with full path when entry is folder name
        patch_config(
            &mut config,
            "C:/code/my-tool",
            &BriefUpdate {
                about: Some("updated about".to_string()),
                ..Default::default()
            },
        )
        .expect("patch applies");

        let briefs = config["briefs"].as_array().expect("array");
        assert_eq!(briefs.len(), 1, "must not create a duplicate entry");
        assert_eq!(briefs[0]["about"], "updated about");
    }

    #[test]
    fn post_with_etag_handles_precondition_failed_cas_conflict() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let Some(mut socket) = accept_within(&listener) else {
                return;
            };
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.to_lowercase().contains("if-match: \"rev-1\""),
                "If-Match header must be sent by post_with_etag: {request}"
            );
            // Simulate 412 Precondition Failed
            let response = "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\n\r\n";
            socket.write_all(response.as_bytes()).expect("write");
            socket.flush().expect("flush");
        });

        let url = format!("http://{addr}/test");
        let result = post_with_etag(&url, "{}".to_string(), Some("\"rev-1\""));
        let _ = server.join();

        assert_eq!(
            result,
            Ok(false),
            "412 status must return Ok(false) for CAS retry"
        );
    }

    /// **AN OUTAGE MUST NOT UNDO A FINISHED INTAKE.**
    ///
    /// With the store unreachable, `intake_task` told every session to
    /// complete the brief before writing code — including projects whose
    /// brief had been finished for months. A service that holds a DOCUMENT
    /// going down was sending every project on the machine back to intake.
    ///
    /// The cache is honest because the only route to editing a brief is
    /// `save_brief`, which needs the same service that is not answering.
    #[test]
    fn a_complete_project_keeps_working_while_the_store_is_down() {
        let dir = tempfile::tempdir().expect("temp dir");

        // A brief good enough to code from, as the last successful check
        // would have left it.
        let brief = ProjectBrief {
            project: "proj-x".to_string(),
            about: "a thing".to_string(),
            goals: "work".to_string(),
            synopsis: "does the thing".to_string(),
            // The roadmap item names a section, or the intake rightly objects
            // that scope arrived without a decision behind it.
            prd: "# PRD\n\n## the first thing\n\nWhat it is.\n\n## Non-goals\n\nNot this.\n"
                .to_string(),
            roadmap: vec![RoadmapItem {
                id: "r-1".to_string(),
                title: "the first thing".to_string(),
                status: "todo".to_string(),
            }],
            ..Default::default()
        };
        assert!(
            brief.missing().is_empty(),
            "the fixture must be a COMPLETE brief: {:?}",
            brief.missing()
        );
        remember_locally(dir.path(), &brief);

        // Nothing is listening there.
        std::env::set_var(MEMORY_URL_ENV, "http://127.0.0.1:9/");
        let gate = check(dir.path());
        std::env::remove_var(MEMORY_URL_ENV);

        match &gate {
            Gate::Ready(found) => assert_eq!(found.project, "proj-x"),
            other => panic!("a cached brief was ignored while the store was down: {other:?}"),
        }
        assert!(
            gate.may_code(),
            "a finished project was sent back to intake by an outage: {:?}",
            gate.intake_task()
        );
    }

    /// **NO CACHE MEANS THE OUTAGE IS REPORTED, NOT PAPERED OVER.**
    ///
    /// With nothing cached there is genuinely nowhere to read a brief from
    /// and nowhere to save one to, so the original message is the right one.
    #[test]
    fn an_outage_with_nothing_cached_still_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::env::set_var(MEMORY_URL_ENV, "http://127.0.0.1:9/");
        let gate = check(dir.path());
        std::env::remove_var(MEMORY_URL_ENV);

        assert!(matches!(gate, Gate::Unreachable { .. }), "got: {gate:?}");
        assert!(!gate.may_code());
    }

    /// **A HALF-FINISHED BRIEF IS NEVER CACHED.**
    ///
    /// Falling back to one would skip the intake it is in the middle of —
    /// the project would look ready on the strength of a document that is
    /// still being written.
    #[test]
    fn an_incomplete_brief_is_not_used_as_a_fallback() {
        let dir = tempfile::tempdir().expect("temp dir");
        remember_locally(
            dir.path(),
            &ProjectBrief {
                project: "half".to_string(),
                about: "started".to_string(),
                ..Default::default()
            },
        );

        std::env::set_var(MEMORY_URL_ENV, "http://127.0.0.1:9/");
        let gate = check(dir.path());
        std::env::remove_var(MEMORY_URL_ENV);

        assert!(
            matches!(gate, Gate::Unreachable { .. }),
            "an unfinished brief was treated as good enough: {gate:?}"
        );
    }
}
