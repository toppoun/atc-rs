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

## Cookie がない場合

cookie が未設定の場合は、認証なしの状態として扱われます。

認証が必要な操作で問題が起きた場合は、`atc login` で状態を確認してください。
