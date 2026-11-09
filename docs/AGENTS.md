# docs/AGENTS.md

## Scope

The [root AGENTS.md](../AGENTS.md) rules on accuracy, safety, and validation
apply to all documentation. Verify relative links for both audiences. The
writing and guide-specific verification rules below apply to the
[user-facing README](../README.md) and user-facing guides under `docs/`.

`docs/internal/` is developer-facing. Use the [internal index](internal/README.md)
to distinguish current references from historical or scope-limited records.
Preserve historical records and label their status when they could be mistaken
for current guidance. Apply the Japanese and simplification rules only to
user-facing material. Developer-facing source comments, commit messages,
code identifiers, and test names follow those rules only when explicitly
requested.

## Audience and language

Write in natural, concise Japanese. Use familiar words and short sentences.
Readers may be new to CLI/TUI tools, package managers, workspaces, configuration
files, authentication cookies, or compiler setup. Explain unfamiliar steps
needed to use atc-rs without assuming readers are new to programming or AtCoder
or giving long explanations of general concepts.

Keep code, commands, paths, file names, settings keys, UI labels, and technical
identifiers in their original form when that is clearer. Use consistent names
for the same operation and UI label across the README and guides.

## Reading path and guide roles

Lead from the README to installation and the first usable Contest, then to
everyday operations, optional customization, and troubleshooting as needed.
Feature guides should also work when opened directly. Give each topic one
authoritative home. Summarize and link from other pages, using relative links
where appropriate.

Keep the README concise with an overview, main capabilities, short Windows
and macOS installation steps, a quick start, a typical workflow, and links to
the guides. Keep detailed reference tables, shortcuts, and troubleshooting in
their respective guides. Keep internal material out of the required reading
for the README and getting-started guides.

Take a reader from a fresh environment to their first usable Contest by the
shortest reliable path. Identify required steps and introduce optional
configuration and templates after the basic workflow works. Require external
tools only for the features that need them. Require TOML editing or detailed
knowledge of routing, schemas, or authentication only when the operation
requires it. Link to the relevant guides for details beyond initial setup.

| Guide | Primary responsibility |
| --- | --- |
| [Installation](installation.md) | Platform prerequisites and install checks, environment diagnosis with `atc doctor`, and setup from the working directory and Workspace through opening, editing, and testing in the first Contest. Cover authentication and submission as needed, common setup failures, and links to optional customization. |
| [Workspace](workspace.md) | Workspace setup and use, Contest layout and locations, configuration, Refresh, and effects on existing files. Explain routing by showing where files go. |
| [TUI](tui.md) | Global Home, Workspace Home, and Contest operations, including keyboard and mouse input, modals, testing, and submission. Use shortcut tables when they help. |
| [Configuration](configuration.md) | How to open and edit settings, relevant paths, accepted values, defaults, examples, and when changes apply. Keep reference tables accurate and complete. |
| [Templates](templates.md) | When built-in templates suffice, where and how to edit templates, how source files are created, fallback behavior, and effects on existing files. |
| [Testing](testing.md) | Running sample tests and adding and using custom cases, plus Watch, Debug, timeouts, verdicts, and common testing problems. |
| [Stress](stress.md) | A minimal example of preparing and running a comparison, including required files, the generator and brute-force solution, and how to reproduce and save a failure. |
| [Authentication](authentication.md) | When authentication is needed, how to set, replace, verify, and reset a Cookie, the storage path, automatic Cookie updates and when changes take effect, and how to protect credentials and recover from invalid or expired credentials. |
| [Troubleshooting](troubleshooting.md) | Recognizable symptoms, what to check, how to recover, and links to the relevant guides. |

## Behavior, examples, and recovery

- Describe what the user can do, what to enter or press, and what happens next.
  Preserve important visible behavior. Include implementation details only when
  users need them for correct use or recovery.
- Give short, copyable examples that match supported behavior. Explain any
  placeholders and where to run a command when it matters. Keep uncommon cases
  and advanced options in later sections or the relevant reference. Use
  fictional examples free of real credentials, private paths, and account data.
- Distinguish file creation, modification, replacement, deletion, and
  preservation. Check the implementation before claiming safety or
  reversibility, and explain any required confirmation for destructive actions.
- Explain when settings and authentication changes take effect. If an operation
  requires opening a new Contest for changes to apply, say so in plain language.
- Distinguish failure, interruption, and an unknown outcome, including when an
  operation may already have succeeded despite an error. Distinguish rejected
  authentication from an inability to verify it. Give the user the next safe
  step without requiring internal diagnosis. Verify that recovery instructions
  preserve unrelated user files.

## Documentation verification

Before editing, read the relevant pages and identify each topic's authoritative
home. Inspect the implementation and relevant distribution files, scripts, and
configuration. Identify outdated explanations and retain useful information
when consolidating duplicates.

Separate platform procedures where commands or prerequisites differ. Mark
unvalidated procedures as unverified. Do not infer behavior from old
documentation, design notes, or release instructions alone. Do not invent
commands, package names, compiler requirements, supported versions, or
platform behavior. Omit unverified claims that would only make a guide appear
more complete.

After editing, verify commands and examples, shortcuts and UI labels, paths and
file names, and configuration keys against the implementation. Check Windows and
macOS differences against available platform validation results. Verify relative
links and referenced files. Read the result as a first-time user and check that
it uses natural Japanese, explains required steps without assuming internal
knowledge, and retains reference precision.
