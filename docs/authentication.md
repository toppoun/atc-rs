# AtCoder 認証

`atc` の認証は任意です。公開されているコンテストの取得など、認証が不要な操作では cookie を設定せずに使用できます。

## `atc login` について

```bash
atc login
```

名前は `login` ですが、ブラウザを開いてログインしたり、ID / パスワードを保存したりするコマンドではありません。

設定済みの AtCoder セッションが現在有効かを確認する**ステータスチェック**です。

cookie ファイルやディレクトリも作成しません。cookie の保存や置き換えは Home の `Authentication` から行えます。

## Home から設定する

Workspace Home と Global Home のどちらでも、`a` を押すと `Authentication` が開きます。

- 未設定では `p` の `Paste Cookie`
- 設定済みでは `p` の `Replace Cookie`
- 内容や権限に問題がある場合は `p` の `Repair Cookie`
- 設定済みまたは修復が必要な場合は `r` の `Reset Authentication`

入力できるのは session value だけ、または `REVEL_SESSION=<value>` の1行です。入力内容は画面に表示されず、空なら `REVEL_SESSION=<empty>`、入力済みなら長さに関係なく `REVEL_SESSION=<hidden>` と表示されます。

保存後は同じ cookie を使ってAtCoderへ1回だけ認証確認を行います。確認中もHomeは描画やresizeに応答します。成功時は `Authenticated` と表示し、AtCoderの応答から安全に取得できた場合だけaccount名も表示します。

network error、timeout、rate limit、AtCoder側の一時的な障害などでは `Verification unavailable` と表示します。この場合でもcookieの保存自体は成功していることがあります。再確認するには`Authentication`を閉じて開き直してください。認証確認結果とaccount名は保存せず、modalを閉じると破棄します。

`Reset Authentication` は空のvalueを書き込む操作ではなく、確認後にcookie fileそのものを削除します。通常fileとして安全に削除できない状態では、targetをたどったり再帰削除したりせずerrorを表示します。

## Cookie の形式

cookie ファイルには次の1行だけを保存します。

```text
REVEL_SESSION=<value>
```

`<value>` には自分の AtCoder セッション値を入れます。

セッション値はパスワードと同様に扱い、Git repository、README、スクリーンショット、ログなどへ載せないでください。

## 保存場所

### Windows

```text
%APPDATA%\atc\state\cookie
```

通常は次のような場所です。

```text
C:\Users\<ユーザー名>\AppData\Roaming\atc\state\cookie
```

### macOS / Linux

```text
${XDG_STATE_HOME:-~/.local/state}/atc/cookie
```

通常は:

```text
~/.local/state/atc/cookie
```

## macOS / Linux の権限

Unix 系では、cookie を他のユーザーから読める権限にしていると拒否されます。

```bash
chmod 600 ~/.local/state/atc/cookie
```

## 認証確認

cookie を配置したあと:

```bash
atc login
```

`atc` は HTTPS で AtCoder の設定ページを確認し、認証済みかどうかを報告します。

セッション値そのものを通常の出力へ表示することはありません。

## Contest と AtCoder による更新

新しい Contest を開くと、開始時にcookie fileを1回だけ読み込みます。Contest中にHomeや別processがcookie fileを変更しても、開いているContestはその変更を再読込しません。

AtCoder 自身が trusted HTTPS response の `Set-Cookie` で `REVEL_SESSION` を更新した場合は例外です。active session はその successor を次の request から使用し、cookie file が読み込んだ時点の値のままである場合に限って安全に永続化します。外部の値が既に変更・削除されている場合は上書きや再作成をせず、active session だけが AtCoder の successor で継続します。

Refresh は同じ認証状態を使用します。Switch、新しいContest、Homeからの入り直し、contestへの直接entryでは、その時点のcookie fileを読み直します。HomeでPaste / Replace / Repair / Resetした内容は、次に開くContestから反映されます。

## Cookie がない場合

cookie が未設定の場合は、認証なしの状態として扱われます。

未設定または invalid な状態で AtCoder response が `REVEL_SESSION` を返しても、自動的に認証済みへ昇格したり cookie file へ保存したりはしません。AtCoder が active credential を明示的に削除した場合も同様で、以後の submit は network request 前に拒否されます。

認証が必要な操作で問題が起きた場合は、`atc login` で状態を確認してください。
