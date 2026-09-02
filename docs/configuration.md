# 設定

`atc` は設定ファイルがなくても動作します。変更したい項目だけを `config.toml` に追加する方式です。

## 設定ファイルを作る

```bash
atc config init
```

このコマンドは、設定ファイルがまだない場合だけ作成します。既存の設定ファイルは上書きしません。

初期状態のファイルはコメントのみで、組み込みのデフォルト設定がそのまま使われます。

## 保存場所

### Windows

```text
%APPDATA%\atc\config.toml
```

通常は次のような場所です。

```text
C:\Users\<ユーザー名>\AppData\Roaming\atc\config.toml
```

### macOS / Linux

```text
${XDG_CONFIG_HOME:-~/.config}/atc/config.toml
```

通常は次の場所です。

```text
~/.config/atc/config.toml
```

## 設定例

```toml
[defaults]
language = "cpp"

[runner]
python = "python"
cpp_compiler = "g++"
cpp_flags = ["-std=c++23", "-O2", "-Wall", "-Wextra"]
timeout_seconds = 2.0
compile_timeout_seconds = 10.0

[submit]
python_runtime = "cpython"

[editor]
command = "nvim"
args = []
mode = "terminal"
```

必要な項目だけを書けばよく、書かれていない項目には組み込みのデフォルト値が使われます。

## 設定項目と組み込みデフォルト

| 項目 | 組み込みデフォルト | 内容 |
| --- | --- | --- |
| `defaults.language` | `"cpp"` | 新規ソースと明示しない実行言語 |
| `runner.python` | `"python"` | Python と Stress Helper の実行コマンド |
| `runner.cpp_compiler` | `"g++"` | C++ コンパイラ |
| `runner.cpp_flags` | `["-std=c++23", "-O2", "-Wall", "-Wextra"]` | C++ コンパイラへ渡す引数 |
| `runner.timeout_seconds` | `2.0` | 候補プログラム、Generator、Brute Force の実行制限時間 |
| `runner.compile_timeout_seconds` | `10.0` | C++ コンパイルの制限時間 |
| `submit.python_runtime` | `"cpython"` | Python 提出で使う runtime (`"cpython"` / `"pypy"`) |
| `editor.command` | なし | エディタの実行コマンド |
| `editor.args` | `[]` | `command` の後、対象パスの前に渡す引数 |
| `editor.mode` | コマンド名から推測 | `"terminal"` または `"external"` |

`[editor]` セクションは任意ですが、書く場合は `command` が必要です。

## `[defaults]`

### `language`

新しくソースを作成するときのデフォルト言語です。

```toml
[defaults]
language = "cpp"
```

指定できる値:

- `cpp`
- `python`

コマンドに `-l` / `--language` がある場合は、コマンドラインで指定した言語が優先されます。

```bash
atc new abc466 -l python
atc create A -l python
atc test A -l python
```

## `[runner]`

### `python`

Python ソースと Stress Helper の実行に使うコマンドです。

```toml
[runner]
python = "python3"
```

デフォルト:

```text
python
```

### `cpp_compiler`

C++ のコンパイルに使うコマンドです。

```toml
[runner]
cpp_compiler = "g++"
```

デフォルト:

```text
g++
```

### `cpp_flags`

C++ コンパイラへ渡す引数です。

```toml
[runner]
cpp_flags = ["-std=c++23", "-O2", "-Wall", "-Wextra"]
```

デフォルトも上記と同じです。

### `timeout_seconds`

候補プログラムの実行時間制限です。

```toml
[runner]
timeout_seconds = 2.0
```

デフォルトは `2.0` 秒です。

### `compile_timeout_seconds`

C++ のコンパイル時間制限です。

```toml
[runner]
compile_timeout_seconds = 10.0
```

デフォルトは `10.0` 秒です。

`timeout_seconds` と `compile_timeout_seconds` には、0 より大きい有限値を指定する必要があります。

## `[submit]`

### `python_runtime`

Python ソースを AtCoder へ提出するときの runtime を指定します。

```toml
[submit]
python_runtime = "pypy"
```

指定できる値:

- `cpython`
- `pypy`

デフォルトは `cpython` です。`atc submit` の `--runtime` が指定された場合はコマンドラインが優先されます。

```bash
atc submit A --runtime pypy
atc submit A -l python --runtime cpython
```

`--runtime` は提出元の言語を選びません。C++ と Python の両方のソースがある場合は、従来どおり `-l cpp` または `-l python` が必要です。また、C++ 提出に `--runtime` は指定できません。C++ の提出先は通常の GCC の最新候補に固定されます。

## `[editor]`

TUI の `Open Source`、`Open Settings`、`Open Template` などから起動するエディタを指定できます。

### Vim / Neovim

```toml
[editor]
command = "nvim"
args = []
mode = "terminal"
```

`terminal` モードでは TUI を一時的に離れてエディタを開き、エディタ終了後に TUI へ戻ります。

### VS Code

```toml
[editor]
command = "code"
args = ["-r"]
mode = "external"
```

`external` モードでは GUI エディタを起動し、TUI はそのまま動作を続けます。

`args` はシェル文字列ではなく、引数を1件ずつ配列で指定します。対象ファイルのパスは `atc` が最後の引数として追加します。

### `mode` を省略した場合

エディタ名から起動方法を推測します。

`external` として扱われる代表例:

- VS Code (`code`, `code-insiders`)
- Cursor (`cursor`)
- Sublime Text (`subl`)
- Zed (`zed`)
- Windsurf (`windsurf`)

それ以外は基本的に `terminal` として扱われます。

## エディタの決定順

`[editor]` を書いていない場合、次の順番でエディタを探します。

1. `config.toml` の `[editor]`
2. Windows / macOS で VS Code / Cursor の統合ターミナルを自動検出
3. `VISUAL` 環境変数
4. `EDITOR` 環境変数

どれも見つからない場合、TUI からファイルを開こうとした時点でエラーになります。

たとえば:

```bash
export EDITOR=nvim
```

または:

```bash
export VISUAL='code --reuse-window'
```

## 設定ファイルの検証

`atc` は次のような設定をエラーとして扱います。

- 未対応の設定項目
- `cpp` / `python` 以外の言語
- `cpython` / `pypy` 以外の Python submit runtime
- 空の `python` / `cpp_compiler`
- 0 以下または有限でないタイムアウト
- 空の `editor.command`
- `terminal` / `external` 以外の `editor.mode`
- 不正な TOML

既存の不正な設定ファイルを `atc config init` が勝手に置き換えることはありません。

## 現在の設定を確認する

```bash
atc doctor
```

`doctor` では、各設定が組み込み値かユーザー設定かも確認できます。
