# Lanes as pull requests — design

Status: proposed, not built. Builds on the lane check (each lane's check runs
in its own checkout before anything leaves it), which is built.

## What changes for the operator

Today a `/run` with `INCODE_LANES=N` merges each lane's branch into the
checked-out branch on this machine. With `INCODE_LANES_AS=pr`, a lane whose
own check passed is pushed and opened as a pull request instead, and nothing
is merged locally. A person reviews and merges it on GitHub. The run goes on
with the other tasks.

## Why it is more than swapping `merge_lane` for `/pr`

The ledger's law is that a task reaches `Done` only through a passing check on
the tree the work is in (`src/ledger.rs:42-57`, `src/runner.rs:1482`). A lane
opened as a pull request is neither done (its work is not in the tree) nor
running (no agent is on it) nor failed. It needs a state of its own, and a way
back to `Done` once a person has merged it.

## The design

1. **A state: `InReview`.** Open (`is_open()` is true), not `Ready`, so no
   runner picks it up and its dependants stay `Pending` until it is done —
   exactly as now, because their work assumes its change.
2. **The task records its pull request:** `pull_request: Option<String>` (the
   URL), set when the lane is opened as one, alongside the existing
   `unmerged_branch`.
3. **Opening:** in `run_batch`, after the lane check passes, call the existing
   `/pr` path (`tools::pull_request::open`, with its refusals: default branch,
   nothing new, credentials in the diff). Its result becomes
   `Merged::Opened { url }`; `landed()` stays false.
4. **Coming back:** at the start of every `/run`, for each `InReview` task,
   ask the forge (`Forge::pr_status`) whether its PR was merged. If merged:
   `git pull` the base, run the task's check on the main tree, and complete it
   through the ordinary `verify` path — the same single edge into `Done`. If
   closed without merging: the task goes back to `Ready` with its branch
   remembered, as for a conflict. If still open: nothing.
5. **Off by default.** Without `INCODE_LANES_AS=pr` nothing changes.

## What it refuses

- No GitHub remote, or `gh` not logged in: the run says so at the start and
  merges locally as today, rather than discovering it per lane.
- A lane that failed its check is never opened as a pull request — the same
  rule as for a local merge.

## Tests it needs

- A lane opened as a PR leaves the main tree untouched and the task `InReview`
  with the URL recorded (fake forge, as `/pr`'s tests use).
- A merged PR completes the task only after the check passes on the pulled
  tree; a failing check after the merge records a failure, not `Done`.
- A closed PR returns the task to `Ready` with its branch.
- Dependants of an `InReview` task are not started.
