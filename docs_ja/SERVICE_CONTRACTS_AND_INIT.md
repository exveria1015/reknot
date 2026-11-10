# 複数サービス要求と初期化データの型付け

[English](../docs/SERVICE_CONTRACTS_AND_INIT.md) | 日本語

**Handler ABI 5** のハンドラーは、主対象サービスに加えて必要なサービスと、
世代ごとの初期化データのスキーマを宣言できます。
Services ABI 2、タスク制御 ABI 2、Activity ABI 1 の契約を使います。
ホストとロジック DLL を一緒に再ビルドしてください。旧 Handler ABI 2～4 は拒否します。

## ロジックの宣言

```rust
const REQUIRED: &[ServiceRequirement] = &[
    ServiceRequirement::new(1, COUNTER_V1),
    ServiceRequirement::new(2, FORMAT_V1),
];

#[reknot::reload_handler(PRIMARY, requires = REQUIRED, init = Config)]
async fn handle(ctx: RequestContext) -> Result<(), ServiceError> {
    let config = ctx.initial::<Config>()?;
    let mut count = [0; 4];
    ctx.call_service(1, 0, &config.step.to_le_bytes(), &mut count)?;
    let reply = ctx.call_service_async(2, 0, &count, 64)?.await?;
    ctx.call(0, &reply, &mut [])?;
    Ok(())
}
```

`requires`、`init`、既存の `state` は任意の順で併用できます。同じオプションの重複は拒否します。
省略時の宣言は従来と同じ単一主対象・初期化データなしです。
`call` / `call_async` は要求ごとの主対象に、`call_service` / `call_service_async` は
指定番号の追加資源へ送ります。追加の非同期呼び出しも既存のキャンセル・join の管理に含めます。

binding 番号はロジックが用途ごとに決める非ゼロの u64 です。サービスの ID／版とは別です。
同じサービス種別の2つの資源にも、別番号を割り当てられます。
追加要求は最大32件で、同じ binding 番号の重複・無効なサービスID／版を拒否します。
宣言は上限付きのインライン配列にコピーするため、外国側の配列ポインタを保持しません。

## ホストの割り当て

```rust,ignore
let environment = HandlerEnvironment::default()
    .bind(1, counter.id())
    .bind(2, formatter.id())
    .with_initial(InitData::from_value(&Config { step: 2 })?);
let mut router = unsafe {
    RequestRouter::load_with_environment(
        library, resources, environment, RouterOptions::default(),
    )
}?;
```

各 binding へ **資源IDを明示的に**割り当てます。同じサービスID／版の資源が複数ある場合にも
ホストが選びます。レジストリを走査して最初の一致を選ぶ処理や、失効した資源から置換資源への
自動的な結び直しは行いません。初回ロードで必要な追加サービスが揃わなければ拒否します。

宣言と割り当てた資源のサービスID・版を完全一致で照合します。版の範囲指定、優先順位による
フォールバック、オプショナル要求、サービス内の各操作のシグネチャ検査は提供しません。
サービスの操作番号・入出力形式・意味は、そのサービス版の契約で定義してください。
未要求の割り当てを環境に含めることはできますが、RequestContext に渡すのは宣言されたものだけです。
環境の割り当ては最大32件で、番号の重複は要求の有無にかかわらず拒否します。

`RequestContext::binding(id)` で解決済みの `ServiceBinding` を取得できます。
ID・版の照合は信頼済みコード間の契約確認であり、悪意のあるネイティブコードに対する権限隔離ではありません。

## 所有する型付き初期化データ

`Config` は `InitPayload` を実装します。

- `SCHEMA: ServiceKey`：データのスキーマID／版。
- `encode(&self)`：所有するバイト列を生成。
- `decode(&[u8])`：形式と値を検査し、所有する Config を復元。

完全な実装は [共有プロトコル](../examples/contracts_protocol/src/lib.rs) にあります。
この例は `CFG1` のマジック、固定長、little-endian の数値、値域1～100を検査します。
SDK に新しい必須シリアライズ依存は追加していません。

ホストは `InitData::from_value(&config)` で所有するコピーを作ります。
通常はスキーマIDと形式を共有コードで定義し、形式・意味の互換性が変わる場合は版を変えます。
`InitData::new(schema, bytes)` で外部から取得したデータを渡すこともできますが、同じ検査を受けます。

