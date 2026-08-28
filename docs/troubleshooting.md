# トラブルシューティング

問題が起きたら、まず次を実行してください。

```bash
atc doctor
```

`doctor` は、atc のバージョン、OS、設定、C++ コンパイラ、Python、テンプレートなどをまとめて確認します。

## `atc` が見つからない

### Windows / Scoop

```powershell
where.exe atc
scoop info atc
```

Scoop 版は通常:

```text
C:\Users\<ユーザー名>\scoop\shims\atc.exe
```

から起動されます。

古い Cargo / Python 版など、別の `atc` が先に表示される場合は PATH の順序を確認してください。

### macOS / Homebrew

```bash
which atc
type -a atc
brew info toppoun/atc/atc
```

Apple Silicon の Homebrew では通常:

```text
/opt/homebrew/bin/atc
```

です。

インストール直後なのに古い `atc` が実行される場合、zsh が以前のコマンド位置をキャッシュしていることがあります。

```bash
rehash
```

その後:

```bash
which atc
atc --version
```

を確認してください。

## C++ コンパイラが見つからない

デフォルトでは `g++` を使います。

```bash
g++ --version
```

が通るか確認してください。

別のコマンドを使う場合:

```toml
[runner]
cpp_compiler = "g++"
```

`atc doctor` でも確認できます。

macOS では環境によって `g++` が Apple Clang を指す場合があります。C++23 で必要な機能が利用できることを確認してください。

## Python が見つからない

デフォルトでは `python` を使います。

```bash
python --version
```

環境で `python3` を使う場合:

```toml
[runner]
python = "python3"
```

Stress Helper もこの Python 設定を使います。

## TUI からエディタを開けない

エディタは次の順で探します。

1. `[editor]` 設定
2. VS Code / Cursor の統合ターミナル
3. `VISUAL`
4. `EDITOR`

どれも設定されていない通常のターミナルでは、エディタを自動では選びません。

例:

```bash
export EDITOR=nvim
```

または `config.toml`:

```toml
[editor]
command = "nvim"
mode = "terminal"
```

詳しくは [設定](configuration.md) を参照してください。

## TUI の表示に問題がある

TUI を使わずに Watch できます。

```bash
atc watch --plain
```

まず `--plain` でテスト実行自体が正常かを確認すると切り分けしやすくなります。

## Watch 起動直後にテストされない

正常な動作です。

`atc watch` は起動時に全問題をテストせず、**起動後のソース変更**を監視します。

すぐにテストしたい場合:

```bash
atc test A
```

または TUI で `r` を押してください。

## `atc test A` が別言語のソースを使ってくれない

言語は自動フォールバックしません。

たとえば設定が `cpp` の状態で `A.py` しかない場合は:

```bash
atc test A -l python
```

と明示してください。

## ワークスペースが見つからない

`atc` は `.atc-workspace.toml` を親ディレクトリまで検索しません。

ワークスペース機能を使う場合は、そのファイルがあるディレクトリへ移動してください。

```bash
cd /path/to/atcoder
atc contest abc466
```

詳しくは [ワークスペース](workspace.md) を参照してください。

## workspace の振り分けが意図どおりにならない

`.atc-workspace.toml` の `pattern` は正規表現です。部分一致も成立するため、`^abc[0-9]+$` のように `^` と `$` を付けた full match 形式を推奨します。

複数ルールに一致するとエラーになります。振り分けが不要なら、`[[paths]]` をすべて削除して次の内容だけにできます。

```toml
version = 1
```

この場合はすべて workspace 直下へ保存されます。

## `refresh` が `tests/` で止まる

`atc refresh` は、管理対象として認識できないファイルやディレクトリを勝手に消しません。

`tests/` 以下へ自分で置いたファイルなどがある場合、それが更新を止めることがあります。

必要なファイルを `tests/` の外へ移動してから、もう一度:

```bash
atc refresh
```

を実行してください。

## 設定ファイルのエラー

`config.toml` は未知の項目や不正な値を無視せず、エラーにします。

```bash
atc doctor
```

で設定ファイルのパスを確認し、[設定](configuration.md) の対応項目と比較してください。

## TOML parse error

`config.toml` と `.atc-workspace.toml` のどちらも、不正な TOML を推測で修復しません。エラーに表示されたファイルを開き、特に次を確認してください。

- 文字列の `"` が閉じているか
- `[runner]` や `[[paths]]` の括弧が正しいか
- `pattern` や `path` が文字列になっているか
- 対応していない項目名を書いていないか

修正後に `atc doctor` を実行すると、global config と現在の workspace config をまとめて確認できます。

## AtCoder 認証を確認したい

```bash
atc login
```

`login` は認証情報を書き込むコマンドではなく、現在のセッションが有効かを確認するコマンドです。

詳しくは [AtCoder 認証](authentication.md) を参照してください。
