# atc-rs の構造マップ

開発者向け。変更したい機能から、現行コードの入口・主な呼び出し先・回帰テストをたどるための地図です。
作成時に確認したコードは `86d10697`。資料の分類は[内部資料の索引](README.md)を参照してください。

## 1. 全体構造

```text
atc → cli.rs / main.rs → commands/ → workspace.rs → Contest の管理データ
                  │           ├──→ atcoder.rs / atcoder/ → AtCoder
                  │           └──→ Test / Stress → runner.rs → コンパイラ・解答など
                  └──→ commands/watch_tui.rs ↔ tui/（Home・Contest）
                              ├──→ ファイル監視・実行ワーカー → Test / User Input / Stress
                              └──→ TUI の提出ワーカー → commands/submit.rs
```

コマンドと TUI は、設定に `config`、認証に `auth`、グローバルな保存先の解決に `paths` を使います。
Workspace・Stress・設定などのファイル更新は、それぞれの管理モジュールが担当し、`safe_file` の共通操作を必要に応じて使います。
CLI の進捗と Test・Stress の実行結果は `ui::Event` / `Reporter` で受け渡します。TUI の実行ワーカーは `ChannelReporter` でこれらの実行イベントをメッセージに変換します。

## 2. 主要モジュールの責務

テスト欄の「同ファイル」は実装と同居する単体テストです。`tests/` の `*_cli.rs` は `atc` バイナリを起動する統合テストです。

