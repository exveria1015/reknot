# 低レイヤーから組み込む

[English](../docs/GETTING_STARTED.md) | 日本語

ホスト資源の登録や、ロード・切り替えのタイミングを自分で制御する場合の導入手順です。
通常の関数で処理を書き、入出力の変換と保存後の更新を SDK に任せる場合は、[型付きハンドラー入門](TYPED_HANDLERS.md) を使ってください。

## 接続を維持するサンプル

Rust と Cargo を用意し、リポジトリのルートで実行します。
必要なバージョンや環境は [互換性と対象範囲](COMPATIBILITY.md) を参照してください。

```sh
cargo run -p service_host --locked
```

TCP クライアントで `127.0.0.1:4000` に接続し、改行付きのテキストを送ります。
次に [ロジックのソース](../examples/service_logic/src/lib.rs) の応答ラベルを変えて保存し、同じ接続からもう一度送ってください。
新しいコードの応答を受け取れます。

`slow` と改行を送ると約5秒の処理になります。その間に更新すると、旧コードの処理が続く様子を確認できます。

このサンプルは単一接続を扱い、ビルドは同期処理です。
変更はメッセージの境界や要求を poll するときに処理するため、入力待ち中には切り替わりません。
画面で編集と更新を試す場合は [Tauri GUI Lab](TAURI_GUI_LAB.md)、
ワーカーの停止・再生成を試す場合は [Restart のガイド](RESTART_AND_MIGRATION.md) を参照してください。

## ホストとロジックを分ける

ホストを長寿命の実行ファイル、ロジックを更新可能なライブラリにします。
どちらも同じ `reknot` SDK に依存します。

| 担当 | 持つもの |
| --- | --- |
| ホスト | ソケットや DB、継続する状態、要求の受付、タスクを進める処理 |
| ロジック | 入力に応じた処理、ホストが公開する操作の呼び出し |

ホストが持つ Rust オブジェクト自体は DLL へ渡しません。
サービス ID・版・操作番号と、入出力のバイト列の形式を両側で共有します。
ID が一致しても、データの意味まで自動で検証されるわけではありません。

### ロジックの crate を作る

たとえば `examples/my_logic` に作る場合、Cargo.toml は次の形です。
作成した crate は workspace の `members` に追加してください。

```toml
[package]
name = "my_logic"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
reknot = { path = "../../reknot", default-features = false }
```

ネイティブ構成では、ホストからロジックへの静的リンク依存は不要です。
ビルドしたライブラリをホストがロードします。

## ホストの操作を呼ぶ

次のハンドラーは、受け取った入力をホストの操作0へ渡します。

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};

const SESSION: ServiceKey = ServiceKey::new(0x4c49_4e45, 1);

#[reknot::reload_handler(SESSION)]
async fn handle(ctx: RequestContext) -> Result<(), ServiceError> {
    ctx.call(0, ctx.input(), &mut [])?;
    Ok(())
}
```

操作0の実装はホスト側で定義します。たとえば、上限のある送信キューへ入力をコピーする処理です。
`HostResource::call` は短い、ブロックしない処理にします。
ソケットの受信・送信や、メッセージの区切りを判定する処理はホストに置きます。

ホスト全体の実装は [service_host](../examples/service_host/src/main.rs)、
サービスの登録方法は [ホスト資源と受付切り替え](HOST_RESOURCES_AND_ROUTING.md) を参照してください。

## ロードして呼び出す

ホストでは、次の順に組み立てます。

1. `HostResources` に資源を登録する。
2. `unsafe RequestRouter::load(path, resources)` でロジックをロードする。
3. `dispatch(resource.id(), input)` で要求を渡し、返された Future を poll する。

`dispatch` は入力をコピーし、実行するコード世代と資源へのアクセスを保持します。
Future を進めるのはホストの役割です。

ネイティブロードの `unsafe` は、初期化・終了処理やコールバックを含め、ロードするコードが SDK の契約を守ることを要求します。
将来の置換候補も同じ条件で扱います。条件は [互換性と対象範囲](COMPATIBILITY.md) で確認してください。

## 切り替え方式を選ぶ

### 要求ごとに切り替える：Drain

`unsafe prepare(path)` で候補を準備し、`activate(candidate)` で新しい受付先にします。
既存の要求は旧世代で続き、新しく受け付ける要求は新世代を使います。
サービス契約と状態スキーマは一致が必要です。

詳しい順序は [ホスト資源と Drain](HOST_RESOURCES_AND_ROUTING.md) を参照してください。

### 常駐ワーカーを作り直す：Restart

`start_worker` で対象を登録し、必要な状態をチェックポイントへ明示的に保存します。
`restart(candidate, migrate)` は、協調停止と終了確認の後、状態を変換してワーカーを再生成します。

停止待ち中も、ホストが所有する通常タスクの poll を進めてください。
再生成するのは登録ワーカーだけです。通常のリクエストを自動で再実行することはありません。
[Restart と状態移行](RESTART_AND_MIGRATION.md) に、失敗時の復旧も含めて説明しています。

静的リンクと feature の指定は [ビルド構成](API_REFERENCE.md#ビルド構成) を参照してください。

[日本語 README](README.md) · [ドキュメント一覧](INDEX.md)
