//! The channel agents talk to each other on.
//!
//! # The failure this exists for
//!
//! Work is split by role and handed to different models. One is given the
//! login flow, another the schema underneath it — and the login flow cannot be
//! written without knowing what the schema calls a user. Today the second agent
//! guesses, and the guess is found at integration, by which time the fix
//! touches both and whichever agent wrote the first has long since finished.
//!
//! [`crate::contracts`] freezes the shapes that CAN be written down before the
//! split. This is for everything else: the question that only occurs to someone
//! once they are inside the work.
//!
//! # Why a file, and not a socket or a queue
//!
//! Same reason as [`crate::desktop::journal`], and it is the same constraint.
//! An agent is a SUBPROCESS with its own loop; the harness does not share
//! memory with it, and there may be no second agent alive at the moment a
//! question is asked. A file is the one medium every participant can reach,
//! and it keeps the conversation when nobody is listening yet.
//!
//! # Nobody is blocked waiting for an answer
//!
//! The obvious design — ask, and wait — deadlocks the moment the only other
//! participant is the agent doing the waiting, which in a sequential run is
//! always. So an ask RECORDS and returns. The question stays open in the
//! channel, [`Channel::open_questions`] finds it, and the orchestrator puts it
//! in front of whoever can answer. A question nobody can answer surfaces as an
//! unanswered question, which is a thing an operator can see and act on — not
//! as a hung run.
//!
//! # The sequence number is the cursor
//!
//! As in the desktop journal: the file is trimmed, trimming moves every later
//! record, and a reader holding a byte offset would resume mid-line or replay.
//! Each message carries a monotonic [`Message::seq`] instead.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the channel lives inside a workspace.
pub const FILE: &str = ".incord/team.jsonl";

/// How much conversation to keep before the oldest is dropped.
pub const MAX_BYTES: u64 = 256 * 1024;

/// How many recent messages an agent is shown.
///
/// A whole run's conversation in every prompt is the context cost the per-task
/// session clearing exists to avoid, re-introduced through the back door.
pub const SHOWN: usize = 12;

/// The orchestrator's name on the channel.
///
/// Not an agent fingerprint: the main agent is the harness itself, and giving
/// it an identity that looks like a worker's would let a reader think some
/// model answered when the answer came from the loop.
pub const ORCHESTRATOR: &str = "orchestrator";

/// The person's name on the channel, when they answer from the interface.
///
/// It carries no extra weight: the channel file lives in the workspace, where
/// an agent can write too, so a message signed `owner` is information like any
/// other — never an instruction that outranks the task.
pub const OWNER: &str = "owner";

/// Everyone.
pub const EVERYONE: &str = "all";

/// The channel path for a workspace.
#[must_use]
pub fn path_in(workspace: &Path) -> PathBuf {
    workspace.join(FILE)
}

/// What kind of message this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Something said, expecting nothing back.
    Note,
    /// A question. Open until something answers it.
    Question,
    /// An answer to the question with this sequence number.
    Answer { to: u64 },
}

/// One thing said on the channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub seq: u64,
    /// RFC 3339, UTC — the same spelling the rest of the harness uses.
    pub at: String,
    /// Who said it: an agent fingerprint, or [`ORCHESTRATOR`].
    pub from: String,
    /// Who it is for: a fingerprint, a role name, or [`EVERYONE`].
    pub to: String,
    pub kind: Kind,
    /// The task being worked when it was said, so a reader can tell which
    /// piece of the plan a question came out of.
    pub task: Option<String>,
    pub text: String,
}

impl Message {
    /// Whether this message is addressed to `who`.
    #[must_use]
    pub fn reaches(&self, who: &str) -> bool {
        self.to == EVERYONE || self.to.eq_ignore_ascii_case(who)
    }

