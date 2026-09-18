# docs/AGENTS.md

These files are user-facing documentation for `atc-rs`.

## Language

Write all user-facing documentation in this directory in Japanese.

Use clear, concise Japanese aimed at users of the tool rather than developers
of its internals.

Keep commands, paths, configuration keys, source code, and UI labels in their
original notation where appropriate.

## Document responsibilities

README.md
- product overview
- short install
- short quick start
- main features
- representative commands
- links to detailed docs

installation.md
- zero to first usable Contest
- install
- doctor
- authentication
- workspace creation
- workspace layout
- optional template/config customization

workspace.md
- workspace creation
- contest directory layout
- workspace config
- routing rules
- refresh behavior
- user-visible filesystem behavior

tui.md
- visible screens
- keyboard/mouse operations
- modals
- contest workflow
- submit/user-input/stress entry points

configuration.md
- Global Config reference
- defaults
- compiler/python/editor/submit settings
- when changes take effect

templates.md
- source templates only
- initialization/editing
- paths
- fallback behavior

testing.md
- sample/user-input testing
- watch
- debug
- timeout
- verdicts

stress.md
- generator
- brute force
- candidate comparison
- seeds
- saved counterexamples

authentication.md
- REVEL_SESSION setup
- status
- replace/reset
- storage path
- security guidance
- user-visible session behavior

troubleshooting.md
- symptom -> check -> fix
- link to the authoritative detailed doc

## Writing rules

Each concept should have one authoritative home.

Other documents should summarize briefly and link to the authoritative page
instead of duplicating the full explanation.

Prefer:

"Contestを開き直すと最新の設定が反映されます。"

over:

"A new ContestSession captures a fresh Config snapshot."

Prefer:

"既存のsource fileは上書きしません。"

over descriptions of staging, revalidation, or atomic publish internals.

Do not mention internal types such as:

- ContestSession
- AuthSnapshot
- SessionAuth
- SubmissionHub

unless the document is explicitly developer-facing.

Keep examples practical and copyable.