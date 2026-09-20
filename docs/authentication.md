# AtCoder 認証

atc-rs から解答を提出するには、AtCoder にログイン済みのブラウザから `REVEL_SESSION` Cookie を設定します。公開されている問題の取得やローカルでのテストだけなら、通常は認証情報を設定しなくても利用できます。

atc-rs に AtCoder の username / password を入力することはありません。

## `REVEL_SESSION` を取得する

atc-rsから提出するには、AtCoderにログイン済みのブラウザからCookieをコピーします。ここではGoogle Chromeでの取得方法を説明します。

1. Chromeで[AtCoder](https://atcoder.jp/)にログインする
2. AtCoderのページを開いたまま、`F12`（macOSでは`⌥ Option + ⌘ Command + I`）で開発者ツールを開く
3. 上部の`Application`タブを選ぶ。見当たらない場合は`>>`から探す
4. 左側の`Storage` → `Cookies` → `https://atcoder.jp`を選ぶ
5. 一覧から`REVEL_SESSION`を探し、その行の`Value`をコピーする

コピーするのは`REVEL_SESSION`の**Valueだけ**です。`Name`、`Domain`、`Path`などは必要ありません。

`REVEL_SESSION=`が付いた形式でコピーしても、atc-rsでそのまま貼り付けられます。

Cookieはログイン情報として使われるため、他人に送ったり、GitHubやスクリーンショットへ載せたりしないでください。

Chrome以外のブラウザでも、開発者ツールのCookie一覧から同じ値を取得できます。画面名や操作方法はブラウザによって異なります。


## Cookie を設定する

Global Home または Workspace Home で `a` を押すと、`Authentication` 画面が開きます。

初めて設定する場合は `p` を押して `Paste Cookie` を開き、AtCoder の `REVEL_SESSION` の値を貼り付けます。次のどちらの形式でも受け付けます。

```text
<value>
```

```text
REVEL_SESSION=<value>
```

`Cookie:` header 全体、複数の Cookie、`Path` や `Secure` などの属性は貼り付けないでください。

入力欄には本物の値ではなく、次のように表示されます。

```text
REVEL_SESSION=<hidden>
```

値が画面へ再表示されることはありません。`Enter` で保存し、`Esc` で保存せずに戻ります。

## 認証状態を確認する

`Authentication` 画面には次の項目が表示されます。

- `Status` — Cookie の設定・確認状態
- `Account` — AtCoder から安全に確認できた場合のアカウント名
- `Path` — Cookie ファイルの保存先

主な `Status` は次のとおりです。

| 表示 | 意味 | 対処 |
| --- | --- | --- |
| `Authenticated` | AtCoder で認証できた | そのまま提出できます |
| `Not configured` | Cookie が保存されていない | `p` で `Paste Cookie` を開きます |
| `Invalid` | 保存形式やファイルの状態に問題がある | `p` で `Repair Cookie` を開きます |
| `Not authenticated` | AtCoder が Cookie を受け付けなかった | 期限切れや値の誤りを確認し、置き換えます |
| `Verification unavailable` | 通信上の理由などで確認できなかった | 接続を確認し、後で画面を開き直します |

`Authenticated` は認証確認の結果です。`Account` は、アカウント名まで取得できた場合だけ表示する補助情報です。`Authenticated` でも `Account` が `—` になることがありますが、アカウント名を取得できなかっただけで、認証の失敗を意味しません。`Account` が `—` という理由だけで Cookie を置き換えたり Reset したりする必要はありません。

確認中は `Inspecting...` や `Verifying...`、変更中は `Saving...` や `Resetting...` と表示されることがあります。

Cookie ファイルを保存できたことと、AtCoder で認証できたことは別です。たとえばネットワーク障害で `Verification unavailable` になっても、保存自体は成功している場合があります。接続が戻った後に `Authentication` 画面を開き直して確認してください。

コマンドから現在の Cookie を確認することもできます。

```bash
atc login
```

`atc login` は Cookie を保存したり、username / password でログインしたりするコマンドではありません。保存済み Cookie を使って認証状態を確認します。

## Replace / Repair

Cookie が設定済みの場合、`p` は `Replace Cookie` と表示されます。期限切れや別アカウントへの変更時に、新しい値を貼り付けてください。

Cookie ファイルの形式や権限に問題がある場合は、`Repair Cookie` と表示されます。安全でない既存ファイルを無理に上書きすることはないため、画面のエラーと[トラブルシューティング](troubleshooting.md#cookie-を設定できない)を確認してください。

## Reset Authentication

`r` を押すと `Reset Authentication` を実行し、確認後に Cookie ファイル自体を削除します。

この操作は atc-rs に保存した認証情報だけを削除します。ブラウザの AtCoder セッションをログアウトしたり、AtCoder 側で Cookie を無効化したりはしません。

削除後は `Not configured` になり、再び提出するには Cookie の設定が必要です。

## 保存先

Cookie は、他の設定とは別のファイルに次の形式で保存されます。

```text
REVEL_SESSION=<value>
```

保存先は次のとおりです。

- Windows: `%APPDATA%\atc\state\cookie`
- macOS / Linux: `${XDG_STATE_HOME:-~/.local/state}/atc/cookie`

Unix 系環境では、Cookie ファイルを他のユーザーが読める権限にしないでください。手動で権限を直す場合は次を実行します。

```bash
chmod 600 "${XDG_STATE_HOME:-$HOME/.local/state}/atc/cookie"
```

Cookie を共有フォルダへ置いたり、repository に追加したりしないでください。エラー報告、画面写真、ログにも本物の値を含めないでください。

## 変更が反映されるタイミング

Home の `Authentication` で Cookie を変更した場合は、次に Contest を開いたときに反映されます。すでに Contest を開いている場合は、Workspace Home に戻って開き直すか、`Switch Contest` で入り直してください。`Refresh Contest` だけでは、開いている Contest の認証情報を読み直しません。

AtCoder からの応答によって Cookie が更新された場合、atc-rs は現在の操作にも新しい値を使用し、安全に保存できるときは Cookie ファイルも更新します。

Cookie ファイルを別のアプリで変更または削除した場合、開いている Contest がその変更を上書きすることはありません。最新のファイルを使うには Contest を開き直してください。

## 期限切れや認証失敗への対処

1. ブラウザで AtCoder にログインできているか確認する
2. ブラウザから現在の `REVEL_SESSION` の値だけを取得する
3. `Authentication` 画面で `Replace Cookie` を開く
4. 新しい値を保存し、`Authenticated` になることを確認する
5. Contest を開き直す

提出後の結果を確認できなかった場合、atc-rs は安全のため自動再提出しません。同じ起動中は、同じ Contest・問題への再提出も止めます。AtCoder の My Submissions で提出の有無を確認してから、必要なら atc-rs を再起動してやり直してください。