    /// One line, for a prompt or a report.
    #[must_use]
    pub fn line(&self) -> String {
        let marker = match self.kind {
            Kind::Note => String::new(),
            Kind::Question => " [question]".to_string(),
            Kind::Answer { to } => format!(" [answer to #{to}]"),
        };
        let addressed = if self.to == EVERYONE {
            String::new()
        } else {
            format!(" -> {}", self.to)
        };
        format!(
            "#{} {}{addressed}{marker}: {}",
            self.seq, self.from, self.text
        )
    }
}

/// The team's conversation, on disk.
#[derive(Debug, Clone)]
pub struct Channel {
    path: PathBuf,
    next_seq: u64,
    /// The file as this handle left it after its own last write — size and
    /// modified time. Different at the next write means someone else wrote.
    left_as: Option<(u64, std::time::SystemTime)>,
}

/// Size and modified time of the channel file, the cheap way: no open, so no
/// read of the contents.
fn signature(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

impl Channel {
    /// Open (or create) the channel at `path`.
    ///
    /// Resumes numbering after whatever is there, so a restarted harness does
    /// not hand out sequence numbers that answers already refer to.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        let next_seq = read_all(&path).last().map_or(0, |m| m.seq + 1);
        Self {
            path,
            next_seq,
            left_as: None,
        }
    }

    /// The channel for a workspace.
    #[must_use]
    pub fn in_workspace(workspace: &Path) -> Self {
        Self::at(path_in(workspace))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Say something. Returns the message as recorded, with its number.
    ///
    /// # Errors
    ///
    /// The underlying I/O error. A caller treats this as non-fatal: losing a
    /// message is bad, losing the WORK because a message could not be written
    /// would be worse.
    pub fn say(
        &mut self,
        from: &str,
        to: &str,
        kind: Kind,
        task: Option<&str>,
        text: &str,
    ) -> std::io::Result<Message> {
        // Numbered from the FILE when anyone else has written to it: every
        // agent's team server, the worker and `--serve` each hold their own
        // `Channel`, and a number read once at open was handed out twice.
        //
        // Read only when the file changed since this handle's own last write.
        // Reading it on every message was measured at ~36 ms a write on
        // Windows once the file neared its cap — opening a freshly written
        // file for reading is scanned — against microseconds for this check.
        //
        // What remains is the moment between the read and the append below,
        // which a file lock would close — `File::lock` needs Rust 1.89 and this
        // crate's floor is 1.88.
        if signature(&self.path) != self.left_as {
            self.next_seq = self.next_seq.max(next_seq_in(&self.path));
        }
        let message = Message {
            seq: self.next_seq,
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            from: from.to_string(),
            to: if to.trim().is_empty() {
                EVERYONE.to_string()
            } else {
                to.trim().to_string()
            },
            kind,
            task: task.map(str::to_string),
            text: text.trim().to_string(),
        };

        // Trimmed BEFORE the append, so the file is never over the cap between
        // the two operations.
        self.trim()?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(&message).unwrap_or_default();
        line.push('\n');
        // One write of one whole line, so a reader polling mid-append sees the
        // line or nothing, never half of it.
        file.write_all(line.as_bytes())?;
        drop(file);

        self.next_seq += 1;
        self.left_as = signature(&self.path);
        Ok(message)
    }

    /// Answer message `#question` as `from`.
    ///
    /// The one place the rule lives, for an agent's `team_answer` and for the
    /// person answering from the interface alike: an answer filed against a
    /// message that does not exist would leave the real question open AND read
    /// as settled, which is worse than not answering.
    ///
    /// # Errors
    ///
    /// Empty text, no such message, or the answer could not be written — each
    /// in words that say nothing was recorded.
    pub fn answer(&mut self, from: &str, question: u64, text: &str) -> Result<Message, String> {
        if text.trim().is_empty() {
            return Err("`text` is required: there is no answer to send".to_string());
        }
        if !self.all().iter().any(|m| m.seq == question) {
            return Err(format!(
                "there is no message #{question} on this channel, so nothing was recorded"
            ));
        }
        self.say(from, EVERYONE, Kind::Answer { to: question }, None, text)
            .map_err(|e| format!("the answer could not be written: {e}"))
    }

    /// Everything said, oldest first.
    #[must_use]
    pub fn all(&self) -> Vec<Message> {
        read_all(&self.path)
    }

    /// Messages after `cursor` that reach `who`, oldest first.
    #[must_use]
    pub fn unread_for(&self, who: &str, cursor: Option<u64>) -> Vec<Message> {
        read_all(&self.path)
            .into_iter()
            .filter(|m| cursor.is_none_or(|last| m.seq > last))
            // Never your own words back. An agent shown its own question as
            // unread reads it as someone else asking, and answers itself.
            .filter(|m| !m.from.eq_ignore_ascii_case(who))
            .filter(|m| m.reaches(who))
            .collect()
    }

    /// Open questions `who` could answer, oldest first.
    ///
    /// # Why this ignores the cursor
    ///
    /// [`Self::unread_for`] shows each message once, which is right for
    /// everything said — nobody wants the same note three times. It is wrong
    /// for an open question, and that is how the channel used to lose them: a
    /// question asked while one agent was mid-turn is already behind that
    /// agent's cursor by the time it reads again, so it is shown to nobody
    /// ever again and stays open forever.
    ///
    /// A question is different from a note precisely because it is not done
    /// with until somebody answers it. So it keeps being put in front of
    /// whoever could, and stops the moment an answer arrives.
    #[must_use]
    pub fn open_for(&self, who: &str) -> Vec<Message> {
        self.open_questions()
            .into_iter()
            .filter(|m| !m.from.eq_ignore_ascii_case(who))
            .filter(|m| m.reaches(who))
            .collect()
    }

    /// What to put in front of `who`: messages after `cursor`, plus every
    /// question still open that `who` could answer, oldest first, each once.
    ///
    /// The one definition of "what is waiting for you", used by the worker's
    /// briefing and by `team_read` mid-turn. The second is not a duplicate of
    /// the first: a question asked while `who` was mid-turn is already behind
    /// its cursor, so unread messages alone is how a question gets asked once
    /// and seen by nobody ([`Self::open_for`]).
    #[must_use]
    pub fn waiting_for(&self, who: &str, cursor: Option<u64>) -> Vec<Message> {
        let mut waiting = self.unread_for(who, cursor);
        for question in self.open_for(who) {
            if !waiting.iter().any(|m| m.seq == question.seq) {
                waiting.push(question);
            }
        }
        waiting.sort_by_key(|m| m.seq);
        waiting
    }

    /// Questions nothing has answered yet, oldest first.
    ///
    /// The orchestrator's list. A question that no agent can answer is not a
    /// hung run here — it is an entry on this list, which is a thing a person
    /// can be shown.
    #[must_use]
    pub fn open_questions(&self) -> Vec<Message> {
        let all = read_all(&self.path);
        let answered: Vec<u64> = all
            .iter()
            .filter_map(|m| match m.kind {
                Kind::Answer { to } => Some(to),
                _ => None,
            })
            .collect();
        all.into_iter()
            .filter(|m| m.kind == Kind::Question && !answered.contains(&m.seq))
            .collect()
    }

    /// Drop the oldest messages if the file has grown past [`MAX_BYTES`].
    ///
    /// Keeps the newest half, so the rewrite happens once per half-cap of
    /// talking rather than on every message once the cap is reached.
    fn trim(&self) -> std::io::Result<()> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Ok(());
        };
        if meta.len() <= MAX_BYTES {
            return Ok(());
        }
        let text = std::fs::read_to_string(&self.path)?;
        let lines: Vec<&str> = text.lines().collect();
        let mut kept = lines.split_at(lines.len() / 2).1.join("\n");
        if !kept.is_empty() {
            kept.push('\n');
        }
        std::fs::write(&self.path, kept)
    }
}

