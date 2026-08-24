# atc Pre-release Inventory

This inventory describes the repository state after the 2026-08-24 pre-release
correctness and safety audit. It is source material for later documentation, not
a README or an installation guide.

## 1. Product summary

`atc` is a Rust command-line tool for an AtCoder workflow. It can route contest
directories inside a workspace, fetch contest metadata and samples, create C++
or Python sources, run and watch sample tests, find and retain stress-test
counterexamples, open an interactive terminal UI, initialize user templates and
configuration, and check a manually configured AtCoder session.

The binary currently reports version `atc 0.1.0`.

## 2. User-facing command tree

The actual command tree is:

```text
atc
├── init
├── config
│   └── init
├── new
├── contest (alias: c)
├── refresh
├── test
├── watch
├── stress
│   └── init
├── create
├── template
│   └── init
└── login
```

- `atc init`
  - Initializes `.atc-workspace.toml` in the exact current directory.
  - Preserves an existing valid file. An invalid file or wrong object type is an
    error rather than an overwrite.
- `atc new <CONTEST> [-l|--language <cpp|python>]`
  - Fetches a contest and creates its routed contest directory, metadata,
    samples, and one source per problem.
  - Uses the current directory as the workspace root and does not search parent
    directories.
  - An existing destination is reported as existing before configuration,
    templates, or the network are consulted.
- `atc contest <CONTEST_ID>` or `atc c <CONTEST_ID>`
  - Opens an existing contest in the watch TUI, creates a missing contest, or
    offers to repair missing/invalid metadata.
  - A metadata identity mismatch or unsupported newer metadata version is a hard
    error.
- `atc refresh [-c|--contest <CONTEST>] [-f|--force]`
  - Refetches contest metadata and samples without replacing source files.
  - Without `-c`, the exact current directory is the contest target. With `-c`,
    the contest is resolved from the exact current workspace root.
  - `--force` supports metadata recovery; it is not permission to delete entries
    that `atc` does not own under `tests/`.
- `atc test <PROBLEM> [-c|--contest <CONTEST>] [-l|--language <cpp|python>] [-d|--debug]`
  - Compiles or starts the selected source, runs official samples, then runs a
    usable saved stress regression.
  - `--debug` is supported only for C++.
- `atc watch [-c|--contest <CONTEST>] [--plain]`
  - Watches exact contest source paths and tests a source after it changes.
  - The default mode is the interactive TUI; `--plain` uses terminal event
    reporting.
- `atc stress <PROBLEM> [-c|--contest <CONTEST>] [-l|--language <cpp|python>] [-d|--debug] [--count <N> | --forever] [--seed <SEED>]`
  - Runs a contest-local Python generator, the candidate, and a contest-local
    Python brute-force reference until the requested limit or the first
    candidate failure.
  - `--count` must be nonzero. The default is 100 cases. `--forever` and
    `--count` are mutually exclusive. `--debug` is C++-only.
- `atc stress init <PROBLEM> [-c|--contest <CONTEST>]`
  - Creates the missing generator and brute-force helper files for the canonical
    problem index without replacing either existing path.
  - Stress run options are not accepted by this nested command.
- `atc create <NAME> [-l|--language <cpp|python>]`
  - Creates one source file in the exact current directory from the selected
    normal source template.
  - It does not require or modify contest/workspace metadata and never replaces
    an existing source path.
- `atc template init [cpp|python]`
  - Creates the selected active user source template, or both templates when no
    language is supplied, from the embedded built-ins.
  - Existing valid templates are preserved byte-for-byte.
- `atc config init`
  - Creates a comments-only global config file that leaves all current binary
    defaults active.
  - Existing valid configuration is preserved; malformed existing configuration
    remains untouched and is reported as an error.
- `atc login`
  - Checks whether the configured `REVEL_SESSION` authenticates at AtCoder.
  - It does not prompt for credentials or write a cookie.

All commands support their generated `--help`; the top level also supports
`-V`/`--version`.

## 3. Main user workflows

### Start and route a workspace

1. Change to the directory that will be the workspace root.
2. Run `atc init`.
3. Keep or edit the generated routing rules in `.atc-workspace.toml`.
4. Run contest commands from that exact root when using `-c` or a contest ID.

### Open or create a contest

