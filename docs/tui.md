# TUI の使い方

atc-rs の TUI は、workspace を選ぶ Home 画面と、問題を解く Contest 画面で構成されます。

```text
Global Home
  └─ Workspace Home
       └─ Contest
```

`atc` を引数なしで起動すると、正確な現在のフォルダだけを確認します。

- workspace のルートで起動した場合: `Workspace Home`
- それ以外で起動した場合: `Global Home`

親フォルダにある workspace は自動では探しません。

Contest を直接開く場合は、workspace ルートまたは通常のフォルダで次を実行します。

```bash
atc contest abc466
```

既存の Contest フォルダから直接 TUI を開く場合は `atc watch`、workspace ルートから Contest を指定する場合は `atc watch -c abc466` も利用できます。

## Global Home

Global Home は、フォルダを探して workspace を開くための画面です。

左側の Explorer でフォルダを選び、`o` の `Open` で開きます。通常のフォルダを開いた場合は `Initialize Workspace` が表示されるため、`Enter` の `Init Here` でその場所を workspace にできます。初期化せずに戻るには `Esc` を押します。

画面の幅が狭い場合は、`o` で Explorer の選択画面が開きます。その中でもう一度 `o` を押すと、選択中のフォルダを開きます。`Esc` で閉じます。

| キー | 操作 |
| --- | --- |
| `j` / `↓` | 次の項目へ移動 |
| `k` / `↑` | 前の項目へ移動 |
| `l` / `→` | フォルダを展開、または子へ移動 |
| `h` / `←` | フォルダを閉じる、または表示中の親へ移動 |
| `Enter` | フォルダの展開・折りたたみ |
| `Backspace` | Explorer の起点を親フォルダへ移動 |
| `r` | Explorer を再読み込み |
| `o` | 選択中のフォルダを開く |
| `g` | `Go to Path` を開く |
| `s` | Global Settings を開く |
| `G` | Global Config をエディタで開く |
| `t` | Template を開く |
| `a` | Authentication を開く |
| `?` | `Explorer Shortcuts` を表示 |
| `q` | 終了 |

`Go to Path` ではフォルダの path を入力し、`Enter` で移動します。`Esc` でキャンセルします。

Global Config がまだない場合は `Initialize & Open` が表示されます。`Enter` でコメントだけの設定ファイルを作成して開き、`Esc` で戻ります。既存ファイルは上書きしません。

## Workspace Home

Workspace Home は、現在の workspace で Contest を開くための入口です。

| キー | 操作 |
| --- | --- |
| `c` | Contest を開く、または作成する |
| `s` | Global Settings を開く |
| `w` | Workspace Config をエディタで開く |
| `G` | Global Config をエディタで開く |
| `t` | Template を開く |
| `a` | Authentication を開く |
| `q` | 終了 |

`c` を押したら Contest ID を入力し、`Enter` を押します。既存の Contest はそのまま開き、まだない Contest は AtCoder から問題情報とサンプルを取得して作成します。

Workspace Home の `G` でも、Global Config がまだない場合は `Initialize & Open` で作成できます。

Workspace Config は異なります。Workspace Home を開いた後に `.atc-workspace.toml` がなくなった場合、`w` を押すと `Workspace Config Unavailable` と `Workspace config is missing and was not recreated` が表示されます。`Enter` または `Esc` で閉じられますが、marker は自動再作成されません。

