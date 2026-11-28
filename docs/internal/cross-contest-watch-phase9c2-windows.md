# Cross-Contest Watch Phase 9C-2 Windows 最終回帰確認

2026-11-27。Windows 11 25H2（26200.9974）、ローカル NTFS、Ryzen 7 9800X3D、Rust 1.97.1（`x86_64-pc-windows-msvc`）、Python 3.14.4 で実施した。比較元は Windows Phase 9B の `40ac947`、比較先は macOS Phase 9C-1 の修正を含む `6591718`。両方のリリース版を同じ Windows 環境でビルドした。テストと実機検証は AppContainer 外の通常ユーザー権限（中整合性、`S-1-16-8192`）で実行した。[Phase 9B Windows](cross-contest-watch-phase9b-windows.md)・[Phase 9C-1 macOS](cross-contest-watch-phase9c1-macos.md)・[OS 別検証手順](cross-contest-watch-validation.md)

**判定は公開準備へ進行可。** Phase 9C-1 の共通コード修正を反映した Windows リリース版で、上限内の有効なマニフェストのピーク Working Set が低下した。16 MiB 上限、対象世代の失効、他 Contest の継続、復元後の復旧を Windows と macOS で確認した。機能の公開を妨げる問題は確認されず、Cross-Contest Watch の実装・最適化・検証工程は完了とする。

## 共通コードと Windows への影響

- Phase 9B は診断とスナップショットの読込を 16 MiB に制限した。Phase 9C-1 の調査では、空の `Vec` から `read_to_end` で読むと 16 MiB のファイルに 32 MiB の容量を確保していた。診断・再照合、保存受付、実行前検査の読み直しで同じ確保が繰り返された。修正後は開いたファイルの属性が上限内と確認できた時点で、そのサイズを `try_reserve_exact` に渡す。上限超過は確保前に拒否し、読込中に増加した場合は上限と判定用の 1 バイトまで読む。既存の境界テストに容量のアサーションを追加した。[共通読込](../../src/safe_file.rs)・[境界の回帰テスト](../../src/workspace.rs)
- 両コミットの共通読込関数をそのまま取り出し、Windows の実ファイルで容量を測定した。16,777,215 バイトでは 16,777,216 → 16,777,215 バイト、16,777,216 バイトでは 33,554,432 → 16,777,216 バイト。16,777,217 バイトは両方で `InvalidData` になった。
- Windows では従来の no-follow オープン、通常ファイルと reparse point の判定、ボリュームとファイル ID による実体照合を通る。スナップショットの内容・実体・ポーリング用属性は同じハンドルから取得する。上位の配置、所有する Contest ID、Recovery 状態、世代の再検証も従来の経路を通った。notify 8.2.0 の非再帰の登録、`.atc` を常設監視しない構成、ソースの `PathBuf` の扱いは今回の差分に含まれない。[安全なファイル操作](../../src/safe_file.rs)・[診断](../../src/workspace/diagnosis.rs)・[Workspace 監視](../../src/commands/workspace_watch.rs)・[notify のアダプター](../../src/watcher.rs)

## マニフェスト再照合と復旧

同じ ConPTY ハーネスで、ビルドの順番を交互にして比較した。各試行は新しいプロセス、隔離した APPDATA、2 件の合成 Contest を使う。マニフェストを同じディレクトリ内で置換し、ソースの保存、ConPTY の表示、Python が出力した実行記録を照合した。Refresh と Migrate はローカルの AtCoder fixture を使用した。

