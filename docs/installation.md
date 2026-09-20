# インストールと最初の Contest

このページでは、atc-rs をインストールし、最初の Contest を開いてサンプルテストを実行するところまで案内します。設定ファイルの編集は必要ありません。

ビルド済みバイナリの対象は次の環境です。

- Windows x86_64
- macOS Apple Silicon

Intel Mac など、それ以外の環境では[ソースからインストールする](#ソースからインストールする)方法を利用してください。

## 1. atc-rs をインストールする

### Windows

Windows では [Scoop](https://scoop.sh/) を使います。Scoop が入っていない場合は、通常ユーザーとして PowerShell を開き、次を実行します。

```powershell
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
Invoke-RestMethod -Uri https://get.scoop.sh | Invoke-Expression
```

Scoop のインストール後、新しい PowerShell を開いて atc-rs をインストールします。

```powershell
scoop bucket add atc https://github.com/toppoun/scoop-atc
scoop install atc/atc
```

### macOS

macOS では [Homebrew](https://brew.sh/) を使います。Homebrew が入っていない場合は、ターミナルで次を実行します。

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

インストールの最後に、Homebrew を利用可能にするためのコマンドが表示された場合は、その案内も実行してください。その後、atc-rs をインストールします。

```bash
brew install toppoun/atc/atc
```

### 動作確認

新しいターミナルを開き、次を実行します。

```bash
atc --version
```

`atc` が見つからない場合は、一度ターミナルを開き直してください。それでも解決しない場合は、[インストール後に `atc` が見つからない](troubleshooting.md#インストール後に-atc-が見つからない)を参照してください。

## 2. 使用する言語を準備する

atc-rs 本体とは別に、解答を実行するための compiler または Python が必要です。使わない言語の環境を用意する必要はありません。

初期設定では C++ を使用します。Python を最初から使う場合は、Python の準備後に[デフォルト言語を変更](configuration.md#最初に変更することが多い設定)してください。

### C++ を使う場合

初期設定では `g++` を次の option で実行します。

```text
-std=c++23 -O2 -Wall -Wextra
```

使用する compiler は、これらの option と解答で使う C++23 の機能に対応している必要があります。atc-rs は特定の compiler version を要求しません。

Windows では、Scoop から GCC をインストールできます。

```powershell
scoop install gcc
g++ --version
```

macOS では、Apple の Command Line Tools を利用できます。

```bash
xcode-select --install
g++ --version
```

macOS の `g++` は GNU GCC ではなく Apple Clang を指すことがあります。コマンド名だけで種類を判断せず、`g++ --version` の表示と、実際の `atc test` で対応状況を確認してください。

別の compiler や option を使う場合は、[C++ の実行設定](configuration.md#runner)を変更できます。

### Python を使う場合

Windows では、Scoop から Python をインストールできます。

```powershell
scoop install python
python --version
```

macOS では、Homebrew から Python をインストールできます。

```bash
brew install python
python3 --version
```

Homebrew の Python は通常 `python3` で実行します。atc-rs の初期設定は `python` なので、macOS では設定ファイルで次のように変更してください。

```toml
[runner]
python = "python3"
```

設定ファイルの作成場所と編集方法は[設定](configuration.md)を参照してください。

Stress Test の Generator と Brute Force は、C++ の解答を検証する場合でも Python を使用します。

### エディタ

ソースの編集には任意のエディタを使えます。ファイルを自分で開いて編集するだけなら、atc-rs 側の設定は不要です。

TUI の `Open Source` や `Open Template` からエディタを起動する場合は、次の順で利用可能なエディタを探します。

1. 設定ファイルの `[editor]`
2. VS Code または Cursor の統合ターミナル
3. `VISUAL` 環境変数
4. `EDITOR` 環境変数

見つからない場合は、[editor 設定](configuration.md#editor)を追加してください。

## 3. 環境を診断する

次のコマンドは、設定、実行コマンド、テンプレート、現在の workspace などを確認します。

```bash
atc doctor
```

`atc doctor` はローカル環境を読むだけで、設定やファイルを変更しません。また、AtCoder への接続やログイン状態は確認しません。

使用する言語の runner が `ERROR` なら、その言語の準備または設定を見直してください。使っていない言語に対する `WARN` は、すぐに解消しなくても利用を始められます。

なお、C++ compiler の診断は `--version` を実行する確認です。C++23 の個々の機能まで検査するものではないため、最終的にはサンプルテストで確認してください。

## 4. 作業フォルダを作る

Contest をまとめるフォルダを作成します。PowerShell と macOS の shell で同じコマンドを使えます。

```bash
mkdir atcoder
cd atcoder
atc init
```

`atc init` は現在のフォルダに `.atc-workspace.toml` を作成します。既存の同名ファイルは上書きしません。

初期設定では、`abc`、`arc`、`agc` で始まる Contest をそれぞれ `ABC`、`ARC`、`AGC` フォルダへ保存し、それ以外は workspace 直下へ保存します。保存先は後から変更できます。詳しくは[ワークスペース](workspace.md)を参照してください。

## 5. 最初の Contest を開く

workspace のフォルダで TUI を起動します。

```bash
atc
```

Workspace Home が表示されたら、次のように操作します。

1. `c` を押して `Open / Create Contest` を開く
2. `abc123` のような Contest ID を入力する
3. `Enter` を押す

Contest がまだない場合は、AtCoder から問題情報とサンプルを取得し、問題ごとのソースを作成してから Contest 画面を開きます。既存のソースは上書きしません。

接続エラーや Contest ID の間違いで開けない場合は、画面のメッセージを確認して `Esc` で戻れます。

## 6. ソースを編集してテストする

Contest 画面で `:` を押し、`Open Source` を選びます。C++ / Python を選択し、既存のソースを `Enter` で開きます。まだない言語のソースは `i` で作成して開けます。

エディタを TUI から起動できない場合は、Contest フォルダにある `A.cpp` や `A.py` を直接開いて編集してください。

編集後、Contest 画面で `r` を押すと、選択中の問題の公式サンプルを実行します。別のターミナルから実行する場合は、`.atc-workspace.toml` がある workspace ルートで Contest ID を指定します。

```bash
atc test A -c abc123
```

`-c abc123` を指定すると、workspace の振り分け設定に従って `ABC/abc123` が選ばれます。Contest フォルダへ移動して実行する場合は `atc test A` でも構いません。`A` は問題名です。

結果の読み方、User Input、保存時の自動テストは[テストと Watch](testing.md)を参照してください。

## 7. 必要になったら認証して提出する

テストだけなら認証情報は不要です。AtCoderへ提出するときは、Workspace Homeで`a`を押して`REVEL_SESSION` Cookieを設定します。

Cookieは、AtCoderにログインしたChromeなどのブラウザから取得できます。詳しい手順は[AtCoder認証](authentication.md#revel_session-を取得する)を参照してください。

設定後にContestを開き、`t`を押して提出するソースを選びます。

## ソースからインストールする

Rust と Cargo が利用できる環境では、repository からインストールできます。

```bash
cargo install --git https://github.com/toppoun/atc-rs.git --locked
```

この方法でも、解答に使う compiler や Python は別途必要です。

## 次に読むページ

- [TUI](tui.md) — 画面ごとの操作
- [ワークスペース](workspace.md) — Contest の保存先と切り替え
- [テストと Watch](testing.md) — 普段のテスト操作
- [設定](configuration.md) — 言語や実行環境のカスタマイズ
- [トラブルシューティング](troubleshooting.md) — 導入や実行で困ったとき
