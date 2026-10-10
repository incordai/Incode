# Contributing to Incode

Thank you for helping. Incode's own rule is that work is proven done before it
is called done, and contributions are held to the same rule.

## Ways to help

- **Report a bug.** Open an issue with the command or session you ran, the
  agent and model, what you expected and what happened. Logs help; remove
  keys, tokens and anything private before pasting them.
- **Fix something.** Small pull requests that do one thing are reviewed
  fastest.
- **Add a provider, a tool or a platform.** Read
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) first: it describes how the
  model providers, the tools and the guards fit together.
- **Improve the documents.** The files under `docs/` are input the loop reads,
  so a wrong sentence there misleads the agent as well as people.

## Building and checking

You need a stable Rust toolchain (1.88 or newer) and git. On Linux, the
desktop backends (screen capture, input, accessibility) also need these
development packages:

```sh
sudo apt-get install pkg-config clang libclang-dev libwayland-dev libpipewire-0.3-dev \
  libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev libdbus-1-dev libegl-dev libgbm-dev
```

Before opening a pull request, run these checks; a pull request is merged only
when all four pass:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --lib
cargo test --bins
```

The tests create git repositories and commit in them, so git needs a name and
email configured.

## How changes are written

[docs/CONVENTIONS.md](docs/CONVENTIONS.md) is how the code should read, and
[docs/VERIFICATION.md](docs/VERIFICATION.md) is what counts as done. In short:

- **A change in behaviour comes with a test of that behaviour.** The test runs
  the changed code, asserts what it did, and fails when the change is taken
  back out. Check that last part before opening the pull request.
- **No silenced checks.** Do not add `#[allow(...)]`, skip a test, or loosen a
  type to make CI pass; fix the cause, or say in the pull request why it
  cannot be fixed yet.
- **Root causes, not patches.** If the wrong value comes from another module,
  fix that module rather than correcting the value where it arrives.
- **Stay in scope.** Unrelated renames, reformatting and refactors belong in
  their own pull request.
- **Explain the wiring.** A non-trivial function's doc comment says who calls
  it and what it calls, including other code that does the same job by a
  different route.
- **New dependencies are justified.** A new crate is pinned, its licence and
  `rust-version` checked, and the reason written beside it in `Cargo.toml`,
  as the existing entries are.
- **No secrets.** Never commit keys, tokens, `.env` files or personal data.

## Pull requests

1. Fork the repository and branch from `main`.
2. Make the change, with its test.
3. Run the four commands above.
4. Open the pull request: say what changed, why, and how you verified it.
   Update `docs/ROADMAP.md` when the change finishes or opens an item there.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License, Version 2.0](LICENSE), the license of the project (section 5
of the license).
