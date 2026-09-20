# atc

AtCoder のコンテスト準備、サンプルテスト、提出までをターミナルでまとめて行う CLI / TUI ツールです。

![atc demo](docs/assets/demo.gif)

## できること

- コンテストを作成し、問題ごとのソースと公式サンプルを用意する
- 画面上で問題を切り替え、サンプルテストを実行する
- ソースの保存を監視して、自動で再テストする
- Generator と Brute Force を使って Stress Test を行う
- C++ / Python の解答を AtCoder へ提出し、結果を確認する
- ソーステンプレート、実行コマンド、エディタなどを自分の環境に合わせる
- AtCoder の認証情報を画面上で安全に設定・確認する

## インストール

ビルド済みバイナリは Windows x86_64 と macOS Apple Silicon 向けです。

### Windows

[Scoop](https://scoop.sh/) を使います。

```powershell
scoop bucket add atc https://github.com/toppoun/scoop-atc
scoop install atc/atc
```

### macOS

[Homebrew](https://brew.sh/) を使います。

```bash
brew install toppoun/atc/atc
```

Scoop / Homebrew の準備、C++ compiler や Python の選び方、ソースからのインストールは[インストールガイド](docs/installation.md)を参照してください。

## クイックスタート

作業用フォルダを作成し、atc-rs の workspace として初期化します。

```bash
mkdir atcoder
cd atcoder
atc init
atc
```

Workspace Home が開いたら、次の順に操作します。

1. `c` を押す
2. `abc123` のような Contest ID を入力して `Enter`
3. 問題一覧が表示されたら、ソースをエディタで開いて編集する
4. `r` を押して、選択中の問題をテストする
5. 提出するときは `t` を押す

提出には AtCoder の `REVEL_SESSION` Cookie が必要です。Workspace Home で `a` を押すと設定できます。

初回の環境準備からテストまでを順に進めたい場合は、[インストールガイド](docs/installation.md)から始めてください。

## よく使う操作

Contest 画面では、次のキーをよく使います。

| キー | 操作 |
| --- | --- |
| `h` / `←`、`l` / `→` | 前後の問題へ移動 |
| `j` / `↓`、`k` / `↑` | 前後のテストケースへ移動 |
| `r` | 選択中の問題をテスト |
| `t` | 提出画面を開く |
| `S` | Stress Test を開始 |
| `:` | Command Palette を開く |
| `?` | ショートカットを表示 |
| `q` | 終了 |

すべての画面と操作は [TUI ガイド](docs/tui.md)で確認できます。コマンド一覧と option は、ターミナルでも表示できます。

```bash
atc --help
atc test --help
atc submit --help
```

## ガイド

- [インストール](docs/installation.md) — 導入、必要な外部ツール、最初の Contest
- [TUI](docs/tui.md) — 画面ごとの操作とショートカット
- [ワークスペース](docs/workspace.md) — Contest の保存先、作成、切り替え、更新
- [テストと Watch](docs/testing.md) — サンプルテスト、User Input、自動テスト
- [ストレステスト](docs/stress.md) — Generator / Brute Force と反例の保存
- [AtCoder 認証](docs/authentication.md) — Cookie の設定、確認、置換、リセット
- [テンプレート](docs/templates.md) — 新しく作るソースのひな形
- [設定](docs/configuration.md) — 言語、実行環境、timeout、editor、提出 runtime
- [トラブルシューティング](docs/troubleshooting.md) — 症状から対処方法を探す

`atc` は設定ファイルを作らなくても使い始められます。環境に問題がありそうな場合は、変更を加えない診断コマンドを実行してください。

```bash
atc doctor
```

## License

[MIT License](LICENSE)
