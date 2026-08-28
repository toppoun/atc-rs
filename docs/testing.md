# テストと Watch

`atc` は、公式サンプルの単発テストと、ソース保存時の自動テストに対応しています。

## サンプルテスト

コンテストディレクトリで:

```bash
atc test A
```

ワークスペースのルートからコンテストを指定する場合:

```bash
atc test A -c abc466
```

言語を明示する場合:

```bash
atc test A -l cpp
atc test A -l python
```

`-l` を省略すると、設定の `[defaults].language`、その次に組み込みの `cpp` が使われます。

指定された言語のソースが存在しない場合、別の言語のソースへ自動で切り替えることはありません。

問題番号は contest metadata と大文字・小文字を区別せずに照合します。C++ は最初に1回コンパイルしてから各ケースを実行し、Python はコンパイルせずに各ケースを実行します。どちらも contest ディレクトリを作業ディレクトリとして動きます。

公式サンプルも保存済み Stress ケースもない場合は `No Samples` と表示し、コンパイルや候補プログラムの実行は行いません。

## C++ Debug

C++ では `--debug` を利用できます。

```bash
atc test A --debug
```

Debug ビルドでは、通常の `cpp_flags` に `-DLOCAL` と組み込み `debug.hpp` の include path を追加します。ヘッダーはユーザーの cache ディレクトリへ展開されます。

Python では `--debug` は使用できません。

## 判定方法

出力比較では次を許容します。

- CRLF / LF の違い
- 各行末の空白の違い
- 最終改行の有無

一方、次は区別されます。

- 行頭の空白
- 行の途中の空白
- 空行の構造

候補プログラムが `stderr` を出していても、`stdout` が正しければそれだけで不正解にはなりません。必要に応じて `stderr` は結果に表示されます。

主な結果:

- Accepted
- Wrong Answer
- Runtime Error
- Time Limit Exceeded
- Compile Error
- Compile Timeout

## 保存済み Stress ケース

Stress Test で反例が保存されている場合、公式サンプルの後に回帰テストとして実行されます。

つまり、一度見つけた反例は次回以降の:

```bash
atc test A
```

でも確認できます。

詳しくは [ストレステスト](stress.md) を参照してください。

## Watch

TUI で監視する場合:

```bash
atc watch
```

テキスト出力だけで監視する場合:

```bash
atc watch --plain
```

ワークスペースのルートからコンテストを指定する場合:

```bash
atc watch -c abc466
atc watch -c abc466 --plain
```

Watch は `.cpp` / `.py` の対象ソースが変更されたときにテストします。Stress Helper など無関係なファイルの変更は通常のソース変更として扱いません。

### 起動直後の挙動

Watch は**起動時に全問題を一度テストする方式ではありません**。

起動後にソースファイルが変更・保存されたタイミングでテストが走ります。

今すぐ実行したい場合は、TUI で `r` を押すか、別途 `atc test` を実行してください。

## タイムアウト

デフォルト:

```text
実行:       2.0 秒
コンパイル: 10.0 秒
```

変更方法:

```toml
[runner]
timeout_seconds = 3.0
compile_timeout_seconds = 15.0
```

詳しくは [設定](configuration.md) を参照してください。

## 出力サイズ

各プロセスの `stdout` / `stderr` はそれぞれ最大 8 MiB まで取得します。

候補プログラムの出力が上限を超えて切り詰められた場合、その出力を Accepted として扱うことはありません。
