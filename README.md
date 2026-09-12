# atc

Fast AtCoder workflow from your terminal.

AtCoder のコンテスト環境の作成、サンプルテスト、ファイル監視、ストレステスト、提出を、
ターミナルからまとめて扱うための CLI / TUI ツールです。

![atc demo](docs/assets/demo.gif)

## インストール

[GitHub Releases](https://github.com/toppoun/atc-rs/releases) では、次の環境向けにビルド済みバイナリを配布しています。

- Windows x86_64
- macOS Apple Silicon

### Windows / Scoop

```powershell
scoop bucket add atc https://github.com/toppoun/scoop-atc
scoop install atc/atc
```

### macOS / Homebrew

```bash
brew install toppoun/atc/atc
```

Rust 環境がある場合は、repository からインストールすることもできます。

```bash
cargo install --git https://github.com/toppoun/atc-rs.git --locked
```

インストール後は、次のコマンドで環境を確認できます。

```bash
atc doctor
```

## クイックスタート

```bash
mkdir atcoder
cd atcoder
atc init
atc
```

作業用ディレクトリを作り、`atc init` で atc workspace として初期化し、`atc` を起動すると Workspace Home が開きます。
`c` で contest ID を入力すると、既存 contest を開くか、問題情報とサンプルを取得して新しく作成できます。

## 起動画面

`atc` を引数なしで起動したときは、正確な現在ディレクトリだけを調べます。

- 現在ディレクトリが workspace なら Workspace Home を開く
- workspace でなければ Global Home を開く

親ディレクトリの workspace は自動探索しません。

### Global Home

workspace 外で `atc` を起動したときに開く補助的な入口です。Explorer で directory を探して既存 workspace を開けます。
通常の directory を選んで Open すると、確認後にその場所を Init Here して workspace 化し、Workspace Home を開けます。
`G` で global config、`t` で C++ / Python の user source template を editor で開けます。`a` は authentication cookie を開かず、安全なinspectionによる設定状態とpathを表示します。`?` は Explorer Shortcuts を表示します。

### Workspace Home

workspace root で `atc` を起動すると直接開きます。contest を開く、または作成して Contest 画面へ進む入口です。
`w` で workspace config、`G` で global config、`t` で global な C++ / Python source template を editor で開けます。`a` はauthentication cookieの安全な設定状態とpathを表示します。
Contest 画面からも Command Palette の `Back to Workspace Home` で戻れます。

現在、Workspace Home から別 workspace への切り替えはできません。別 workspace へ移る場合は、一度終了して移動先で `atc` を起動してください。

## 主な機能

- Workspace Home / Global Home と directory Explorer
- 問題・ケース・実行結果を確認できる Contest TUI
- 公式サンプルの実行とソース保存時の自動テスト
- 任意の stdin を作成・編集・保存・個別実行・削除できる User Input
- 反例を探索して保存する Stress Test
- CLI / TUI からの AtCoder 提出、source / language の選択、Python runtime の設定
- 提出 status の追跡、最新 receipt、TUI の submission history 表示
- 問題情報とサンプルの Refresh、および既存 contest の Repair
- ソース、設定、テンプレートを開く editor 連携
- Config / source template による実行環境のカスタマイズ
- `atc doctor` によるローカル環境の診断

User Input は公式サンプルとは別に任意の stdin を保存し、1件ずつ実行できます。外部で変更された保存済み入力も TUI へ同期されます。

## TUI

Workspace Home から contest を開くほか、`atc contest` や `atc watch` から Contest TUI を直接起動できます。

Contest 画面の主な操作:

| キー | 操作 |
| --- | --- |
| `h` / `←` | 前の問題 |
| `l` / `→` | 次の問題 |
| `j` / `↓` | 次のケース |
| `k` / `↑` | 前のケース |
| `r` | 選択中の問題を再テスト |
| `t` | Submit |
| `s` | side pane の表示切り替え |
| `v` | Samples / Submissions の切り替え |
| `d` | C++ Debug の切り替え |
| `S` | Stress Test |
| `i` | 必要な Stress Helper の作成 |
| `c` | Contest の切り替え（workspace 内のみ） |
| `:` | Command Palette |
| `q` | 終了 |

User Input の作成・編集・保存・実行・削除、Submit modal、submission history、Workspace Home への戻り方など、詳しい操作は [TUI](docs/tui.md) を参照してください。

Contest の source template は Command Palette の `Open Template` から開きます。Contest の `t` は引き続き Submit です。

## Submit と AtCoder 認証

解答は CLI の `atc submit <problem>` または Contest TUI の `t` から提出できます。C++ / Python source を選択でき、Python は設定または CLI の `--runtime` で runtime を選べます。提出後は AtCoder の submission status を追跡し、TUI では最新 receipt と history も表示します。

提出には AtCoder の `REVEL_SESSION` cookie を atc の cookie file へ手動で配置する必要があります。file には `REVEL_SESSION=<value>` の1行だけを保存します。
Home の Authentication Cookie action は credential file を開かず、安全なinspectionによる設定状態とpathだけを表示します。missing file は自動生成しません。

```bash
atc login
```

`atc login` は username / password を入力してログインするコマンドではありません。設定済み session の有効性を確認し、未設定の場合は cookie file の保存場所と必要な形式を表示します。詳しくは [AtCoder 認証](docs/authentication.md) を参照してください。

提出結果を確定できない場合は、安全のため自動再送しません。
また、そのアプリケーションセッション中は同じコンテスト・問題への再提出をブロックします。
AtCoder の My Submissions で提出状況を確認してから再試行してください。

## Standalone contest

Global Home を経由せず、明示した contest を直接開く従来の導線も利用できます。

```bash
atc contest abcXXX
atc c abcXXX
```

workspace root では workspace の振り分け設定を使います。workspace でない directory でも、その実行ディレクトリ直下の contest を開く、または作成して Contest TUI を起動できます。

## コマンド

| コマンド | 説明 |
| --- | --- |
| `atc` | 現在地に応じて Workspace Home または Global Home を開く |
| `atc init` | 現在ディレクトリを workspace として初期化 |
| `atc new <contest>` | 実行ディレクトリ直下に contest を作成 |
| `atc contest <contest>` / `atc c <contest>` | contest を開く、または作成して TUI を起動 |
| `atc refresh` | 問題情報とサンプルを更新 |
| `atc test <problem>` | サンプルと保存済み Stress case を実行 |
| `atc watch` | ソースを監視して自動テスト |
| `atc stress <problem>` | Stress Test を実行 |
| `atc stress init <problem>` | Stress Helper を作成 |
| `atc submit <problem>` | 解答を AtCoder へ提出して status を追跡 |
| `atc create <name>` | ソースファイルを作成 |
| `atc template init [language]` | user source template を作成 |
| `atc config init` | global 設定ファイルを作成 |
| `atc login` | 設定済み AtCoder session の認証状態を確認 |
| `atc doctor` | ローカル環境を診断 |

各コマンドの詳しい option は `--help` で確認できます。

```bash
atc --help
atc contest --help
atc submit --help
```

## 設定

`atc` は設定ファイルなしでも使用できます。

```bash
atc config init
```

デフォルト言語、C++ compiler、Python、timeout、提出 runtime、editor などを変更できます。
詳しくは [設定](docs/configuration.md) を参照してください。

## テンプレート

C++ / Python の source template を自分用に変更できます。

```bash
atc template init cpp
```

作成された template を編集すると、それ以降に作成する source file へ反映されます。
詳しくは [テンプレート](docs/templates.md) を参照してください。

## 動作環境

C++ を使用する場合は、C++23 に対応した compiler が必要です。デフォルトでは `g++` を使用します。

Python を使用する場合は `python` command が必要です。Stress Test の Generator / Brute Force にも Python を使用します。

環境に問題がある場合は、まず `atc doctor` を実行してください。

## ドキュメント

- [設定](docs/configuration.md)
- [ワークスペース](docs/workspace.md)
- [テストと Watch](docs/testing.md)
- [TUI](docs/tui.md)
- [ストレステスト](docs/stress.md)
- [テンプレート](docs/templates.md)
- [AtCoder 認証](docs/authentication.md)
- [トラブルシューティング](docs/troubleshooting.md)

## License

[MIT License](LICENSE)
