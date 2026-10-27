//! The loop's handling of a line, without an agent or a terminal behind it:
//! which commands end the loop, and what the commands that change the
//! loop's own state — the directories, the model, the session record — do
//! to it.

use super::*;

/// A loop over a session on `ws`, keeping its data in `data`. No agent is
/// started: nothing here runs a turn.
fn repl_on(ws: &Path, data: &Path) -> Repl {
    let session = crate::cli::agent_turn::AgentSession::new(
        ws.to_path_buf(),
        "claude-code",
        "claude".to_string(),
        &crate::guards::Guards::default(),
        Vec::new(),
    );
    let prepared = Prepared {
        workspace: ws.to_path_buf(),
        incode_dir: data.to_path_buf(),
        admin: crate::admin::AdminConfig::default(),
        permission_mode: PermissionMode::WorkspaceWrite,
        tool_cache: ToolStatusCache::load_or_create(&data.join("tool-status.json")),
        lifecycle_hooks: LifecycleHooks::default(),
        git_context: None,
        system_prompt: Vec::new(),
        output_style: None,
        catalogue: crate::agent::catalogue::Catalogue::default(),
        agent: crate::agent::DetectedAgent {
            id: "claude-code".to_string(),
            display_name: "Claude Code".to_string(),
            command: "claude".to_string(),
            path: None,
            version: None,
            protocol: crate::agent::Protocol::ClaudeStreamJson,
        },
        fleet: crate::fleet::Fleet::new(Vec::new()),
        session,
    };
    Repl::new(
        IncordCodeConfig::default(),
        prepared,
        crate::runtime::cancel::CancelToken::new(),
    )
}

/// The plain editor, with no box pinned: what a pipe gets.
fn plain_terminal(data: &Path) -> Terminal {
    let mut editor = LineEditor::new(None);
    let ux = ux::Ux::start(
        &crate::admin::AdminConfig::default(),
        data,
        None,
        &mut editor,
    );
    Terminal {
        footer: None,
        editor,
        ux,
    }
}

/// A workspace, a data directory, and a loop over them.
struct Fixture {
    ws: tempfile::TempDir,
    data: tempfile::TempDir,
    repl: Repl,
    term: Terminal,
}

impl Fixture {
    fn new() -> Self {
        let ws = tempfile::tempdir().expect("ws");
        let data = tempfile::tempdir().expect("data");
        let repl = repl_on(ws.path(), data.path());
        let term = plain_terminal(data.path());
        Self {
            ws,
            data,
            repl,
            term,
        }
    }

    fn command(&mut self, cmd: SlashCommand) -> Next {
        self.repl.command(cmd, &mut self.term)
    }

    fn line(&mut self, line: &str) -> Next {
        let custom = crate::custom_commands::Registry::default();
        self.repl.on_line(line.to_string(), &custom, &mut self.term)
    }

    /// The records kept for this workspace.
    fn kept(&self) -> Vec<crate::session::Record> {
        crate::session::for_workspace(self.data.path(), self.ws.path())
    }
}

/// **ONLY `/quit` ENDS THE LOOP.** Every other command — a bad one, one
/// missing its argument, one that fails — is answered and the next line is
/// read.
#[test]
fn only_quit_ends_the_loop() {
    let mut f = Fixture::new();
    assert_eq!(f.command(SlashCommand::Quit), Next::Quit);
    assert_eq!(f.line("/quit"), Next::Quit);
    assert_eq!(f.line("/exit"), Next::Quit);

    for cmd in [
        SlashCommand::Help,
        SlashCommand::Unknown("/nope".to_string()),
        SlashCommand::Usage("/kill <job id>".to_string()),
        SlashCommand::Kill(4242),
        SlashCommand::Mcp,
        SlashCommand::Model(None),
        SlashCommand::Resume(None),
    ] {
        assert_eq!(f.command(cmd.clone()), Next::Continue, "{cmd:?}");
    }
}

/// An empty line and a bare greeting are answered without a turn: no agent
/// is started for either.
#[test]
fn an_empty_line_or_a_greeting_starts_no_turn() {
    let mut f = Fixture::new();
    assert_eq!(f.line(""), Next::Continue);
    assert_eq!(f.line("hi"), Next::Continue);
    assert_eq!(f.repl.session.turns(), 0);
    assert!(f.repl.session_record.is_none());
}

/// **`/add-dir` WIDENS THE SESSION WITH THE LIST.** A directory that exists
/// joins both the loop's list and the session's roots — what undo
/// snapshots and the guards watch; one that does not exist joins neither.
#[test]
fn add_dir_widens_the_session_only_to_a_directory_that_exists() {
    let mut f = Fixture::new();
    let other = tempfile::tempdir().expect("other");
    let other_dir = other.path().to_path_buf();

    assert_eq!(
        f.command(SlashCommand::AddDir(Some(other_dir.display().to_string()))),
        Next::Continue
    );
    assert_eq!(f.repl.extra_dirs, std::slice::from_ref(&other_dir));
    assert!(
        f.repl.session.roots().contains(&other_dir),
        "the session's reach did not widen with the list: {:?}",
        f.repl.session.roots()
    );

    let missing = other_dir.join("not-there");
    f.command(SlashCommand::AddDir(Some(missing.display().to_string())));
    f.command(SlashCommand::AddDir(None));
    assert_eq!(f.repl.extra_dirs, [other_dir]);
    assert_eq!(f.repl.session.roots().len(), 2);
}

