# 任意のバイナリ型付きメッセージ

[English](../docs/BINARY_MESSAGES.md) | 日本語

既定はJSONです。ホストとロジック（二層構成ならsupport）の両方で`typed-binary`を有効にし、
共有するハンドラー宣言に`WireFormat::MessagePack`を指定します。通常DLL、静的リンク、JITで
同じ宣言を使えます。

## ハンドラーごとに選ぶ

```rust
use reknot::typed::{Handler, WireFormat};

const SAMPLES: Handler<Vec<f32>, Vec<f32>> = Handler::new("audio.samples", 1)
    .with_format(WireFormat::MessagePack);
```

`HandlerHandle::call`と`HandlerClient::call`は引き続き型付きの結果を返します。
同じモジュール内でJSONとMessagePackのハンドラーを併用できます。登録ワーカーの入出力も
ハンドラーの形式を使い、チェックポイントの直列化はJSONを維持します。
`typed-binary`は任意依存の`rmp-serde`とその推移的依存を追加します。

## 互換性と上限

| 項目 | 動作 |
| --- | --- |
| 形式の識別 | `messagepack-named-v1`を`HandlerSpec`の契約に含める。形式が異なる候補は受付変更前に拒否する |
| 構造体の表現 | フィールド名を持つmapを使い、宣言順の変更に対応する。構造体を位置だけのtupleにしない |
| Serde設定 | エンコーダーとデコーダーの両方でhuman-readable表現を選び、カスタムSerdeの表現選択をJSONと合わせる |
| 契約診断 | JSON Schemaと明示的な版を維持する。生成スキーマだけで独自直列化の動作すべてを証明することはできない |
| エンコード後の上限 | 入力・出力それぞれ1 MiB。エンコード中とデコード前に検査する |
| 所有権 | 所有権のあるバイト列を使い、Rustの割り当て、デストラクター、借用参照をコード境界で共有しない |
| エラー | 変換失敗や末尾の余分なMessagePackデータを拒否する。不正な入力でハンドラー本体を呼ばない |

形式の変更はプロトコル変更です。契約の版を上げ、ホストも再ビルド・再起動してください。
`HandlerSpec`には公開フィールド`format`が加わるため、手書きの構造体リテラルは更新が必要です。
JSONではメタデータにこのフィールドを出力せず、既存の既定メタデータを維持します。
メタデータと診断メッセージは読みやすいJSON・テキストのままです。

[rmp-serdeのエンコーダー](https://docs.rs/rmp-serde/1.3.1/rmp_serde/encode/struct.Serializer.html)では
`with_struct_map`と`with_human_readable`を使い、
[デコーダー](https://docs.rs/rmp-serde/1.3.1/rmp_serde/decode/struct.Deserializer.html)も表現を合わせます。
独自のSerde実装は選択した形式で試験してください。扱える値は形式によって異なり、JSON専用の
カスタム直列化がすべて同じように動くと保証するものではありません。

## 選ぶ前に測定する

[比較ベンチマーク](BENCHMARKS.md)で、型付きの往復全体とエンコード後の大きさを測れます。
小さな文字列では有利にならない場合があり、数値配列では費用が変わります。
バイト数の削減だけではアプリケーションの応答時間が短くなったとはいえません。

これはバイナリ直列化の選択肢であり、ゼロコピーではありません。ツールチェインとlockfileを
合わせても、寿命、参照の重複、アラインメント、アロケーターの条件は残ります。
大きな資源をホストが所有し、明示的なサービスを通してアクセスする方法も使えます。

[型付きハンドラー](TYPED_HANDLERS.md) · [開発契約](DEVELOPMENT_CONTRACTS.md)
