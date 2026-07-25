-- Sakurasato — 動画添付対応。
--
-- `media.media_type` は 0009_media.sql の時点で「常に image/webp。将来の
-- 動画対応に備えて TEXT」とコメントされていた通り、型変更は不要。
-- 動画特有のメタデータ (再生時間) と、将来のポスターフレーム対応
-- (フェーズ2、別 issue) 用のカラムのみ追加する。

ALTER TABLE media
    ADD COLUMN duration_ms BIGINT
        CHECK (duration_ms IS NULL OR duration_ms > 0);
-- NULL = 画像 (常に NULL)。動画は video_pipeline 側で duration 抽出に
-- 失敗した場合アップロード自体を reject するため、動画行で NULL になる
-- ことは無い想定。

ALTER TABLE media
    ADD COLUMN poster_storage_key TEXT;
-- フェーズ2 (ポスターフレーム抽出) 用に先行して用意する nullable カラム。
-- 現時点では常に NULL。値が入る場合は versitygw 上のキー
-- (media.storage_key と同じ命名規則) を想定。

COMMENT ON COLUMN media.duration_ms IS
    '動画の再生時間 (ミリ秒)。画像行では常に NULL。';
COMMENT ON COLUMN media.poster_storage_key IS
    'ポスターフレーム画像の versitygw キー。フェーズ2まで常に NULL。';
