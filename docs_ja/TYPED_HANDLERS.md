# 型付きハンドラー入門

[English](../docs/TYPED_HANDLERS.md) | 日本語

このガイドでは、プロジェクトを生成し、返信処理を編集して、ホストから呼び出すまでを説明します。
ハンドラーは同期・非同期どちらの関数でも書けます。入力と戻り値の変換は SDK が担当します。

実行だけ試したい場合は、[README のコンソール Bot](README.md#まず動かす) から始めてください。

## 自分のプロジェクトを作る

Rust 1.95 以上と Cargo を用意し、SDK リポジトリのルートで実行します。
ここでは、まだ存在しない `../my-bot` にプロジェクトを作ります。

```sh
cargo run -p reknot --bin reknot-new --locked -- ../my-bot
cd ../my-bot
cargo run
```

初回ビルドが終わって `Ready.` と表示されたら、文字を入力してください。
`logic/src/lib.rs` の `logic-v1` を変更して保存し、`New events use generation ...` の表示を待つと、
次の入力から返信が変わります。`/count` はホストに残る入力件数、`/quit` は終了です。

初回実行で作られた `Cargo.lock` もバージョン管理します。以降の起動は `cargo run --locked` で行えます。
静的リンクを含む実行手順は、生成先の README にもあります。
名前や参照先を変える場合は、後半の [生成オプション](#生成先と-sdk-の場所) を参照してください。

## 編集する場所を知る

生成したプロジェクトは3つの crate で構成されます。
通常の返信や計算の変更は、`logic` から始めます。

| ファイル | 役割 |
| --- | --- |
| `logic/src/lib.rs` | 属性付きのハンドラーと、その処理に使う補助関数 |
| `protocol/src/lib.rs` | 入力・戻り値の型、ハンドラーの名前と契約の版、モジュール定義 |
| `host/src/main.rs` | 入力の受付、継続する状態、ロジックの呼び出し、更新処理の実行 |

接続・DB・履歴など、更新をまたいで残したいものはホストが持ちます。
ロジックには必要なデータを渡し、返された結果をホストで使います。

## ハンドラーを書く

`my-bot` として生成したプロジェクトの返信処理は、次の形です。
`REPLY` は、共有プロトコルで宣言したハンドラーの契約です。

```rust
use my_bot_protocol::{module, Message, REPLY};

#[reknot::handler(REPLY)]
async fn reply(message: Message) -> Result<String, String> {
    let text = message.text.trim();
    if text.is_empty() {
        return Err("Please enter a message.".into());
    }
    Ok(format!(
        "logic-v1 / message {}: {}",
        message.received,
        decorate(text)
    ))
}

fn decorate(text: &str) -> String {
    text.to_uppercase()
}

reknot::export_handlers!(module(), [reply]);
```

`handler` 属性が呼び出し口を作り、`export_handlers!` がその crate のハンドラーをまとめて公開します。
更新時は、このハンドラーと `decorate` のような補助関数を同じ DLL として切り替えます。

引数は所有する入力1個、戻り値は所有する出力です。入力が不要なら `()`、複数の値を渡すなら構造体やタプルを使います。
ユーザー定義型には Serde の `Serialize` と `Deserialize` を実装します。
借用した SDK オブジェクト、`&self` を持つメソッド、ジェネリック関数は対象外です。

`Result<String, String>` は、この例では返信または入力エラーを表します。
ホストから呼んだ結果には、これを包む SDK 側の `Result` が加わります。
外側が変換・実行などの失敗、内側がアプリケーションの処理結果です。

## 入出力の約束を定義する

`protocol/src/lib.rs` は、ホストとロジックが共有する約束を置く場所です。
上の例では、次の内容を定義しています。

```rust
use reknot::typed::{Handler, Module};
use serde::{Deserialize, Serialize};

pub const REPLY: Handler<Message, Result<String, String>> =
    Handler::new("my-bot.reply", 1);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub text: String,
    pub received: u64,
}

pub fn module() -> Module {
    Module::new("my-bot", 1).with(REPLY)
}
```

`"my-bot.reply"` は安定した識別名、`1` は入力・戻り値の契約の版です。
Rust の関数名を変えても、この識別名は維持できます。
読み込んだコードの「世代」と、入出力の約束を管理する「契約の版」は別の値です。

**型名が同じでも、入出力の互換性が保たれるとは限りません。**
フィールド、Serde 属性、データの形式や意味を変えたら、対応する契約の版を上げ、ホストも再ビルド・再起動してください。
SDK は同名型の構造や意味の変化を自動判定しません。

## 変更ごとの進め方

| 変更するもの | 反映する方法 |
| --- | --- |
| 既存ハンドラーの処理、返信文、補助関数 | 保存し、ビルド・検査の成功を待つ |
| 入力・戻り値の形式や意味 | 対応する契約の版を上げ、ホストも再ビルド・再起動する |
| ハンドラーの追加・削除 | `protocol` の定義、`export_handlers!`、ホストの登録を更新し、ホストを再ビルド・再起動する |
| ホストの処理、SDK・ABI | ホストとロジックを再ビルドし、ホストを再起動する |

生成したホストは、ビルド失敗や契約不一致では動作中の世代を維持します。
失敗した候補の処理や、完了済みのイベントを自動で再実行することはありません。

ここまでが日常のハンドラー編集です。
以下は、独自のホストや既存のフレームワークへ組み込むときに参照してください。

## ホストから呼ぶ

生成したホストには、ロジックへ要求を渡す `client` と、更新を進めるループが用意されています。
`client.handler(REPLY)` で呼び出し口を一度取得すれば、更新のたびに取り直す必要はありません。

次は、呼び出し側の処理を抜き出した例です。**結果を待つ間も、所有スレッドでは更新ループを動かします。**

```rust,ignore
let reply = client.handler(REPLY)?;
let result = reply.call(Message {
    text: "hello".to_owned(),
    received: 1,
}).await?;
```

生成したホストでは、`DevModule::update()` を約25msごとに呼びます。
この処理が変更の検出、ビルド結果の適用、キューからの実行、キャンセル後の終了待ちを進めます。
入力待ちや要求の実行中も呼び続ける構成です。

### 独自のホストへ組み込む場合

`DevModule::start(module().builder(), DevOptions::new(manifest, package))` で開発用の更新制御を開始し、
`development.bridge(BridgeOptions::default())` でクライアントを取得します。
初回のビルドは同期処理です。起動後のビルドはバックグラウンドで行います。

`start` は `unsafe` です。ロードするプロジェクト、依存先、将来の編集が SDK のネイティブ実行契約を守ることを、
呼び出し側が確認します。[互換性と対象範囲](COMPATIBILITY.md) も参照してください。

同じスレッド内で直接呼ぶ場合は、`development.host().handler(REPLY)` も使えます。
[コンソール Bot のホスト](../examples/typed_bot_host/src/main.rs) は、このローカル呼び出しの例です。

| 呼び出し口 | 世代を選ぶ時点 |
| --- | --- |
| ローカルの `HandlerHandle::call` | `call` した時点。最初の poll より前に確定する |
| キューを使う `HandlerClient::call` | 所有スレッドが取り出し、実行を受け付けた時点 |

どちらも、選んだ世代で要求を完了まで実行します。キューで待機している間は、まだ世代は確定していません。

## 別スレッドのフレームワークから呼ぶ

`ModuleClient` と `HandlerClient` は `Send + Sync`、返される `RemoteCall` は `Send` です。
`reply` を clone して、フレームワークのイベント処理へ渡せます。

DLL を動かす `ModuleHost` や `BridgeDriver` は、作成した所有スレッドに置きます。
Tokio なら `current_thread` または `LocalSet` を使い、そこで更新処理を続けます。

別スレッドの Tokio ランタイムから呼ぶ [実行例](../examples/typed_bot_host/examples/framework_bridge.rs) もあります。
SDK リポジトリのルートで実行してください。

```sh
cargo run -p typed_bot_host --example framework_bridge --no-default-features --features static-logic --locked
```

フレームワーク固有のイベントは、ホストで所有する入力データへ変換します。
返された返信案も、ホストの SDK で送信します。Discord・Serenity・Twilight 専用のアダプターは含まれていません。

## キャンセルと終了待ち

`call.cancel()` はキャンセルを要求します。その後も要求を await すると、登録した子処理の終了を確認できます。
結果が先に完了していれば、その結果が返る場合もあります。要求の Future を Drop した場合もキャンセルを要求します。

ブリッジ全体を閉じるときは、次のどちらかを使います。

- `client.close().await` — 新規受付を閉じ、受け付けた処理の完了を待つ。
- `client.shutdown().await` — 新規受付を閉じ、受け付けた処理をキャンセルして、登録した子処理の終了を待つ。

待っている間も、所有側の driver を動かす必要があります。
生成したホストのように、所有スレッドでは `select!` で更新処理と終了待ちを並行させてください。
この終了待ちが対象とするのは、そのブリッジの要求です。

driver の Drop は正常な終了待ちの代わりにはなりません。
ローカル要求を Drop した場合や driver を破棄した場合の後処理は、[API の終了契約](API_REFERENCE.md#キャンセルと終了) を参照してください。
停止要求に応じない処理の強制終了や、送信済みの返信の巻き戻しは行いません。

## 処理中にホストの資源を使う

単純な処理は、入力を受け取り結果を返す形で書けます。
途中で DB や I/O が必要なら、先頭引数に `RequestContext` を追加し、ホストサービスを呼び出します。

サービスの割り当て、使用できる API、子処理の登録は [型付き API のホストサービス](API_REFERENCE.md#型付きハンドラーからのホストサービス) を参照してください。
継続する状態はホストに持たせ、未登録の独自 spawn を SDK が自動管理する前提にはしないでください。

## 監視せずに実行する

生成したプロジェクトのルートで、静的リンク構成を選べます。
ハンドラーの書き方と呼び出し方は同じです。編集を反映するにはアプリを再ビルド・再起動します。

```sh
cargo run --no-default-features --features static-logic --locked
```

## 生成先と SDK の場所

`reknot-new` は新しいディレクトリに生成します。既存のディレクトリは、空でも指定できません。
生成時にはファイルだけを作り、ビルドや Git の初期化は行いません。

| 指定 | 使う場面 |
| --- | --- |
| `--name my-bot` | フォルダー名に日本語や空白を使うなど、Cargo のパッケージ名を別に決めたいとき |
| `--sdk-path PATH` | `reknot-new` のビルド時とは別の SDK ソースを参照するとき。`reknot` crate のディレクトリを指定する |

生成した `Cargo.toml` は、SDK の**ローカルソース**を参照します。
SDK を移動した場合は、`workspace.dependencies.reknot.path` も変更してください。

## 詳しい仕様を確認する

- [型付き API の契約と上限](API_REFERENCE.md#型付きアプリケーションapi) — 契約の照合、JSON 変換、容量、スレッド、終了待ち。
- [開発時の運用](DEVELOPMENT.md) — 監視するファイル、依存関係の変更、ビルド失敗からの復旧。
- [DLL の寿命と回収](UNLOAD_SAFETY.md) — 既定の保持と明示回収の条件。ロード試行の既定上限は32回で、回収しても枠は戻りません。

現在の型付き API は、単一モジュールの Drain を対象とします。
複数 DLL の一括更新・依存世代の対応表、Restart の高レイヤー化、型の構造からの自動互換性判定は未提供です。

[日本語 README](README.md) · [ドキュメント一覧](INDEX.md)