| 実装 | 責務と主な連携先 | テストの所在 |
| --- | --- | --- |
| [src/main.rs](../../src/main.rs)、[src/cli.rs](../../src/cli.rs) | `clap` の引数定義、コマンドの振り分け、CLI の終了コード。引数なしの起動は TUI の Home へ進む | `cli.rs`、[tests/help_cli.rs](../../tests/help_cli.rs)、[tests/verdict_exit_cli.rs](../../tests/verdict_exit_cli.rs) |
| [src/commands/](../../src/commands/)、[src/ui.rs](../../src/ui.rs) | 操作ごとの入力解決と処理の組み立て。`commands/mod.rs` は Contest 情報・サンプル取得を共有し、`ui.rs` は進捗イベントと端末出力を定義 | [src/commands/tests.rs](../../src/commands/tests.rs)、各コマンドの単体テスト、[tests/](../../tests/) |
| [src/app_context.rs](../../src/app_context.rs)、[src/workspace.rs](../../src/workspace.rs) | 起動場所の Workspace 判定、Contest の配置解決、メタデータ・サンプルの読み書きと置換。CLI と TUI の両方が利用 | 同ファイル、[tests/init_cli.rs](../../tests/init_cli.rs)、[tests/contest_owner_cli.rs](../../tests/contest_owner_cli.rs) |
| [src/workspace_registry.rs](../../src/workspace_registry.rs)、[store.rs](../../src/workspace_registry/store.rs) | Workspace と Contest の最近使用順を管理する索引。モデルは TOML の変換・検証と操作の適用を担当し、`RegistryStore` は専用ロック下で最新の Registry に操作を適用して保存する。Phase 3-1b の入口はストアの API | [モデルのテスト](../../src/workspace_registry/tests.rs)、[保存と排他のテスト](../../src/workspace_registry/store/tests.rs) |
| [src/atcoder.rs](../../src/atcoder.rs)、[src/atcoder/](../../src/atcoder/) | `AtCoderClient` の HTTP・Cookie 連携と HTML 解析。Contest 情報・サンプル取得には fixture 経路もある。提出フォーム送信と結果追跡はサブモジュール | `atcoder.rs` の fixture・ローカル HTTP テスト、`atcoder/submit.rs` と `atcoder/submission_tracking.rs` の単体テスト |
| [src/runner.rs](../../src/runner.rs)、[src/comparator.rs](../../src/comparator.rs) | Test・Stress・User Input が使う C++ コンパイル、Python・実行ファイルの起動、出力取得、タイムアウト・中断。出力比較は `comparator` が担当 | 同ファイル、[tests/verdict_exit_cli.rs](../../tests/verdict_exit_cli.rs) |
| [src/stress.rs](../../src/stress.rs)、[src/user_input.rs](../../src/user_input.rs) | Stress の比較ループと反例の保存・再読込。User Input の永続化は `user_input`、実行はコマンド側 | 同ファイル、[src/commands/user_input_run.rs](../../src/commands/user_input_run.rs) |
| [src/watcher.rs](../../src/watcher.rs)、[src/commands/watch_source.rs](../../src/commands/watch_source.rs) | ファイルイベントの集約と、変更パスから問題・言語への対応付け。Plain Watch と TUI Watch が共有 | 同ファイル、[src/commands/watch.rs](../../src/commands/watch.rs)、[src/commands/watch_tui.rs](../../src/commands/watch_tui.rs) |
| [src/tui/](../../src/tui/)、[src/commands/watch_tui.rs](../../src/commands/watch_tui.rs) | `watch_tui` は画面間の遷移と Contest セッションの開始・終了を管理。`tui/mod.rs` は Contest のイベントループ、`app.rs` は状態遷移、`view.rs` は描画。Home と提出履歴は別モジュール | `watch_tui.rs`、`tui/mod.rs`、`tui/app.rs`、[src/tui/tests/](../../src/tui/tests/) |
| [src/config.rs](../../src/config.rs)、[src/settings.rs](../../src/settings.rs)、[src/settings_store.rs](../../src/settings_store.rs)、[src/paths.rs](../../src/paths.rs) | 実行用設定の既定値と検証、Global Settings の TOML 編集と保存、OS 別のパス解決 | 同ファイル、[src/settings_store/tests/macos_tests.rs](../../src/settings_store/tests/macos_tests.rs)、[tests/config_init_cli.rs](../../tests/config_init_cli.rs) |
| [src/auth.rs](../../src/auth.rs) | Cookie の読込・更新・削除、`AuthSnapshot` から作る `SessionAuth`、HTTP 応答に伴う Cookie 更新の永続化。Home の Authentication と `AtCoderClient` が利用 | 同ファイル、[src/tui/authentication_modal.rs](../../src/tui/authentication_modal.rs)、[src/atcoder.rs](../../src/atcoder.rs) |
| [src/safe_file.rs](../../src/safe_file.rs)、[src/user_config_fs.rs](../../src/user_config_fs.rs) | 上書きしない配置、ファイル置換、スナップショット、復旧レコードの共通部品。`user_config_fs` は設定・テンプレート用のファイル調査などを担当 | 同ファイル、呼び出し元の `workspace.rs`、`stress.rs`、`settings_store.rs` |

## 3. 主要な処理フロー

### Workspace 初期化

- `atc init` → `commands::init` → カレントディレクトリを渡して `workspace::initialize_workspace`。既存設定の確認と既定の `.atc-workspace.toml` の作成はここで行う。
- 引数なしの `atc` → `run_application` → `AppContext::from_launch_root`。起動ディレクトリ直下の `.atc-workspace.toml` を検査し、通常ファイルとして開ければ Workspace Home、欠損していれば Global Home に進む。種類が不正な場合や、検査・オープンに失敗した場合はエラーになる。この分岐ではファイルの内容を解析しない。Global Home から選んだディレクトリの初期化も `initialize_workspace` を呼び、初期化後に開き直す。

### Contest 作成・切替・修復