元の `.atc-workspace.toml` を復元できる場合は復元してください。復元できず、初期設定の振り分けでよい場合は、TUI を終了し、正確な workspace ルートで `atc init` を実行してから `atc` を起動し直します。`atc init` は marker がない場合だけ既定ファイルを作成し、既存の不正なファイルやディレクトリを上書きしません。カスタムした振り分けは自動では戻らないため、Contest を開く前に[workspace config](workspace.md#workspace-config)を確認してください。

Contest の管理ファイルやサンプルが不足している場合は、修復の確認が表示されます。ソースは上書きされません。`Esc` でキャンセルできます。

Workspace Home から別の workspace へは切り替えられません。`q` で終了し、移動先の workspace ルートで `atc` を起動してください。

## Global Settings

Global Home または Workspace Home で `s` を押すと、Global Config の全項目を表示・編集できます。Workspace Config のrouting設定は対象外です。

一覧では `↑` / `↓` または `j` / `k` で移動し、`Enter` で編集、`r` で選択中のキーをreset、`e` でTOMLをエディタに開き、`Esc` で戻ります。resetは組み込み値を書き込む操作ではなく、対象キーをTOMLから削除する操作です。

文字列入力では貼り付け、Unicode、`Home`、`End`、左右キー、`Backspace`、`Delete` を利用できます。複数行の貼り付けは1行へ結合せず拒否します。timeoutは秒単位で、0より大きい有限値だけを保存できます。

配列の編集では、`a` で追加、`d` で削除、`J` / `K` で並べ替え、`Enter` で1要素を編集、`s` で配列全体を保存します。`Esc` は最も内側の編集だけをキャンセルします。

外部変更との競合、読み取り専用のファイル、不正なConfig、保存結果を確定できないエラーがある場合、Settingsは元ファイルを自動上書きしません。競合画面の `Esc` は編集中の値を保持し、`r Reload` はdiskを読み直したうえで編集中の値を画面に残します。確認後に保存するか、`Esc`で取り消してください。Reloadだけでは自動保存されません。詳しい保存契約は[設定](configuration.md#tui-の-settings-で編集する)を参照してください。

`e` で外部エディタから戻ると、Settingsは`config.toml`を再読込します。不正な内容になった場合は以前の値を現在値として表示せず、`Invalid Config`画面から再度エディタを開くか`r`で再読込できます。

## Authentication

Global Home または Workspace Home で `a` を押すと、AtCoder へ提出するための Cookie を管理できます。

| キー | 表示される操作 |
| --- | --- |
| `p` | `Paste Cookie` / `Replace Cookie` / `Repair Cookie` |
| `r` | `Reset Authentication` |
| `Esc` | Home へ戻る |

Cookie の入力画面には `Paste the REVEL_SESSION value.` と表示されます。値だけ、または `REVEL_SESSION=<value>` の形式で貼り付け、`Enter` の `Save` で保存します。`Esc` の `Cancel` では変更しません。

状態は `Authenticated`、`Not configured`、`Invalid`、`Not authenticated`、`Verification unavailable` などで表示されます。詳しい意味と対処は[AtCoder 認証](authentication.md)を参照してください。

## Template

Global Home または Workspace Home で `t`、Contest の Command Palette で `Open Template` を選ぶと、C++ / Python のテンプレートを選択できます。

`↑` / `↓` または `j` / `k` で言語を選びます。

- `Ready`: `Enter` の `Open` でエディタに開く
- `Missing`: `Enter` の `Initialize & Open` で作成して開く
- `Invalid`: 通常のファイルとして修復できる場合は `Enter` の `Open to Repair` で開く

安全に扱えないファイルの場合は開けません。`Esc` で戻ります。詳細は[テンプレート](templates.md)を参照してください。

## Contest

Contest 画面では、問題、テストケース、実行結果を確認できます。

画面には次の情報が表示されます。

- header: Contest ID、選択中の source / language、C++ Debug、実行状態
- problem row: 問題ごとの sample test または submission status
- side pane: `Samples` または `Submissions`
- detail pane: Input / Expected / Actual / stderr、compile や Stress Test の詳細
- footer: 最新の submission receipt と、現在利用できる主なキー

terminal の幅が狭い場合は side pane を表示せず、detail pane を優先します。

### 問題とケースを移動する

| キー | 操作 |
| --- | --- |
| `h` / `←` | 前の問題 |
| `l` / `→` | 次の問題 |
| `j` / `↓` | 次のテストケース |
| `k` / `↑` | 前のテストケース |
| `r` | 選択中の問題をテスト |
| `t` | Submit を開く |
| `S` | Stress Test の準備状態を確認し、準備済みなら開始 |
| `i` | 不足している Stress Helper を作成 |
| `c` | `Switch Contest` を開く（workspace 内のみ） |
| `:` | Command Palette を開く |
| `?` | ショートカットを表示 |
| `q` | 終了 |

`r` は公式サンプルと保存済みの Stress Test の反例を実行します。User Input は 1 件ずつ画面から実行します。

modal や入力欄を開いている間は、そちらの操作が通常のショートカットより優先されます。原則として `Esc` で閉じ、`Enter` で選択を確定します。

### 表示を切り替える

| キー | 操作 |
| --- | --- |
| `s` | side pane の表示・非表示 |
| `v` | side pane の `Samples` / `Submissions` を切り替える |
| `d` | C++ の Debug mode を切り替える |

Debug mode は C++ にだけ適用され、`LOCAL` macro と debug header を有効にします。

### Command Palette

`:` で Command Palette を開きます。文字を入力して絞り込み、`↑` / `↓` で選び、`Enter` で実行します。`j` / `k` を含む文字キーは検索 query へ入力されます。`Backspace` で入力を消し、`Esc` で閉じます。

利用できる操作は次のとおりです。

| 操作 | 内容 |
| --- | --- |
| `Run Tests` | 選択中の問題をテスト |
| `Submit` | 提出画面を開く |
| `Open Source` | ソースを選んでエディタで開く |
| `Open Template` | テンプレートを開く |
| `Toggle Debug` | C++ Debug mode を切り替える |
| `Toggle Side Pane` | side pane の表示を切り替える |
| `Change Side Pane Mode` | `Samples` / `Submissions` を切り替える |
| `Start Stress` | Stress Test を開始 |
| `Stop Stress` | 実行中の Stress Test を停止 |
| `Initialize Stress` | Stress Helper を作成 |
| `Refresh Contest` | 問題情報と公式サンプルを更新 |
| `Switch Contest` | workspace 内の別 Contest を開く |
| `Back to Workspace Home` | Workspace Home へ戻る |

`Switch Contest` と `Back to Workspace Home` は workspace から開いた Contest でのみ利用できます。
現在の状態で使えない操作は、理由とともに unavailable と表示されます。

`Refresh Contest` は AtCoder から問題情報と公式サンプルを更新し、ソースは上書きしません。開始後は途中でキャンセルできません。詳しいファイルの扱いは[ワークスペースの refresh](workspace.md#refresh)を参照してください。

### ソースを開く

Command Palette の `Open Source` では、`↑` / `↓` または `j` / `k` で C++ / Python を選びます。

- ソースがある場合: `Enter` で開く
- ソースがない場合: `i` の `Create & Open` でテンプレートから作成して開く
- 戻る場合: `Esc`

既存のソースは上書きされません。エディタを自動で開けない場合は、Contest フォルダの `A.cpp` や `A.py` を直接編集してください。

## Testing と User Input

`r` または `Run Tests` を実行すると、画面下部に compile、実行、判定の状態が表示されます。`Accepted`、`Wrong Answer`、`Runtime Error`、`Time Limit Exceeded`、`Compile Error` などの結果を確認できます。

任意の入力を試すには、`Samples` pane の `+ New Input` をクリックします。

1. 入力欄へ stdin を入力する
2. `Ctrl+S` または `[Save]` で保存する
3. `[Run]` でその入力だけを実行する
4. 後から変更する場合は `[Edit]` を選ぶ
5. 編集を取り消す場合は `Esc` または `[Cancel]`

編集中は文字、改行、tab のほか、矢印、`Home`、`End`、`Backspace`、`Delete` を使えます。`[Run]` は、編集中なら現在の内容をそのまま実行します。

User Input には expected output がないため、AC / WA の比較は行いません。終了状態、stdout、stderr を確認してください。

削除するには `×` をクリックし、確認のためもう一度 `×` をクリックします。保存した入力ファイル自体が削除されます。

外部エディタで User Input を変更した場合は、問題を開いたとき、編集を始めたとき、または実行するときに読み直します。TUI で編集中の内容は外部の変更で置き換えません。

詳しい判定方法と保存先は[テストと Watch](testing.md)を参照してください。

## Stress Test

| キー | 操作 |
| --- | --- |
| `S` | Stress Test を開始 |
| `i` | 不足している Stress Helper を作成 |

初回は Generator と Brute Force の準備が必要です。`i` で作成し、エディタで内容を書いてから `S` を押します。実行中は `S` で再度開始するのではなく、Command Palette の `Stop Stress` で停止します。

反例が見つかると保存され、以後の `r` でも公式サンプルの後に実行されます。詳しくは[ストレステスト](stress.md)を参照してください。

## Submission

`t` または Command Palette の `Submit` で提出画面を開きます。

1. `↑` / `↓` または `j` / `k` で提出するソースを選ぶ
2. `Enter` で提出する
3. 提出しない場合は `Esc` で戻る

提出後は画面を閉じても status の確認が続き、最新の結果が footer に表示されます。`v` で `Submissions` pane に切り替えると、現在の起動中に行った提出の履歴を確認できます。

Python の runtime は設定の `submit.python_runtime` を使用します。TUI から一時的に変更することはできません。

提出には AtCoder 認証が必要です。提出結果を確定できなかった場合は自動で再送しません。AtCoder の My Submissions で確認してから次の操作を行ってください。

## マウス操作

- `Samples` pane の行をクリック: ケースを選択
- `Samples` pane 上で wheel: ケースを移動
- `+ New Input`、`[Edit]`、`[Save]`、`[Run]`、`[Cancel]`、`×` をクリック: User Input を操作
- detail pane 上で wheel: 詳細を scroll
- detail の scrollbar をクリックまたは drag: scroll 位置を変更
- detail の section heading をクリック: section を折りたたみ・展開

modal や Command Palette を開いている間は、背後のマウス操作を受け付けません。

## 設定変更が反映されるタイミング

Global Settings、Global Config、Workspace Configを変更した場合は、Contestを開き直すと反映されます。Settingsの一覧は保存直後に更新されますが、開いているContestのsnapshotは変わりません。`Refresh Contest`だけでは実行設定や認証情報を読み直しません。

テンプレートの内容は、次に新しいソースを作成するときのファイル内容が使われます。既存のソースは変更されません。
