# atc-rs 仕様書

作成日: 2026-08-11

## 1. 概要

`atc-rs` は、AtCoderでの競技プログラミングを支援するRust製CLIツールである。

既存のPython版 `atc` を参考にはするが、Python版との互換性維持を目的とはしない。
Python版で得た知見をもとに、Rust版では公開前に責務・ファイル配置・永続化形式・CLIインターフェースを整理し、より単純で変更しやすい構成として再設計する。

リポジトリ名は `atc-rs` とするが、最終的な実行コマンド名は以下とする。

```text
atc
```

最終的にはRustのネイティブバイナリとして配布し、Python環境および `online-judge-tools (oj)` を必要としない構成を目標とする。

---

# 2. 基本設計方針

## 2.1 Python版との互換性

Python版との完全互換、設定ファイル互換、内部メタデータ互換は行わない。

Rust版は新しいCLIとして設計し直す。

ただし、競技プログラミング用workspaceとして自然な以下の構造については引き続き利用する。

```text
abc321/
├─ A.cpp
├─ B.cpp
├─ C.cpp
├─ ...
├─ tests/
│  ├─ A/
│  ├─ B/
│  └─ ...
└─ .atc/
   └─ contest.toml
```

Python版の過去仕様を維持するためのfallbackやmigration処理は原則として実装しない。

---

## 2.2 エラー処理方針

Rust版では、異常状態を暗黙のfallbackで隠さない。

基本方針は以下とする。

```text
正常な入力・状態
    ↓
処理を継続

異常な入力・状態
    ↓
原因を明示したエラー
    ↓
処理終了
```

特に以下については推測やfallbackを行わない。

* AtCoderのtasksページが取得できない
* tasksページを正常にparseできない
* configが不正
* 必須pathが不正
* 必須templateが存在しない
* contest IDを解釈できない
* 認証が必要なのにsessionが無効

「とりあえず別の方法で続行する」動作はできるだけ避ける。

ただし、contest全体の成立に必須ではない処理については部分失敗を許容する。

代表例はsample取得である。

---

# 3. 配布形態

## 3.1 バイナリ

Rustコードは最終的に単一のネイティブ実行ファイルとしてビルドする。

例：

```text
Windows
atc.exe

macOS / Linux
atc
```

ソースコード上では複数moduleに分離するが、通常の配布時にはそれらを個別ファイルとしてユーザーへ配布しない。

概念的には以下となる。

```text
cli
config
auth
atcoder
workspace
runner
ui
built-in resources
        │
        ▼
     Rust build
        │
        ▼
       atc
```

---

## 3.2 外部依存

`oj` には依存しない。

AtCoderに対する以下の処理はRust版自身で行う。

* HTTP通信
* contest task一覧取得
* 問題ページ取得
* HTML parse
* sample parse
* login session利用
* Cookie永続化

C++ソースを利用する場合は外部C++ compilerを利用する。

初期対応compilerは `g++` 互換を前提とする。

Pythonソース実行対応を追加する場合は、Python/PyPy runtimeは外部programとして利用する。

---

# 4. 責務単位

初期段階では細かくmoduleを分割しすぎず、以下の大きな責務単位を基本とする。

```text
cli
commands
config
auth
atcoder
workspace
runner
ui
```

必要になった時点で内部をさらに分割する。

---

## 4.1 `cli`

CLI入力のparseを担当する。

例：

```text
atc new abc321
atc run A
atc login
atc config show
```

を型付きのcommandへ変換する。

想定ライブラリは `clap`。

`cli` は実際のHTTP通信、filesystem操作、compile等を行わない。

---

## 4.2 `commands`

各CLI commandの処理手順を組み立てる。

例：

```text
new
    ↓
AtCoderから取得
    ↓
workspace生成
```

`commands` 自身がHTML parseやC++ compile等の専門処理を実装するのではなく、各責務moduleを呼び出す。

---

## 4.3 `config`

ユーザー設定を管理する。

責務：

* config path解決
* `config.toml` 読み込み
* deserialize
* validation
* default値適用
* template path解決
* contest root設定

異常な設定については可能な限り早期にエラーとする。

