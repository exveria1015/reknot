# Tauri GUI で編集とリロードを試す

[English](../docs/TAURI_GUI_LAB.md) | 日本語

英語 UI の、信頼済みローカル Rust コード向けの開発サンプル。

## 起動

リポジトリのルートで実行する。

```powershell
cargo run -p tauri_tokio --locked
```

npm や Tauri CLI は不要。Tauri の Windows ビルド環境と WebView2 が必要。
起動するとロジック DLL を別プロセスでビルドする。初回は依存クレートのコンパイルを待つ間も GUI を操作できる。
ウィンドウを閉じるとエンジンスレッドを終了し、そのスレッド上で Future・ホスト資源を後始末する。

## 画面での編集

1. 右側のエディターで `LABEL`、`ACCENT`、`STEP` を変更する。「v1 · +1」「v2 · +5」でも編集例をセットできる。
2. 「Save & reload」または Ctrl+S で `examples/tauri_tokio_logic/src/lib.rs` へ保存する。
3. 裏でビルドが進み、成功した場合だけ候補を検査して切り替える。画面のホスト PID と稼働時間は維持される。
4. カウンター、保存済みの値、受付の世代、ワーカーの世代、LOADS、履歴を比較する。

編集中のバッファと保存済みファイルは別。編集例・構文エラー・起動時コードのボタンは、保存するまでファイルへ反映しない。
外部エディターから保存しても監視でビルドする。画面内の編集バッファは自動で上書きせず、「Reload file」でファイルを読み直す。
再現手順を揃える場合は「v1 · +1」をセットして保存し、その後 v2 へ変更する。保存するとロジックのソースファイル自体を書き換える。

画面の HTML／CSS／JavaScript 自体はホストのアセット。画面構造を変更した場合はホストを再ビルド・再起動する。
このサンプルでホットリロードするのは Rust ロジックであり、色・文言・数値などをホスト資源 API 経由で画面へ反映する。

## Drain と Restart の違い

| 方式 | 常駐カウンター | 新しいリクエスト |
| --- | --- | --- |
| Drain | 旧世代・旧加算幅で継続 | 新世代の文言で応答 |
| Restart | チェックポイントを引き継ぎ、新世代の加算幅で再開 | 切り替え後は新世代で応答 |

v1 のワーカーを動かしたまま Drain で v2 を保存しても、プレビューが Mint / v1 のままなのは正常。
「Send now」を押すと新しい世代の応答を確認できる。カウンター自体も更新するには Restart を選んで保存または再ビルドする。
方式の選択だけでは切り替えず、次のビルド成功時に適用する。

Restart の状態は u64 カウンターの8バイト形式。停止時に保存し、同じスキーマで復元する。
この例ではスキーマ変更の変換は行わない。異なる形式への移行は [Restart の例](RESTART_AND_MIGRATION.md) を参照。
値を維持するのは同じホスト内のリロード間であり、アプリ終了後の永続保存ではない。

## 実行中のリクエスト

「5-second request」を押し、完了を待つ間にロジックを変更する。
Drain なら受付時の旧コードで完了し、次のリクエストから新コードになる。
Restart では停止通知を受けて待機を短縮し、旧コードで応答してから切り替える。応答には「completed after a stop request」と表示する。
通常リクエストを自動再実行することはない。

この GUI はホストが WebView と画面状態を保持する例。TCP の接続維持は `service_host` サンプルと実 DLL テストで別に確認している。

## ビルド失敗からの復帰

1. 「Build error」を押して保存する。
2. 「Build details」に Rust のエラーが表示される。旧世代のカウンターとリクエスト処理は継続する。
3. v1／v2 の編集例を押すと、サンプルが追加した構文エラーを除去できる。保存して再試行する。

ビルド失敗は DLL のロード前なので世代枠を消費しない。
ABI・スキーマの検査失敗や停止待ち失敗は別の失敗。Restart が中断した場合は受付停止を維持し、必要なら「Resume requests」で明示的に再開する。
実行中のロジックが poll から戻らない場合の強制停止や、任意の実行時 panic からの自動復旧は提供しない。

保持予算は既定32枠。枯渇後はウィンドウを閉じて開発プロセスを再起動する。
DLL コピー先はユーザー一時領域の `reknot/managed-v1/sessions/<SessionId>/copies/`。
起動時の作業ディレクトリが読み取り専用でも使える。
`reknot-cache inspect` で調べ、終了後の適格なセッションだけを `reknot-cache clean --apply` で掃除する。
詳細は [保持量の観測とコピーの掃除](RETENTION_AND_CLEANUP.md) を参照。

## 実装

- `examples/tauri_tokio/dist/`: 手書きの UI ソースとエディター。ビルド生成物ではなく、Tauri が埋め込むため削除しない。
- `examples/tauri_tokio/src-tauri/gen/schemas/`: tauri-build の生成物。Git 管理から除外し、ビルド時に再生成する。
- `examples/tauri_tokio/src-tauri/src/main.rs`: Tauri command とアプリ終了時のスレッド join。
- `examples/tauri_tokio/src-tauri/src/engine.rs`: バックグラウンドビルド、ソース監視、受付切り替え、ホスト画面資源。
- `examples/tauri_tokio_logic/src/lib.rs`: Handler ABI 5 の async ハンドラー。

SDK のサービス操作は短い同期処理。時計と表示データをホストへ問い合わせ、ローカル実行ループが16msごとに Future を poll する。
ソース保存先はこのロジックファイルに固定。ローカルの信頼済み Rust コードを実行する開発サンプルとして使う。
ビルドログはロジック用 target ディレクトリ内の `gui-build.log` に保存する。
Tauri command とグローバル invoke の利用方法は [Tauri 公式ドキュメント](https://v2.tauri.app/develop/calling-rust/) に基づく。

## 検証

実ウィンドウでは、起動、カウンター更新、画面内の編集例、ソース保存、世代更新を確認する。
同じホスト PID のまま Drain では worker / gen 1 が継続し、Restart 後に Coral / v2・+5 のワーカーへ切り替わることを確かめる。

自動検査には、独立した一時プロジェクトを使うネイティブ統合テストを使う。
通常の GUI ソースを変更せず、作業ディレクトリ内に target を作れない状況、Drain、Restart、カウンター維持、
構文エラー中の旧ワーカー継続、修正後の再開、世代予算を確認する。テスト後にエンジンを停止し、一時プロジェクトを掃除する。

```powershell
cargo test -p tauri_tokio --bin tauri_tokio --locked -- --ignored --test-threads=1
cargo clippy -p tauri_tokio -p tauri_tokio_logic --all-targets --locked -- -D warnings
node --check examples/tauri_tokio/dist/main.js
```

ヘッドレス統合テストはネイティブ DLL と GUI バックエンドを検証するもので、実ウィンドウの操作確認とは区別する。
CI の設定と実行方法は [テスト](TESTING.md) を参照。