- `atc new` → `commands::new` → `workspace::contest_path` でカレントディレクトリ直下の作成先を決める。`fetch_contest_data` が問題一覧とサンプルを取得し、`new.rs` がソース・サンプル・メタデータを一時ディレクトリに組み立てて配置する。
- `atc contest` / `atc c` → `commands::contest` → 配置先を解決し、`inspect_contest_target` で状態を調べる。未作成なら作成し、要修復なら確認後に修復する。健全な Contest を `watch_tui_at` で開く。
- Workspace Home の Contest オープンと Contest 画面の切替は `watch_tui.rs` の `resolve_contest_open` → `prepare_workspace_contest_entry` をたどる。現在の Workspace 設定で行き先を解決し、作成・修復が必要なら確認を経て `contest.rs` の共通処理を呼ぶ。準備済みの入力で新しいセッションを開始し、切替時は先に旧セッションを終了する。
- 修復の分岐は `contest.rs` の `plan_repair`。健全なメタデータがあるときはそれを基準にサンプルを取り直す。欠損・不正などの場合は AtCoder の問題一覧とサンプルを取得する。計画に応じて、`workspace.rs` がメタデータのみ、サンプルのみ、または両方を置換する。

### Refresh と Recovery

- CLI の `atc refresh` → `resolve_refresh_contest_id` → `prepare_refresh` → `apply_refresh`。準備で問題一覧とサンプルを取得し、適用で `workspace::replace_refresh_data_with_snapshot` に渡す。`--force` による対象決定の違いは `resolve_refresh_contest_id` から追う。
- Contest 画面の Refresh → `contest_refresh_task` がバックグラウンドで `prepare_refresh` → `rebuild_active_contest_session_after_refresh` が旧セッションを終了し、適用後にディスクから読み直す。適用失敗時も再読込・再起動を試み、成功すれば失敗表示を引き継ぐ。この組み直しには直前の設定・認証を使う。
- ディスク上の Recovery は `workspace.rs` の置換・ロールバック処理と `contest_recovery_state` を起点に追う。`safe_file` が復旧レコードを書き、完了した作業領域を退避先に移す。メタデータやテスト入力の読込でも、確認が必要な中断状態を検出する。Stress 側の対応箇所は `stress.rs` の `ensure_no_interrupted_generation`。

### Sample Test と User Input

- `atc test` → `commands::test` → `test_problem`。TUI の Sample Test は `AttemptExecutor` から `test_problem_with_cancel` に入る。共通処理で `workspace::load_test_samples` と `stress::load_saved_case_for_task` を呼び、公式サンプルの後に再実行可能な保存済み反例を回す。`runner` が実行し、`comparator` が期待出力と照合する。
- TUI の User Input は `run_selected_user_input` → `WatchApp::enqueue_selected_user_input` → 実行ワーカー → `commands/user_input_run.rs` の `run`。保存済みの入力または編集中のバッファを標準入力にして一度実行し、実行状態・出力を返す。保存・削除は `user_input.rs` が担当し、保存成功時の実行も同じワーカー経路を使う。

### Stress Test と反例保存

- `atc stress init` と TUI の Stress 初期化は `commands/stress.rs` の `initialize_stress_files_at` で `<問題>_gen.py` と `<問題>_brute.py` を用意する。
- `atc stress` と TUI の `RunKind::Stress` → `build_request` → `stress::run`。seed ごとに生成器・解答・参照解を `runner` で動かし、実行結果と出力を比較する。反例は `finish_failure` → `persist_failure_for_task` で保存し、後続の Sample Test が保存データを読み込む。

### Plain Watch と TUI Watch

- `atc watch --plain` → `commands/watch.rs`。`FileWatcher::next_batch` で変更を集め、対応するソースを選び、その問題・言語で `test_problem` を同期実行する。
- `atc watch` → `watch_tui_at` → `ContestSession`。ファイル監視の通知を `tui/mod.rs` が受け、ソース変更時に `WatchApp` を更新して Sample Test をキューに入れる。`watch_worker.rs` と `run_scheduler.rs` が実行順と中断を扱い、`attempt_executor.rs` が Sample Test・User Input・Stress に振り分ける。結果は `Message` として TUI に戻る。