---

## 4.4 `auth`

AtCoderへの認証状態を管理する。

責務：

* session読み込み
* Cookie保存
* Cookie削除
* login
* logout
* session有効性確認

認証情報はconfigとは別のstateとして管理する。

---

## 4.5 `atcoder`

AtCoderという外部サービスとの境界。

責務：

* HTTP client
* 認証済みrequest
* tasksページ取得
* tasksページparse
* 問題ページ取得
* 問題HTML parse
* sample抽出

このmoduleは、取得したsampleが最終的にどのdirectoryへ保存されるかを知らない。

例：

```text
AtCoder HTML
    ↓
atcoder
    ↓
Problem
├─ title
├─ url
└─ samples
```

---

## 4.6 `workspace`

ローカルのcontest workspaceを管理する。

責務：

* contest directory生成
* source file生成
* tests directory生成
* sample file保存
* `.atc/contest.toml` 読み書き
* workspace内pathの規約管理
* source file探索

AtCoderへのHTTP通信は行わない。

---

## 4.7 `runner`

solutionの実行を担当する。

責務：

* source選択
* C++ compile
* program実行
* timeout
* stdout / stderr取得
* expected outputとの比較
* AC / WA / RE / TLE / CE等の結果生成

表示そのものは `ui` に任せる。

---

## 4.8 `ui`

人間向けの表示を担当する。

例：

```text
AC 3/3
WA sample-2
compile error
sample download failed
```

`ui` はfilesystemやHTTPを直接操作しない。

---

# 5. ユーザーデータ配置

ユーザー自身が管理する設定と、`atc` が内部的に管理するstateを分離する。

---

## 5.1 Config領域

全OSでXDG風の論理構造を基本とする。

```text
~/.config/atc/
├─ config.toml
└─ templates/
   ├─ cpp.cpp
   └─ python.py
```

ここに置くものはユーザー自身が編集・dotfiles管理することを想定する。

### config領域の性質

```text
ユーザーが管理する
変更内容を理解している
複数環境へ同期してもよい
Git / chezmoi等で管理可能
```

---

## 5.2 State領域

`atc` 自身が管理する永続状態はconfigと分離する。

```text
~/.local/state/atc/
└─ session.json
```

### state領域の性質

```text
atc自身が生成・変更する
ユーザーが通常編集しない
dotfiles管理しない
Gitへ入れない
machine固有の状態を含んでよい
```

特にCookie等の認証情報をconfig directoryに保存しない。

---

# 6. Template

## 6.1 配置

ユーザーtemplateは以下で固定する。

```text
~/.config/atc/templates/
```

例：

```text
~/.config/atc/
├─ config.toml
└─ templates/
   ├─ cpp.cpp
   └─ python.py
```

---

## 6.2 configからの指定

`config.toml` ではtemplateへのpathを指定する。

例：

```toml
[templates]
cpp = "templates/cpp.cpp"
python = "templates/python.py"
```

相対pathは `config.toml` が存在するdirectoryを基準とする。

したがって、

```toml
cpp = "templates/cpp.cpp"
```

は、

```text
~/.config/atc/templates/cpp.cpp
```

を意味する。

---

## 6.3 初期template

バイナリ内部には初期templateをresourceとして埋め込むことができる。

ただし、通常実行時にユーザーtemplateが存在しない場合、暗黙にbuilt-in templateへfallbackする仕様にはしない。

想定する用途は初期化時の展開である。

例：

```text
atc config init
```

実行時、

```text
~/.config/atc/
├─ config.toml
└─ templates/
   ├─ cpp.cpp
   └─ python.py
```

を生成する。

その後templateが削除されている場合は、

```text
C++ template not found:
~/.config/atc/templates/cpp.cpp
```

のようなエラーとする。

必要に応じて、

```text
Run `atc config init` to create the default templates.
```

のような案内を表示する。

---

# 7. 認証

## 7.1 基本仕様

最低限以下のcommandを提供する。

```text
atc login
atc logout
```

必要に応じて後から、

```text
atc auth status
```

等を追加できる。

---

## 7.2 Session保存

