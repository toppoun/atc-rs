# トラブルシューティング

問題が起きたら、まず次を実行してください。

```bash
atc doctor
```

`doctor` は、atc-rs の version、OS、設定、C++ compiler、Python、テンプレート、現在の workspace などを読み取り、`OK`、`WARN`、`ERROR` で表示します。ファイルや設定は変更しません。AtCoder への接続や認証状態は確認しないため、認証は `Authentication` 画面または `atc login` で確認してください。

## インストール後に `atc` が見つからない

**確認すること:** 実行される `atc` の場所と、package manager のインストール状態を確認します。

Windows / Scoop:

```powershell
where.exe atc
scoop info atc
```

Scoop 版は通常、次の shim から起動されます。

```text
C:\Users\<ユーザー名>\scoop\shims\atc.exe
```

古い Cargo / Python 版など、別の `atc` が先に表示される場合は PATH の順序を確認してください。何も表示されない場合は、新しい PowerShell を開いてからインストールをやり直します。

macOS / Homebrew:

```bash
which atc
type -a atc
brew info toppoun/atc/atc
```

Apple Silicon の Homebrew では通常 `/opt/homebrew/bin/atc` です。インストール時に表示された `shellenv` の案内が未実行なら、その案内を実行して新しい shell を開いてください。

古いコマンド位置が残っている場合は、次を実行します。

```bash
rehash
which atc
atc --version
```

