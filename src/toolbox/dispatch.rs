//! The tools whose answer is text alone, by family.
//!
//! Wired to: [`Toolbox::dispatch_text`] is reached from `Toolbox::dispatch`
//! (`toolbox/mod.rs`) once every gate has passed (`Toolbox::decide_and_run`,
//! the same file). Siblings that also run a tool, by other routes and from
//! the same `dispatch`, which tries them first: `Screenshot` and `RenderPdf`
//! through the headless browser (`toolbox/browser.rs`, `toolbox/pdf.rs`), the
//! desktop's tools (`Toolbox::run_desktop`) and the client's MCP servers'
//! (`Toolbox::run_client_tool`), both in `toolbox/mod.rs`.

use super::*;

impl Toolbox {
    /// The tools whose answer is text alone, reached from `Toolbox::dispatch`.
    ///
    /// Every such tool has one arm in one of three tables, by family — the
    /// file tools ([`Self::file_tool`]), the session's own
    /// ([`Self::session_tool`]) and the memory's ([`Self::memory_tool`]) —
    /// and a tool none of them names is asked of the modules that own their
    /// tools ([`Self::tool_elsewhere`]). The tables name distinct tools, so
    /// their order decides nothing; `tests.rs` asserts every advertised tool
    /// runs.
    pub(super) fn dispatch_text(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
    ) -> Result<String, String> {
        // The file and shell tools parse their own JSON from a string, which is
        // the interface they were written to; re-serialising here keeps that
        // contract rather than rewriting six working implementations.
        let raw = input.to_string();
        let ws = self.root_of_call(name, input);
        self.file_tool(name, ws, &raw, input)
            .or_else(|| self.session_tool(call_id, name, ws, &raw, input))
            .or_else(|| self.memory_tool(call_id, name, ws, &raw, input))
            .unwrap_or_else(|| self.tool_elsewhere(name, input))
    }

    /// The workspace, or the added directory the call names a path in —
    /// and for `Read` alone, a folder only it may open.
    fn root_of_call(&self, name: &str, input: &Value) -> &Path {
        if name == READ {
            self.read_root_for(input)
        } else if matches!(
            name,
            WRITE | EDIT | GLOB | GREP | MULTI_EDIT | NOTEBOOK_EDIT | LS | OUTLINE
        ) {
            self.root_for(input)
        } else {
            self.workspace.as_path()
        }
    }

    /// [`Self::dispatch_text`]'s file tools; `None` for any other name.
    fn file_tool(
        &self,
        name: &str,
        ws: &Path,
        raw: &str,
        input: &Value,
    ) -> Option<Result<String, String>> {
        Some(match name {
            READ => crate::tools::file_ops::read_file(ws, raw).map_err(|e| e.to_string()),
            WRITE => crate::tools::file_ops::write_file(ws, raw).map_err(|e| e.to_string()),
            EDIT => crate::tools::file_ops::edit_file(ws, raw).map_err(|e| e.to_string()),
            MULTI_EDIT => edits::multi_edit(ws, input),
            NOTEBOOK_EDIT => edits::notebook_edit(ws, input),
            APPLY_PATCH => edits::apply_patch(input, &|path| self.resolve(path)),
            LS => listing::list(ws, input),
            GLOB => crate::tools::search::glob_search(ws, input),
            GREP => crate::tools::search::grep_search(ws, input),
            OUTLINE => outline::run(ws, input),
            CONVERT_DOCUMENT => self.convert_document(input),
            DIAGNOSTICS => checks::diagnostics(ws, input, &|path| self.resolve(path)),
            _ => return None,
        })
    }

    /// [`Self::dispatch_text`]'s session tools — the shell, the todo list,
    /// skills, subagents and questions; `None` for any other name.
    fn session_tool(
        &self,
        call_id: &str,
        name: &str,
        ws: &Path,
        raw: &str,
        input: &Value,
    ) -> Option<Result<String, String>> {
        Some(match name {
            BASH => self.run_bash(call_id, ws, raw, input),
            BASH_OUTPUT => shell::output(&self.state, input),
            KILL_SHELL => shell::kill(&self.state, input),
            TODO_WRITE => todo::write(&mut self.state.todos(), input),
            SKILL => self.read_skill(input),
            AGENT => self.run_subagent(input),
            ASK_USER_QUESTION => self.ask_here(input),
            _ => return None,
        })
    }