- `atc contest abc466` resolves `abc466`, creates it if absent, and enters the
  watch TUI.
- `atc new abc466` performs creation only and returns to the shell.
- An existing damaged contest can be repaired through `atc contest` after
  confirmation, or with the constrained `atc refresh --force` recovery flow.

### Create and test a source

- `atc create scratch -l python` creates `scratch.py` in the current directory.
- `atc test A` tests the selected language for problem A in the current contest.
- `atc test A -c abc466` resolves the contest from the exact current workspace
  root. Compiler and child-process working directories are the resolved contest
  directory in both forms.

### Refresh fetched data

- Run `atc refresh` inside a contest, or `atc refresh -c abc466` from its exact
  workspace root.
- Metadata and `atc`-owned samples are replaced as one recoverable update.
  Sources are not modified. Unexpected entries under `tests/` stop the refresh
  before those entries can be discarded.

### Watch sources

- `atc watch` starts the interactive terminal UI.
- `atc watch --plain` reports changed sources and test results without the TUI.
- Watch mode reacts to changes after startup; it does not run every source once
  at startup.

### Stress test a solution

1. Run `atc stress init A` if `A_gen.py` or `A_brute.py` is missing.
2. Edit both helpers for the problem.
3. Run `atc stress A`, optionally with `--count`, `--forever`, and `--seed`.
4. Reproduce a saved counterexample through the next `atc test A` run.

In the TUI, `S` inspects or starts stress testing, while `i` initializes helpers
only after the UI reports that setup is required.

### Customize defaults

- `atc config init` creates the global overrides file; add only keys that differ
  from built-in behavior.
- `atc template init cpp` or `atc template init python` creates a normal source
  template for editing. These templates do not control stress helpers.

### Check AtCoder authentication

1. Place exactly one `REVEL_SESSION=<value>` record in the platform cookie file.
2. Run `atc login`.
3. The command performs a final HTTPS check against AtCoder settings and reports
   authenticated or unauthenticated status without printing the secret value.

## 4. Supported languages

Only `cpp` and `python` are accepted. Matching is ASCII case-insensitive, but
aliases such as `c++`, `py`, `python3`, and `pypy` are not accepted.

### C++

- Source extension: `.cpp`.
- Built-in compiler command: `g++`.
- Built-in flags: `-std=c++23`, `-O2`, `-Wall`, `-Wextra`.
- The candidate is compiled to a temporary executable before tests or stress
  cases run.
- `--debug` is supported. The embedded debug header is materialized below the
  `atc` cache and included in the debug build.

### Python

- Source extension: `.py`.
- Built-in interpreter command: `python`.
- The source path is passed directly to the configured interpreter.
- `--debug` is rejected. Toggling debug in the TUI does not rerun Python.

Language selection order for commands that expose `-l` is command-line value,
then `[defaults].language`, then the built-in C++ default. The runner does not
silently fall back to a source file in the other language.

## 5. Global configuration

The path abstraction comes from `etcetera::choose_base_strategy()`:

- Windows default: `%APPDATA%\atc\config.toml`.
- Windows templates: `%APPDATA%\atc\templates\`.
- Windows cache: `%LOCALAPPDATA%\atc\`.
- XDG strategy default, including the current Linux/macOS strategy:
  `${XDG_CONFIG_HOME:-~/.config}/atc/config.toml`,
  `${XDG_CONFIG_HOME:-~/.config}/atc/templates/`, and
  `${XDG_CACHE_HOME:-~/.cache}/atc/`.

Configuration is overrides-only. A missing file, an absent key, or the
comments-only file produced by `atc config init` uses the defaults compiled into
the current binary. Supported keys are:

```toml
[defaults]
language = "cpp"                    # cpp or python

