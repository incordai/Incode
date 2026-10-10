# Security

Please **do not** open a public issue for a vulnerability.

Report it privately with a GitHub Security Advisory on this repository
(**Security → Report a vulnerability**). Include the version you run
(`incode --version`), your platform, the agent and model, and enough detail to
reproduce it. You will get an answer, and credit in the fix if you want it.

## What counts

Incode runs every tool a model calls through its own guards, so the most
serious reports are the ones where that promise breaks:

- a tool call that runs, writes or deletes something a guard should have
  refused, or that escapes the OS sandbox;
- text from a repository, a file, a web page or a tool's output that makes the
  agent take an action the user did not approve (prompt injection that crosses
  a permission boundary);
- a key, token or other secret written to a log, a transcript, the memory
  store or a commit;
- computer use acting outside what the user allowed.

## What to expect from Incode itself

Incode uses the agent CLIs you already have, run headless on your own
subscriptions. A session can edit files and run commands, within the mode and
permissions you chose. Read [Safety](README.md#safety) and
[Security notes](README.md#security-notes) in the README for what each mode
allows and what Incode will not do.

## Supported versions

Fixes are made on `main` and in the next release.
