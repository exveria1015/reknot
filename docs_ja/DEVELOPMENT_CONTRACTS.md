# 必要になったときに読む開発契約

[English](../docs/DEVELOPMENT_CONTRACTS.md) | 日本語

初回は[README](README.md)の手順で動かしてください。生成したホストには、更新を進め、
編集するロジックの外に状態を保持する処理が入っています。構成や処理の寿命を変更するときに
このページを参照します。

## 変更に関係する契約を選ぶ

| やりたいこと | 守ること | 詳細 |
| --- | --- | --- |
| 返信や計算を変更する | `logic`を編集して保存し、更新通知を待つ。受付済みの処理は元のコードで完了する（Drain） | [型付きハンドラー](TYPED_HANDLERS.md) |
| 入出力、エンコード形式、意味を変える | 共有プロトコルと契約の版を更新し、ホストも再ビルド・再起動する | [スキーマ](CONTRACTS_AND_SCHEMAS.md)、[バイナリメッセージ](BINARY_MESSAGES.md) |
| 接続やアプリの状態を保持する | ホストが所有し、サービスとしてロジックから使う | [ホスト資源](HOST_RESOURCES_AND_ROUTING.md) |
| 常駐ワーカーを動かす | ワーカーとチェックポイントを登録し、再生成が必要なら協調的Restartを選ぶ | [型付きRestart](TYPED_RESTART.md) |
| 子処理やcallbackを作る | 寿命を登録して後始末の完了を待つ。キャンセルは外部副作用を巻き戻さない | [管理アクティビティ](MANAGED_ACTIVITIES.md) |
| 既定のKeepLoaded累積32回に達する | 開発プロセスを再起動する。失敗したネイティブロードも枠を消費する場合がある | [保持と掃除](RETENTION_AND_CLEANUP.md) |
| DLLリンクの代わりにJITを使う | std/supportを常駐させ、安定した非ジェネリック処理をsupportへ置く。基盤変更後は再ビルド・再起動する | [型付きJIT](TYPED_JIT.md) |
| JITロジックに世代ごとのデータを置く | `GenerationLocal`を使う。再ロードするオブジェクトの`thread_local!`は未対応 | [世代ローカル](MANAGED_LOCALS_AND_NOTIFICATIONS.md) |
| ManagedでJITコードを回収する | 外部callbackやWakerの最終帰還を含む追加の寿命契約を明示的に受け入れる | [二層開発](LAYERED_DEVELOPMENT.md) |

## 要件が変わるまでは既定を使う

ひな形の既定は通常DLL、JSON、Drain、KeepLoadedです。Rust/Cargoと対象OSの開発環境で動きます。
Restart、バイナリメッセージ、JITはそれぞれ別の選択で、最初のハンドラーを編集する際には不要です。
どちらのエンコード形式も所有権のあるメッセージを使い、入力・出力それぞれ1 MiBに制限します。
メタデータとチェックポイントはJSONのままです。

ローカルの信頼できるコード、依存、ビルドスクリプトを使い、ホストとロジックのSDKを合わせてください。
コンパイラーやlockfileの一致は同一性の検査であり、アンロードされる世代をまたいで
Rustオブジェクトの参照を保持できることの証明ではありません。

[互換性](COMPATIBILITY.md) · [トラブルシューティング](TROUBLESHOOTING.md)
