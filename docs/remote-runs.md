# Remote runs: `incode remote`

`incode remote run "<task>"` runs the task with `incode -t` on GitHub Actions
instead of on your machine, waits for it, and prints its result. The run
pushes whatever the agent changed to a branch `incode/remote-<id>`; it opens
no pull request.

```sh
incode remote run "fix the flaky test in tests/net.rs"
incode remote run "update the changelog" -R incordai/incode --ref release -m "$MODEL_ID"
incode remote run "bump the MSRV to 1.85" --no-wait     # print the run's address and return
incode remote list                                      # recent runs: id, state, title, address
incode remote logs 1234567890                           # one run's state, address and result
```

## How a run goes

1. `incode remote run` makes an id and dispatches the repository's
   `.github/workflows/incode-remote.yml` (`workflow_dispatch`) with the inputs
   `task`, `model` (when `-m` is given) and `id`, on `--ref` or the
   repository's default branch.
2. It finds the run that started by its title, `incode remote <id>`, and
   prints its address (on stderr).
3. It polls the run every 10 seconds until it completes, for up to
   `--timeout` minutes (60 by default). If the wait runs out it says the run
   is still going, prints its address and exits non-zero; the run is **not**
   cancelled, and `incode remote logs <run-id>` reads its result later.
4. It downloads the run's `incode-result` artifact and prints its
   `result.json` on stdout: the object `incode -t --output-format json`
   prints, plus `branch` — `incode/remote-<id>` when the agent changed
   something, else `null`.

It exits non-zero when the run did not succeed or the result says
`"is_error": true`.

## Setup

1. **The workflow.** Copy
   [`.github/workflows/incode-remote.yml.example`](../.github/workflows/incode-remote.yml.example)
   to `.github/workflows/incode-remote.yml` in your repository and commit it
   to the default branch (GitHub dispatches a workflow only once it knows
   it), and to any branch you will pass as `--ref`.
2. **The model key.** Add the API key as a repository secret,
   `ANTHROPIC_API_KEY`. For another provider, change the workflow's
   "Configure the model" step (`KEY_ENV`, `API_URL`, `API_FORMAT` — the same
   fields as the [GitHub Action](github-action.md)'s inputs).
3. **The model.** Set a repository variable `INCODE_MODEL` to the model id,
   or pass `-m` on each run.
4. **Your token.** `incode remote` reads a GitHub token from `GH_TOKEN`, else
   `GITHUB_TOKEN`. It needs **Actions: read and write** on the repository (a
   fine-grained token), or the `repo` and `workflow` scopes (a classic one).
   With the GitHub CLI signed in:

   ```sh
   GH_TOKEN=$(gh auth token) incode remote run "..."
   ```

The repository is `-R owner/name`, else the github.com repository your
`origin` remote names (https or ssh). GitHub Enterprise servers are not
supported.

## What the workflow does

`run-name: incode remote <id>`; then, on `ubuntu-latest`:

- checks the id is letters, digits and dashes (it names the branch);
- checks out the repository and installs the latest incode release with
  `scripts/install.sh`, as the GitHub Action does;
- writes a one-route `admin.json` (the key's variable name only) into a
  private `INCODE_DIR`;
- runs `incode -t "$TASK" --spec-gate off --output-format json`, writing
  `result.json` outside the checkout;
- if the working tree changed, commits it to `incode/remote-<id>` and pushes
  that branch;
- adds `branch` to `result.json` and uploads it as the artifact
  `incode-result`;
- fails the job when the task failed.

## Safety

- **Who can run it.** Dispatching a workflow takes write access to the
  repository, and the run holds your model key and a token that can push.
- **The task is untrusted text.** The workflow passes it, and every other
  input, through environment variables; none is spliced into a script.
- **No pull request, no existing branch.** The run pushes only a new
  `incode/remote-<id>` branch; a person reviews it and opens the pull request.
  A push made with the workflow's `GITHUB_TOKEN` starts no other workflow.
- **Your token stays with GitHub.** `incode remote` sends it only to
  `api.github.com` over https, in a header. The artifact download is
  redirected to GitHub's storage; that request is made without the token.
  The token is never printed or put in an address.