[runner]
python = "python"
cpp_compiler = "g++"
cpp_flags = ["-std=c++23", "-O2", "-Wall", "-Wextra"]
timeout_seconds = 2.0
compile_timeout_seconds = 10.0
```

Table TOML and equivalent dotted keys are accepted. Unknown fields are rejected.
Program values must contain a non-whitespace character. Both timeout values must
convert to positive, finite, nonzero durations. A malformed, invalid-UTF-8, or
semantically invalid existing config is reported with its path and is not
replaced by `config init`.

## 6. Source templates

Normal source templates have two layers:

- Embedded built-ins: `assets/templates/default.cpp` and
  `assets/templates/default.py`.
- Active overrides: `<config-dir>/templates/cpp.cpp` and
  `<config-dir>/templates/python.py`.

A genuinely absent selected override falls back to its embedded built-in. A
selected regular UTF-8 file is used exactly, including an empty file. A valid
file symlink, linked templates directory, and Windows directory junction are
followed. A dangling link, directory at a template-file path, invalid UTF-8, or
other broken observed target is an error. The unselected language's override is
not inspected.

`atc template init [LANGUAGE]` creates only missing selected templates and
preflights all selected targets before creating any. It does not load or modify
`config.toml`. Existing valid template contents remain owned by the user.

`atc new`, missing-contest creation through `atc contest`, and `atc create` use
this resolver. Refresh, test, watch, login, config initialization, and stress
helper initialization do not resolve normal source templates. Stress helpers
come from separate embedded `stress_gen.py` and `stress_brute.py` assets.

## 7. Workspace behavior

`atc init` creates `.atc-workspace.toml` in the exact current directory. The
generated version-1 file contains three routing rules:

```text
^abc[0-9]+$ -> ABC
^arc[0-9]+$ -> ARC
^agc[0-9]+$ -> AGC
```

For a contest ID, zero matching rules place `<contest-id>` directly below the
workspace root, one match places it below the rule's relative path, and multiple
matches are an error. Mapping paths must be portable relative components.
Missing mapping directories can be created for contest creation; existing
intermediate files, symlinks, junctions, or other reparse points are rejected.

Workspace and `-c` resolution never search parent directories. Running a command
from a nested directory changes the root; the user must explicitly change to the
intended root or contest directory.

Contest IDs and problem indices are validated before path construction. Path
separators, absolute/prefixed paths, `.`/`..`, Windows alternate data streams,
reserved device components, trailing dots/spaces, and case-insensitive problem
index collisions are rejected where applicable.

## 8. Contest layout / files created by atc

### Workspace-local files

- `.atc-workspace.toml`: workspace routing configuration created by `atc init`.
- `<routed-path>/<contest-id>/`: contest directory.
- `<contest>/.atc/contest.toml`: versioned contest ID, problem indices, and URLs.
- `<contest>/<INDEX>.cpp` or `<INDEX>.py`: user source created from the selected
  normal template. Existing sources are never replaced.
- `<contest>/tests/<INDEX>/sample-N.in` and `sample-N.out`: fetched sample pairs.
  `atc` treats only known problem directories and these exact sample-name forms
  as refresh-owned. Other entries block refresh.
- `<contest>/<INDEX>_gen.py` and `<INDEX>_brute.py`: user-editable stress helpers.
- `<contest>/.atc/stress/<INDEX>/failed.in`: latest saved failing input.
- `<contest>/.atc/stress/<INDEX>/actual.out`: candidate output for that failure.
- `<contest>/.atc/stress/<INDEX>/expected.out`: reference output for current
  saved failures.
- `<contest>/.atc/stress/<INDEX>/stderr.txt`: candidate stderr when nonempty.
- `<contest>/.atc/stress/<INDEX>/meta.toml`: failure kind, contest/problem
  identity, case number, base seed, and case seed.
- `<contest>/.atc/stress/.<INDEX>.lock`: internal generation lock.

There is no persisted "current contest" file in the current implementation.

### Global user files

- `<config-dir>/config.toml`: optional overrides-only global config.
- `<config-dir>/templates/cpp.cpp` and `python.py`: optional normal source
  overrides.
- `<state-dir>/atc/cookie`, or the Windows data-directory fallback
  `%APPDATA%\atc\state\cookie`: manually provisioned AtCoder session record.
- `<cache-dir>/atc/include/debug.hpp`: materialized embedded C++ debug header.

## 9. Testing and watch behavior

`atc test` resolves a metadata problem index case-insensitively, while rejecting
ambiguous indices. It requires the exact selected source. C++ compilation and
each candidate run have separate configurable timeouts; Python skips compilation.
All compiler, candidate, generator, and reference processes run with the resolved
contest directory as their working directory.

Each process has piped stdin, stdout, and stderr. Timeout and cancellation stop
the process tree and reap the child. On Unix this uses a process group; on
Windows it uses a kill-on-close Job Object with suspended startup. Captured
stdout and stderr are each limited to 8 MiB while the remainder is drained. A
truncated candidate output or invalid UTF-8 candidate output cannot be accepted.

Sample comparison normalizes CRLF, ignores trailing whitespace on each line, and
allows a missing final newline. Blank-line structure and leading or internal
whitespace remain significant. Candidate stderr is displayed when relevant but
does not by itself change an accepted stdout verdict. Compile failure, compile
timeout, runtime error, timeout, wrong answer, and accepted results remain
distinct.

After official samples, a coherent saved stress counterexample with expected
output is run as a separate regression case. Missing samples produce a
user-visible no-samples result without starting a compiler or candidate.

Watch mode observes exact `.cpp` and `.py` source paths from contest metadata,
coalesces file events, and ignores unrelated, missing, removed, or directory
paths. It runs on changes after startup rather than performing an initial run.
The TUI scheduler uses monotonically newer requests so older results cannot
replace current state.

## 10. Stress testing

The run form is:

```text
atc stress [OPTIONS] <PROBLEM>
```

For each case, `<INDEX>_gen.py` is started with the decimal seed as its first
argument; its UTF-8 stdout becomes candidate input. The candidate and
`<INDEX>_brute.py` receive that input on stdin. The reference's UTF-8 stdout is
the expected answer. Helpers and candidates run in the contest directory.

The default limit is 100. `--count N` selects another nonzero finite limit;
`--forever` removes the limit. A supplied `--seed` is the base seed. Without it,
the current Unix-epoch nanosecond value is converted to `u64`. Case `n` uses
`base_seed + n - 1`. A finite range that would overflow is rejected before any
compiler or child process starts.

Candidate WA, nonzero exit, timeout, truncated output, or invalid UTF-8 stops the
run and saves the latest failure under `.atc/stress/<INDEX>/`. Generator or
reference nonzero exit, timeout, output beyond 8 MiB, or invalid UTF-8 is a typed
stress error rather than a candidate counterexample. Completed finite runs and
cancelled runs report the number of passed cases.

`atc stress init <PROBLEM>` creates the two missing Python helpers independently.
It preserves existing bytes and rejects links, directories, and other wrong
object types. In the TUI, helper setup uses `None`, `Required`, `Initialized`,
and `Error` states: `S` re-inspects, `i` only acts in `Required`, initialization
does not auto-run, and the next `S` starts stress when ready.

## 11. TUI

The default `atc watch` interface displays problem tabs and current status,
sample and saved-stress cases, expected/actual/stderr detail, compilation and run
progress, stress progress/failure, and distinct stress-setup attention states.
It supports Unicode-aware wrapping, resize handling, cell or pixel mouse input,
foldable detail sections, and detail/sample scrolling.

Implemented keys are:

- `q`: quit.
- `d`: toggle C++ debug and rerun; for Python the state can toggle without a
  rerun.
- `r`: rerun the selected problem's confirmed source.
- `S`: re-inspect stress helpers, or queue actual stress when ready.
- `i`: initialize stress helpers only while setup is `Required`.
- `s`: toggle the samples pane.
- `h` / Left and `l` / Right: previous/next problem.
- `j` / Down and `k` / Up: next/previous case.
- Mouse wheel: change samples over the samples pane or scroll detail over the
  detail pane; the detail scrollbar also supports clicks and dragging.

Initialized setup is not displayed as a successful stress result. Existing
actual success or failure status remains authoritative.

## 12. Authentication

Authentication state is optional. The cookie file must be a regular, bounded
file containing exactly `REVEL_SESSION=<value>` with a valid RFC cookie-octet
value. Percent characters are accepted. The secret header is marked sensitive,
and normal error/status output does not include its value.

On Unix, group- or other-readable cookie files are rejected. Symlinked cookie
files, symlinked state directories, dangling links, directories, and other
special objects are rejected without following or replacing them.

`atc login` is a status check. It sends the configured session over HTTPS and
requires the final `/settings` response to identify an authenticated session.
Missing, stale, or invalid authentication is reported with cookie-location
guidance. The command never writes authentication state.

The default cookie location is `%APPDATA%\atc\state\cookie` on Windows and
`${XDG_STATE_HOME:-~/.local/state}/atc/cookie` under the current XDG strategy.

## 13. Platform support and external requirements

### Confirmed in this audit

- Windows x86-64 MSVC was compiled and natively tested with Rust/Cargo 1.97.1.
- Windows no-clobber moves, path validation, junction/reparse checks, process
  Job Object cleanup, terminal adapters, and CLI/release execution were covered
  by the native suite.
- Paths containing spaces and Unicode are handled through `Path`/`PathBuf` and
  direct process argument construction rather than shell command strings.

### Intended portable behavior

- macOS uses the Unix process-group cleanup path, the current XDG/etcetera path
  strategy, symlink-aware file checks, and atomic exclusive rename primitives.
- Linux uses the same Unix behavior and a `renameat2(RENAME_NOREPLACE)` move.
- Apple platforms use `renamex_np(RENAME_EXCL)` for no-clobber moves.

### Validation gap

- macOS and Linux were statically reviewed but were not natively compiled or
  executed in this audit because only `x86_64-pc-windows-msvc` was installed.

### External requirements

- Network access to `https://atcoder.jp` for creation, refresh, and login checks.
- A C++23-capable compiler reachable as `g++`, or a configured replacement, for
  C++ workflows.