/// Every message in a channel file, oldest first.
///
/// An unparseable line is SKIPPED, not fatal: a half-written last line during
/// an append must not blank the conversation.
/// The number the next message in the file should get, reading only its end.
///
/// Called before every write, so it must not parse the whole file: a channel
/// near [`MAX_BYTES`] is thousands of lines, and parsing all of them on each
/// message made filling one quadratic. The tail holds the newest line; a line
/// longer than the tail (one enormous message) falls back to a full read.
fn next_seq_in(path: &Path) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 16 * 1024;

    let whole = || read_all(path).last().map_or(0, |m| m.seq + 1);
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL);
    let mut tail = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut tail).is_err() {
        return whole();
    }
    let text = String::from_utf8_lossy(&tail);
    // Reading from the middle cuts the first line; it is not a message.
    let newest = text
        .lines()
        .skip(usize::from(start > 0))
        .filter_map(|line| serde_json::from_str::<Message>(line).ok())
        .map(|m| m.seq)
        .max();
    match newest {
        Some(seq) => seq + 1,
        None if start > 0 => whole(),
        None => 0,
    }
}

fn read_all(path: &Path) -> Vec<Message> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Message>(line).ok())
        .collect()
}

/// What an agent is told it may say, and how.
///
/// Tagged blocks in the reply, the same shape [`crate::plan`] already uses to
/// get structured output out of an agent — one format for the whole harness
/// rather than a second one to learn.
#[must_use]
pub fn prompt_block(for_agent: &str) -> String {
    format!(
        "<team>\nYou are working alongside other agents on the same project. \
         To reach them, put one of these on its own line in your reply:\n\n\
         <ask to=\"NAME\">the question</ask>   — you need something only they know\n\
         <tell to=\"NAME\">the fact</tell>     — they will need this and do not know it\n\
         <answer to=\"#N\">the answer</answer> — you are answering question #N\n\n\
         NAME must be one of: {} — or `{EVERYONE}` for everyone. \
         You are `{for_agent}`. A name that is not on that list is sent to \
         everyone instead, because a message addressed to nobody is a question \
         that is never answered and never missed.\n\n\
         An ask does NOT pause you and no reply arrives during this turn — \
         record what you assumed, keep working, and the answer reaches whoever \
         needs it next. Ask when a WRONG GUESS would be expensive to undo: what \
         a field is called, which side owns a check, what an error should do. \
         Do not narrate here.\n</team>",
        crate::roles::KNOWN_ROLES.join(", ")
    )
}

