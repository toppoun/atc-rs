# TUI

`atc` の TUI は、workspace を探す Global Home、workspace 内の入口となる Workspace Home、contest 作業を行う Contest 画面の3層で構成されます。

```text
Global Home
    ↓ Open / Init Here
Workspace Home
    ↓ Open / Create Contest
Contest
    ↓ Back to Workspace Home
Workspace Home
```

workspace root で `atc` を起動した場合は Workspace Home から、workspace 外で起動した場合は Global Home から始まります。判定対象は正確な現在ディレクトリだけで、親 workspace は自動探索しません。

`atc contest <contest-id>` / `atc c <contest-id>` と `atc watch` は、従来どおり Contest 画面を直接起動できます。

## Global Home

workspace 外で引数なしの `atc` を起動すると Global Home が開きます。Explorer で directory を選択し、既存 workspace を Workspace Home で開くための画面です。

幅の広い terminal では Explorer が左 pane に表示されます。狭い terminal では `o` で Explorer overlay を開き、directory を選んでもう一度 `o` を押します。

### Explorer

| キー | 操作 |
| --- | --- |
| `j` / `↓` | 次の directory を選択 |
| `k` / `↑` | 前の directory を選択 |
| `Enter` | 選択 directory を展開 / 折りたたみ |
| `l` / `→` | 展開する。展開済みなら最初の子 directory へ移動 |
| `h` / `←` | 折りたたむ。折りたたみ済みなら表示上の親 directory へ移動 |
| `Backspace` | Explorer root を1つ上の directory へ移動 |
| `r` | 選択 directory を再読み込み |
| `o` | 選択 directory を Open |
| `g` | Go to Path を開き、入力した directory を Explorer root にする |
| `:` | Global Home の Command Palette |
| `?` | shortcut help |
| `q` | 終了 |

`Go to Path` では `Enter` で移動し、`Esc` で cancel します。Global Home の Command Palette には `Open`、`Go to Path`、`Quit` があります。

### Open / Init Here

既存 workspace を選択して `o` を押すと、その workspace の Workspace Home が開きます。

ordinary directory を選択して Open した場合は、`Initialize Workspace` confirmation が表示されます。`Enter` で Init Here すると workspace を初期化し、そのまま Workspace Home を開きます。`Esc` で cancel すると Global Home へ戻ります。

## Workspace Home

workspace root で `atc` を起動すると直接 Workspace Home へ入ります。Global Home で workspace を Open または Init Here した場合も、この画面へ進みます。

| キー | 操作 |
| --- | --- |
| `c` | Open Contest modal を開く |
| `:` | Workspace Home の Command Palette |
| `?` | shortcut help |
| `q` | 終了 |

`c` で contest ID を入力します。既存 contest なら `Enter` で Open し、存在しなければ `Enter` で Create & Open します。修復が必要な contest は、確認後に Repair & Open できます。`Esc` で cancel します。

Workspace Home の Command Palette には `Open Contest` と `Quit` があります。contest を開くと Contest 画面へ遷移します。

現在、Workspace Home から別 workspace へ切り替える機能はありません。別 workspace へ移るには `q` で終了し、移動先の workspace root で `atc` を起動してください。

## Contest 画面

Contest 画面では、問題ごとの実行状態、公式 sample、保存済み Stress case、User Input、Expected / Actual / stderr、提出状況を確認できます。

起動方法の例:

```bash
# Workspace Home から c で contest を開く
atc

# workspace root または ordinary directory から contest を直接開く
atc contest abc466
atc c abc466

# contest directory から起動
atc watch

# workspace root から contest を指定
atc watch -c abc466
```

TUI が使いにくい terminal では、plain 表示を利用できます。

```bash
atc watch --plain
```

### 画面構成

- header: contest ID、選択中の source / language、C++ Debug、実行状態
- problem row: 各問題の sample test または submission status と現在選択中の問題
- side pane: Samples または Submissions
- detail pane: 選択 case の Input / Expected / Actual / stderr、User Input editor、compile / Stress の詳細
- footer: 最新 submission receipt。`?` で表示する help には現在利用できる主な shortcut