/// `/model <name>` says the new model wherever the session is described.
#[test]
fn a_model_switch_renames_what_the_session_runs_on() {
    let mut f = Fixture::new();
    f.command(SlashCommand::Model(Some("model-b".to_string())));
    assert_eq!(f.repl.display_model, "model-b via Claude Code");
}

/// **`/clear` FINISHES THE RECORD IN HAND.** It is saved — so `/sessions`
/// still lists the conversation just ended — and let go, so the next turn
/// opens a fresh record instead of stamping its id over this one.
#[test]
fn clear_keeps_the_ended_conversation_and_lets_its_record_go() {
    let mut f = Fixture::new();
    f.repl.session_record = Some(crate::session::Record::new("conv-1", f.ws.path(), "m"));

    f.command(SlashCommand::Clear);

    assert!(f.repl.session_record.is_none());
    let kept = f.kept();
    assert!(
        kept.iter().any(|r| r.agent_session == "conv-1"),
        "the ended conversation was not kept: {kept:?}"
    );
}

/// `/resume <id>` finishes the record in hand the same way; without an id
/// it changes nothing.
#[test]
fn resume_finishes_the_record_only_when_it_names_a_session() {
    let mut f = Fixture::new();
    f.repl.session_record = Some(crate::session::Record::new("conv-1", f.ws.path(), "m"));

    f.command(SlashCommand::Resume(None));
    assert!(f.repl.session_record.is_some());
    assert!(f.kept().is_empty());

    f.command(SlashCommand::Resume(Some("conv-2".to_string())));
    assert!(f.repl.session_record.is_none());
    assert!(f.kept().iter().any(|r| r.agent_session == "conv-1"));
}

/// `/save <name>` names the record in hand and writes it; a blank name, or
/// no record yet, writes nothing.
#[test]
fn save_names_the_record_in_hand() {
    let mut f = Fixture::new();
    f.command(SlashCommand::Save(Some("nightly-fix".to_string())));
    assert!(f.kept().is_empty(), "nothing to name before the first turn");

    f.repl.session_record = Some(crate::session::Record::new("conv-1", f.ws.path(), "m"));
    f.command(SlashCommand::Save(Some("   ".to_string())));
    assert!(f.kept().is_empty(), "a blank name is no name");

    f.command(SlashCommand::Save(Some("  nightly-fix ".to_string())));
    let held = f.repl.session_record.as_ref().expect("still in hand");
    assert_eq!(held.title, "nightly-fix");
    let kept = f.kept();
    assert!(
        kept.iter().any(|r| r.title == "nightly-fix"),
        "the name was not written: {kept:?}"
    );
}

/// The input box shows the model, or the fallback when the agent picks its
/// own, beside the workspace's own name and the branch.
#[test]
fn the_input_box_names_the_model_the_folder_and_the_branch() {
    let git = GitContext {
        branch: "main".to_string(),
        is_dirty: true,
        has_staged: false,
        short_status: String::new(),
    };
    let shown = footer_session(
        Some("model-a"),
        "Claude Code",
        Path::new("/w/proj"),
        Some(&git),
    );
    assert_eq!(
        shown,
        footer::Session {
            model: "model-a".to_string(),
            workspace: "proj".to_string(),
            branch: Some("main".to_string()),
            dirty: true,
        }
    );

    let own = footer_session(None, "Claude Code", Path::new("/w/proj"), None);
    assert_eq!(own.model, "Claude Code");
    assert_eq!(own.branch, None);
    assert!(!own.dirty);
}

/// **EVERY CTRL-C STOPS THE TURN, AND A LISTENER THAT FAILS ENDS QUIETLY.**
/// Two signals then a failure: the turn is tripped by each — reset between
/// them, as a new turn does — and the loop ends at the failure rather than
/// spinning on it. A failure first trips nothing.
#[tokio::test]
async fn each_ctrl_c_stops_the_turn_until_the_listener_fails() {
    use std::sync::{Arc, Mutex};
    let cancel = crate::runtime::cancel::CancelToken::new();
    let tripped = Arc::new(Mutex::new(Vec::new()));
    let mut signals = vec![Ok(()), Ok(()), Err(std::io::Error::other("no console"))].into_iter();
    let seen = (cancel.clone(), Arc::clone(&tripped));
    forward_signals(cancel.clone(), move || {
        // What the previous signal did, recorded before the next arrives.
        seen.1.lock().unwrap().push(seen.0.is_cancelled());
        seen.0.reset();
        let next = signals.next().expect("the loop stops at the failure");
        async move { next }
    })
    .await;
    assert_eq!(
        *tripped.lock().unwrap(),
        [false, true, true],
        "each signal trips the turn"
    );

    let untouched = crate::runtime::cancel::CancelToken::new();
    forward_signals(untouched.clone(), || async {
        Err(std::io::Error::other("no console"))
    })
    .await;
    assert!(!untouched.is_cancelled(), "a failed listener trips nothing");
}
