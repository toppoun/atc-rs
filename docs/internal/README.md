# atc-rs 内部資料

作業に必要な資料を、下の索引から選んでください。現行の動作は、対象コミットの本番コードと関連する回帰テストを優先して確認し、ユーザーに見える手順や説明は現行のユーザー向け文書と照合します。歴史資料と食い違う場合は、現在の実装・テストとの差を確認し、根拠が足りない点は未確認として扱います。

| 資料 | 分類・状態 | 対象時点・範囲 | 現行の確認先 |
| --- | --- | --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Current architecture map | 作成時点のコード構造、モジュールの責務、主要な処理フロー | [エントリーポイント](../../src/main.rs)、[コマンド実装](../../src/commands/) |
| `INVARIANTS.md` | Current behavioral contracts（作成予定） | 作成時点の動作契約と安全性 | 作業対象の本番コード、同じモジュールのテストと[統合テスト](../../tests/) |
| [SPEC.md](SPEC.md) | Historical design | 2026-08-11 の Rust 移行設計と当時の構想 | [CLI 定義](../../src/cli.rs)、[ワークスペース実装](../../src/workspace.rs) |
| [pre-release-inventory.md](pre-release-inventory.md) | Historical audit | 2026-08-24 のリリース前監査で棚卸しした機能・操作・対応環境 | [CLI 定義](../../src/cli.rs)、[ヘルプの回帰テスト](../../tests/help_cli.rs)、[ユーザー向け README](../../README.md) |
| [macos-settings-store-p1-validation.md](macos-settings-store-p1-validation.md) | Scope-limited validation record | 2026-09-21 の macOS arm64 / APFS における Global Settings の既存 Config 保存の検証。結果は当時の環境と対象経路に限る | [Settings Store](../../src/settings_store.rs)、[macOS 固有の実装](../../src/settings_store/macos.rs)、[macOS の回帰テスト](../../src/settings_store/tests/macos_tests.rs) |