`s` で side pane を表示 / 非表示にし、`v` で Samples / Submissions mode を切り替えます。terminal 幅が狭い場合は side pane を表示せず、detail pane を優先します。

### キー操作

次の shortcut は、modal や User Input editor を開いていない通常状態で有効です。

| キー | 操作 |
| --- | --- |
| `q` | application を終了 |
| `h` / `←` | 前の問題 |
| `l` / `→` | 次の問題 |
| `j` / `↓` | 次の sample / Stress case / User Input |
| `k` / `↑` | 前の sample / Stress case / User Input |
| `r` | 選択中の問題の公式 sample と保存済み Stress case を再実行 |
| `t` | Submit modal を開く |
| `d` | C++ Debug を切り替え |
| `s` | side pane を表示 / 非表示 |
| `v` | side pane を Samples / Submissions へ切り替え |
| `S` | Stress Helper を確認し、準備済みなら Stress Test を開始 |
| `i` | 必要な Stress Helper を作成 |
| `c` | Switch Contest modal を開く（workspace 内のみ） |
| `:` | Command Palette を開く |
| `?` | shortcut help を表示 |

## User Input

User Input は、公式 sample とは別に任意の stdin を作成し、選択中の source へ1件ずつ渡して実行する機能です。Expected output を持たないため AC / WA の比較は行わず、終了 status、Output、stderr を detail pane へ表示します。`r` で実行する公式 sample / 保存済み Stress case のテストには含まれません。

User Input は Samples mode の side pane に表示されます。

1. `+ New Input` を click すると、新しい Draft と inline editor が開きます。
2. 文字、改行、tab を入力し、矢印、`Home`、`End`、`Backspace`、`Delete` で編集します。
3. `Ctrl+S` または `[Save]` の click で保存します。新しい Draft は保存済みの `Input N` になります。
4. 保存済み User Input を選択して `[Edit]` を click すると再編集できます。
5. `[Run]` を click すると、保存済み内容または編集中 buffer の現在内容を個別実行します。
6. 保存済み User Input の `×` を2回 click すると削除します。1回目の `×?` は削除確認です。

case の選択には `j` / `k`、`↑` / `↓`、row の click を利用できます。編集中は `Esc` または `[Cancel]` で編集を cancel します。通常 shortcut より inline editor の入力が優先されるため、たとえば編集中の `j` や `q` は stdin text として入力されます。

問題を移動したときや、保存済み入力を編集・実行するときは、外部で変更された User Input を再読み込みします。外部削除や同期失敗があれば problem row 付近に notice を表示し、編集中の内容は勝手に置き換えません。

User Input の作成、編集、保存、実行、削除は現在 Command Palette 項目ではなく、Samples / detail pane 上の click action です。

## Submit

Contest 画面で `t`、または Command Palette の `Submit` を実行すると Submit modal が開きます。

- `↑` / `↓` または `j` / `k`: 存在する C++ / Python source を選択
- `Enter`: 選択した source / language で提出を確定
- `Esc`: 提出せず modal を閉じる

modal には problem、source file、language と提出 policy が表示されます。Python runtime は `config.toml` の `[submit].python_runtime` に従います。CLI では `atc submit <problem> --runtime <runtime>` で override できます。

提出開始後は AtCoder 上の submission を特定し、Waiting for Judge、Judging、AC / WA などの verdict を追跡します。Submit modal を閉じても追跡は継続します。

### 最新 receipt と Submissions pane

通常の footer には、最新 submission の problem、language、status などを含む receipt が表示されます。

`v` を押すと side pane が Submissions mode へ切り替わり、現在の application session で行った submission history と各 status を新しい順に確認できます。problem row も submission status 表示へ切り替わります。もう一度 `v` を押すと Samples mode へ戻ります。

### Unknown outcome

提出結果を確定できない場合は `Unknown` と表示し、安全のため自動再送しません。同じ application session では同じ contest / task への再送もブロックします。

AtCoder の My Submissions を確認し、実際に提出されていないことを確認してから、必要なら atc を再起動して再試行してください。

## Back to Workspace Home

workspace から開いた Contest 画面では、Command Palette の `Back to Workspace Home` で現在の contest を閉じ、Workspace Home へ戻れます。この action に直接の keyboard shortcut はありません。