- A Python interpreter reachable as `python`, or a configured replacement, for
  Python candidates and all stress helpers.
- An interactive compatible terminal for the TUI; `atc watch --plain` is the
  non-TUI alternative.
- VS Code is not used or required by current workflows.

## 14. Safety / preservation behavior relevant to users

- Contest creation stages a complete workspace before an atomic no-clobber
  install. A racing destination is preserved and reported as a conflict.
- Existing source files, config files, source templates, and stress helpers are
  not overwritten by initialization commands.
- Multi-file template and helper initialization preflights selected targets so
  an invalid existing target does not cause an unrelated new file to be created.
- Refresh changes metadata and `atc`-owned samples, not sources. Unknown files,
  directories, links, or malformed names under `tests/` stop refresh instead of
  being deleted. A late change to the moved previous generation is retained at
  a reported recovery path rather than cleaned up.
- Stress failure replacement validates the previous generation and preserves
  recovery data if a swap and rollback cannot both complete or if the previous
  generation changes before cleanup.
- Invalid or unsupported metadata/configuration produces an error rather than a
  guessed path or destructive repair.
- Timeout and cancellation terminate descendants rather than leaving ordinary
  child processes running.

## 15. Representative commands for README

- `atc contest abc466`: combines workspace routing, fetch/create behavior, and
  the primary watch TUI workflow.
