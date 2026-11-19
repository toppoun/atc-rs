# ワークスペース

ワークスペースを使うと、ABC / ARC / AGC などの Contest を1つのフォルダ以下へ整理できます。Contest ID に応じて保存先を振り分けられますが、初期設定のままでも利用できます。

## ワークスペースを作る

TUI から作るには、[Global Home の Open Folder](tui.md#global-home) を使います。以下の手順ではコマンドで作成します。

ワークスペースにしたいディレクトリで実行します。

```bash
mkdir atcoder
cd atcoder
atc init
```

現在のディレクトリに次のファイルが作成されます。

```text
.atc-workspace.toml
```

既に有効なファイルがある場合は上書きしません。不正な既存ファイルも勝手に置き換えず、エラーを返します。

## Contest の保存先

初期設定では、Contest は次の場所へ作成されます。

```text
<workspace>/ABC/abc123
<workspace>/ARC/arc123
<workspace>/AGC/agc123
```

どのルールにも一致しない Contest は `<workspace>/<contest-id>` に作成されます。保存先を変更する場合だけ `.atc-workspace.toml` を編集します。

## workspace config

`.atc-workspace.toml` で使用できる項目は次のとおりです。

| 項目 | 内容 |
| --- | --- |
| `version` | workspace config のバージョン。現在は `1` |
| `[[paths]].pattern` | contest ID と照合する正規表現 |
| `[[paths]].path` | 一致した contest の保存先となる workspace 内の相対パス |

未知の項目、不正な TOML、未対応の `version`、不正な正規表現やパスはエラーになります。

## デフォルトの振り分け

`atc init` で作成される設定は、次のようにコンテストを振り分けます。

```text
abc123 -> ABC/abc123
arc123 -> ARC/arc123
agc123 -> AGC/agc123
```

設定ファイルは次の形式です。

```toml
version = 1

[[paths]]
pattern = "^abc[0-9]+$"
path = "ABC"

[[paths]]
pattern = "^arc[0-9]+$"
path = "ARC"

[[paths]]
pattern = "^agc[0-9]+$"
path = "AGC"
```

どの `pattern` にも一致しないコンテストは、ワークスペース直下に作られます。

複数の `pattern` に一致した場合はエラーになります。

### 振り分けを使わない

`[[paths]]` はすべて削除またはコメントアウトできます。`paths` フィールド自体を省略し、次の内容だけにしても有効です。

```toml
version = 1
```

この場合、すべてのコンテストがワークスペース直下へ配置されます。

### 推奨する正規表現

正規表現は contest ID の一部に一致するだけでもルールに一致します。ただし、意図しない一致や複数ルールへの一致を避けるため、`^` と `$` を付けて contest ID 全体へ一致させる書き方を推奨します。

TOML では、バックスラッシュの扱いを気にせず読める `[0-9]` を `\d` より優先すると分かりやすくなります。

```toml
[[paths]]
pattern = "^abc[0-9]+$"
path = "ABC"

[[paths]]
pattern = "^arc[0-9]+$"
path = "ARC"

[[paths]]
pattern = "^agc[0-9]+$"
path = "AGC"

[[paths]]
pattern = "^ahc[0-9]+$"
path = "AHC"

[[paths]]
pattern = "^typical90$"
path = "Typical90"

[[paths]]
pattern = "^math-and-algorithm$"
path = "MathAndAlgorithm"

[[paths]]
pattern = "^tessoku-book$"
path = "TessokuBook"

[[paths]]
pattern = "^adt_.+$"
path = "ADT"
```

後半のルールは、たとえば次の contest ID を想定しています。

- `typical90`
- `math-and-algorithm`
- `tessoku-book`
- `adt_all_20260831_1`
- `adt_easy_20260831_2`

ADT は contest ID の種類が変わることがあるため、この例では `adt_` で始まる ID をまとめて `ADT` へ振り分けています。

## Contest を開く・切り替える

workspace ルートで `atc` を起動し、Workspace Home で `c` を押す方法が基本です。Contest ID を入力して `Enter` を押すと、既存の Contest を開くか、新しく作成します。

Contest 画面から別の Contest へ移るには、`:` の Command Palette で `Switch Contest` を選びます。Workspace Home へ戻る場合は `Back to Workspace Home` を選びます。

コマンドから直接開くこともできます。

ワークスペースのルートで:

```bash
atc contest abc466
atc c abc466
```

`atc c` は `atc contest` の短い別名です。

`abc466` がまだなければ、AtCoder から問題情報とサンプルを取得し、設定したデフォルト言語のソースを作成します。正常な既存の Contest はそのまま TUI で開きます。

`.atc-workspace.toml` がない場合も利用でき、その場合は実行ディレクトリ直下の `abc466` を対象にします。既存データに問題が見つかった場合は、[修復の条件](#壊れたコンテストの修復)を確認してください。

## `atc new`

TUI を起動せず、コンテストを作成するコマンドです。

```bash
atc new abc466
atc new abc466 -l python
```

`atc new` は `.atc-workspace.toml` の routing を使わず、常に**実行したディレクトリの直下**へ `<contest-id>` を作ります。上の例を `atcoder/` で実行すると、保存先は `atcoder/abc466` です。

workspace の `[[paths]]` に従って保存したい場合は、workspace root で `atc contest <contest-id>` を使ってください。

保存先ディレクトリが既に存在する場合、`atc new` はその中へファイルを追加したり、既存ファイルを書き換えたりしません。新規作成時も完成した contest を準備してから、既存の保存先を置き換えない方法で配置します。

## `-c` でコンテストを指定する

いくつかのコマンドは、ワークスペースのルートからコンテストを指定できます。

```bash
atc test A -c abc466
atc watch -c abc466
atc refresh -c abc466
atc stress A -c abc466
atc stress init A -c abc466
atc submit A -c abc466
```

## 親ディレクトリは自動検索しない

`atc` はワークスペース設定を親ディレクトリまで探しません。コマンドを起動した正確なカレントディレクトリだけを workspace root の候補として調べます。

たとえば:

```text
atcoder/
├── .atc-workspace.toml
└── ABC/
    └── abc466/
```

`atcoder/ABC/` で `atc test A -c abc466` を実行しても、`atcoder/` の設定は自動では使われません。

ワークスペース機能を使うコマンドは、`.atc-workspace.toml` がある**そのディレクトリ**から実行してください。

この仕様により、意図していない親ディレクトリの設定を勝手に使うことを避けています。

## コンテストディレクトリの中身

作成された Contest は、おおむね次の構成になります。Stress Test や User Input のフォルダは、必要になったときに作成されます。

```text
abc466/
├── .atc/
│   ├── contest.toml
│   ├── stress/
│   └── user-inputs/
├── tests/
│   ├── A/
│   │   ├── sample-1.in
│   │   ├── sample-1.out
│   │   └── ...
│   └── ...
├── A.cpp
├── B.cpp
└── ...
```

使用する言語が Python の場合、通常のソースは `.py` で作成されます。

## 外部ツールからローカルソースを取得する

`atc source` は、保存済みの問題情報とソースコードを読み取り、JSON を返します。問題は Contest の `index` か AtCoder の `task_id` のどちらか一方で指定します。

```bash
atc source abc484 A --workspace /path/to/workspace --json
atc source abc484 --task-id abc484_a --workspace /path/to/workspace --json
atc source abc484 A --language python --json
```

`/path/to/workspace` は自分のワークスペースのパスに置き換えてください。`--workspace` を省略すると、現在のディレクトリから[ワークスペースの振り分け](#contest-の保存先)に従って Contest を探します。問題ラベルの比較では ASCII の大文字小文字を区別せず、task ID は `.atc/contest.toml` の値と完全一致で照合します。

候補は `.cpp` と `.py` です。両方ある場合は `--language cpp` または `--language python` を指定してください。指定した場合は、その言語のファイルだけを調べます。ソースは UTF-8 で 1 MiB 以下の通常ファイルが対象です。シンボリックリンクや Windows の reparse point は拒否します。ファイルの作成・更新や AtCoder への通信は行いません。

`--json` は必須です。成功時の標準出力には JSON オブジェクトが1つだけ出ます。失敗時は非ゼロの終了コードを返し、標準エラー出力に理由を表示します。

```json
{
  "schema_version": 1,
  "contest_id": "abc484",
  "index": "A",
  "task_id": "abc484_a",
  "url": "https://atcoder.jp/contests/abc484/tasks/abc484_a",
  "language": "cpp",
  "source_path": "/path/to/workspace/ABC/abc484/A.cpp",
  "source": "int main() {}\n"
}
```

`source_path` はローカルの絶対パスです。公開用のノートを生成する場合は、既定で公開内容から外してください。取得結果には現在のローカルソースが入ります。AC や提出履歴を確認するときは AtCoder の提出結果を参照してください。

## `refresh`

現在のコンテストの問題情報とサンプルをローカルで検査します。サンプルの欠損など、修復できる問題がある場合は、AtCoder から問題一覧と公式サンプルを取得して修復します。

```bash
atc refresh
```

ワークスペースのルートから指定する場合は `-c` を使います。`.atc/contest.toml` が欠けていても、`.atc/` があり、既存の `tests/` がなければ、この指定で修復できます。

```bash
atc refresh -c abc466
```

`refresh` が更新するのは `atc` が管理する問題メタデータとサンプルです。自分で書いたソースファイルは上書きしません。

ローカルのデータが健全なら `local data healthy; refresh skipped` と表示し、通信やファイル更新なしで終了します。健全性の判定にはローカルのメタデータとサンプルの構造を使います。AtCoder 側の変更を取り込みたい場合は `--force` を付けます。

```bash
atc refresh --force
```

`--force` でも Contest ID と既存の `tests/` の所有権を確認します。管理外の項目や確認が必要な復旧データがある場合や、既存の更新ロックが通常ファイルでない場合は、取得前に理由を表示して停止します。既存の `tests/` の所有権を確認できない場合や、メタデータのバージョンが未対応の場合は、[`atc migrate --dry-run`](#migrate) で移行できるか確認してください。

単体でも `--all` でも、取得前に確認したメタデータと `tests/` を置換前に照合します。サンプルの編集、追加、削除、入れ替えを検出すると中断します。`-c` または `--all` では、ワークスペースの振り分けルールと対象の経路も再確認します。

Refresh と Migrate は、対象のフォルダや振り分け先までの途中に別の `.atc-workspace.toml` があると更新を拒否します。表示されたワークスペースのルートに移動し、そこで `-c` または `--all` で対象を選んでください。`--force` や `--yes` を指定した場合も同じです。

更新前の問題メタデータとサンプルは、コンテストディレクトリ内の `.atc-recovery-refresh-*` に残ります。`atc contest` などから行う修復では `.atc-recovery-repair-*` が作られます。正常に完了した回復領域は、通常の読み込みを妨げません。回復領域は自動削除されないため、更新を繰り返すとディスク使用量が増えます。整理する前に現在の `.atc/contest.toml` と `tests/`、回復領域の内容を確認し、必要なファイルを別の場所にバックアップしてください。

### 更新や修復が途中で止まった場合

途中で止まった場合は、一時保存先の確認が必要です。表示された場所と現在の管理ファイルを確認し、バックアップを取ってから復旧してください。一時保存先を自動で削除・復元することはありません。

サンプルの修復に失敗して元の状態に戻った場合、保存しようとしたデータが `.atc-aborted-repair-*` に残ることがあります。正常に記録された中断時の退避先は、テストや修復の再実行を妨げません。

準備中にエラーが起きた場合も、作成済みの一時保存先が残ることがあります。内容を確認する前に削除しないでください。

エラーに `installed new data` と表示された場合は、新しい問題メタデータと `tests/` がすでに反映されている可能性があります。以前の `tests/` は、エラーに表示された recovery path へ保持されている場合があります。再実行する前に、現在の Contest の `.atc/contest.toml` と `tests/`、案内された recovery path の両方を確認してください。エラーだけを見て、現在のファイルや recovery data を削除・上書きしないでください。

それ以外でも recovery path が表示された場合は、その内容を確認してから再実行してください。

## `migrate`

旧ツールで作成したコンテストや、管理情報が欠けたコンテストを 1 件ずつ移行できます。対象のディレクトリ内で、まず移行計画を確認します。`--dry-run` は通信もファイルの変更も行いません。

```bash
atc migrate --dry-run
atc migrate
```

現在のディレクトリ名を Contest ID の候補にします。表示された ID と対象フォルダを確認し、進める場合は `[y/N]` に `y` と答えます。ワークスペースのルートから指定する場合は `atc migrate -c abc466` を使います。ワークスペースの振り分け設定に従います。対話できない環境で実行する場合は、対象を確認したうえで `--yes` を指定します。

`.atc-workspace.toml` があるディレクトリは移行対象から外れます。別のワークスペースを含む経路を指定したときは、そのルートで `atc migrate -c abc466` または [`atc migrate --all`](#workspace-内を一括で確認更新する) を使います。

管理情報が欠けている場合、形式が古い場合、既存の `tests/` に管理外のファイルやフォルダがある場合などに使えます。壊れた `.atc/contest.toml` がある場合は、TOML の `contest_id` を読み取れて対象と一致する必要があります。別の ID が記録されている場合、移行で扱う場所にリンクなどの安全でない項目がある場合、確認が必要な復旧データがある場合は、移行を停止します。ローカルデータが健全なら通信せず終了します。サンプルの欠損だけを修復する場合は [`atc refresh`](#refresh) を使ってください。

移行は AtCoder から公式データを取得したあと、既存の `tests/` と `.atc/contest.toml` を退避してから新しいデータを配置します。ソースや `.atc/` 内の Stress、User Input などはそのまま使えます。移行前の独自テストも、フォルダ構造ごとバックアップに残ります。移行後の `tests/` には公式サンプルが入ります。

### 移行のバックアップから戻す

完了すると、コンテスト内の `.atc-recovery-migrate-*` の場所が表示されます。移行が途中で止まった場合は、エラーに表示された `.atc-migrate-*` などを確認してください。処理が止まった時点によっては、新しいファイルがすでに配置されています。

1. 実行中の `atc` を終了し、現在の `tests/`、`.atc/contest.toml`、表示された復旧フォルダを別の場所にコピーします。
2. 移行前に戻す場合は、復旧フォルダの `previous-tests/` と `previous-contest.toml` を確認します。対応する現行ファイルやフォルダを別の場所に退避し、確認できた旧データをそれぞれ `tests/` と `.atc/contest.toml` に戻します。旧データが現在の場所に残っている場合は、そのまま保持します。元から存在しなかったと確認できた項目は、移行後のデータを退避したままにします。`.atc/` フォルダごとの置き換えは避けてください。
3. 中断した移行の `.atc-migrate-*` が残っていると、通常の読み込みや再更新が止まる場合があります。必要なデータをすべて回収し、現在の状態を確認してから、復旧フォルダをコンテストの外へ移動してください。

## Workspace 内を一括で確認・更新する

`.atc-workspace.toml` がある Workspace ルートで実行します。Contest のディレクトリや振り分け先の中から実行する場合は、先にルートへ移動してください。`--all` と `-c` は同時に指定できません。

```bash
atc refresh --all --dry-run
atc refresh --all
atc migrate --all --dry-run
atc migrate --all
```

探索範囲は Workspace 直下、設定した振り分け先の直下、既知の `ABC`・`ARC`・`AGC` フォルダの直下です。`ABC(Atcoder Beginner Contest)`、`ARC(Atcoder Regular Contest)`、`AGC(Atcoder Grand Contest)` も対象です。振り分けルールに一致する ID は、そのルールの保存先でのみ処理します。

`.atc/` のあるディレクトリを候補にします。`.atc/` がない旧形式では、`abc083` のように `abc`・`arc`・`agc`・`ahc` と数字で構成される ID、または設定された振り分け先にある ID を調べ、`tests/`・更新ロック・復旧用フォルダなどが残っていれば候補にします。フォルダ名は候補 ID として使い、更新前には既存データの所有権を検査します。同じ ID の重複、複数ルールへの一致、リンクや Windows の再解析ポイントを含む経路は理由とともに表示します。別の Workspace や設定外の階層を再帰的に探索することはありません。

- `--dry-run` は対象パス、ローカル診断、処理予定と拒否理由を表示します。通信やファイル更新、更新ロックや作業フォルダの作成は行いません。
- `refresh --all` は修復が必要な対象を更新します。`--force` を付けると健全な対象も取得し直します。`.atc/` がない場合は通常の refresh を拒否します。旧形式の移行は `migrate --all --dry-run` で確認してください。既存の `tests/` の所有権と復旧状態は引き続き検査します。
- `migrate --all` は全体の計画を表示し、端末上で一度確認します。`y` と答えると、移行できる対象を順番に処理します。元のデータは各 Contest の移行バックアップに残ります。サンプルだけに問題がある場合は `refresh --all` を案内します。
- 対話できない環境で変更する場合は、計画を確認してから `atc refresh --all --yes` または `atc migrate --all --yes` を実行してください。健全な対象のスキップや `--dry-run` に `--yes` は不要です。

HTTP リクエストの開始間隔は、問題一覧・個々のサンプル・再試行を含めて最低 1 秒です。再試行しても HTTP 429 が続く場合や、サーバーが `Retry-After` で 60 秒を超える待機を求めた場合は、理由を表示して残りの更新を停止します。リダイレクトが返った対象は理由を表示して失敗として集計します。その他の個別の失敗や復旧が必要な項目があっても、独立した対象の処理は続けます。

`Retry-After` は秒数と HTTP 日付に対応します。日付の場合は、応答の `Date` が有効ならサーバーの時刻を、そうでなければ端末の現在時刻を基準にします。求められた時間は応答を受け取ってから待ちます。基準時刻で期限が過ぎていれば通常のリクエスト間隔だけを適用し、指定がない場合や不正な場合は 2 秒待ちます。単体の取得でも、指定が 60 秒を超えた時点でその取得を終了します。

探索、診断、計画の表示、承認待ち、更新中に Ctrl+C を押すと、次の対象には進みません。準備中の対象は、置換開始を記録する前に中断が確定すれば `NOT RUN` になります。その場合も、作成済みの作業フォルダが残ることがあります。置換開始の記録に入った対象は、更新を完了するか、復旧できる状態で終了します。通信中の中断は、実行中のリクエストが終了するまで待つ場合があります。

集計の `MIGRATION REQUIRED` と `REFRESH REQUIRED` は案内です。ほかに失敗がなければ終了コードは 0 です。探索エラー、`Blocked`、個別の処理失敗が一つでもあれば非ゼロです。Ctrl+C で実行を中断すると終了コード 130 になります。再実行時は、すでに健全になった対象をスキップします。

探索途中の集計では、その時点までに見つかった件数を `Discovered (partial)` と表示します。`Undiagnosed` は診断に進んでいない件数で、`Not run` は更新予定のうち実行しなかった件数です。出力先のパイプが詰まっているときは、集計を途中で打ち切る場合があります。

## 壊れたコンテストの修復

`.atc/contest.toml` で複数の問題に同じ task ID が指定されている場合や、AtCoder の問題 URL が記録された Contest ID・task ID と食い違う場合は、修復の対象になり得ます。この状態のメタデータを使う通常のテストや提出はエラーになります。

修復の確認が表示されたら、Workspace Home では `Repair & Open`、Contest の切り替え時は `Repair & Switch` を選びます。`atc contest <contest-id>` では `[y/N]` に `y` と答えます。確認を取り消すと修復は始まりません。

修復先のフォルダ名と、既存の `.atc/contest.toml` に記録された Contest ID は、開こうとした Contest ID と一致する必要があります。メタデータがない場合は、ほかの条件を満たせば作り直せます。既存ファイルから Contest ID を確認できない場合や、未対応のメタデータバージョン、安全でない問題 URL（外部サイトの URL など）がある場合は修復を拒否します。

### 既存の `tests/` の扱い

状態に応じて、メタデータだけ、サンプルだけ、または両方を修復します。既存の `tests/` がある場合は、Contest ID に加えて、問題フォルダやサンプルファイルが atc-rs の管理対象かを旧メタデータから確認します。`tests/` を置き換える場合も、メタデータだけ修復して `tests/` を残す場合も、管理対象か確認できなければ処理は止まります。

task ID の重複や URL の不一致がある旧メタデータでは、記録された URL が AtCoder から取得した公式の問題 URL と一致する必要があります。旧メタデータ内で URL が Contest ID・task ID と食い違う問題は、問題ラベル（`A` など）も一致する必要があります。件数や入出力の内容が公式サンプルと一致していても、それだけでは管理対象と判断できません。

`.atc/` と管理情報が欠け、既存の `tests/` がない Contest は、対象のディレクトリ内で [`atc refresh --force`](#refresh) を使って再構築できます。この場合はディレクトリ名が Contest ID になります。既存の `tests/` の所有先を確認できない場合は、[`atc migrate --dry-run`](#migrate) で確認してください。

修復に失敗すると、復旧用のフォルダが残る場合があります。削除せず、[更新や修復が途中で止まった場合](#更新や修復が途中で止まった場合)の手順を確認してください。

## パスに関する制限

ワークスペースの `path` には、ワークスペース内の相対ディレクトリを指定します。

`..`、絶対パス、Windows の予約デバイス名など、ワークスペース外へ出たり環境によって危険になるパスは拒否されます。