sessionは `atc` 自身が所有する1ファイルに保存する。

```text
~/.local/state/atc/session.json
```

バイナリ更新時にはこのファイルを変更しない。

したがって、

```text
atc v1
↓ login
session.json生成
↓
atcをv2へ更新
↓
session.jsonはそのまま
↓
Cookieが有効なら再login不要
```

となる。

---

## 7.3 Logout

`atc logout` は少なくともローカルに保存された認証状態を削除する。

---

## 7.4 Session失効

保存されたCookieがAtCoder側で無効になっている場合、自動的な複雑なfallbackや再loginは行わない。

明示的に認証エラーを表示し、

```text
atc login
```

の再実行を案内する。

---

# 8. Contest workspace

基本構造は以下とする。

```text
abc321/
├─ A.cpp
├─ B.cpp
├─ C.cpp
├─ ...
├─ tests/
│  ├─ A/
│  │  ├─ sample-1.in
│  │  ├─ sample-1.out
│  │  ├─ sample-2.in
│  │  └─ sample-2.out
│  ├─ B/
│  └─ ...
└─ .atc/
   └─ contest.toml
```

---

# 9. Sample file

sample testcaseは以下の命名規則とする。

```text
tests/<problem>/sample-<n>.in
tests/<problem>/sample-<n>.out
```

例：

```text
tests/A/sample-1.in
tests/A/sample-1.out
tests/A/sample-2.in
tests/A/sample-2.out
```

単純な `1.in` / `1.out` ではなく、ファイル単体を見てもsampleであることが分かる名前を採用する。

将来的に、

```text
custom-1.in
custom-1.out
```

等の独自testcaseを混在させる余地も残る。

---

# 10. `contest.toml`

## 10.1 目的

`contest.toml` は、

**`atc new` 時点でAtCoderから取得したcontest metadataをローカルへ保存したもの**

とする。

ローカルfilesystemの状態を記録するファイルにはしない。

---

## 10.2 配置

```text
<contest>/.atc/contest.toml
```

例：

```text
abc321/.atc/contest.toml
```

---

## 10.3 保存する情報

例：

```toml
version = 1
contest_id = "abc321"

[[problems]]
index = "A"
title = "321-like Checker"
task_id = "abc321_a"
url = "https://atcoder.jp/contests/abc321/tasks/abc321_a"

[[problems]]
index = "B"
title = "Cutoff"
task_id = "abc321_b"
url = "https://atcoder.jp/contests/abc321/tasks/abc321_b"
```

---

## 10.4 保存しない情報

以下は保存しない。

```toml
source = "A.cpp"
tests = "tests/A"
```

理由は、それらがAtCoder側のmetadataではなくローカルfilesystem上の状態だからである。

sourceの存在はfilesystemから判定する。

例：

```text
A.cppあり
→ C++ sourceあり

A.pyあり
→ Python sourceあり

A.cpp + A.py
→ 両方存在
```

`manual` 等で後からsourceを追加しても、`contest.toml` を更新する必要はない。

tests pathも規約で決まっているため保存しない。

---

## 10.5 用途

`contest.toml` は将来的に以下で利用できる。

* 問題一覧表示
* watchで問題名表示
* `open A`
* `submit A`
* refresh
* contest metadata参照
* VS Code UIで問題名を表示する場合
* network不要でcontest構造を確認

`run all` の実装方法については現時点では決定しない。

---

## 10.6 Legacy metadata

Python版との互換性は原則保証しない。ただし既存contest workspaceについては、atc contest 実行時に既知の旧 contest.toml schemaを検出した場合、現在のschemaへ自動migrationする。migration後は旧schemaへの書き戻しを行わない。不明・破損したschemaは自動修復せずエラーとする。

---

# 11. `atc new`

## 11.1 目的

指定したcontestを、現在のworking directory直下へ生成する。

```text
atc new abc321
```

を、

```text
D:/atcoder
```

で実行した場合、

```text
D:/atcoder/abc321
```

を対象とする。

`new` 自身はABC/ARC等による保存先振り分けを行わない。

---

## 11.2 処理フロー

想定処理：

