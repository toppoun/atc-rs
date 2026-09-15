# AtCoder 認証

`atc` の認証は任意です。公開されているコンテストの取得など、認証が不要な操作では cookie を設定せずに使用できます。

## `atc login` について

```bash
atc login
```

名前は `login` ですが、ブラウザを開いてログインしたり、ID / パスワードを保存したりするコマンドではありません。

設定済みの AtCoder セッションが現在有効かを確認する**ステータスチェック**です。

cookie ファイルやディレクトリも作成しません。必要な場合は、表示された保存場所へ自分で用意します。

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

## ContestSession と AtCoder による更新

新しい ContestSession は、開始時に外部の cookie file を1回だけ読み込みます。session 中に browser や別 process が cookie file を変更しても、active session はその変更を再読込しません。

AtCoder 自身が trusted HTTPS response の `Set-Cookie` で `REVEL_SESSION` を更新した場合は例外です。active session はその successor を次の request から使用し、cookie file が読み込んだ時点の値のままである場合に限って安全に永続化します。外部の値が既に変更・削除されている場合は上書きや再作成をせず、active session だけが AtCoder の successor で継続します。

Refresh は同じ evolving session auth を使用します。Switch、新しい ContestSession、Home からの再 entry、direct contest entry は、その時点の外部 cookie file から新しく開始します。

## Cookie がない場合

cookie が未設定の場合は、認証なしの状態として扱われます。

未設定または invalid な状態で AtCoder response が `REVEL_SESSION` を返しても、自動的に認証済みへ昇格したり cookie file へ保存したりはしません。AtCoder が active credential を明示的に削除した場合も同様で、以後の submit は network request 前に拒否されます。

認証が必要な操作で問題が起きた場合は、`atc login` で状態を確認してください。