マクロが純粋な検証コールバックを生成し、ロード／prepare で候補 DLL の `decode` を実行します。
スキーマ不一致はデコーダーを呼ぶ前に拒否し、一致しても不正長・不正形式・値域外なら拒否します。
SDK の検証コールバック内の unwind は `PANICKED` へ変換します。
初期化データは最大1 MiBです。スキーマIDだけで Rust のレイアウトや型の同一性を証明しているわけではありません。
異なる意味に解釈できるバイト列について、作者の意図を自動判定することもできません。

デコード・検証中のデストラクタも含め、バリデーターは純粋・有界とし、I/O・子タスク開始・
アプリケーション状態の変更を行わない契約です。ロードに伴う DLL の OS 初期化も従来の unsafe 契約に従います。
データ不正を拒否しても、任意の DLL 初期化コードの副作用を隔離・巻き戻すものではありません。
SDK のバイト数上限は、デコーダー内部の任意の追加メモリ確保を制限するものではありません。

この初期化データは **世代ごとの設定** です。要求ごとの `ctx.input()`、ワーカーの Checkpoint、
旧 `hot_init` / `with_context` とは別です。通常要求全体の型付け、チェックポイント形式の自動検証、
旧コンテキストAPIの安全化を実装したものではありません。

## prepare、切り替え、失効

`prepare(path)` / `prepare_api(api)` は現在の環境のコピーで候補を検査します。
設定・割り当て・スキーマを変える場合は `prepare_with_environment` /
`prepare_api_with_environment` に新しい環境を渡してください。
静的 API では `from_api_with_environment(api, resources, environment, options)` も使えます。

| 検査時点 | 動作 |
| --- | --- |
| load / prepare | マニフェスト、全追加サービス、初期化データを検査。本体・要求 factory は実行しない |
| Drain の activate | 候補の基準世代等と、解決済み追加資源が失効していないことを再確認 |
| Restart の開始 | 追加資源を再確認してから受付停止・停止通知を行う |
| Restart の切り替え | 停止中に失効していないことを確認し、新ワーカーを段階的に準備して最後に再検査 |
| 新規 dispatch / worker 生成 | 必要な追加資源を再確認し、不正なら factory を呼ばない |
| 既存実行のサービス呼び出し | 資源ID・版を照合。失効は REVOKED。別の資源へ置換しない |

prepare の不成功や、停止前の契約拒否は既存の受付と実行を維持します。
Restart の停止開始後に失効した場合は旧世代を保持し、受付を停止したまま失敗します。
既に行った停止や I/O の巻き戻しは行いません。

候補と世代は環境を所有し、要求 factory は解決済みサービス表と初期化バイト列を呼び出し中にコピーします。
旧要求は旧 DLL・旧設定・旧割り当て、新要求は新しい組み合わせで実行します。
既存実行より長く資源を有効にするか、失効を許可するかはホストが決めます。
初期化設定の変更は Checkpoint の移行やホスト資源・DB の移行を代替しません。

代表的なエラーは `MissingService`、`ContractMismatch`、`InitSchemaMismatch`、
`InitData(ServiceError)`、`InvalidEnvironment` です。拒否された候補のネイティブロードも
従来の累積世代予算を消費し、KeepLoaded の対象になります。

## 実行と検証

```powershell
cargo run -p contracts_host --locked
cargo test -p reknot --test service_contracts --locked
cargo test -p contracts_host --test native_contracts --locked -- --ignored --test-threads=1 --nocapture
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

サンプルはソースを変更せず、実 DLL のv1／v2を別々に作り、有限の検証後に終了します。
ビルド出力は `target/contracts-build/<PID>/` に分離し、入れ子の Cargo がホストのロックを待ち続けないようにします。
主対象に加えてカウンターと非同期整形サービスを使います。
不足・版違い・初期化データ不一致の候補が本体の状態を変更しないこと、Router 破棄後も
旧／新実行がそれぞれの設定・割り当てで完了することを確認します。
別のネイティブ fixture では、旧 ABI 4 の短いヘッダーだけを読んで拒否する経路を検査します。

Miri は DLL を使わない同じ契約・所有・切り替え経路を両借用モデルで検査します。
実 DLL のネイティブ試験は別途実行します。これらの検査は、任意のデコーダーやサービス実装の正しさまで保証しません。
実行方法と対象範囲は [テスト](TESTING.md) を参照してください。