- `atc test A`: demonstrates the short edit/run/sample-feedback loop and saved
  stress regression.
- `atc stress init A` followed by `atc stress A --seed 1`: demonstrates
  reproducible counterexample discovery and persistence.
- `atc template init cpp`: demonstrates user-owned source customization without
  editing repository assets.
- `atc config init`: introduces the optional overrides-only configuration model.

## 16. Quick Start facts

Factual prerequisites for a future Quick Start are:

- The `atc` binary must be available on `PATH`.
- Internet access is needed to create or refresh a contest.
- The built-in C++ path requires `g++` with C++23 support; Python workflows and
  stress helpers require `python`. Both commands can be overridden in config.
- A minimal workspace sequence is:

  ```text
  mkdir <workspace>
  cd <workspace>
  atc init
  atc contest <contest-id>
  ```

- A contest-local sequence is `edit <INDEX>.cpp`, then `atc test <INDEX>` or let
  the already-running TUI react to the save.
- Authentication is optional for public access and is manually provisioned when
  required.
- Binary distribution, package-manager installation, signing, and notarization
  are not finalized in this repository; no installation command is documented
  here.

## 17. Screenshot / GIF candidates

- Watch TUI sample feedback: show problem tabs, an accepted and wrong-answer
  sample, and expanded expected/actual detail after saving a source.
- Stress setup transition: show `Required`, press `i`, show `Initialized`, then
  press `S` to begin real stress progress. This makes the non-auto-run behavior
  visible.
- Reproducible mismatch: run `atc stress A --seed 1`, show the WA seed and saved
  artifact path, then show the saved case in `atc test A` or the TUI.
