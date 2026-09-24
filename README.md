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

### 1. Workspace を作る

AtCoder のファイルを保存したい場所で、作業用フォルダを作成します。

```bash
mkdir atcoder
cd atcoder
atc init
```

`atc init` は、現在のフォルダを Workspace として初期化します。

Workspace は、複数のコンテストをまとめて管理するためのフォルダです。

初期設定では、ABC / ARC / AGC ごとに保存先が振り分けられます。すべてのコンテストを Workspace 直下に置きたい場合は、[Contest の保存先を変更する](docs/workspace.md#contest-の保存先)を参照してください。

### 2. atc を起動する

```bash
atc
```

Workspace Home が開きます。

### 3. コンテストを開く

1. `c` を押す
2. `abc123` のような Contest ID を入力して `Enter`
3. 問題一覧が表示されたら、ソースをエディタで開いて編集する
4. `r` を押して、選択中の問題をテストする
5. 提出するときは `t` を押す

ソースを保存すると、自動テストも利用できます。

**提出には AtCoder の認証が必要です。** Workspace Home で `a` を押すと Authentication を開けます。

Cookie の取得方法は[AtCoder 認証ガイド](docs/authentication.md)を参照してください。

認証を設定しなくても、問題の取得やローカルでのテストは利用できます。

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

画面の移動やその他の操作は、[TUI ガイド](docs/tui.md)で確認できます。

CLI のコマンド一覧とオプションは、ターミナルからも確認できます。

```bash
atc --help
atc test --help
atc submit --help
```

## やりたいことから探す

<img src="docs/assets/icons/download.svg" width="24" height="24" alt=""> **[インストールして使い始める](docs/installation.md)**  
導入方法・必要なツール・最初のコンテスト

## License

atc-rs is licensed under the [MIT License](LICENSE).

Icons are provided by [Lucide](https://lucide.dev/).
See the [Lucide License](docs/assets/licenses/LUCIDE-LICENSE).