`atc contest` や `atc watch` を workspace 外から直接起動した Standalone Contest では、この action は unavailable です。Workspace Home から Global Home へ戻る workspace switching も現在は実装されていません。

## Command Palette

`:` を押すと Command Palette が開きます。文字を入力して action を絞り込み、`↑` / `↓` で選択、`Enter` で実行します。`Backspace` で query を消し、`Esc` で閉じます。

Contest 画面の現在の action:

- Run Tests
- Submit
- Open Source
- Open Settings
- Open Workspace Settings
- Open Template
- Toggle Debug
- Toggle Side Pane
- Change Side Pane Mode（Samples / Submissions）
- Start Stress
- Stop Stress
- Initialize Stress
- Refresh Contest
- Switch Contest
- Back to Workspace Home

現在の状態で実行できない action は、理由付きで unavailable と表示されます。`Open Workspace Settings`、`Switch Contest`、`Back to Workspace Home` は workspace 外の Standalone Contest では利用できません。

## Modal の操作

modal または Command Palette の表示中は、その操作が通常 shortcut より優先されます。原則として `Esc` で cancel / close し、`Enter` で選択や確認を確定します。

たとえば Submit modal の表示中に `q` を押しても application は終了せず、Submit modal が先に入力を処理します。modal を閉じてから通常 shortcut を利用してください。

## Open Source

Command Palette の `Open Source` から、選択中の問題の source を editor で開けます。

C++ / Python を選択でき、source がまだ存在しない場合は `i` で作成してから開けます。

```text
Enter  既存 file を開く
i      file を作成して開く
↑/↓    language を選択
j/k    language を選択
Esc    閉じる
```

新しく作る source には通常の source template が使われます。

## Open Settings / Workspace Settings / Template

`Open Settings` は global 設定ファイルを開きます。まだ存在しない場合は `i` で `atc config init` 相当の初期化を行い、そのまま editor で開けます。

`Open Workspace Settings` は、workspace から TUI を起動している場合に `.atc-workspace.toml` を開きます。workspace 外では unavailable です。workspace config の新規作成は、対象 directory で `atc init` を実行します。

`Open Template` は C++ / Python の通常 source template を開きます。まだ存在しない場合は `i` で初期化して開けます。

詳しくは [設定](configuration.md) を参照してください。

## Editor 連携

editor は次の順で決定されます。

1. `config.toml`の`[editor]`
2. Windows / macOS で VS Code / Cursor の統合 terminal を自動検出
3. `VISUAL`
4. `EDITOR`

Vim / Neovim などは通常 `terminal` mode で起動します。

```toml
[editor]
command = "nvim"
mode = "terminal"
```

TUI の terminal 制御を一時的に戻して editor を起動し、終了後に TUI を復元します。

VS Code などは `external` mode で起動できます。

```toml
[editor]
command = "code"
args = ["-r"]
mode = "external"
```

## Stress Test

`S` を押したとき、Stress Helper がなければセットアップが必要な状態として表示されます。`i` で Helper を作成し、編集後にもう一度 `S` を押して開始します。Helper を作成しただけでは Stress Test は自動開始されません。

詳しくは [ストレステスト](stress.md) を参照してください。

## Contest の Refresh / Switch

Command Palette の `Refresh Contest` で、現在の contest の問題情報と sample を更新できます。更新処理は開始後に cancel できません。source は上書きしません。

workspace から起動した Contest 画面では、`c` または Command Palette の `Switch Contest` を利用できます。contest ID を入力すると、同じ workspace 設定を使って対象 contest へ切り替えます。存在しない contest は確認後に作成されます。

## マウス操作

- Samples pane の row を click: case を選択
- Samples pane 上で wheel: case を移動
- `+ New Input`、`[Edit]`、`[Save]`、`[Run]`、`[Cancel]`、`×`: User Input 操作
- Detail pane 上で wheel: 詳細を scroll
- Detail の scrollbar を click / drag: scroll 位置を変更
- Detail の section heading を click: section を折りたたみ / 展開

modal や Command Palette を開いている間は、背後のマウス操作を処理しません。