- Workspace routing: show `atc init` followed by `atc contest abc466` creating
  `ABC/abc466` from the generated default mapping.
- Template ownership: show `atc template init cpp`, edit the user template, then
  create a contest source with the custom exact contents.

## 18. Known limitations / deferred work

### Platform validation gaps

- macOS and Linux behavior has no native execution result from this audit.
- Packaging, signing, notarization, and installer behavior were outside this
  audit and are not defined by the repository.

### Current V1 limitations

- Only C++ and Python normal sources are supported.
- Stress generators and brute-force references are Python files with fixed
  contest-local names.
- Watch mode is change-triggered and performs no initial test run.
- Authentication must be provisioned manually; `atc login` only checks status.
- There is no persisted current-contest selection and no parent-directory
  workspace discovery.
- Each captured stdout/stderr stream is limited to 8 MiB. Candidate truncation
  is WA; generator/reference truncation is an error.
- Refresh owns only its documented `tests/<INDEX>/sample-N.in|out` namespace;
  user material inside `tests/` must be moved elsewhere before refresh.
- The repository's `docs/SPEC.md` is historical migration/design material and
  is not an authoritative description of the current CLI.

### Non-blocking implementation concern

- New source creation uses exclusive direct creation to preserve native file
  permissions and avoid clobbering. A rare mid-write I/O failure can leave a
  partial newly created source; an existing source is never replaced.

## 19. README-safe facts

- `atc` creates and routes AtCoder contest workspaces from the terminal.
- It fetches problem metadata and samples and creates C++ or Python sources.
- It runs sample tests once or whenever a source changes.
- The TUI shows per-problem status and sample/stress details.
- Stress testing can generate, save, and later rerun a reproducible
  counterexample.
- Compiler, interpreter, timeouts, and default language are optional overrides;
  the config can remain absent.
- C++ and Python source templates can be customized without editing the binary
  or repository.
- Existing user config, templates, helpers, and source files are preserved.
- Windows is the currently native-tested platform; macOS/Linux need native
  release validation.

## 20. Detailed-doc candidates

- Configuration reference: platform paths, every supported key, validation,
  examples for table and dotted TOML, and override precedence.
- Workspace routing: exact-root behavior, mapping regexes, path restrictions,
  default routes, and nested-directory examples.
- Contest storage and refresh: metadata schema/version behavior, owned sample
  namespace, repair modes, and partial sample-fetch reporting.
- Testing: comparison policy, debug behavior, timeouts, output limits, process
  working directory, and saved regression ordering.
- Stress testing: helper protocol, seeds, limits, failure artifacts, replay, TUI
  setup states, and recovery behavior.
- Authentication: platform cookie paths, secure file requirements, manual setup,
  and status meanings without exposing session material.
- Platform notes: required external tools, Windows terminal/process behavior,
  XDG paths, and the macOS/Linux validation matrix.
- TUI key reference and mouse/scroll behavior.

## 21. Internal-only notes

- Commands emit domain `Event` values through a `Reporter`; terminal and TUI
  presentation remain reporter responsibilities.
- Contest creation, refresh swaps, refresh rollbacks, and stress-generation swaps
  use platform-native no-clobber moves. Windows uses `MoveFileExW` without
  replacement, Linux uses `renameat2(RENAME_NOREPLACE)`, and Apple uses
  `renamex_np(RENAME_EXCL)`.
- Refresh derives owned problem indices from validated staged metadata plus
  valid prior metadata and validates the tests tree both before and after moving
  it into private staging.
- Stress generations are serialized per problem by an internal lock, validated
  before replacement, and read under a shared lock for a coherent replay
  snapshot.
- Runner pipe readers retain bounded prefixes while continuing to drain both
  streams. Candidate verdict policy separately tracks stdout truncation and
  UTF-8 validity.
- Unix process groups and Windows Job Objects provide descendant cleanup on
  timeout/cancellation. Windows children are attached while suspended before
  execution resumes.
- Request pacing is measured between AtCoder request starts, HTTP 429 responses
  have three retries, and `Retry-After` waits are capped at 60 seconds.
- `docs/SPEC.md` records migration history and stale design possibilities,
  including state concepts absent from the current product. Current code, help,
  tests, and observed release behavior are authoritative for documentation.