    /// [`Self::dispatch_text`]'s memory tools — verify, recall, the recorder
    /// and the brief; `None` for any other name.
    fn memory_tool(
        &self,
        call_id: &str,
        name: &str,
        ws: &Path,
        raw: &str,
        input: &Value,
    ) -> Option<Result<String, String>> {
        Some(match name {
            crate::verify::VERIFY_TOOL => crate::verify::run_tool(ws),
            crate::verify::PLAN_TOOL => Ok(crate::verify::run_plan_tool(ws)),
            crate::recall::INDEX_TOOL => crate::recall::run_index_tool(raw, ws),
            crate::recall::GET_TOOL => crate::recall::run_get_tool(raw),
            crate::recall::TIMELINE_TOOL => crate::recall::run_timeline_tool(raw),
            crate::recorder::LOOKUP_TOOL => self.lookup_recorded(call_id, ws, input),
            crate::brief::SAVE_TOOL => self.save_brief(raw),
            crate::brief_read::READ_TOOL => crate::brief_read::run(ws),
            _ => return None,
        })
    }

    /// A tool no table of [`Self::dispatch_text`] names, asked of the modules
    /// that own their tools, in order: knowledge, the session record, the web
    /// (with the headless browser when one is installed) and the team.
    fn tool_elsewhere(&self, other: &str, input: &Value) -> Result<String, String> {
        // The page's script, run by the headless browser, for a WebFetch
        // that needs it — when the browser is installed.
        let render = |url: &str| match self.browser() {
            Some(bin) => browser::render_html(bin, url, self.filter_env),
            None => Err("no headless browser is installed".to_string()),
        };
        let render: Option<crate::web::Render<'_>> = self.browser().is_some().then_some(&render);
        crate::knowledge::handle_tool(
            other,
            input,
            &crate::knowledge::CodeContext {
                cwd: &self.workspace,
                session: &self.session,
                resolve: &|path| self.resolve(path),
            },
        )
        .or_else(|| {
            crate::session_record::handle_tool(
                other,
                input,
                &crate::session_record::Record {
                    session: &self.session,
                    workspace: &self.workspace,
                },
            )
        })
        .or_else(|| crate::web::handle_tool(other, input, render))
        .or_else(|| self.team.handle(other, input))
        .unwrap_or_else(|| Err(format!("there is no tool called `{other}`")))
    }

    /// `ConvertDocument`, when LibreOffice was found.
    fn convert_document(&self, input: &Value) -> Result<String, String> {
        match self.soffice() {
            Some(soffice) => {
                convert::run(soffice, input, self.filter_env, &|path| self.resolve(path))
            }
            None => Err("LibreOffice was not found".to_string()),
        }
    }

    /// `Bash`: started as a job when it asks to run in the background
    /// (`toolbox/shell.rs`), otherwise run here (`Toolbox::run_shell`).
    fn run_bash(
        &self,
        call_id: &str,
        ws: &Path,
        raw: &str,
        input: &Value,
    ) -> Result<String, String> {
        if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            return shell::start(&self.state, ws, input, &self.sandbox()?);
        }
        // A write through the shell is credited to this agent: the
        // engine's window around the command (`shell_window`).
        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        self.shell_window("begin", command, call_id);
        let ran = self.run_shell(call_id, ws, raw);
        self.shell_window("end", command, call_id);
        ran
    }

    /// The recorder's lookup tool; the watch, if there is one, is told
    /// whether the recorder had the call.
    fn lookup_recorded(&self, call_id: &str, ws: &Path, input: &Value) -> Result<String, String> {
        crate::recorder::run_lookup_tool(ws, input).map(|found| {
            if let Some(watch) = &self.watch {
                watch.recorded(call_id, found.hit, found.observations);
            }
            found.text
        })
    }
}
