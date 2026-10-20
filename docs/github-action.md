# incode in GitHub Actions

`.github/actions/incode` is a composite action with two modes:

- **`comment`** — when an issue, an issue comment or a pull request review
  comment contains `@incode`, the text and the issue / pull request it was
  written on (title, description, and for a review comment the file, line and
  diff hunk) become one `incode -t` instruction. The answer is posted back as
  a comment. If the model changed files, they are committed to a new branch
  `incode/<number>-<run id>` and the comment links to it and to a "compare"
  page to open a pull request.
- **`review`** — on a pull request, runs `incode review --base origin/<base> -p`
  and posts the findings (correctness bugs first, then security, then
  simplifications, each cited as `path:line`; or "No issues found.").

incode is installed from its GitHub releases with `scripts/install.sh`
(`INCODE_VERSION` picks the tag; `latest` by default). The model is reached
over its API: the action writes a one-entry `admin.json` (route `ci`) into a
private `INCODE_DIR` and exports the key under the variable that entry names,
so nothing else on the runner is configured.

## Setup

1. Add the API key as a repository secret, e.g. `ANTHROPIC_API_KEY`.
2. Set the model id as a repository variable `INCODE_MODEL` (or write it in
   the workflow).
3. Copy [`.github/workflows/incode.yml.example`](../.github/workflows/incode.yml.example)
   to `.github/workflows/incode.yml`.

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0          # review mode diffs against the base branch
- uses: incordai/incode/.github/actions/incode@main
  with:
    mode: comment           # or: review
    api_key: ${{ secrets.ANTHROPIC_API_KEY }}
    model: ${{ vars.INCODE_MODEL }}
```

## Inputs

| Input | Default | What it does |
| --- | --- | --- |
| `mode` | `comment` | `comment` or `review`. |
| `api_key` | (required) | The key's value, from a secret. A composite action cannot read a secret by name, so the workflow passes it. |
| `api_key_env` | `ANTHROPIC_API_KEY` | The variable name the key is exported as (the route's `key_env`). |
| `api_url` | `https://api.anthropic.com/v1/messages` | The endpoint: `.../v1/messages` or `.../chat/completions`. |
| `api_format` | `anthropic` | `anthropic` or `openai`. |
| `model` | (required) | The model id the endpoint knows. |
| `context_window` | incode's default | Tokens; the conversation is compacted past 80% of it. |
| `allowed_tools` | all (`comment`), `Read,Grep,Glob` (`review`) | Comma-separated incode tools the model may use (`INCODE_ALLOWED_TOOLS`). |
| `trigger_phrase` | `@incode` | What a comment must contain. |
| `allowed_associations` | `OWNER,MEMBER,COLLABORATOR` | Who may trigger a run. |
| `push` | `true` | Push the model's changes to a new branch in `comment` mode. |
| `base` | the PR's base branch | The ref `review` mode compares with. |
| `incode_version` | `latest` | The release tag to install. |
| `github_token` | `github.token` | Used by `gh` to comment and by `git` to push. |

Outputs: `skipped` (`true` when the event was not for incode) and `branch`
(where changes were pushed).

## Safety

- The run holds your API key and a token that can push, so by default only
  owners, members and collaborators can trigger it (`allowed_associations`).
- Comment text is untrusted: the action reads it from the event file with
  `jq` and hands it to incode through files and environment variables, never
  by splicing it into a script.
- The model never pushes to an existing branch; a person reviews the branch
  and opens the pull request.
- `pull_request` runs from forks get no secrets, so `review` mode only works
  for branches in the same repository (or under `pull_request_target`, which
  you should only use if you understand what it exposes).
