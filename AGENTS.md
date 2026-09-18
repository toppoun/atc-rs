# AGENTS.md

## Project

atc-rs is a Rust CLI/TUI for AtCoder workflows.

Primary platforms:

- Windows
- macOS

The project values:

- correctness
- filesystem safety
- predictable user-visible behavior
- cross-platform behavior
- preservation of user files
- explicit failure over unsafe fallback

## Working tree safety

Do not commit, push, stash, reset, restore, checkout, discard, or otherwise
remove existing changes unless the user explicitly asks.

Before modifying files:

1. inspect `git status`
2. distinguish existing changes from task changes
3. preserve unrelated work

## Implementation

Read the actual implementation before proposing or changing architecture.

Do not infer behavior only from docs, tests, or old prompts when source code
can answer it.

Prefer small changes that reuse existing abstractions.

Do not introduce a new abstraction unless it clearly reduces duplication or
enforces an important contract.

## Safety

Do not weaken existing filesystem safety.

In particular, preserve existing policies around:

- symlinks / reparse points
- non-regular files
- no-clobber behavior
- atomic file replacement
- authentication credential secrecy
- submission at-most-once behavior

Do not expose authentication credentials in:

- UI
- logs
- errors
- Debug output
- test failure messages

## Tests

Do not weaken assertions merely to make tests pass.

When a test fails on one platform, first determine whether the cause is:

- a production bug
- a cross-platform inconsistency
- a test portability bug
- an OS-specific semantic difference
- a concurrency/flakiness bug

Prefer tests that exercise production paths.

After relevant code changes, run the appropriate subset first, then the full
validation when practical:

cargo fmt --all -- --check
cargo check --locked --all-features
cargo clippy --locked --all-targets --all-features
cargo test --locked --all-features --no-fail-fast
git diff --check

Existing unrelated warnings are not part of the task unless requested.

## Cross-platform behavior

Do not assume Windows filesystem/path behavior applies to macOS or vice versa.

Avoid introducing platform-specific normalization merely to satisfy tests.

Preserve PathBuf identity unless the product contract explicitly requires
canonicalization.

## Documentation

`README.md` and files under `docs/` are user-facing documentation unless a file
explicitly states otherwise.

### Language

Write `README.md` and user-facing files under `docs/` in Japanese.

Use natural, concise Japanese intended for users of `atc-rs`.

Keep code, commands, file names, configuration keys, UI labels, and technical
identifiers in their original form when translating them would make the
documentation less clear.

Developer-facing source comments, commit messages, code identifiers, and test
names are not governed by this rule unless explicitly requested.

### Content

User-facing documentation should explain:

- what the user can do
- how to do it
- when settings take effect
- what files may be changed
- how to recover from common errors

Avoid implementation details that users do not need, such as internal Rust
type names, ownership boundaries, CAS revisions, transaction internals,
worker generations, or request serialization.

See `docs/AGENTS.md` for documentation-specific structure and style.