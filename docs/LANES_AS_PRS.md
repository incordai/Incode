# Lanes as pull requests

Status: built. Builds on the lane check (each lane's check runs in its own
checkout before anything leaves it).

## What changes for the operator

Today a `/run` with `INCODE_LANES=N` merges each lane's branch into the
checked-out branch on this machine. With `INCODE_LANES_AS=pr`, a lane whose
own check passed is pushed and opened as a **draft** pull request onto the
branch the run is on, and nothing is merged locally. A person reviews and
merges it on GitHub. The run goes on with the other tasks.

## Why it is more than swapping `merge_lane` for `/pr`

The ledger's law is that a task reaches `Done` only through a passing check on
the tree the work is in. A lane opened as a pull request is neither done (its
work is not in the tree) nor running (no agent is on it) nor failed.

## What was built

1. **It waits `Blocked`, with the URL recorded** (`Task.pull_request`,
   `Ledger::await_review`). The proposal was an eighth state, `InReview`; the
   ledger holds itself to seven (`src/ledger.rs`), and `Blocked` already
   means "a decision is outstanding with a person", which a review is. So
   nothing picks it up, its dependants stay `Pending`, a run that drains
   around it stops with `NothingRunnable` naming it, and the existing
   progress and dashboard views show it as waiting on a person — with the
   URL in its note.
2. **Opening** (`parallel::open_for_review`, `Merged::Opened { url }`): after
   the lane check passes, the lane is sealed and opened through `/pr`'s own
   path (`pull_request::open_onto`) with its refusals — nothing new, a
   credential in the diff. `landed()` is false, and no report is cached, so
   the runner never checks a tree without the change. A task with no check is
   merged here instead: nothing could finish it after a review.
3. **The base is the branch the run is on**, not the forge's default: a lane
   cut from a feature branch would otherwise carry that branch's commits.
   Checked once at the start (`parallel::review_base`: a branch, not detached;
   an `origin`; `gh` reachable; the branch on `origin`) and before each batch
   (`parallel::nothing_unpushed`: HEAD has nothing `origin` lacks). If either
   fails, the run says why and merges locally, as before.
4. **Coming back** (`reviews::settle`, at the start of every run, whatever
   `INCODE_LANES_AS` says): for each task waiting on a pull request, the
   forge is asked its state (`Forge::review`, `gh pr view --json
   state,baseRefName`).
   - Merged: if this checkout is on that base, it is fast-forwarded from
     `origin` (only a fast-forward), the task's check runs, and only a pass
     completes it. A failing check is a failed attempt. A checkout on another
     branch, or one that cannot fast-forward, is left alone and the task
     waits with a note saying to pull.
   - Closed unmerged: a failed attempt; back to the queue with its branch
     remembered (or set aside, if out of attempts).
   - Open, or unreadable: left waiting.
5. **Off by default.** Without `INCODE_LANES_AS=pr` nothing is opened.

## Tests

- `parallel::a_lane_opened_for_review_is_pushed_and_not_merged`
- `parallel::a_review_base_must_be_on_the_remote_with_nothing_unpushed`
- `parallel::lanes_are_merged_here_unless_pull_requests_are_asked_for`
- `reviews::a_merged_pull_request_is_pulled_checked_and_done`
- `reviews::a_merged_pull_request_whose_check_fails_is_a_failed_attempt`
- `reviews::a_closed_pull_request_sends_the_task_back_with_its_branch`
- `reviews::an_open_pull_request_leaves_the_task_waiting`
- `reviews::a_checkout_on_another_branch_is_not_merged_into`
- `reviews::the_dependants_of_a_task_in_review_are_not_started`
- `pull_request::a_pull_requests_state_and_base_are_read`
- `pull_request::a_branch_is_opened_onto_the_base_it_is_given`

## Not yet exercised

A live run against GitHub. Every test stands in for the forge; the `gh`
calls (`pr view --json state,baseRefName`, `pr create --draft`) are the ones
`/pr` already makes, plus the state query.