### AtCoder への提出と結果追跡

- CLI の `atc submit` は `resolve_submit_plan` で問題・ソース・ランタイムを決める。TUI は選択中のソースから計画を作り、`SubmissionHub::start` で提出時点のソースを `prepare_submit` で読み取って、提出専用ワーカーを起動する。両経路は `execute_prepared_with_client` に合流する。
- `atcoder/submit.rs` が提出ページからフォームと言語を解決し、POST 直前のフックで `atcoder/submission_tracking.rs` が提出一覧の基準の取得を試みる。POST の受理後、基準を取得できており追跡を続ける場合は、新しい提出 ID を探索し、特定できた ID の状態をポーリングする。基準を取得できなかった場合は、受理済みとして扱い、ID 探索とポーリングを省略する。受理と追跡の結果は CLI の出力または TUI の履歴に反映する。分岐は `execute_prepared_with_tracking_diagnostics` を読む。

### 設定・認証の読み込みと保存

- 実行用のグローバル設定は `Config::load` → `paths::config_file` → 既定値と TOML の解決・検証。`atc config init` は `initialize_config_at` で初期ファイルを作る。Home の Global Settings は `SettingsPage::load` → `SettingsStore::load`、編集は `SettingsDocument`、書き込みは `SettingsStore::save`。Workspace Config の参照・編集入口は `workspace.rs` と Workspace Home の `open_workspace_config`。
- `atc login` は `authentication_status` で保存済み Cookie を確認する。Home の Authentication は `AuthenticationModalController` から `AuthSnapshot` を読み、貼り付け保存・リセットは `write_explicit_credential` / `reset_explicit_credential`、通信による確認は `atcoder::verify_authentication` を呼ぶ。
- HTTP では `AtCoderClient::new` が保存済み認証を読み、TUI の Contest はオープン時に作った `SessionAuth` を共有する。サーバーからの Cookie 更新は `SessionCookieProvider` を経て `SessionAuth::persist_pending` が保存を試みる。Workspace で新しい Contest を開くと設定・認証を読み直す。

## 4. データの配置

`<workspace>` は Workspace のルート、`<contest>` は解決後の Contest ディレクトリです。
Workspace の `paths` に Contest ID と一致する規則があれば、指定された相対パスの下に Contest ID を付けます。一致する規則がなければ `<workspace>/<contest-id>` です。
`resolve_contest_target` を使うコマンドは、`--contest` なしではカレントディレクトリを対象とします。

| データ | 配置 | 実装の確認先 |
| --- | --- | --- |
| Workspace 設定 | `<workspace>/.atc-workspace.toml` | `workspace::initialize_workspace`、`resolve_contest_path`、`resolve_active_workspace_contest_path` |
| Contest メタデータ | `<contest>/.atc/contest.toml` | `workspace::save_metadata` / `load_metadata` |
| 公式サンプル | `<contest>/tests/<問題>/sample-<番号>.in` と `sample-<番号>.out`。番号は 1 から | `workspace::save_samples` / `load_test_samples` |
| Stress 反例 | `<contest>/.atc/stress/<問題>/` に `failed.in`、`expected.out`、`actual.out`、`meta.toml` と、内容がある場合は `stderr.txt` | `stress::persist_failure_for_task` / `load_saved_case_for_task` |
| User Input | `<contest>/.atc/user-inputs/<問題>/<ID>.in` と問題ごとの `meta.toml` | `user_input.rs` |
| Contest 更新の復旧データ | `<contest>/.atc-refresh-*` / `.atc-repair-*` が作業領域。処理結果に応じて `.atc-recovery-*` / `.atc-aborted-*` に退避し、`previous-tests` や `previous-contest.toml` が残る場合がある | `workspace::contest_recovery_state` / `complete_contest_staging` |
| Stress 保存の復旧データ | `<contest>/.atc/stress/` 内の `.stress-staging-*`、`.stress-recovery-*`、`.stress-aborted-*`。新旧の反例を作業領域の `new` / `previous` に置く | `stress::persist_failure_inner` / `complete_stress_recovery` |
| グローバル設定 | Windows は `%APPDATA%\atc\config.toml`。macOS / Linux は `XDG_CONFIG_HOME` 配下の `atc/config.toml`（既定 `~/.config/atc/config.toml`） | `paths::config_file` |
| Cookie | Windows は `%APPDATA%\atc\state\cookie`。macOS / Linux は `XDG_STATE_HOME` 配下の `atc/cookie`（既定 `~/.local/state/atc/cookie`） | `paths::cookie_location` |

