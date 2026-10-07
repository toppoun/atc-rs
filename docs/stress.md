# ストレステスト

Stress Test では、小さいランダムケースを生成し、自分の解答と愚直解の出力を比較して反例を探します。

問題 A なら、基本の流れは次のとおりです。

```bash
atc stress init A
# A_gen.py と A_brute.py を編集
atc stress A
```

Generator と Brute Force は常に Python で実行します。Candidate が C++ の場合でも Python が必要です。

## 準備

問題 A の Helper を作成します。

```bash
atc stress init A
```

コンテストディレクトリに次の2ファイルが作られます。

```text
A_gen.py
A_brute.py
```

既に存在する Helper は上書きしません。

workspace ルートから指定する場合:

```bash
atc stress init A -c abc466
```

## Generator

`A_gen.py` は、コマンドライン第1引数として seed を受け取ります。

例:

```python
import random
import sys

seed = int(sys.argv[1])
rng = random.Random(seed)

n = rng.randint(1, 10)
print(n)
```

Generator の標準出力が、そのテストケースの入力になります。

## Brute Force

`A_brute.py` は生成された入力を標準入力から受け取り、正しい答えを標準出力へ出します。

例:

```python
n = int(input())
print(n)
```

## Candidate

Candidate は検証したい通常の解答ソースです。`A.cpp` または `A.py` のように、通常の `atc test` と同じソースを使います。

各ケースでは次の順に処理します。

1. Generator が seed から入力を生成
2. Candidate と Brute Force の両方へ同じ入力を渡す
3. Brute Force の出力を期待値として Candidate の出力と比較

出力比較の空白・改行の扱いは通常のサンプルテストと同じです。

## 実行

```bash
atc stress A
```

デフォルトでは最大 100 ケース実行します。

workspace ルートから指定する場合:

```bash
atc stress A -c abc466
```

### ケース数を指定

```bash
atc stress A --count 1000
```

`--count` には 1 以上を指定します。

### 見つかるまで続ける

```bash
atc stress A --forever
```

`--count` と `--forever` は同時に指定できません。

### seed を固定

```bash
atc stress A --seed 1
```

同じ Generator と同じプログラムであれば、同じ seed から同じケース列を再現できます。

ケース 1 が `--seed` の値を使い、以降は1ずつ増えます。

```text
case 1 -> seed 1
case 2 -> seed 2
case 3 -> seed 3
...
```

`--seed` を省略した場合は現在時刻をもとに base seed を決めます。

## 言語と Debug

候補プログラムの言語を指定できます。

```bash
atc stress A -l cpp
atc stress A -l python
```

C++ では Debug も使用できます。

```bash
atc stress A --debug
```

`--debug`（`-d`）は C++ でのみ利用できます。`-l` を省略した場合は、設定のデフォルト言語を使います。指定した言語のソースがない場合、別の言語へ自動では切り替えません。

## 反例が見つかったとき

候補プログラムが次のいずれかになった場合、Stress Test を停止して反例を保存します。

- Wrong Answer
- Runtime Error
- Time Limit Exceeded
- 出力上限超過
- 不正な UTF-8 出力

保存先:

```text
.atc/stress/<PROBLEM>/
```

問題 A の場合は `.atc/stress/A/` です。同じ問題で次の反例が見つかると、このフォルダには最新の反例が安全に保存されます。

更新前の反例は `.atc/stress/.stress-recovery-*` に残ります。保存が正常に完了した回復領域があっても、通常のテストは続行できます。初回保存でも回復領域が作られることがあります。古い反例は自動削除されないため、保存を繰り返すとディスク使用量が増えます。不要になった領域を整理する場合は、現在の反例と回復領域の内容を確認し、必要なファイルを別の場所にバックアップしてください。

保存済みの反例が読み込めない場合は、新しい反例で上書きせず、保存を停止します。元のファイルを確認し、必要ならバックアップを取ってから復旧してください。

主なファイル:

```text
failed.in      反例の入力
expected.out   Brute Force の出力
actual.out     候補プログラムの出力
stderr.txt     候補プログラムの stderr（ある場合）
meta.toml      case番号・seed・失敗種別など
```

## 保存した反例を再テストする

保存済みの反例は、次回の通常テストで公式サンプルの後に回帰テストとして実行されます。

```bash
atc test A
```

そのため、バグを修正したあとに同じ反例を別途コピーして管理する必要はありません。

以前の形式で保存した Runtime Error / Time Limit Exceeded の反例には、`expected.out` がない場合があります。ただし、後からファイルが欠けた可能性も区別できません。この場合、公式サンプルはテストしますが、反例は未検証としてAC件数に含めず、`atc test` は失敗の終了コードを返します。WatchとTUIにも未検証であることを表示します。反例のファイルは自動で変更しません。

対処する際は、まず `.atc/stress/<PROBLEM>/` と関連する `.stress-recovery-*` を別の場所へバックアップしてください。信頼できる控えから `expected.out` を復元できるなら、復元後に `atc test <PROBLEM>` を再実行できます。期待出力を確認できない場合は、反例をバックアップしたうえで現在の反例フォルダを `.atc/stress/` の外へ手動で移し、`atc stress <PROBLEM>` で新しい反例を探してください。新しい反例を保存するときも、旧世代は回復領域に残ります。回復領域を確認前に削除しないでください。

それ以外で反例の入力・期待出力・記録が欠けているなど、保存済みの反例を読み込めない場合、通常テストはエラーになります。反例は自動削除されません。`.atc/stress/<PROBLEM>/` のファイルを確認し、必要ならバックアップを取ってから復旧してください。

反例の保存が途中で止まった可能性がある場合も、通常テストは復旧確認が必要なことを表示して停止します。表示された場所に残るファイルを削除せず、内容を確認してから復旧してください。

古い形式の一時保存先に問題を特定できる記録が残っていない場合、他の問題のテストも停止することがあります。表示された一時保存先を確認し、必要なファイルをバックアップしてから手動で復旧してください。

TUIでは、対象問題が分かる反例の読込エラーがあっても、他の問題を開いてテストできます。エラーがある問題には復旧先を表示し、その問題のテストは成功扱いにしません。問題を特定できない一時保存先がある場合は、TUIの起動を停止します。

保存エラーに `installed` や `recovery completion failed` が表示された場合、新しい反例は `.atc/stress/<PROBLEM>/` に設置済みの可能性があります。表示された一時保存先の完了確認が必要です。両方の場所を確認し、必要なファイルを別の場所へバックアップしてから手動で復旧してください。確認前に同じ問題のStress Testを再実行しないでください。

## Generator / Brute Force のエラー

Generator や Brute Force 自体が失敗した場合は、候補プログラムの反例とは扱わず Stress Test 自体のエラーになります。

例:

- Generator / Brute Force の Runtime Error
- Timeout
- 出力上限超過
- 不正な UTF-8

候補プログラムのバグなのか、検証側のバグなのかを分けて扱います。

## TUI から使う

`atc watch` の TUI でも Stress Test を使えます。

- `S`: Stress の準備状態を確認 / 準備済みなら開始
- `i`: Helper が必要なときに初期化

Helper を `i` で作成しただけでは Stress Test は自動で開始しません。`A_gen.py` と `A_brute.py` を編集してから、もう一度 `S` を押してください。

## 実行時間制限

候補プログラム、Generator、Brute Force の実行には runner のタイムアウト設定が使われます。C++ 候補のコンパイルには compile timeout が使われます。

詳しくは [設定](configuration.md) を参照してください。
