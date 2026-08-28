# テンプレート

`atc` は C++ / Python の組み込みテンプレートを持っているため、設定なしでもソースを作成できます。

自分用に変更したい場合だけ、ユーザーテンプレートを初期化します。

## テンプレートを作る

C++:

```bash
atc template init cpp
```

Python:

```bash
atc template init python
```

両方:

```bash
atc template init
```

既に存在するテンプレートは上書きしません。

両方を初期化するときは、既存の対象を先に確認してから不足分を作ります。テンプレートの場所にディレクトリなど不正な対象がある場合はエラーになり、無関係なもう一方だけを先に作ることはありません。

## 保存場所

### Windows

```text
%APPDATA%\atc\templates\cpp.cpp
%APPDATA%\atc\templates\python.py
```

### macOS / Linux

```text
${XDG_CONFIG_HOME:-~/.config}/atc/templates/cpp.cpp
${XDG_CONFIG_HOME:-~/.config}/atc/templates/python.py
```

通常は:

```text
~/.config/atc/templates/cpp.cpp
~/.config/atc/templates/python.py
```

## どのテンプレートが使われるか

対象言語のユーザーテンプレートが存在する場合、その内容をそのまま使います。

存在しない場合は、`atc` に組み込まれたデフォルトテンプレートを使います。

ユーザーテンプレートが空ファイルの場合も「存在するテンプレート」として扱われるため、空のソースを作ることができます。

## テンプレートが使われるコマンド

通常ソースを新しく作成する操作で使われます。

```bash
atc new abc466
atc contest abc466   # コンテストがまだない場合
atc create A
```

TUI の `Open Source` からまだ存在しないソースを作る場合も同じテンプレートを使います。

作成先に同名のソースファイルが既にある場合、その内容は上書きしません。

## Stress Helper とは別

通常ソースのテンプレートと Stress Helper は別管理です。

```text
cpp.cpp / python.py        通常ソース用
A_gen.py / A_brute.py     Stress Test 用
```

`atc template init` で Generator / Brute Force のテンプレートが変わることはありません。

Stress Helper は:

```bash
atc stress init A
```

で作成します。

## TUI から編集する

Command Palette の `Open Template` から C++ / Python のテンプレートを開けます。

まだ初期化していない言語は `i` で作成して、そのままエディタで開けます。
