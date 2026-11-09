# AGENTS.md

## Project and implementation

atc-rs is a Rust CLI/TUI for AtCoder workflows. Windows and macOS are the
primary platforms. Prioritize correctness, predictable user-visible behavior,
and filesystem safety. Protect user files and credentials.

Read the actual implementation before proposing or changing architecture.
Establish behavior from the current source and relevant tests. Do not infer it
from documentation, tests, or old prompts alone when the source can answer.
Prefer small changes that reuse existing abstractions. Add an abstraction only
when it clearly reduces duplication or enforces an important contract.

## Working tree safety

Before editing, inspect `git status`, distinguish existing changes from task
changes, and preserve unrelated work. Do not commit, push, stash, reset, restore,
checkout, discard, or otherwise remove existing changes unless the user
explicitly authorizes the action.

## Filesystem, credential, and submission safety

- Do not weaken filesystem safety. Preserve existing safeguards for symlinks,
  reparse points, and non-regular files, along with no-clobber behavior and
  atomic file replacement. Protect user data and fail explicitly if proceeding
  would require an unsafe fallback.
- Check Windows and macOS filesystem and path behavior separately. Do not add
  platform-specific normalization merely to pass tests. Preserve `PathBuf`
  identity unless the product contract explicitly requires canonicalization.
- Do not expose Cookie values or other credentials in the UI, logs, errors,
  `Debug` output, or test failure messages.
- Preserve at-most-once submission by sending at most one POST per submission
  operation. Never resend automatically when acceptance is unknown.

## Tests and validation

Prefer tests that exercise production paths. Do not weaken assertions merely
to make tests pass. When a test fails on one platform, first distinguish a
production bug, a cross-platform inconsistency, a test portability bug, an
OS-specific semantic difference, and a concurrency or flakiness bug.

After relevant code changes, run the relevant tests first, then run the full
validation when practical.

```sh
cargo fmt --all -- --check
cargo check --locked --all-features
cargo clippy --locked --all-targets --all-features
cargo test --locked --all-features --no-fail-fast
git diff --check
```

Report what ran and what was skipped. Claim success only for checks that ran
and passed. Existing unrelated warnings are outside the task unless requested.

## References by task

| When you need to | Read |
| --- | --- |
| Locate a module, identify its responsibilities, or trace a data flow | The relevant part of [ARCHITECTURE.md](docs/internal/ARCHITECTURE.md) |
| Work on contracts such as file preservation, ownership checks, recovery, verdicts, authentication, submission, or concurrency | The relevant contracts in [INVARIANTS.md](docs/internal/INVARIANTS.md) |
| Identify an internal document's purpose or status | The [internal index](docs/internal/README.md) |
| Change user-visible behavior | The relevant user-facing guides under `docs/*.md` and the [README](README.md) where relevant |

Read only the sections relevant to the task. Do not treat historical records
as current specifications. Use references to locate the implementation and
tests, and verify both before relying on a description of current behavior.

## When to update documentation

- Update ARCHITECTURE when module responsibilities or major processing flows
  change.
- Update INVARIANTS when behavioral contracts, rejection conditions, or the
  scope of guarantees change.
- Update the relevant user-facing documentation when operations or important
  user-visible behavior change.

Minor internal fixes that leave these areas unchanged do not require a
documentation update. If code and documentation disagree, investigate the
relevant implementation and tests before correcting the documentation.

The README and files under `docs/` are user-facing unless marked otherwise.
`docs/internal/` is developer-facing. Follow [docs/AGENTS.md](docs/AGENTS.md)
for the language, structure, and verification of user-facing documentation,
including the root README.
