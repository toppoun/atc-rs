# docs/AGENTS.md

These instructions apply to user-facing documentation for `atc-rs`.

Read the repository root `AGENTS.md` as well.

The goal is to help users start using `atc-rs` and understand its
visible behavior without needing to understand its implementation.

## Audience

Write for people who want to use `atc-rs`, not people who want to
understand or maintain its internals.

Assume that a new user may be unfamiliar with:

- CLI/TUI tools
- package managers
- workspace concepts
- configuration files
- authentication cookies
- compiler installation

Do not assume that every user is unfamiliar with programming or AtCoder.

Explain unfamiliar operations when they are necessary, but avoid
lengthy explanations of general concepts that are not specific to
using `atc-rs`.

A user should be able to complete the initial setup without reading
internal specifications or editing configuration files unnecessarily.

## Language

Write user-facing documentation in natural, concise Japanese.

Prefer familiar words and short sentences.

Avoid unnecessarily formal or technical wording.

Keep commands, paths, file names, configuration keys, UI labels,
and technical identifiers in their original notation when useful.

Use the same name for the same operation across README and docs.

Do not translate an established UI label differently in each document.

## Documentation journey

Organize documentation around the order in which users need it:

1. Understand what `atc-rs` does.
2. Install it.
3. Complete the first usable Contest workflow.
4. Learn everyday operations.
5. Customize behavior when needed.
6. Find solutions when something goes wrong.

The recommended reading path is:

README
→ Installation
→ Feature-specific guides
→ Reference documentation
→ Troubleshooting

Do not require users to read all documentation in this order.
Each feature-specific page should also be useful when opened directly.

## Explain behavior, not implementation

Describe:

- what the user can do
- what to enter or press
- what happens afterward
- when a change takes effect
- which files may be created, replaced, or deleted
- how to recover when an operation fails

Do not explain internal implementation unless it is necessary for
correct use.

Avoid internal types and mechanisms such as:

- ContestSession
- AuthSnapshot
- SessionAuth
- SubmissionHub
- ownership boundaries
- CAS revisions
- transaction internals
- worker generations
- request serialization

Prefer:

"コンテストを開き直すと、変更した設定が反映されます。"

over:

"A new ContestSession captures a fresh Config snapshot."

Prefer:

"別の場所でCookieが変更されていた場合、その変更は上書きしません。"

over descriptions of compare-and-swap internals.

Do not remove important user-visible behavior merely because its
implementation is complicated.

If an implementation detail does not affect what the user does,
sees, or needs to know, omit it.

## Getting started

The initial setup guide should take a user from a fresh environment
to their first usable Contest workflow.

Prefer the shortest reliable path.

The main path should not require:

- understanding workspace routing
- reading configuration schemas
- editing TOML files
- customizing source templates
- understanding authentication internals

unless the operation genuinely requires them.

Introduce optional customization only after the basic workflow works.

Use actual, copyable commands.

Explain where the user should execute a command when that matters.

Distinguish required steps from optional steps.

Do not turn the installation guide into a complete command reference.

## Platform-specific instructions

Windows and macOS are primary supported platforms.

Separate platform-specific steps when commands or prerequisites differ.

Verify instructions against:

- the actual source code
- current package/distribution files
- relevant scripts and configuration
- available platform validation results

Do not infer current behavior solely from old documentation,
historical design notes, or previous release instructions.

Do not invent installation commands, package names, compiler
requirements, supported versions, or platform behavior.

If a procedure has not been validated on a platform, do not present
it as confirmed.

Explain required external tools only where they are needed.

Do not require users to install optional tools for features they
do not intend to use.

## Document responsibilities

### README.md

README is the entry point, not the complete manual.

Include:

- short product overview
- main capabilities
- short Windows/macOS installation instructions
- minimal quick start
- representative everyday workflow
- links to detailed documentation

Keep it concise.

A user should quickly understand what the tool does and how to
try it.

Avoid detailed configuration tables, complete shortcut lists,
internal architecture, and exhaustive troubleshooting.

### installation.md

Guide a new user from installation to the first usable Contest.

Include:

- platform-specific installation
- necessary prerequisites
- installation verification
- `atc doctor`
- working directory preparation
- initial workspace creation
- opening a Contest
- editing and testing a solution
- authentication and submission when needed
- links to optional customization

Explain failures that are common during initial setup.

Avoid requiring unnecessary configuration.

### workspace.md

Explain:

- creating and using a workspace
- organizing contests
- visible directory layout
- choosing contest locations
- workspace configuration when needed
- refresh behavior
- effects on existing files

Explain routing rules in terms of where files are created,
not internal resolver algorithms.