```text
atc new abc321
        ↓
cwd/abc321 を対象にする
        ↓
AtCoder tasksページ取得
        ↓
tasks parse
        ↓
Problem一覧生成
        ↓
contest directory作成
        ↓
各problemのsource生成
        ↓
各problemページ取得
        ↓
sample parse
        ↓
tests保存
        ↓
contest.toml生成
```

---

## 11.3 tasksページ取得

対象：

```text
https://atcoder.jp/contests/<contest>/tasks
```

tasksページを取得できなかった場合は処理を終了する。

推測problem一覧へのfallbackは行わない。

---

## 11.4 tasks parse

tasksページを正常にparseできなかった場合も処理を終了する。

「A〜Gだろう」といった推測はしない。

---

## 11.5 Existing contest directory

対象contest directoryが既に存在する場合、既存ファイルを破壊しない。

基本原則：

```text
存在するもの
→ 何もしない

存在しないもの
→ 必要に応じて作成
```

既存sourceを自動的に上書きしない。

---

## 11.6 Source生成

各problemについてtemplateを利用してsource fileを生成する。

例：

```text
A.cpp
B.cpp
C.cpp
```

既に同名sourceが存在する場合は何もしない。

default languageやtemplate選択の詳細は `config.toml` schema確定時に決定する。

---

## 11.7 Sample取得失敗

sample取得はcontest metadata取得ほど強い必須条件にはしない。

例えば、

```text
A sample成功
B sample成功
C sample取得失敗
D sample成功
```

の場合、Cでcontest全体を中止せず、D以降も続行する。

Cについては明示的なwarning/error reportを残す。

理由：

* 問題によってsampleが存在しない可能性
* 一時的に一部problem pageだけ取得に失敗する可能性
* contest全体を利用不能にする必要はない

ただし「sampleが存在しない」ことと「HTTP/parse error」は内部的には区別する。

---

# 12. `atc contest`

`contest` commandは、

**保存先解決 + `new` 相当のcontest準備**

を行うcommandとして設計する。

概念的には、

```text
contest = path resolution + new
```

とする。

---

## 12.1 想定例

configでrootが、

```text
D:/atcoder
```

の場合、

```text
atc contest abc321
```

によって最終的に、

```text
D:/atcoder/abc/abc321
```

を利用する。

ARCなら、

```text
D:/atcoder/arc/arcXXX
```

とする。

---

## 12.2 Contest grouping

想定構造：

```text
root/
├─ abc/
│  ├─ abc321/
│  ├─ abc400/
│  └─ ...
├─ arc/
│  ├─ arc180/
│  └─ ...
├─ agc/
└─ ahc/
```

ただし、

* prefixから自動判定するか
* configでgroup mappingを書くか

将来的にはconfigのordered group mappingによってcontest IDから保存先を解決する。ユーザーごとにworkspace構成が異なるため、ABC/ARC等をプログラム内部に固定しない。

については未決定とする。

`atc new` の実装にはこの問題を持ち込まない。

---

# 13. Config

## 13.1 配置

```text
~/.config/atc/config.toml
```

---

## 13.2 現時点の想定schema

暫定案：

```toml
[paths]
root = "/path/to/atcoder"

[templates]
cpp = "templates/cpp.cpp"
python = "templates/python.py"

[defaults]
language = "cpp"

[runner]
cpp_compiler = "g++"
cpp_flags = [
    "-std=c++23",
    "-O2",
    "-Wall",
    "-Wextra"
]
timeout_seconds = 2.0
compile_timeout_seconds = 10.0
```

このschemaは現時点では確定ではない。

特にcontest groupingについては未決定。

---

## 13.3 Validation

configはRustのtyped structとしてdeserializeする。

不正な型、認識できない値等については可能な限り早期にエラーとする。

Python版のようにconsumerごとに異なるfallbackを持たせない。

---

# 14. VS Code拡張との連携

Rust CLIとVS Code拡張は独立したcomponentとして実装する。

双方の内部実装を直接依存させない。

連携には固定されたインターフェースのみ利用する。

概念：

```text
Rust CLI
   │
   │ 固定schema
   ▼
current-contest.json
   │
   ▼
VS Code extension
```