| 確認項目 | Windows での結果 |
| --- | --- |
| 正常・境界 | 通常サイズのマニフェストと有効な TOML の 16,777,215／16,777,216 バイトで保存を受け付け、`1/1 AC` を表示した。通常通知による保存では別 Contest への自動切り替えを確認した。 |
| 読み込み中の増加 | 実ファイル 8 バイトを開き、最初の読込後に別ハンドルから 32 バイトを追記する既存テストが成功した。テスト内で 16 バイトに縮小した上限を使い、`InvalidData`、読取位置 17 バイト、ファイルの最終サイズ 40 バイトを確認した。 |
| 上限超過・旧世代 | 表示中のマニフェストを 16,777,217 バイトの有効な TOML に置換すると世代変更が表示され、旧結果が消えた。`r` と対象ソースの保存では子プロセスが起動しなかった。非表示 Contest の 64 MiB 置換でもその Contest の保存による実行・自動切り替えは起きなかった。監視の回帰テストでは旧世代の購読が拒否され、別 Contest の `RunScope` が維持された。 |
| reparse point・所有権 | マニフェストファイルを同じ内容の外部ファイルへの symlink に置換し、`.atc` を元の内容へ向く junction に置換し、別 Contest ID を持つ有効な TOML に置換した。すべて旧結果が消え、対象の保存による実行を拒否した。両ビルドで、各ケース中に健全な Contest は実行し、元の通常ファイルまたはディレクトリを戻した後に対象の保存を受け付けた。 |
| 健全な Contest と復旧 | 1 バイト超過と 64 MiB 置換の各試行で、健全な Contest は 1 回実行し `1/1 AC` を表示した。元のマニフェストを復元し、Atomic Save で対象の 1 回の実行、自動切り替え、`1/1 AC` を確認した。監視の回帰テストでは復元前後の `RunScope` が異なることも確認した。 |

### メモリの前後比較

