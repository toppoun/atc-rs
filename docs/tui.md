# TUI

`atc watch` のデフォルト画面では、問題ごとの状態、サンプル結果、Expected / Actual / stderr などをターミナル上で確認できます。

`atc contest <contest-id>` も、コンテストを開いた後に同じ TUI を起動します。

## 起動

コンテストディレクトリで:

```bash
atc watch
```

ワークスペースのルートから:

```bash
atc contest abc466
```

または:

```bash
atc watch -c abc466
```

TUI が使いにくい端末では、プレーン表示を利用できます。

```bash
atc watch --plain
```

## 画面構成

- 上端: contest ID、選択中の source / language、Debug、実行状態
- 問題行: 各問題の状態と、現在選択している問題
- Samples ペイン: 公式サンプルと保存済み Stress ケース
- Detail ペイン: Expected / Actual / stderr、コンパイルや Stress の詳細
- 下端: 現在利用できる主なキー操作

端末幅が狭い場合、Samples ペインは表示されず Detail ペインが優先されます。

## キー操作

| キー | 操作 |
| --- | --- |
| `q` | TUI を終了 |
| `h` / `←` | 前の問題 |
| `l` / `→` | 次の問題 |
| `j` / `↓` | 次のテストケース |
| `k` / `↑` | 前のテストケース |
| `r` | 選択中の問題を再テスト |
| `d` | C++ Debug の切り替え |
| `s` | Samples ペインの表示切り替え |
| `S` | Stress Helper を確認し、準備済みなら Stress Test を開始 |
| `i` | 必要な Stress Helper を作成 |
| `c` | Contest を切り替え |
| `:` | Command Palette を開く |

## マウス操作

マウスホイールにも対応しています。

- Samples ペイン上: テストケースを移動
- Detail ペイン上: 詳細をスクロール

Detail のスクロールバーはクリックとドラッグにも対応しています。

Detail 内のセクション見出しを左クリックすると、そのセクションを折りたたみ・展開できます。モーダルや Command Palette を開いている間は、背後のマウス操作を処理しません。

## Command Palette

`:` を押すと Command Palette が開きます。

文字を入力してコマンドを絞り込み、`↑` / `↓` で選択、`Enter` で実行します。`Backspace` で入力を消し、`Esc` で閉じます。

利用できる主な操作:

- Run Tests
- Open Source
- Open Settings
- Open Workspace Settings
- Open Template
- Toggle Debug
- Toggle Samples
- Start Stress
- Stop Stress
- Initialize Stress
- Refresh Contest
- Switch Contest

現在の状態で実行できない操作は、理由付きで unavailable と表示されます。

## Open Source

Command Palette の `Open Source` から、選択中の問題のソースをエディタで開けます。

C++ / Python を選択でき、まだソースが存在しない場合は `i` で作成してから開けます。

```text
Enter  既存ファイルを開く
i      ファイルを作成して開く
↑/↓    言語を選択
j/k    言語を選択
Esc    閉じる
```

新しく作るソースには通常のソーステンプレートが使われます。

## Open Settings

グローバル設定ファイルを開きます。

設定ファイルがまだない場合は `i` で `atc config init` 相当の初期化を行い、そのままエディタで開けます。

## Open Workspace Settings

ワークスペースから TUI を起動している場合、`.atc-workspace.toml` を開けます。

この項目は TUI 起動時の**正確なワークスペースルート**に基づきます。親ディレクトリのワークスペースは自動検索しません。

workspace 外から起動した場合は unavailable になります。workspace config の新規作成は TUI ではなく、対象ディレクトリで `atc init` を実行します。

## Open Template

C++ / Python の通常ソーステンプレートを開けます。

テンプレートがまだない場合は `i` で初期化して開けます。

```text
Enter  既存テンプレートを開く
i      テンプレートを初期化して開く
↑/↓    言語を選択
j/k    言語を選択
Esc    閉じる
```

## エディタ連携

エディタは次の順で決定されます。

1. `config.toml` の `[editor]`
2. Windows / macOS で VS Code / Cursor の統合ターミナルを自動検出
3. `VISUAL`
4. `EDITOR`

### Terminal editor

Vim / Neovim などは通常 `terminal` モードで起動します。

```toml
[editor]
command = "nvim"
mode = "terminal"
```

TUI の端末制御を一時的に戻してエディタを起動し、終了後に TUI を復元します。

Vim / Neovim などのエディタが終了するまで `atc` は待機します。復帰時には TUI の端末モードと入力処理を作り直すため、エディタ起動前に溜まっていたキー入力は再生されません。

### External editor

VS Code などは `external` モードで起動できます。

```toml
[editor]
command = "code"
args = ["-r"]
mode = "external"
```

TUI を終了せず、外部エディタを別プロセスとして開きます。

詳しくは [設定](configuration.md) を参照してください。

## Stress Test

`S` を押したとき、Stress Helper がなければすぐにテストを始めるのではなく、まずセットアップが必要な状態として表示されます。

その状態で `i` を押すと Helper を作成します。作成しただけでは Stress Test は自動開始されません。Helper を編集した後、もう一度 `S` を押して開始します。

詳しくは [ストレステスト](stress.md) を参照してください。

## Contest の更新

Command Palette の `Refresh Contest` で、現在の contest の問題情報とサンプルを更新できます。更新処理は開始後にキャンセルできません。

取得と安全確認が完了してから監視セッションを停止してデータを入れ替え、更新後の内容で TUI を再開します。通常の `atc refresh` と同様に source は上書きしません。

## Contest の切り替え

ワークスペースから起動した TUI では `c` または Command Palette の `Switch Contest` を利用できます。

コンテスト ID を入力すると、同じワークスペース設定を使って対象コンテストへ切り替えます。存在しないコンテストは必要に応じて作成されます。