/// Whether `name` is somebody the channel can actually deliver to.
///
/// Roles are a CLOSED vocabulary ([`crate::roles::KNOWN_ROLES`]) because the
/// planner picks from it, so a name outside it belongs to nobody. An agent
/// asked to name a recipient will invent a plausible one — `schema`, `auth` —
/// and a message addressed to an invented name is lost in the one way that
/// looks like success from both ends: the asker believes it asked, and nobody
/// was ever going to answer.
#[must_use]
pub fn deliverable(name: &str) -> bool {
    name.eq_ignore_ascii_case(EVERYONE)
        || name.eq_ignore_ascii_case(ORCHESTRATOR)
        || crate::roles::KNOWN_ROLES
            .iter()
            .any(|role| role.eq_ignore_ascii_case(name))
}

/// What an agent is shown of the conversation.
///
/// `None` when there is nothing for them, so a quiet channel costs no prompt.
#[must_use]
pub fn unread_block(messages: &[Message]) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let shown: Vec<&Message> = messages.iter().rev().take(SHOWN).rev().collect();
    let mut out = String::from("<team-messages>\n");
    if messages.len() > shown.len() {
        out.push_str(&format!(
            "({} older message(s) not shown)\n",
            messages.len() - shown.len()
        ));
    }
    for message in shown {
        out.push_str(&message.line());
        out.push('\n');
    }
    out.push_str(
        "Answer any question addressed to you with <answer to=\"#N\">, in the \
         same reply as your work.\n</team-messages>",
    );
    Some(out)
}