Rust側のmodule構成を変更しても、`current-contest.json` のschemaが維持されていればVS Code側を変更する必要はない。

逆も同様。

---

## 14.1 `current-contest.json`

具体的schemaは未確定だが、最低限versionを持たせる。

例：

```json
{
  "version": 1,
  "contest_dir": "/path/to/abc321"
}
```

必要になれば後からfieldを追加する。

このファイルはCLIとVS Code拡張間のインターフェースであるため、内部実装より変更を慎重に扱う。

---

# 15. Schema version

長期間保存されるデータ、または複数component間で共有するデータについてはversion fieldを持たせる。

例：

```toml
version = 1
```

```json
{
  "version": 1
}
```

候補：

* `contest.toml`
* `current-contest.json`
* `session.json`

これにより将来formatを変更した場合、

```text
version 1
→ v1 parser

version 2
→ v2 parser
```

のように明示的に扱える。

`config.toml` に独立したschema versionを設けるかは未決定。

---

# 16. 初期実装対象

当面は機能を広げすぎない。

最初の主要実装対象は、

```text
CLI
↓
atc new
↓
AtCoder parse
↓
workspace生成
```

とする。

具体的には、

```text
atc new abc321
```

によって、

```text
abc321/
├─ A.cpp
├─ B.cpp
├─ ...
├─ tests/
│  ├─ A/
│  │  ├─ sample-1.in
│  │  └─ sample-1.out
│  └─ ...
└─ .atc/
   └─ contest.toml
```

が生成される状態を最初の大きな完成地点とする。

---

# 17. 後回しにする機能

以下は初期設計には考慮するが、現時点では詳細仕様を確定しない。

* `run all`
* watch詳細仕様
* stress
* refresh
* doctor
* submit
* open
* VS Code terminal再利用
* machine-readable / JSON output
* clang++正式対応
* MSVC正式対応
* contest groupingの高度なcustom rule
* config migration
* Python版からのmigration
* sample custom testcase管理
* atomic file write詳細

---

# 18. 現時点で確定している主要方針

```text
Repository
  atc-rs

Executable
  atc

Language
  Rust

Python版互換
  なし

oj依存
  なし

AtCoder取得
  Rust自身でHTTP + HTML parse

Config
  ~/.config/atc/config.toml

Templates
  ~/.config/atc/templates/

Session
  ~/.local/state/atc/session.json

Template missing
  暗黙fallbackしない
  エラー + config init案内

Authentication
  atc login
  atc logout

Session persistence
  binary updateとは独立

Contest workspace
  <contest>/
  ├─ source
  ├─ tests/
  └─ .atc/contest.toml

Sample naming
  sample-N.in
  sample-N.out

contest.toml
  AtCoder contest metadataのみ
  source/testsは保存しない

new
  cwd直下に作る

contest
  保存先解決 + new 相当

tasks取得失敗
  fallbackせず終了

tasks parse失敗
  fallbackせず終了

sample部分失敗
  他problemは続行

既存file
  上書きしない

VS Code
  CLIとは独立
  固定されたfile schemaをinterfaceとして利用
```

---

# 19. 未決事項

現時点で主に残っている設計事項は以下。

1. `config.toml` の最終schema
2. contest root / group振り分け

   * prefix自動判定
   * configによるmapping
   * 両方を組み合わせるか
3. `session.json` の具体schema
4. AtCoder login時のCookie入力・取得方式
5. `current-contest.json` の正式schema
6. `contest.toml` にtitle / task_id / url以外のmetadataを保存するか
7. time limit / memory limitを保存するか
8. default languageと複数source存在時の選択規則
9. `atc new` でsource生成とsample取得のどこまでを成功条件とするか
10. `atc config init` の既存設定への挙動
11. unknown config keyをエラーにするか
12. Windows/macOS/Linux上でXDG風pathを具体的にどう解決するか
13. `atc contest` の既存contestに対する挙動
14. `current-contest.json` をどのcommandで更新するか
15. runnerのstatus・exit code体系

これらは該当機能を実装する前までに順次決定する。
