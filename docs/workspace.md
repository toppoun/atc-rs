# ワークスペース

ワークスペースを使うと、ABC / ARC / AGC などのコンテストを1つのディレクトリ以下へ整理できます。

## ワークスペースを作る

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

## コンテストを開く

ワークスペースのルートで:

```bash
atc contest abc466
```

`abc466` がまだなければ、AtCoder から問題情報とサンプルを取得して作成します。既に存在すればそのコンテストを使い、そのまま TUI を起動します。

`.atc-workspace.toml` がない場合も利用でき、その場合は実行ディレクトリ直下の `abc466` を対象にします。既存データに修復が必要な場合は確認を求め、ユーザーが同意したときだけ `atc` 管理部分を修復します。

## `atc new`

TUI を起動せず、コンテストを作成するコマンドです。

```bash
atc new abc466
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

作成されたコンテストは、おおむね次の構成になります。

```text
abc466/
├── .atc/
│   ├── contest.toml
│   └── stress/
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

## `refresh`

現在のコンテストで問題情報とサンプルを更新します。

```bash
atc refresh
```

ワークスペースのルートから:

```bash
atc refresh -c abc466
```

`refresh` が更新するのは `atc` が管理する問題メタデータとサンプルです。自分で書いたソースファイルは上書きしません。

`tests/` の中に `atc` が管理していないファイルやディレクトリがある場合は、それらを勝手に削除せず、更新を停止してエラーにします。

更新の準備中に contest のディレクトリや metadata、管理対象の samples が変わった場合も、安全のため処理を停止します。エラーに recovery data の保存先が表示された場合は、その内容を確認してから再実行してください。

## 壊れたコンテストの修復

`atc contest <contest-id>` で既存コンテストのメタデータやサンプルに問題が見つかった場合、確認後に修復できる場合があります。

現在のディレクトリをコンテストとして明示的に再構築するための `--force` もあります。

```bash
atc refresh --force
```

`--force` は通常の更新用ではなく、既存メタデータを信用できない状態からの復旧用です。

## パスに関する制限

ワークスペースの `path` には、ワークスペース内の相対ディレクトリを指定します。

`..`、絶対パス、Windows の予約デバイス名など、ワークスペース外へ出たり環境によって危険になるパスは拒否されます。
