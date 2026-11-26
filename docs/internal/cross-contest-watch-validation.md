# Cross-Contest Watch の OS 別検証

OS のバージョン、ファイルシステムと大小文字の設定、notify のバージョン、CPU・メモリ、ビルド種別、測定回数と稼働時間を記録する。Windows の測定値を macOS・Linux に適用しない。

## ビルドと合成データ

実ファイルや認証情報を含まない合成 Workspace を使う。ビルドとテストを終えてから計測する。

```bash
cargo test --locked --all-features --no-fail-fast
cargo build --locked --release
```

macOS・Linux では、以下をリポジトリのルートから同じシェルで実行する。`N` を 100・156・500・1000 に変え、毎回新しい一時ディレクトリを作る。Windows のリリース経路でも同じマニフェスト構成を使える。

```bash
ATC_BIN="$PWD/target/release/atc"
export ATC_FIXTURE="$(mktemp -d)"
export XDG_CONFIG_HOME="$ATC_FIXTURE/config"
export XDG_CACHE_HOME="$ATC_FIXTURE/cache"
export XDG_STATE_HOME="$ATC_FIXTURE/state"
export N=100
mkdir -p "$XDG_CONFIG_HOME/atc" "$ATC_FIXTURE/workspace"
printf '[watch]\ncross_contest = true\n\n[runner]\npython = "python3"\n' > "$XDG_CONFIG_HOME/atc/config.toml"
python3 - <<'PY'
import os
from pathlib import Path

root = Path(os.environ["ATC_FIXTURE"]) / "workspace"
(root / ".atc-workspace.toml").write_text("version = 1\n", encoding="utf-8")
for i in range(1000, 1000 + int(os.environ["N"])):
    contest = f"abc{i}"
    directory = root / contest
    (directory / ".atc").mkdir(parents=True)
    manifest = [f'version = 1\ncontest_id = "{contest}"']
    for index in ("A", "B"):
        task = f"{contest}_{index.lower()}"
        manifest.append(
            f'[[problems]]\nindex = "{index}"\ntitle = "Probe {index}"\n'
            f'task_id = "{task}"\nurl = "https://atcoder.jp/contests/{contest}/tasks/{task}"\n'
            'sample_count = 0'
        )
        (directory / f"{index}.py").write_text("print(42)\n", encoding="utf-8")
    (directory / "A.cpp").write_text("int main() {}\n", encoding="utf-8")
    (directory / ".atc/contest.toml").write_text("\n\n".join(manifest) + "\n", encoding="utf-8")
PY
cd "$ATC_FIXTURE/workspace"
"$ATC_BIN" doctor
"$ATC_BIN" watch -c abc1000
```

別の端末で合成 Workspace に移動して次の保存を行う。Problem B と Python が表示されることを確認する。サンプル実行も確認する場合は、起動前に対象の B を `sample_count = 1` とし、`tests/B/sample-1.in` に空の入力、`tests/B/sample-1.out` に `42` と改行を用意する。

```bash
printf 'print(42)\n# saved\n' > abc1001/B.py
printf 'print(42)\n# atomic save\n' > abc1002/replacement.tmp
mv abc1002/replacement.tmp abc1002/B.py
```

## 確認項目

1. 複数 Contest を連続保存し、最後の通常通知が表示を決めること、後続のテスト・再キューで表示が移らないことを確認する。User Input 編集、モーダル、手動切り替えでも操作対象を保持する。提出は確認画面で止める。
2. 新しい正規 Contest を追加し、初回照合後にソースを保存する。削除、配置ルール変更、別経路への移動を行い、変更の前後で保存する。有効な世代でのみ実行・表示が行われ、他 Contest の監視が継続することを確認する。
3. Atomic Save、ソース削除・再作成、マニフェストの置換を試す。大小文字を区別するボリュームと区別しないボリュームで、正規ファイル、別の表記、symlink を別々に検証する。正規ソースの実行と、安全に解決できない経路の拒否を確認する。
4. 通知飽和を試す場合は、1 Contest の問題数または Contest 数を増やし、4,096 を超える正規パスを短時間で変更する。最終内容を回収しても、順序不明の回収では自動表示が起きないことを確認する。OS が Rescan を送った証拠があるときだけ、OS 側の飽和を再現できたと記録する。通知欠落を決定的に検証するには `rescan_recovers_only_changed_sources_when_notifications_were_lost` を使う。
5. Workspace Home への復帰と再入場を繰り返し、監視登録とワーカーが解放されることを確認する。無効な設定で再入場し、他 Contest の保存による実行・表示が起きないことを確認する。Standalone と `watch --plain` を同じ設定で確認する。

## 計測と OS 固有の確認

- 件数・モードごとに別プロセスで実行し、CPU、メモリ、ファイル記述子またはハンドル、スレッド、最初の描画までの時間を記録する。アイドルは最低45秒、可能なら60秒以上とし、15秒ごとの全体再照合を複数回含める。保存受付は実行要求を送った時点、画面遷移は対象のヘッダーを描画した時点を計る。連続保存・登録増減・Home 往復・終了を分けて計測する。
- macOS では FSEvents の登録集合の追加・削除・更新中にも別 Contest を保存する。Atomic Save、親ディレクトリごとの移動、通知欠落後の再照合、パスの正規化と大小文字を個別に記録する。Activity Monitor の CPU・メモリ・スレッド数と `lsof -p PID` を使って長時間の推移を確認する。
- macOS の一括登録は、初回、変更なし、500 ↔ 680 Contest の追加・解除、Home からの再 Open を分けて測る。登録時間は差分適用から開始結果の確認まで、ストリーム開始回数は `FSEventStreamStart` の呼出回数で記録する。更新区間中に完了した保存を、通常通知・再照合・未回収に集計する。比較対象と条件をそろえるため更新を複数回繰り返し、登録時間が短く全保存が区間内に収まらない場合は区間内外の件数も残す。開始失敗を模擬した場合は障害注入として記録する。同じボリューム内の追加・解除で物理ルートが変わらない場合は開始回数が増えないことを確認する。`RLIMIT_NOFILE` が小さい環境では同一ボリューム上での物理ルート集約と、上限超過時に警告を伴う部分登録も確認する。
- Linux では `/proc/sys/fs/inotify/` の `max_user_watches`、`max_user_instances`、`max_queued_events` を記録する。必要なら隔離した環境で上限近くを試す。登録数の不足と通知バッファの飽和を区別し、部分失敗後の監視継続と Rescan からの最終状態回収を確認する。`/proc/PID/status` の `VmRSS`、`VmHWM`、`Threads` と `/proc/PID/fd` で資源推移を確認する。
- Windows の Private Bytes、Working Set、ピーク Working Set は別の値として報告する。OS をまたいで同じ種類のメモリ量として比較しない。
