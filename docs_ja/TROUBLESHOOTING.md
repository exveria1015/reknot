# トラブルシューティング

[English](../docs/TROUBLESHOOTING.md) | 日本語

型付き API を使っている場合は、まず以下の症状から確認してください。
低レイヤーのエラーは、後半の [メッセージ API](#メッセージ-api) にまとめています。

## 保存しても更新されない

生成したプロジェクトでは、`logic/src/lib.rs` を編集しているか確認します。
`host` の変更はホストの再起動が必要です。`protocol` の変更も、ロジックだけの更新では反映できません。

次に、ホストのログを確認してください。

| 表示・症状 | 対処 |
| --- | --- |
| `Building edited logic` が出ない | `DevModule::update()` が入力待ち中も動いているか、編集したファイルが監視対象かを確認する |
| `Build failed` | コンパイルエラーを直して保存する。`--locked` のエラーなら、通常の Cargo 操作で先に `Cargo.lock` を更新する |
| `Candidate rejected` | ログに出た契約と共有プロトコルを照合する。意図した形式・意味の変更なら、契約の版を上げてホストも再ビルド・再起動する |
| `Source changed during the build` | 追加の編集があり、古い候補を破棄した状態。次のビルド結果を待つ |
| `load-attempt budget reached` | 開発ホストを再起動する。DLL の回収では消費したロード枠は戻らない |

`Error::Contract` は照合段階・ハンドラー・JSON ポインター・期待値と実値・取得できたソース位置を示します。
例えば `/input_schema/deserialize/properties/value/type` は該当フィールドの形式です。
全体は `report.expected` と `report.candidate` で確認できます。[契約エラー](CONTRACTS_AND_SCHEMAS.md) を参照してください。

適用の完了は `New events use generation ...` で確認します。
監視対象外のファイルや `.cargo` の新規作成については、[監視範囲](DEVELOPMENT.md#ソース監視の範囲) を参照してください。

## 呼び出しの結果が返らない

`HandlerClient` はキューへ要求を渡します。実際に処理するのは、所有スレッド上の driver です。
結果を await している間も `DevModule::update()`、または独自ホストの `BridgeDriver` を poll してください。
所有スレッドで結果だけを待つと、キューを処理できません。生成例では `select!` で並行させています。

| エラー | 確認すること |
| --- | --- |
| `Error::Busy` | 同時要求数とキュー容量。完了しても未受領の結果は枠を使うため、結果を受け取るか不要な Future を Drop する |
| `Error::BridgeClosed` | client が閉じられたか、所有側の driver が破棄されていないか |
| 型付きメッセージの変換エラー | JSON で表現できる型か、入出力が各1 MiB以内か、ホストとロジックの契約が一致しているか |

## キャンセルしても終了しない

キャンセルは、登録した子処理への停止要求を含みます。子処理が終了するまで、要求の終了待ちは続きます。
停止要求に応答しているか、外部コールバックの登録解除と終了確認を進めているかを調べてください。

`close()` / `shutdown()` を待つ間も driver を動かします。driver を Drop しただけでは正常な終了確認になりません。
ローカル要求の Drop 後などに必要な `poll_cleanup` は、[API の終了契約](API_REFERENCE.md#キャンセルと終了) を参照してください。
未登録の独自 spawn や、終了しない外部処理の自動停止は行いません。

## メッセージ API

| エラー・症状 | 確認と対処 |
| --- | --- |
| `Abi` / handler ABI mismatch | 同じ SDK で両側を再ビルド。Handler ABI は5、Services とタスク制御 ABI は2 |
| `ContractMismatch` | 資源とハンドラーのサービス ID・版を合わせる。サービス本体の移行はアプリ側の責務 |
| `StateSchemaMismatch` | Drain は同じスキーマが必要。形式変更はチェックポイント変換を伴う Restart へ |
| `WrongRouter` / `StaleCandidate` | 対象 Router の現在世代から候補を準備し直す |
| `GenerationLimit` | 累積ロード予算が枯渇。開発プロセスを再起動。失敗したネイティブロードも消費する |
| Restart 失敗・中断後の `Paused` | 復旧方針を決めて再試行、または `resume()?`。旧ワーカーの再生成とは別 |
| Restart が Pending のまま | 外部所有の RequestTask も poll する。ロジック側で停止要求に応答し、子処理も終了させる |
| `WorkerFailed` | 状態を確認し、必要なら登録を解除・修正して再試行。通常要求を無条件で再実行しない |
| `CheckpointTooLarge` / 移行失敗 | 形式・スキーマ・サイズ上限を確認。旧チェックポイントは残る |
| サービスの `BUSY` / `BUFFER_TOO_SMALL` | 競合する借用を解放、または十分な出力領域を確保。自動リトライはない |
| `REVOKED` / 資源が見つからない | 失効、レジストリ、ResourceId の世代を確認。正しいレジストリの有効 ID を使う |

Drain で常駐ワーカーが旧コードのままなのは正常です。GUI で新しいリクエストの文言だけが変わり、
カウンターの加算幅が変わらない場合は Restart を選択して再ビルドしてください。

## ビルド・リロード

ホストが監視するパスとビルドログを確認します。Router 自体に監視機能はありません。
`service_host` はメッセージ境界または処理の poll 中に変更を扱い、GUI はバックグラウンドでビルドします。
`dev-tools` の監視はホストが明示的に起動します。静的構成では監視・ビルドを行いません。

Windows のアクセス拒否では、エラーに出た**パス**を確認してください。
Cargo の出力先とユーザー一時領域に書き込み権限が必要です。DLL コピー先は現在の作業ディレクトリではなく
`reknot/managed-v1/sessions/<SessionId>/copies/` です。権限と使用中プロセス・ファイルロックを確認し、使用中 DLL のコピーは削除しません。
不完全な成果物は完全にビルドし直してから候補を準備します。

GUI の HTML／CSS／JS はホストの埋め込みアセットです。変更後はホストを再ビルド・再起動してください。
ホットリロード対象は Rust ロジック DLL です。

[日本語 README](README.md) · [ドキュメント一覧](INDEX.md)