16 MiB の有効な TOML は、同じ正常マニフェストに長いコメント 1 行または 80 バイトの短いコメントの繰り返しを加え、各ビルドで各 6 回測定した。境界 3 サイズの連続置換と 64 MiB 置換は各ビルドで各 3 回。置換直後から 2.5 秒間、実測 161〜163 サンプルを取得し、その後で対象ソースを保存した。終了時にプロセス生存期間のピークを取得した。数値は `GetProcessMemoryInfo` の Windows 指標で、単位は MiB、表は全試行の範囲。Private Bytes は `PrivateUsage`、ピーク Commit Charge は `PeakPagefileUsage`、ピーク Working Set は `PeakWorkingSetSize` を使った。[Microsoft のカウンター定義](https://learn.microsoft.com/en-us/windows/desktop/api/psapi/ns-psapi-process_memory_counters_ex)

| 条件 | 指標 | Phase 9B | Phase 9C-1 適用後 |
| --- | --- | ---: | ---: |
| 16 MiB、コメント 1 行 | 再照合区間の最大観測 Private Bytes | 35.19–131.32 | 35.15–35.96 |
| 同上 | 保存前のピーク Commit Charge | 131.30–132.48 | 115.36–116.16 |
| 同上 | 全操作のピーク Commit Charge | 227.80–276.82 | 243.59–244.55 |
| 同上 | 全操作のピーク Working Set | 93.09–103.86 | 76.73–77.56 |
| 16 MiB、短いコメント | 再照合区間の最大観測 Private Bytes | 140.65–142.31 | 124.70–126.38 |
| 同上 | 保存前のピーク Commit Charge | 140.70–142.35 | 124.72–126.44 |
| 同上 | 全操作のピーク Commit Charge | 238.66–295.15 | 163.39–264.49 |
| 同上 | 全操作のピーク Working Set | 103.11–112.67 | 84.49–95.85 |
| 境界 3 サイズを連続置換 | 16 MiB の再照合区間の最大観測 Private Bytes | 156.99–157.80 | 141.01–141.37 |
| 同上 | 全操作のピーク Commit Charge | 295.13–295.87 | 263.04–263.33 |
| 同上 | 全操作のピーク Working Set | 109.23–112.93 | 95.78–96.20 |
| 64 MiB 置換 | 置換前 → 再照合区間の最大観測 Private Bytes | 2.77–3.25 → 2.77–3.25 | 3.12–3.94 → 3.12–3.95 |
| 同上 | 置換前からの最大観測 Private Bytes 増分 | 0.00 | 0.05 |
| 同上 | 再照合区間のピーク Working Set | 12.19–12.40 | 12.30–12.36 |
| 同上 | 復旧と保存を含む全操作のピーク Working Set | 13.75–13.91 | 13.83–13.90 |

上限内の単独置換では再照合時のピーク Commit Charge が両 TOML 構成で約 16 MiB 下がった。保存・実行・自動表示を含むピーク Working Set は、上限内と境界の各条件で修正前の全試行を下回った。プロセス全体のピーク Commit Charge は連続境界試験で約 32 MiB 下がり、16 MiB 単独置換の試行範囲は両ビルドで重なった。64 MiB の拒否時にはメモリの大幅な増加は確認されなかった。

macOS の Phase 9C-1 では、同じ修正で 16 MiB 単独置換のプロセス最大 RSS がコメント 1 行で 181.12–292.28 → 79.88–79.92 MiB、短いコメントで 280.19–296.20 → 103.84–104.00 MiB になった。64 MiB 置換後の RSS 増分は最大 0.11 MiB だった。Windows の Working Set・Commit Charge と macOS の RSS は各 OS 内の前後で比較した。[Phase 9C-1 の計測](cross-contest-watch-phase9c1-macos.md#マニフェスト上限と復旧)

## Windows リリース版の操作確認

| 確認項目 | 結果 |
| --- | --- |
| ON / OFF と設定反映 | Doctor は省略時の `false` を built-in と表示した。Global Settings から ON を保存し、別 Contest の保存で実行・自動切り替えした。稼働中に設定ファイルを OFF にしても手動・自動の切り替え中は ON が続いた。Home からの再入場で OFF を反映し、現在の Contest の保存だけを実行した。 |
| 通常保存・Atomic Save・結果保持 | ON では表示中の Contest への上書き保存と非表示 Contest への Atomic Save で各 1 回実行した。非表示 Contest へ自動で切り替わった後、手動でそれぞれの Contest に戻り、B 問題に保存済みの `1/1 AC` が表示された。手動切り替えによる追加実行はなかった。 |
| Refresh・Migrate | TUI の Refresh で欠けたサンプルを修復した。外部 CLI の Migrate と Refresh をそれぞれ実行し、旧結果の消去、新世代のサンプル実行、Migrate 時の独自テストのバックアップを確認した。 |
| Home・再入場・ハンドル解放 | 両ビルドで `GetProcessHandleCount` は監視中 136、Home 復帰後 126。監視中の Contest 直下を共有なしで開くと `ERROR_SHARING_VIOLATION`、Home 復帰・終了後は両 Contest を共有なしで開き、対象をリネームできた。マニフェストと `.atc` は監視中のアイドル時にも共有なしで開けた。Home 滞在中の保存は実行せず、ON のまま再入場すると Atomic Save で受付を再開した。 |
| 実行中の終了 | Python サンプルの実行中に `q` で終了コード 0 を得た。その時点で Python プロセスも終了済みで、Contest ディレクトリをリネームできた。ConPTY 出力で代替画面、カーソル、マウス、ブラケットペーストの解除を確認した。 |

操作確認の初回は、作業ディレクトリの Workspace ルートも Home で共有なしに開けるとハーネスが仮定して失敗した。2 回目は手動表示で結果が復元された画面に自動表示時と同じファイル名が出ると仮定して待機した。監視中と Home の比較対象を両 Contest 直下とし、手動切り替えの B 問題名と結果を直接照合して両ビルドで再実行した。Cargo の全回帰とは別の試行。

## 全回帰と警告

| コマンド | 結果 |
| --- | --- |
| `cargo test --locked --all-features --no-fail-fast` | **2,319 成功、0 失敗、15 ignored**。ユニット 2,227 件、統合 92 件。初回成功。 |
| `cargo fmt --all -- --check` | 成功。 |
| `cargo check --locked --all-features` | 成功。警告 0 件。 |
| `cargo clippy --locked --all-targets --all-features` | 成功。既存警告は bin 13 件、bin test 13 件、重複 10 件。 |
| `cargo build --locked --release` | 成功。既存警告 1 件。 |
| `git diff --check` | 成功。 |

全回帰の前に読み込み中の増加 1 件、16 MiB 境界 1 件、Workspace 監視 21 件、旧世代のコマンド・完了拒否 1 件を実行し、すべて初回に成功した。既知の認証モーダルの描画回数テストと、Windows のコンソール Ctrl+C を送信する統合テスト 3 件は、全回帰の初回で成功した。フィルター付きで再実行した子プロセスのユニットテスト 3 件は重複集計から除外した。Phase 9B のログとテスト名・結果を照合して一致を確認した。Clippy とリリースビルドも Phase 9B の警告の内容と発生元を比較し、**新規警告は 0 件**だった。

| 実機の最終結果 | ユニット成功 | 統合成功 | 合計成功 | 失敗 | ignored |
| --- | ---: | ---: | ---: | ---: | ---: |
| Windows Phase 9B | 2,227 | 92 | 2,319 | 0 | 15 |
| Windows Phase 9C-2 | 2,227 | 92 | 2,319 | 0 | 15 |
| macOS Phase 9C-1 | 2,234 | 83 | 2,317 | 0 | 17 |

Phase 9B で追加した 3 件は、読込中の増加、サイズ境界、64 MiB 置換時の対象世代の失効・他 Contest の継続・復旧を確認する。Phase 9C-1 は既存の境界テストを強化したため件数は同じ。成功数の OS 間の純差 2 件は、Windows で実行する Ready 前の保存回収テスト 1 件と、コンソール Ctrl+C、junction、DACL、FSEvents、POSIX 属性、ACL、AppKit などの OS 別テストの純差 1 件。macOS の ignored には Unix 専用の detached pipe holder ヘルパー 2 件が含まれる。[Phase 9A の OS 別内訳](cross-contest-watch-phase9a-macos.md#全回帰)・[Phase 9C-1 の全回帰](cross-contest-watch-phase9c1-macos.md#全回帰と警告)

## リリース準備

- README とインストール・Testing・TUI・設定の各ガイドを、現行の設定・監視経路と照合した。既定 OFF、`[watch]` の `cross_contest`、Global Settings の `Watch`、Doctor の設定値・出所、Home からの再入場での適用に一致した。ON の自動切り替え、通常切り替えの結果保持、再照合による回収で画面を切り替えない仕様もコードと一致した。[README](../../README.md)・[インストール](../installation.md)・[Testing](../testing.md#cross-contest-watch)・[TUI](../tui.md#global-settings)・[Global Config](../configuration.md#cross-contest-watch)
- ARCHITECTURE と INVARIANTS の監視・実行のライフサイクル、世代の失効、16 MiB 上限を確認した。上限の対象経路は診断とマニフェスト再照合。[ARCHITECTURE](ARCHITECTURE.md#plain-watch-と-tui-watch)・[INVARIANTS](INVARIANTS.md#5-watch--tui-consistency)
- `Cargo.toml` と `Cargo.lock` のルートパッケージは `0.1.0-rc.13` で一致した。Windows の notify 8.2.0、macOS の notify 9.0.0、共通の notify-types 2.3.1 を両ターゲットの依存木で確認した。ルートパッケージを除く通常・ビルド依存は Windows 246 件、macOS 243 件で、全件が依存ライセンス一覧に掲載されていた。cargo-about 0.9.2 で再生成し、CI と同じ比較処理でパッケージ名・バージョン・掲載回数の一致を確認した。[Cargo.toml](../../Cargo.toml)・[Cargo.lock](../../Cargo.lock)・[依存ライセンス](../../THIRD_PARTY_LICENSES.html)・[通知文書](../../THIRD_PARTY_NOTICES.md)
- Release ワークフローはタグとパッケージ版を照合し、ライセンスを検証する。Windows x86_64 と macOS arm64 でテスト・ビルドし、両プラットフォームのバイナリにライセンスと通知文書を同梱する。Windows は静的 CRT の ZIP、macOS は arm64 Mach-O の tar.gz。両ジョブの成功後に `SHA256SUMS` を生成して GitHub Release を作成する。既存タグ `v0.1.0-rc.13` は `ea05036` を指している。この変更を配布する際には新しい版と対応するタグが必要。今回の照合は構成とローカル検証までで、バージョン変更、タグ作成、配布ワークフローの起動は実施していない。[Release ワークフロー](../../.github/workflows/release.yml)・[GitHub ホストランナーの構成](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)

## 既知の制約

- Linux 実機、自然に発生した OS 通知の飽和、数時間以上の連続運転は未検証。
- 初回指紋取得前の変更は比較の基準に含まれ、再照合で回収できない。Source Snapshot は未導入。
- Windows の大規模 Workspace の初回起動時間は Phase 9B の測定範囲に限る。1,000 Contest で初回 Open の中央値は新規データで約 4.08 秒、ウォームで約 1.32 秒。今回の 2 Contest による回帰確認では大規模起動の再計測をしていない。[Phase 9B の起動計測](cross-contest-watch-phase9b-windows.md#最終性能測定)

比較ビルド、検証スクリプト、初回を含むログはローカルの `target/phase9c2` に保存した。リリース版の検証は合成 Workspace とローカル fixture に限定した。実 AtCoder への通信・提出は実施していない。
