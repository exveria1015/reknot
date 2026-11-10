# 目的別ドキュメント一覧

[English](../docs/INDEX.md) | 日本語

初めて使う場合は [README の実行例](README.md#まず動かす) から始めてください。
このページは、実装や運用で必要になった話題を探すための索引です。

## アプリを作る

- [WebSocketクイズルーム](QUIZ_ROOM.md) — 遊びながら編集し、Axum + ReknotとFastify + JSを比較する。

- [型付きハンドラー入門](TYPED_HANDLERS.md) — プロジェクト生成、通常の関数での実装、ホストへの接続。
- [サンプル一覧](../examples/README.md) — 実行コマンドと、それぞれのサンプルで試せること。
- [Tauri GUI Lab](TAURI_GUI_LAB.md) — 画面内で Rust を編集し、更新やビルド失敗からの復旧を試す。
- [Bevy ゲームへの最小適用](BEVY_GAME.md) — フレーム境界での更新、旧世代の結果の扱い、ワーカーの再生成。

## ホストとロジックをつなぐ

- [別スレッドのフレームワークから呼ぶ](TYPED_HANDLERS.md#別スレッドのフレームワークから呼ぶ) — 型付きクライアントでイベントを渡す。
- [低レイヤーから組み込む](GETTING_STARTED.md) — ホスト資源とバイト列を扱う API の導入手順。
- [ホスト資源と Drain の受付切り替え](HOST_RESOURCES_AND_ROUTING.md) — 資源の登録・失効、サービス操作、更新候補の適用。
- [非同期サービスと複数スレッド](ASYNC_SERVICES_AND_THREADS.md) — ホスト側の I/O を待つ、キャンセルする、スレッド間で通知する。
- [複数サービスと型付き初期化](SERVICE_CONTRACTS_AND_INIT.md) — 必要なサービスを宣言し、世代ごとの設定を渡す。

- [契約エラーとスキーマ](CONTRACTS_AND_SCHEMAS.md) — スキーマ生成、構造差の診断、契約の出力。

## 状態を保ち、処理を終了する

- [型付き Restart](TYPED_RESTART.md) — 型付きワーカーの登録、状態移行、中断後の復旧。
- [Restart と状態移行](RESTART_AND_MIGRATION.md) — 登録ワーカーの協調停止、チェックポイント移行、再生成。
- [ホスト資源・DB の移行](HOST_RESOURCE_MIGRATION.md) — SQLite とキャッシュの排他、確定フック、失敗時の復旧。
- [子タスクと外部コールバック](MANAGED_ACTIVITIES.md) — 子処理の明示登録、停止要求、終了待ち。
- [登録型 TLS とホスト所有の通知](MANAGED_LOCALS_AND_NOTIFICATIONS.md) — 世代ローカル値の破棄、遅れて届く通知、依存先との責任分担。

## 開発を続ける

- [開発時の運用](DEVELOPMENT.md) — ソース監視、ビルドと切り替え、失敗からの復旧、上限の観測。
- [トラブルシューティング](TROUBLESHOOTING.md) — 症状とエラー別の確認点。
- [保持量の観測とコピーの掃除](RETENTION_AND_CLEANUP.md) — DLL の保持状況、キャッシュセッション、終了後のファイル削除。
- [互換性と対象範囲](COMPATIBILITY.md) — 必要環境、SDK・ABI の条件、アプリケーションが担うこと。

## API と内部設計を調べる

- [API リファレンス](API_REFERENCE.md) — API の役割、型付き呼び出しの制約、ビルド構成、ABI。
- [設計と要件](ARCHITECTURE.md) — 所有の分担、更新の境界、状態と失敗の扱い。
- [所有・借用・破棄順序](LIFETIME_REVIEW.md) — 非同期実行とコード世代の寿命。
- [DLL の寿命と管理外タスク](UNLOAD_SAFETY.md) — 保持と明示回収、外部処理・TLS の終了条件。

## 変更を検証する

- [テストと Miri](TESTING.md) — 再実行手順、テストごとの確認範囲。
- [リリースの準備](RELEASING.md) — 対象ソース、CI、配布経路、公開時に明記する制限。
- [変更履歴](../CHANGELOG.md) — 利用者向けの変更点と破壊的変更（英語）。
- [開発への参加](../CONTRIBUTING.md) — ソース配置、確認方法、文書の更新方針（英語）。

[日本語 README](README.md)