現在の Homebrew formula のビルド済み配布は Apple Silicon 向けです。Intel Mac では[ソースからインストール](installation.md#ソースからインストールする)してください。

詳しい導入手順は[インストール](installation.md)を参照してください。

## C++ を compile できない

**症状:** `g++` が見つからない、`Compile Error` になる、C++23 の機能で失敗する。

**確認すること:** 次の両方を実行します。

```bash
g++ --version
atc doctor
```

初期設定では `g++` に `-std=c++23 -O2 -Wall -Wextra` を渡します。macOS の `g++` は Apple Clang を指すことがあるため、名前だけで compiler の種類を判断しないでください。`doctor` は `--version` を確認するだけで、C++23 の機能を compile する検査ではありません。

**対処方法:** 使用する compiler と option を設定します。

```toml
[runner]
cpp_compiler = "g++"
cpp_flags = ["-std=c++23", "-O2", "-Wall", "-Wextra"]
```

導入方法は[インストールの C++](installation.md#c-を使う場合)、設定項目は[設定の runner](configuration.md#runner)を参照してください。

## Python が見つからない

**症状:** Python の解答や Stress Helper を実行できない。

**確認すること:** 環境で使えるコマンド名を確認します。

```bash
python --version
python3 --version
```

**対処方法:** `python3` を使う環境では、次のように設定します。

```toml
[runner]
python = "python3"
```

Stress Test の Generator / Brute Force もこの設定を使います。詳しくは[インストールの Python](installation.md#python-を使う場合)を参照してください。

## TUI からエディタを開けない

**確認すること:** atc-rs は `[editor]`、VS Code / Cursor の統合ターミナル、`VISUAL`、`EDITOR` の順にエディタを探します。

**対処方法:** たとえば shell で次を設定します。

```bash
export EDITOR=nvim
```

または `config.toml` に指定します。

```toml
[editor]
command = "nvim"
mode = "terminal"
```

ソース自体は Contest フォルダから直接開いて編集できます。詳しい起動方法は[editor 設定](configuration.md#editor)を参照してください。

## TUI の表示が崩れる

**確認すること:** terminal のサイズ、色設定、別の terminal での表示を確認します。

**対処方法:** Watch をテキスト表示で実行し、テスト自体が正常か切り分けます。

```bash
atc watch --plain
```

単発テストだけなら `atc test A` も利用できます。

## Watch 起動直後にテストされない

これは正常な動作です。Watch は起動後に Contest フォルダ直下の問題ソースが変更・保存されたときにテストします。

すぐに実行する場合は、次のコマンドまたは Contest 画面の `r` を使います。

```bash
atc test A
```

詳しくは[テストと Watch](testing.md#watch)を参照してください。

## `atc test A` が別言語のソースを使わない

atc-rs は、指定した言語のソースがないときに別言語へ自動で切り替えません。

Python を明示する場合:

```bash
atc test A -l python
```

普段使う言語を変える場合は、[デフォルト言語](configuration.md#language)を設定してください。

## Contest や問題を作成できない

**症状:** Workspace Home の `Open / Create Contest` や `atc contest <contest-id>` で、作成・取得・修復に失敗する。

**確認すること:** 次を順に確認します。

1. workspace を使う場合は、`.atc-workspace.toml` がある正確なフォルダで `atc` を起動しているか
2. `abc123` のような正しい Contest ID を入力しているか。AtCoder の URL 全体や Contest 名は入力しない
3. ブラウザから AtCoder を開けるか。接続できない場合は、network が戻ってから再試行する
4. 既存の Contest に `Contest data requires repair.` と表示されていないか

既存の Contest に修復が必要な場合は、ファイルを削除して作り直さず、TUI の `Repair & Open` または `atc contest <contest-id>` の確認画面を利用してください。自動修復を拒否するエラーが出た場合は、そのファイルとエラー内容を確認し、安易に削除しないでください。

初回の操作は[最初の Contest を開く](installation.md#5-最初の-contest-を開く)、保存先と修復方法は[ワークスペース](workspace.md#壊れたコンテストの修復)を参照してください。

## workspace が見つからない

**症状:** Workspace Home が開かない、`-c` で指定した Contest が見つからない。

**確認すること:** `.atc-workspace.toml` がある正確なフォルダでコマンドを実行しているか確認します。atc-rs は親フォルダを自動検索しません。

```bash
cd /path/to/atcoder
atc
```

Windows PowerShell でも `cd` を使えます。詳しくは[ワークスペース](workspace.md#親ディレクトリは自動検索しない)を参照してください。

## Contest の保存先が意図と違う

`.atc-workspace.toml` の `pattern` は正規表現で、部分一致も成立します。`^abc[0-9]+$` のように `^` と `$` を付けて、Contest ID 全体へ一致させてください。

複数ルールに一致するとエラーになります。振り分けが不要なら、`[[paths]]` をすべて削除して次の内容だけにできます。

```toml
version = 1
```

この場合、すべての Contest が workspace 直下へ保存されます。`atc new` は routing を使わず、常に実行したフォルダの直下へ作成する点にも注意してください。

詳しくは[ワークスペースの保存先](workspace.md#contest-の保存先)を参照してください。

## `refresh` が `tests/` で止まる

**確認すること:** `tests/` 以下に、自分で追加したファイルやフォルダがないか確認します。

`atc refresh` は、管理対象として確認できない項目を勝手に削除しません。

**対処方法:** 必要なファイルを `tests/` の外へ移動し、もう一度実行します。

```bash
atc refresh
```

メタデータが壊れていることが明らかな場合だけ、[ワークスペースの修復手順](workspace.md#壊れたコンテストの修復)を確認してください。

## 設定ファイルを読み込めない

**症状:** 未知の項目、不正な値、TOML parse error が表示される。

**確認すること:** `atc doctor` で設定ファイルの path を確認し、特に次を見直します。

- 文字列の `"` が閉じているか
- `[runner]` や `[[paths]]` の括弧が正しいか
- `pattern` や `path` が文字列になっているか
- 対応していない項目名を書いていないか
- timeout が 0 以下になっていないか

atc-rs は不正な設定を推測で無視したり、`atc config init` で既存ファイルを置き換えたりしません。[設定項目の一覧](configuration.md#設定項目と組み込みデフォルト)と比較して修正してください。

## Cookie を設定できない

**症状:** `Invalid`、`Not authenticated`、`Verification unavailable` になる。

**確認すること:** 状態によって対処が異なります。

- `Invalid`: Cookie ファイルの形式、種類、権限を確認する
- `Not authenticated`: Cookie の値や有効期限を確認する
- `Verification unavailable`: network や AtCoder 側の一時的な問題を確認する

貼り付けるのは値だけ、または次の形式です。

```text
REVEL_SESSION=<value>
```

Cookie header 全体や属性は貼り付けません。`Verification unavailable` の場合、Cookie の保存は成功していることがあります。network が戻った後に画面を開き直してください。

Unix 系環境で権限に問題がある場合は、保存先を画面で確認してから、通常は次のように所有者だけが読める状態へ直します。

```bash
chmod 600 "${XDG_STATE_HOME:-$HOME/.local/state}/atc/cookie"
```

ディレクトリや link など、安全に置き換えられない対象は `Repair Cookie` でも上書きしません。必要なデータを確認し、別の場所へ退避してから再度設定してください。

詳しくは[AtCoder 認証](authentication.md)を参照してください。

## Cookie を保存したのに Contest へ反映されない

Home で Cookie を変更した後は、Contest を開き直してください。`Refresh Contest` だけでは認証情報を読み直しません。

```text
Contest の Command Palette
→ Back to Workspace Home
→ Contest をもう一度開く
```

設定の反映時期は[AtCoder 認証](authentication.md#変更が反映されるタイミング)で確認できます。

## 提出結果を確認できない

network error などで提出結果を確定できない場合、atc-rs は同じ解答を自動再送しません。同じ起動中は、同じ Contest・問題への再提出も止めます。

AtCoder の My Submissions をブラウザで確認してください。提出されていないことを確認できた場合だけ、atc-rs を再起動して再提出します。

## テンプレートを作成・編集できない

**確認すること:** Template 画面の `Ready`、`Missing`、`Invalid` と、表示される path を確認します。

通常は組み込みテンプレートだけで利用できます。カスタマイズする場合は次で作成します。

```bash
atc template init cpp
atc template init python
```

既存テンプレート、ディレクトリ、link などを勝手に上書きすることはありません。詳しくは[テンプレート](templates.md)を参照してください。