### tui.md

Explain the visible interface:

- Global Home
- Workspace Home
- Contest screen
- common actions
- keyboard and mouse operations
- modals
- testing and submission workflow

Organize around what users see and want to do.

Use tables for shortcuts when they improve readability.

Avoid internal event handling, controller states, and worker logic.

### configuration.md

Keep this as a reliable configuration reference.

Include:

- how to open or edit settings
- available settings and accepted values
- defaults
- short practical examples
- when changes take effect
- relevant file paths

Do not force new users to read this page before using the tool.

Accuracy and completeness take priority over minimizing the length
of a reference table.

### templates.md

Explain:

- when customization is necessary
- how to open and edit templates
- how source files are created
- template locations
- fallback behavior
- effects on existing files

Make clear when the built-in defaults are sufficient.

### testing.md

Explain:

- running sample tests
- adding and using custom test cases
- Watch
- Debug
- timeout
- verdicts
- common testing problems

Focus on user actions and visible results.

### stress.md

Explain:

- what Stress Test is useful for
- the required files
- preparing a generator
- preparing a brute-force solution
- running comparisons
- reproducing and saving failures

Start with a practical, minimal example.

Keep advanced options in later sections.

### authentication.md

Explain:

- why authentication may be needed
- how to paste or replace a Cookie
- how to check authentication
- how to reset stored authentication
- how to recover from invalid or expired credentials
- the stored credential path
- when authentication changes take effect
- what the user should know about credential secrecy

Describe automatic Cookie updates as visible behavior.

Do not explain SessionAuth, CAS revisions, or HTTP request
serialization.

Never display real authentication credentials in examples.

### troubleshooting.md

Organize around symptoms users can recognize.

For each issue, prefer:

1. Symptom
2. What to check
3. How to fix it
4. Link to the detailed guide when needed

Avoid making users identify internal error categories before they
can find help.

Do not duplicate full explanations from other pages.

## One authoritative home

Each concept should have one authoritative documentation page.

Other pages should provide a short explanation and link to that page.

For example:

- README links to installation.
- Installation links to authentication when submission requires it.
- TUI links to testing for detailed test behavior.
- Troubleshooting links to the relevant feature guide.

Avoid copying entire sections between files.

Keep links relative where appropriate and verify that they resolve.

## Examples

Use practical, copyable examples.

Use examples that match actual supported behavior.

Explain placeholders when a user must replace them.

Do not use real credentials, private paths, or personal account data.

Keep the main example small.

Move uncommon cases and advanced options into later sections
or the appropriate reference page.

## User-visible files and safety

Clearly distinguish operations that:

- create files
- modify files
- replace files
- delete files
- leave existing files unchanged

Do not claim an operation is safe or reversible without checking
the actual implementation.

When a destructive action requires confirmation, explain it.

When settings or authentication changes apply only to a newly opened
Contest, explain that behavior in ordinary language.

Do not expose filesystem locking, atomic replacement, or other
implementation details unless users need them to recover from an error.

## Troubleshooting and recovery

Prefer actionable guidance over technical diagnosis.

For example:

"Cookieを設定し直してください。"

is generally more useful to a user than an explanation of an
authentication state transition.

Preserve the distinction between:

- an operation that failed
- an operation that may have succeeded despite an error
- an authentication request that was rejected
- a request that could not be verified

Do not promise recovery steps that may overwrite or delete
unrelated user files.

## Internal documentation

`docs/internal/` contains developer-facing specifications,
historical design notes, release inventories, and similar material.

It is not part of the normal user documentation journey.

The user-facing Japanese language and simplification rules in this
file do not apply to explicitly developer-facing material there.

Do not link to `docs/internal/` from the user-facing README or
getting-started guide as required reading.

Historical documentation must not be treated as the source of truth
for current behavior.

Do not silently rewrite historical records to make them appear
current.

If a historical document may be mistaken for current guidance,
label its status clearly when appropriate.

## Documentation verification

Before editing documentation:

1. Read the relevant existing documents.
2. Inspect the actual implementation and relevant distribution files.
3. Identify the authoritative page for each topic.
4. Check which existing explanations have become outdated.
5. Preserve useful information while removing unnecessary duplication.

After editing:

- Verify commands and examples against the implementation.
- Verify Windows/macOS differences.
- Verify relative links and referenced file names.
- Verify keyboard shortcuts and UI labels.
- Verify paths and settings.
- Check that beginner instructions do not require unexplained
  internal knowledge.
- Check that reference documentation retains necessary precision.
- Confirm user-facing documentation is natural Japanese.

Do not add unverified claims merely to make the guide seem complete.