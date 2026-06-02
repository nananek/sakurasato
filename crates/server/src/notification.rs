//! Discord (および Slack / Misskey) 互換 webhook 通知。
//!
//! お一人様サーバには Web UI が無いので、外出中に「メンション / DM / 引用 /
//! リアクション / リノート / フォロー (鍵アカ運用なら follow-request も)」が
//! 来たことを Discord 等にプッシュして気付くための経路。
//!
//! # アーキテクチャ
//!
//! - `notification_channel` テーブル (migration 0013) に宛先 URL とイベント
//!   ごとの notify_* bool を保持する
//! - dispatch handler 側 (`crate::dispatch::{note,reaction,announce,handler}`)
//!   が DB 書き込み commit 後に [`dispatch::notify`] を fire-and-forget で呼ぶ
//! - [`dispatch::notify`] は対象 channel を [`payload::build_payload`] で整形し、
//!   既存の `delivery_queue` に 1 行 enqueue する
//! - delivery worker は `activity.type` が `"Webhook:"` prefix な行を見ると
//!   HTTP 署名を skip して `application/json` で `payload` を POST する分岐
//!   ([`crate::delivery::attempt_post`]) に流れる
//!
//! `payload` モジュールは純粋関数なのでテストしやすく、`dispatch` は
//! `delivery_queue` の enqueue だけに集中する。CLI ハンドラは `cli_runner`。

pub mod cli_runner;
pub mod dispatch;
pub mod payload;