/// One thing an agent asked to say, read out of its reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    pub to: String,
    pub kind: Kind,
    pub text: String,
}

/// The `<ask>`, `<tell>` and `<answer>` blocks in an agent's reply.
///
/// Malformed blocks are skipped rather than failing the reply: the agent's
/// WORK is the point, and a mistyped tag must not turn a finished task into an
/// error. What it costs is a message nobody receives, which is the same
/// outcome as not having asked.
#[must_use]
pub fn parse_directives(reply: &str) -> Vec<Directive> {
    let mut out = Vec::new();
    for (tag, make) in [("ask", 0_u8), ("tell", 1), ("answer", 2)] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut rest = reply;
        while let Some(start) = rest.find(&open) {
            let after = &rest[start + open.len()..];
            let Some(head_end) = after.find('>') else {
                break;
            };
            let attrs = &after[..head_end];
            let body_start = head_end + 1;
            let Some(body_len) = after[body_start..].find(&close) else {
                break;
            };
            let body = &after[body_start..body_start + body_len];
            rest = &after[body_start + body_len + close.len()..];

            let Some(to) = attribute(attrs, "to") else {
                continue;
            };
            let text = body.trim();
            if text.is_empty() {
                continue;
            }

            let kind = match make {
                0 => Kind::Question,
                1 => Kind::Note,
                _ => {
                    // `to="#7"` on an answer names the question, not a
                    // recipient. A malformed number is the one case worth
                    // dropping outright: an answer filed against the wrong
                    // question leaves the real one open AND misleads.
                    let Ok(n) = to.trim().trim_start_matches('#').parse::<u64>() else {
                        continue;
                    };
                    Kind::Answer { to: n }
                }
            };
            // An answer goes to everyone: it cites the question by number, and
            // whoever else was stuck on the same thing benefits from seeing it.
            // A name nobody has becomes a broadcast rather than a message into
            // the void — see [`deliverable`].
            let to = if matches!(kind, Kind::Answer { .. }) || !deliverable(&to) {
                EVERYONE.to_string()
            } else {
                to
            };
            out.push(Directive {
                to,
                kind,
                text: text.to_string(),
            });
        }
    }
    out
}

