# macOS Settings Store P1 検証記録

2026-09-21の開発者向け検証記録。対象はGlobal Settingsの既存Config保存経路。

## 実行環境と作業状態

- macOS 26.6.2 (25G83)、Darwin arm64、APFS。
- Rust 1.97.1、host `aarch64-apple-darwin`。
- 開始時のbranchは `wip/settings-macos`。tracked / untrackedの既存変更なし。
- 検証対象は一時ディレクトリのfixture。実ユーザーのGlobal Configは使用していない。
- commit、push、release、stash、reset、checkoutは行っていない。

## 修正前の再現

本番のmetadata copy直後に呼ばれる既存のstaging sync hookで、別プロセスの
`chmod` / `xattr` コマンドを実行した。本文、inode、mode、mtimeが変わっていないことも確認した。

```text
cargo test --locked --all-features settings_store::tests::macos -- --nocapture
```

次の2テストが修正前に失敗した。

- `acl_change_after_metadata_copy_is_a_conflict`
- `xattr_change_after_metadata_copy_is_a_conflict`

両方とも、期待した `Conflict` に対して `Ok(Saved)` が返り、本文とinodeが置換され、
コピー後に追加・更新された外部メタデータが失われた。staging自体は削除されていた。

## 修正内容

- macOSのbaselineにACLのバイナリ表現と、xattrの名前・値の完全な集合を含めた。
  xattrの列挙順は比較結果に影響しない。属性の名前・値はDebugに表示しない。
- no-followで開いた同一descriptorから本文とメタデータを検証する。
  メタデータを2回取得し、前後のstat（ctimeを含む）も比較する。
- metadata copyを `fcopyfile(COPYFILE_METADATA)` に変更し、元ファイルの
  no-follow descriptorと既存のstaging descriptorを使用する。
- stagingのmode・ACL・xattrがbaselineと一致することを確認した後、
  元Configの最新snapshotをbaselineと比較し、既存のatomic renameへ進む。
- 権限不足、取得エラー、読み取り途中の変化、コピー結果の不一致、検証サイズ上限超過は
  置換前に拒否する。xattrの名前一覧と値の合計上限は16 MiB。
- ACLなしを表すDarwinの `ENOENT` と、取得失敗を区別する。
  `EACCES` / `ENOTSUP` 等を空のACL/xattrとして扱わない。
- 最後の検証とrenameの間を含め、任意の外部writerとの完全なCASは保証しない。
  既存の製品契約を維持している。

## 実機で確認した範囲

| 項目 | 結果 |
| --- | --- |
| modeの変更 | 保存前・metadata copy後・no-op saveで競合を検出 |
| ACLの追加・変更・削除 | 同上。コピー後の変更でも元Configとdraftを保持 |
| xattrの追加・同じ長さの値変更・削除 | 同上。本文が同一でも競合を検出 |
| snapshot取得中の変更 | statの変化で拒否 |
| xattrのサイズ測定後の増加・縮小・消失 | 実際のfd xattr APIを使用して拒否を確認。空からの増加も対象 |
| 正常保存 | mode、複数ACL entry、binary / empty xattr、resource forkを保持 |
| atomic replace | 保存前から開いたfdは旧本文・旧inodeを保持し、置換後のpathは完全な新本文・新inodeを参照 |
| 保存後baseline | no-op saveと次の編集・保存が成功 |
| staging側のmode・ACL・xattr不一致 | replaceを呼ばず拒否し、元Configとdraftを保持 |
| ACL / xattr読み取り拒否 | 元Configを置換せず拒否し、外部から設定した拒否状態を保持 |
| 17 MiBのresource fork | 元Configを置換せず検証上限で拒否 |
| staging cleanup | 成功・競合・検証失敗・既存のsync / replace失敗テストで残留なし |
| symlink / FIFO / no-clobber / post-commit分類 | 既存テストを含めて成功 |

## Focused review、修正、closure review

focused reviewはこの作業内で実施した。別agentによる独立レビューではない。

- 元Configだけを再検証すると、コピー中の一時的な変更を取り込んだstagingを
  見逃しうるため、stagingとbaselineの一致も置換前に検証した。
- xattrが空の場合にも2回目のAPI呼び出しをサイズ測定にしないよう、非ゼロのbufferを使用した。
- ACLのバイナリ出力にはu32 alignmentを持つbufferを使用した。
- snapshotの `Interrupted` 分類の変更はmacOSに限定した。
- 全体テストで既存Settings画面テストの可搬性バグを発見した。
  `old` が画面に含まれないという検証が、macOSの `/var/folders` に反応していた。
  識別可能な古い設定値に変更し、元の設定が読み込めることとInvalid Config画面も検証する。
  製品のpath処理やassertionの意図は変更していない。

closure reviewでは最終差分、FFIのサイズ・解放・エラー処理、取得不能時の拒否、
staging cleanup、保存後の検証、非macOS分岐、ドキュメントの保証範囲を再確認した。
対象範囲内の未解決findingはない。

## 最終検証結果

| コマンド | 結果 |
| --- | --- |
| `cargo test --locked --all-features settings_store:: -- --nocapture` | 30 passed |
| 修正したSettings画面テストの単独実行 | 1 passed |
| `cargo fmt --all -- --check` | 成功 |
| `cargo check --locked --all-features` | 成功 |
| `cargo clippy --locked --all-targets --all-features` | 成功、既存warningあり |
| `cargo test --locked --all-features --no-fail-fast` | unit 1,958 passed / 17 ignored、integration 23 passed、失敗なし |
| `git diff --check` | 成功 |

全体テストの初回はsandboxがsocket作成、cache書き込み、ファイル監視を制限した。
許可されたsandbox外で再実行し、前述の可搬性テスト修正後に成功した。
17件のignoredは既存の子プロセス用helperと手動測定であり、今回追加したテストにskipはない。
既存のunused `permissions`、引数数、enumサイズ、`drop_non_drop` のwarningは対象外として残した。

## 検証の限界とWindowsへの影響

- 実機検証は上記macOS / arm64 / APFSに限定する。Intel Mac、別macOS version、
  外部・network filesystem、圧縮ファイル、停電時の耐久性は未検証。
- 実際のGlobal Settings画面を手操作する検証は行っていない。保存層と画面の自動テストを実行した。
- Windowsの `ReplaceFileW`、DACL、protected attributes、named streams保持処理は変更していない。
  共通のConflict文言にmetadataを追加したが、Windowsの保存動作やエラー分類は変更していない。
- Windows実機は未検証。通常のWindows検証では既存のprotected attributes / custom DACL保持、
  reparse point拒否、no-clobber、replace失敗・post-commit分類のテストを確認する。

## API確認資料

SDKの `sys/acl.h`、`sys/xattr.h`、`copyfile.h` に加えて、次のApple公開実装で
ACLの表現と「ACLなし」の扱いを確認した。

- [acl_translate.c](https://github.com/apple-oss-distributions/Libc/blob/main/posix1e/acl_translate.c)
- [acl_file.c](https://github.com/apple-oss-distributions/Libc/blob/main/posix1e/acl_file.c)
- [filesec.c](https://github.com/apple-oss-distributions/Libc/blob/main/gen/filesec.c)
