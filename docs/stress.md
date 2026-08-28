# ストレステスト

Stress Test では、小さいランダムケースを生成し、自分の解答と愚直解の出力を比較して反例を探します。

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

## 反例が見つかったとき

候補プログラムが次のいずれかになった場合、Stress Test を停止して反例を保存します。

- Wrong Answer
- Runtime Error
- Time Limit Exceeded
- 出力上限超過
- 不正な UTF-8 出力

保存先:

```text
.atc/stress/<INDEX>/
```

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