/// The value of `name="..."` in a tag's attributes.
fn attribute(attrs: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let start = attrs.find(&key)? + key.len();
    let rest = attrs[start..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body = &rest[1..];
    let end = body.find(quote)?;
    let value = body[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_channel(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "incord-team-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(FILE)
    }

    #[test]
    fn a_message_round_trips_and_is_addressed() {
        let mut channel = Channel::at(temp_channel("roundtrip"));
        channel
            .say(
                "implement",
                "review",
                Kind::Note,
                Some("t-1"),
                "the token is a UUID",
            )
            .expect("say");

        let all = channel.all();
        assert_eq!(all.len(), 1);
        assert!(all[0].reaches("review"));
        assert!(all[0].reaches("REVIEW"), "addressing is not case-sensitive");
        assert!(!all[0].reaches("explore"), "it was not sent to explore");
    }

    /// **THE POINT OF THE CHANNEL.**
    ///
    /// The schema agent knows what the column is called; the login agent has
    /// to guess without it. This is that exchange, end to end.
    #[test]
    fn one_agent_asks_and_another_answers() {
        let mut channel = Channel::at(temp_channel("qa"));
        let asked = channel
            .say(
                "implement",
                "schema",
                Kind::Question,
                Some("t-2"),
                "what does the users table call the login id?",
            )
            .expect("ask");

        // Open until answered — that is what makes it findable.
        assert_eq!(channel.open_questions().len(), 1);

        // The other agent sees it, because it is addressed to them.
        let unread = channel.unread_for("schema", None);
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].seq, asked.seq);

        channel
            .say(
                "schema",
                EVERYONE,
                Kind::Answer { to: asked.seq },
                Some("t-3"),
                "it is `email`, and it is unique",
            )
            .expect("answer");

        assert!(
            channel.open_questions().is_empty(),
            "an answered question must stop being open"
        );
        let back = channel.unread_for("implement", Some(asked.seq));
        assert_eq!(back.len(), 1);
        assert!(back[0].text.contains("email"));
    }

    /// **TWO WRITERS ON ONE CHANNEL NEVER HAND OUT THE SAME NUMBER.**
    ///
    /// Each agent's team server, the worker and `--serve` all hold their own
    /// `Channel` on the same file. Numbering was read once, at open, so two of
    /// them issued the same `#n` — and an answer names its question by number.
    #[test]
    fn two_open_channels_do_not_reuse_a_number() {
        let path = temp_channel("two-writers");
        let mut first = Channel::at(path.clone());
        let mut second = Channel::at(path.clone());

        let a = first
            .say("implement", EVERYONE, Kind::Note, None, "from the first")
            .expect("say");
        let b = second
            .say("review", EVERYONE, Kind::Note, None, "from the second")
            .expect("say");
        let c = first
            .say("implement", EVERYONE, Kind::Note, None, "the first again")
            .expect("say");

        assert_ne!(a.seq, b.seq, "two writers issued #{} twice", a.seq);
        assert!(c.seq > b.seq, "#{} was issued after #{}", c.seq, b.seq);
    }

    /// **AN AGENT MUST NOT BE SHOWN ITS OWN WORDS AS INCOMING.**
    ///
    /// A broadcast reaches everyone, including whoever sent it. Handed back as
    /// unread it reads as someone else asking — and the agent answers itself,
    /// closing a question nobody with the answer ever saw.
    #[test]
    fn your_own_broadcast_does_not_come_back_to_you() {
        let mut channel = Channel::at(temp_channel("echo"));
        channel
            .say(
                "implement",
                EVERYONE,
                Kind::Question,
                None,
                "who owns retries?",
            )
            .expect("say");

        assert!(
            channel.unread_for("implement", None).is_empty(),
            "the sender was handed its own question"
        );
        assert_eq!(
            channel.unread_for("review", None).len(),
            1,
            "everyone else must still get it"
        );
    }

    /// The cursor is a sequence number, so a trim cannot make a reader replay.
    #[test]
    fn a_reader_sees_each_message_once_even_after_a_trim() {
        let path = temp_channel("cursor");
        let mut channel = Channel::at(path.clone());
        channel
            .say("a", EVERYONE, Kind::Note, None, "first")
            .expect("say");
        let cursor = channel.all().last().expect("one").seq;
        assert!(channel.unread_for("b", Some(cursor)).is_empty());

        let bulk = "x".repeat(512);
        for _ in 0..800 {
            channel
                .say("a", EVERYONE, Kind::Note, None, &bulk)
                .expect("say");
        }

        let size = std::fs::metadata(&path).expect("exists").len();
        assert!(size <= MAX_BYTES, "the channel grew to {size} bytes");

        let after = channel.unread_for("b", Some(cursor));
        assert!(!after.is_empty(), "the trim took everything");
        let mut previous = cursor;
        for message in &after {
            assert!(message.seq > previous, "sequence went backwards");
            previous = message.seq;
        }
    }

    /// A restarted harness must not reuse numbers an answer already cites.
    #[test]
    fn a_reopened_channel_continues_the_sequence() {
        let path = temp_channel("restart");
        let mut first = Channel::at(path.clone());
        let last = first
            .say("a", EVERYONE, Kind::Question, None, "q")
            .expect("say")
            .seq;
        drop(first);

        let mut again = Channel::at(path);
        let next = again
            .say("b", EVERYONE, Kind::Note, None, "n")
            .expect("say");
        assert!(next.seq > last, "restarted at {} after {last}", next.seq);
    }

    #[test]
    fn directives_are_read_out_of_a_reply() {
        // `r##` because the body contains `"#`, which would end an `r#` string.
        let reply = r##"I changed the handler.

<ask to="review">what is the login column called?</ask>
<tell to="all">the token expiry is now 15 minutes</tell>
<answer to="#4">yes, the client retries once</answer>

Done."##;

        let found = parse_directives(reply);
        assert_eq!(found.len(), 3, "got: {found:?}");

        let question = found
            .iter()
            .find(|d| d.kind == Kind::Question)
            .expect("an ask");
        assert_eq!(question.to, "review");
        assert!(question.text.contains("login column"));

        let note = found.iter().find(|d| d.kind == Kind::Note).expect("a tell");
        assert_eq!(note.to, EVERYONE);

        let answer = found
            .iter()
            .find(|d| matches!(d.kind, Kind::Answer { .. }))
            .expect("an answer");
        assert_eq!(answer.kind, Kind::Answer { to: 4 });
    }

    /// **A MISTYPED TAG MUST NOT FAIL THE WORK.**
    ///
    /// The agent's job is the code. A malformed message costs a message
    /// nobody receives, which is what not asking would have cost anyway.
    #[test]
    fn a_malformed_directive_is_skipped_not_fatal() {
        let reply = r#"<ask>no recipient</ask>
<ask to="">empty recipient</ask>
<ask to="review"></ask>
<answer to="not-a-number">filed against nothing</answer>
<ask to="review">this one is fine</ask>"#;

        let found = parse_directives(reply);
        assert_eq!(found.len(), 1, "got: {found:?}");
        assert_eq!(found[0].text, "this one is fine");
    }

    /// **A NAME NOBODY HAS MUST NOT SWALLOW THE QUESTION.**
    ///
    /// Roles are a closed vocabulary, and an agent told to name a recipient
    /// invents a plausible one — `schema`, `auth`, `db`. Delivered literally
    /// that message is lost in the way that looks like success from both ends:
    /// the asker believes it asked, and nobody was ever going to answer. It
    /// becomes a broadcast instead.
    #[test]
    fn a_question_to_an_invented_recipient_becomes_a_broadcast() {
        let found = parse_directives(r#"<ask to="schema">what is the user id called?</ask>"#);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].to, EVERYONE,
            "a message to a name nobody has went nowhere"
        );

        // And a real role is still addressed to that role alone.
        let direct = parse_directives(r#"<ask to="review">is this shape right?</ask>"#);
        assert_eq!(direct[0].to, "review");
    }

    /// The format tells the agent the names it may actually use.
    ///
    /// Without the list, the model invents one — which is exactly how the
    /// first version of this was written, and the first test of it failed.
    #[test]
    fn the_format_lists_the_names_that_exist() {
        let block = prompt_block("implement");
        for role in crate::roles::KNOWN_ROLES {
            assert!(block.contains(role), "`{role}` is addressable but unlisted");
        }
        assert!(block.contains(EVERYONE));
    }

    /// A reply with no tags produces nothing — the ordinary case.
    #[test]
    fn a_plain_reply_says_nothing_on_the_channel() {
        assert!(parse_directives("I fixed the bug and the tests pass.").is_empty());
    }

    /// The prompt shows recent messages, and says how many it left out.
    #[test]
    fn the_unread_block_is_bounded_and_says_so() {
        let many: Vec<Message> = (0..40)
            .map(|i| Message {
                seq: i,
                at: "2026-09-21T00:00:00.000Z".to_string(),
                from: "other".to_string(),
                to: EVERYONE.to_string(),
                kind: Kind::Note,
                task: None,
                text: format!("message {i}"),
            })
            .collect();

        let block = unread_block(&many).expect("messages to show");
        assert!(block.contains("message 39"), "the newest must be shown");
        assert!(!block.contains("message 0"), "the oldest must be dropped");
        assert!(
            block.contains("older message(s) not shown"),
            "what was left out must be admitted: {block}"
        );

        assert_eq!(unread_block(&[]), None, "a quiet channel costs no prompt");
    }
}