Contest 更新と Stress 保存の作業領域では、`safe_file` が `recovery-intent.toml` と `recovery-complete.toml` を扱います。
グローバルな保存先を追うときは `paths.rs` と、依存ライブラリ `etcetera` の固定バージョンを確認してください。[Cargo.lock](../../Cargo.lock)
`choose_base_strategy` は Windows で `APPDATA` が未設定または空なら Roaming AppData の既知フォルダーを使い、取得できなければホームの `AppData/Roaming` を使います。
macOS / Linux の XDG ベースは、環境変数が絶対パスの場合に採用されます。

## 5. タスク別のコード探索ガイド

| 変更したい機能 | 最初に読む実装 | 関連テスト |
| --- | --- | --- |
| Workspace 初期化 | [commands/init.rs](../../src/commands/init.rs) の `init`、[commands/watch_tui.rs](../../src/commands/watch_tui.rs) の `run_application_locations_with` | [init_cli.rs](../../tests/init_cli.rs)、`watch_tui.rs` の `production_wide_init_here_same_batch_opens_workspace_and_discards_later_key` |
| Contest 作成・切替・修復 | [commands/new.rs](../../src/commands/new.rs)、[commands/contest.rs](../../src/commands/contest.rs) の `inspect_contest_target` / `plan_repair`、[commands/watch_tui.rs](../../src/commands/watch_tui.rs) の `prepare_workspace_contest_entry` | [commands/tests.rs](../../src/commands/tests.rs) の `new_flow_runs_entirely_from_fixtures`、`contest.rs` の `repair_with_healthy_metadata_restores_only_tests`、[contest_owner_cli.rs](../../tests/contest_owner_cli.rs) |
| Refresh・Recovery | [commands/refresh.rs](../../src/commands/refresh.rs) の `prepare_refresh` / `apply_refresh` → [workspace.rs](../../src/workspace.rs) の `replace_refresh_data_with_snapshot` | [refresh_cli.rs](../../tests/refresh_cli.rs)、[commands/tests.rs](../../src/commands/tests.rs) の `prepared_refresh_rejects_destination_metadata_changes_before_apply`、`workspace.rs` の `incomplete_contest_staging_is_not_a_completed_recovery_area`、[commands/watch_tui.rs](../../src/commands/watch_tui.rs) の `refresh_apply_failure_recovers_a_fresh_session_and_reopens_failed_modal` |
| Test・判定 | [commands/test.rs](../../src/commands/test.rs) の `test_problem` / `run_test_cases` → [runner.rs](../../src/runner.rs)、[comparator.rs](../../src/comparator.rs) | [verdict_exit_cli.rs](../../tests/verdict_exit_cli.rs) の `missing_one_official_sample_fails_before_running_solution` / `valid_saved_stress_case_runs_after_official_sample` |
| User Input | [tui/mod.rs](../../src/tui/mod.rs) の `run_selected_user_input`、[tui/app/user_input_run.rs](../../src/tui/app/user_input_run.rs)、[commands/user_input_run.rs](../../src/commands/user_input_run.rs)、[user_input.rs](../../src/user_input.rs) | [tui/tests/user_input_runs.rs](../../src/tui/tests/user_input_runs.rs) の `dirty_run_skips_sync_owns_buffer_and_never_saves`、[tui/tests/user_input_delete.rs](../../src/tui/tests/user_input_delete.rs) |
| Stress 反例 | [commands/stress.rs](../../src/commands/stress.rs) の `build_request` → [stress.rs](../../src/stress.rs) の `run` / `persist_failure_for_task` | `stress.rs` の `interrupted_replacement_blocks_a_new_stress_save_without_touching_recovery`、[verdict_exit_cli.rs](../../tests/verdict_exit_cli.rs) の `stress_counterexample_exits_nonzero_after_reporting_and_saving` |
| Watch・実行順 | [commands/watch.rs](../../src/commands/watch.rs) の `process_changed_paths`、[commands/watch_tui.rs](../../src/commands/watch_tui.rs) の `start_watcher`、[commands/watch_worker.rs](../../src/commands/watch_worker.rs) と [commands/run_scheduler.rs](../../src/commands/run_scheduler.rs) | `watch.rs` の `watch_rechecks_contest_recovery_after_startup`、`watch_worker.rs` の `different_problem_cancel_requeues_after_join_and_reruns_same_id` |
| Submit・結果追跡 | [commands/submit.rs](../../src/commands/submit.rs) の `execute_prepared_with_client` → [atcoder/submit.rs](../../src/atcoder/submit.rs) と [atcoder/submission_tracking.rs](../../src/atcoder/submission_tracking.rs) | [submit_cli.rs](../../tests/submit_cli.rs)、`commands/submit.rs` の `baseline_failure_disables_tracking_but_still_submits_once`、`submission_tracking.rs` の `polling_emits_only_changes_and_stops_at_final_verdict` |
| Auth | [auth.rs](../../src/auth.rs) の `AuthSnapshot` / `SessionAuth` / `AuthStore`、[atcoder.rs](../../src/atcoder.rs) の `SessionCookieProvider`、[tui/authentication_modal.rs](../../src/tui/authentication_modal.rs) | `auth.rs` の `cross_process_change_wins_disk_cas_while_runtime_follows_server`、`authentication_modal.rs` の `write_verify_and_reset_follow_the_shared_controller_flow` |
| Settings | [config.rs](../../src/config.rs) の `Config::load`、[tui/settings_screen.rs](../../src/tui/settings_screen.rs) の `SettingsPage` → [settings.rs](../../src/settings.rs) と [settings_store.rs](../../src/settings_store.rs) | `settings_store.rs` の `exact_external_modification_is_a_conflict`、[macOS の保存テスト](../../src/settings_store/tests/macos_tests.rs)、[config_init_cli.rs](../../tests/config_init_cli.rs) |
| TUI 状態・描画 | [tui/mod.rs](../../src/tui/mod.rs) のイベント処理、[tui/app.rs](../../src/tui/app.rs) の `WatchApp`、[tui/view.rs](../../src/tui/view.rs)。Home の操作は [tui/global_home.rs](../../src/tui/global_home.rs) と [tui/home.rs](../../src/tui/home.rs)、提出の状態は [tui/submission.rs](../../src/tui/submission.rs) | `tui/app.rs` の `stale_run_messages_are_ignored`、`tui/mod.rs` の `old_session_messages_cannot_modify_a_new_session_app`、`tui/submission.rs` の `problem_and_contest_view_changes_keep_records_and_tracking_alive` |

## 6. 文書の保守方針

- モジュールの責務や主要なデータフローが変わったときに更新する。構造を変えないバグ修正では更新は不要。
- 記述と実装が食い違う場合は、対象の本番コードと関連する回帰テストを確認する。
- 安全性の保証・拒否条件・非保証範囲は [INVARIANTS.md](INVARIANTS.md) を参照する。
- 変更時は、探索ガイドと索引から関係する資料を選ぶ。内部資料を一律に全文読む必要はない。
