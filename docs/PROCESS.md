# How work gets done here

The rules in this file are about the *act* of changing this codebase — where
code lives while it is being proved, what has to be true before it moves, and
what counts as proof. `CONVENTIONS.md` is about the code itself.

---

## 1. Local `main` is the test bench. There are no branches.

```
edit  ->  gate  ->  commit to local main  ->  push to origin/main
                    ^                          ^
                    already reversible         only after the gate is green
```

One branch, `main`, local and remote. Nothing else exists.

### Why a branch would add nothing

A branch buys isolation from other people's work and a place to park an
unfinished idea. Neither applies: there is one person in this repository, and
the unfinished idea already has a home — the working tree, and then a local
commit. The separation that matters is **local vs. pushed**, and it is already
there.

What a branch actually cost, the one time it was used: the whole codebase
landed somewhere the owner had to go looking for it, `main` still read as an
empty repository, and a merge decision existed that nobody wanted to make.

### Why a local commit before pushing

Committing is not publishing. A local commit is the undo — every change after
it has something to be a diff against, and nothing is irreversible any more.
That is what makes deleting code, rewriting a module, or trying an approach
safe to do at all.

**The repository had no version control until 2026-09-22.** 20,000 lines, no
undo. If that ever happens again: `git init`, commit the tree as a baseline,
*then* start work.

---

## 2. The gate

Nothing is pushed until all four pass:

```
cargo test --lib      # currently 1279
cargo test --bins     # currently 38
cargo clippy --all-targets    # zero warnings, zero errors
cargo fmt --check             # clean
```

Zero clippy warnings is not aspirational. `CONVENTIONS.md` §11 forbids
`#[allow(...)]`, so a warning has to be fixed rather than silenced.

**A test count that went down is a failure even when nothing is red.** Deleting
a test to make a suite pass is the same defect as deleting a check.

---

## 3. Every load-bearing change gets a control run

Reintroduce the defect, confirm the test fails **on the intended assertion**,
restore the fix. A test that passes with and without the change is decoration.

This is `CONVENTIONS.md` §8, and it is here as well because it is the rule most
often skipped — and skipping it is invisible. Three failures this project has
already had, each caught only by a control:

| What looked fine | What the control showed |
|---|---|
| Lane budget accounting | two lanes spent 10k each and the run was charged 0 |
| Worktree name generation | 64 of 1600 concurrent names collided |
| The unmerged-lane rule | no test existed at all; writing one failed immediately with `["t-1","t-2"]` |

### A control must not be able to do damage

Controls really run the broken code. The first version of the read-only desktop
test typed `hunter2` into whatever window had focus on the machine running the
suite. Rewrite the test so its *failure* mode is harmless — stop safety first,
or pick a tool that moves a mouse rather than one that types.

---

## 4. Verify against the thing, not against your reading of it

- A claim about behaviour needs a line number or a command that was run.
- A claim about an external tool needs that tool's own `--help` or a live call.
- After `cargo install`, run the binary and look at the output.

Failures this project has had from skipping it: a config written with single
backslashes that TOML rejected; `describe` inventing a `role` field for a schema
that takes `{id, button}`; an agent id matched against a binary name, which
compiled, passed a test that had invented the same wrong id on both sides, and
could never once have fired.

---

## 5. Wire it, do not delete it

An unwired capability is something somebody built and nobody reached. The first
question is **where should this have been called**, not *can this go*.

Four of the six originals had a correct home — `PermissionPolicy` and `hooks` at
the desktop's tool choke point, `with_contracts` and `with_dependencies` in
`from_plan`. Deleting any of them would have removed a working answer and left
the gap it was written for still open.

Deletion is the last resort, it needs a reason written down, and it never
happens in the same change as the investigation.

---

## 6. Scope

Touch what the task requires. A defect found on the way gets fixed if it is in
the blast radius of the change being made, and reported if it is not.

Report honestly: if tests fail, say so with the output; if a step was skipped,
say that; if part of the scope was left out, name it and why.
