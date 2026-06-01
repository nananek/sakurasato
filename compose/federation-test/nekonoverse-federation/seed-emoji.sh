#!/usr/bin/env sh
# Sakurasato 側に 1 件だけ「テスト用カスタム絵文字」を import するための
# Misskey 形式 zip を生成する (#58 / #120 PR2c)。`sakurasato-emoji-import`
# 1-shot service が本 zip を `sakurasato-server emoji import` に喰わせて、
# `:sakurasato_test:` 1 件を local emoji 行として upsert する。
#
# 出力:
#   - `${SEED_EMOJI_OUT}` (default `/seed-emoji/sakurasato_test.zip`)
#
# 依存:
#   - alpine + zip + coreutils (= base64 -d) ── compose 側で apk add
#
# zip 構造 (CLAUDE.md §5.4 / `crates/server/src/emoji_import.rs`):
#   meta.json: { metaVersion:2, host:?, exportedAt:?, emojis:[{downloaded:true,fileName:"sakurasato_test.png",emoji:{name:"sakurasato_test",aliases:["sakurasato","sktest"],category:"test"}}] }
#   sakurasato_test.png: 1x1 RGBA PNG (= media-proxy が webp に再エンコードする)
set -eu

SEED_EMOJI_OUT="${SEED_EMOJI_OUT:-/seed-emoji/sakurasato_test.zip}"
SHORTCODE="${SEED_EMOJI_SHORTCODE:-sakurasato_test}"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

cd "$WORK_DIR"

# 1x1 RGBA PNG を base64 で埋め込み (= 70 bytes、再ビルド毎の差異を防ぐため
# 固定値を使う)。Python / Pillow を要求しない方が seed の依存が軽い。
cat <<'B64' | base64 -d > "${SHORTCODE}.png"
iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4//8/AwAI/AL+p5qgoAAAAABJRU5ErkJggg==
B64

# meta.json を組み立て。`aliases` 含む / `category` 含む / `downloaded: true`
# の最小有効スキーマ。
cat > meta.json <<JSON
{
  "metaVersion": 2,
  "host": "sakurasato",
  "exportedAt": "2026-06-01T00:00:00Z",
  "emojis": [
    {
      "downloaded": true,
      "fileName": "${SHORTCODE}.png",
      "emoji": {
        "name": "${SHORTCODE}",
        "category": "test",
        "aliases": ["sakurasato", "sktest"]
      }
    }
  ]
}
JSON

mkdir -p "$(dirname "${SEED_EMOJI_OUT}")"
# `-j` で zip 内パスを basename だけにする (= ルート直下)。
zip -j "${SEED_EMOJI_OUT}" meta.json "${SHORTCODE}.png" >/dev/null

echo "==> seeded emoji zip ${SEED_EMOJI_OUT} (:${SHORTCODE}:)" >&2
