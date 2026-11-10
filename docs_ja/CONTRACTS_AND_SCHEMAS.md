# 契約エラーとスキーマ

[English](../docs/CONTRACTS_AND_SCHEMAS.md) | 日本語

型付きモジュールは、適用前に生成した JSON スキーマを照合します。Rust の型名と契約の版が
同じでも、構造の変更を検出できます。これは宣言された構造の検査であり、スキーマが一致しても
動作や意味の一致を証明するものではありません。

## プロトコルの型を記述する

共有する protocol crate で、Serde の derive と一緒にスキーマ属性を付けます。

```rust
use serde::{Deserialize, Serialize};

#[reknot::schema]
#[derive(Serialize, Deserialize)]
pub struct Message {
    pub author: String,
    #[serde(default)]
    pub attachments: Vec<String>,
}
```

`#[reknot::schema]` は SDK が再公開する `JsonSchema` を derive します。Schemars を直接依存に
追加する必要はありません。構造体と列挙型に対応します。ハンドラーの入出力とワーカーの状態には
`Serialize + DeserializeOwned + JsonSchema + 'static` が必要です。
プリミティブ型、タプル、一般的なコンテナにはスキーマの実装があります。

SDK はシリアライズ・デシリアライズの両方向について JSON Schema 2020-12 を記録します。
フィールド名の変更、既定値、片方向だけの省略など、対応する Serde 属性も反映します。
独自 codec や未対応の属性には、正しい `JsonSchema` 実装、または適切な
`#[schemars(with = "...")]` の型を指定してください。実態と異なるスキーマでは独自 codec の
動作を検証できません。ホストとロジックの SDK・生成器の版をそろえます。rc.1 からの更新では
両方の再ビルドが必要です。

## 拒否の理由を読む

`Error::Contract { report, source }` は、元のルーティングエラーと、所有する
`ContractReport` を保持します。表示には照合段階、ハンドラー、JSON ポインター、期待値・実値、
取得できた export 元のファイル・行・列・関数名が含まれます。例えば、フィールドの変更は
次のように表示されます。

```text
host-to-candidate bot.reply/input_schema/deserialize/properties/value/type:
expected "integer", actual "string"
```

| 段階 | 照合する値 | 対処 |
| --- | --- | --- |
| `host-to-candidate` | ホストの宣言と候補の宣言 | 共有プロトコル・版・フィールドのスキーマを確認する。ハンドラー契約を変える場合はホストも再ビルド・再起動する |
| `declaration-to-export` | 候補の宣言と実際に export されたハンドラー | export の不足・余分な export・宣言の不一致を直す |

ソース位置は診断情報であり、互換性のキーにはしません。ファイルを移動しただけで、同じ契約が
拒否されることはありません。示す位置はハンドラーであり、フィールド宣言そのものではありません。
JSON ポインターを使ってプロトコルのスキーマを確認してください。候補の副作用を持たない
メタデータ callback が、サイズを制限したホストのバッファーへ JSON をコピーします。
古いモジュールや callback の失敗・サイズ超過では詳細を得られない場合がありますが、
元の拒否理由と期待した契約は残ります。

`report.expected` と `report.candidate` は、デコードした宣言全体を保持します。
`report.differences` の上限は32件で、それ以上の差は `truncated` で示します。
表示する値は240文字で省略しますが、所有する値は省略しません。
初期化データと候補のメタデータはそれぞれ最大1 MiBです。レポートには DLL 所有のポインターを残しません。

## スキーマを出力する

`module().schema_json()?` は、形式マーカー `reknot-typed-contract-v2`、モジュールとハンドラーの版、
入出力のスキーマ、宣言されていればワーカー状態の契約を含む、整形した宣言を返します。
内側の型スキーマは JSON Schema 文書です。外側は Reknot のメタデータであり、
全体をそのまま JSON Schema の検証文書として扱うものではありません。

生成したプロジェクトには、実行できる出力用サンプルが含まれます。

```sh
cargo run -p my-bot-protocol --example schema > schema.json
```

パッケージ名は生成した protocol の名前に置き換えてください。出力を保存してレビューしたり、
版ごとに比較したりできます。ネイティブ候補のエラーレポートには、実際の export とソース位置も含まれます。

## 契約を意図的に変更する

照合は生成スキーマの完全一致で行います。互換と推測して差を許容する方式ではありません。
後方互換である可能性のある任意フィールドの追加でも拒否する場合があります。
構造・フィールド名・列挙型の variant・スキーマ属性が結果に影響します。
意味の等しいスキーマ配列でも、並び順の正規化は行いません。

入出力の形式や意味を変えるときはハンドラーの版を上げ、ホストとクライアントを再ビルド・再起動します。
単位・解釈・業務ルールの変更は、スキーマの一致だけでは検出できません。
ワーカー状態は独立した版付きの `WorkerState<S>` で宣言し、[型付き Restart](TYPED_RESTART.md)
で明示的に変換できます。スキーマによって任意の Future の実行位置が移行されたり、外部副作用が
巻き戻されたりすることはありません。

[型付きハンドラー入門](TYPED_HANDLERS.md) · [ドキュメント一覧](INDEX.md)
