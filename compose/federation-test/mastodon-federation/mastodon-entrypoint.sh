#!/bin/bash
# Mastodon entrypoint for federation tests.
#
# - Trusts the shared test CA so outbound calls to https://sakurasato/ work.
# - Waits for postgres / redis.
# - Bootstraps the DB and creates an admin account `bob` (idempotent).
# - Hands off to Puma.
set -e

if [ -f /certs/ca.crt ]; then
  cp /certs/ca.crt /usr/local/share/ca-certificates/test-ca.crt
  update-ca-certificates 2>/dev/null || true
fi

until bundle exec ruby -e "require 'pg'; PG.connect(host: ENV['DB_HOST'], port: ENV.fetch('DB_PORT', 5432), user: ENV['DB_USER'], password: ENV['DB_PASS'], dbname: 'postgres')" 2>/dev/null; do
  echo "Waiting for PostgreSQL..."
  sleep 1
done

until bundle exec ruby -e "require 'redis'; Redis.new(host: ENV['REDIS_HOST'], port: ENV.fetch('REDIS_PORT', 6379)).ping" 2>/dev/null; do
  echo "Waiting for Redis..."
  sleep 1
done

echo "Setting up database..."
SAFETY_ASSURED=1 bundle exec rails db:setup 2>/dev/null || bundle exec rails db:migrate

bundle exec rails runner "Setting.registrations_mode = 'open'" 2>/dev/null || true
bundle exec rails runner "Setting.min_invite_role = 'user'" 2>/dev/null || true

RAILS_ENV=production bundle exec tootctl accounts create bob --email bob@mastodon --confirmed 2>/dev/null || true
bundle exec rails runner "
  account = Account.find_local('bob')
  if account
    user = account.user
    user.password = 'Password1234!'
    user.save!(validate: false)
  end
" 2>/dev/null || true

# Pytest 連合テスト用に bob の OAuth Bearer トークンを一度だけ発行する。
# Mastodon 4.x で OAuth password grant が削除されたため、Doorkeeper 経由で
# 直接 AccessToken を作って共有 volume にファイル出力しておく ── pytest 側
# は `MASTODON_TOKEN_FILE` (= /mastodon-tokens/bob.token) を読むだけ。
# (`/mastodon-tokens` が存在しない & ファイルがすでにある場合はスキップ。
#  pytest profile 外で立ち上げた場合に余計な Rails 起動を増やさない)。
if [ -d /mastodon-tokens ] && [ ! -f /mastodon-tokens/bob.token ]; then
  # stderr は **絶対に捨てない** ── User lookup nil や File.write の権限
  # エラーが無音で落ちて `bob.token` が生まれず、依存している pytest
  # コンテナが `_read_token_file` で FileNotFoundError だけ吐いて原因不明、
  # という詰み方を避ける ([round-1 review M-1] 対応)。stderr を stdout に merge
  # して `docker compose logs mastodon-web` に Rails の例外をそのまま残す。
  # さらに User.find_by が nil のときは `exit 1` で Ruby 側を **明示的に失敗**
  # させ、シェルの後段 (= `|| echo` + 終了直後の `[ -s … ]` ガード) で確実に
  # 検出する ([round-2 review L-1] 対応)。
  bundle exec rails runner "
    user = User.find_by(email: 'bob@mastodon')
    if user.nil?
      warn 'pytest token: User.find_by returned nil for bob@mastodon'
      exit 1
    end
    app = Doorkeeper::Application.create!(
      name: 'sakurasato-pytest',
      redirect_uri: 'urn:ietf:wg:oauth:2.0:oob',
      scopes: 'read write follow'
    )
    token = Doorkeeper::AccessToken.create!(
      application: app,
      resource_owner_id: user.id,
      scopes: 'read write follow'
    )
    File.write('/mastodon-tokens/bob.token', token.token + \"\\n\")
  " 2>&1 || echo 'pytest bearer token issue skipped (see Rails error above)'

  # entrypoint の途中で puma に exec する前に、トークンが本当に書けたかを
  # 一段ガード ── nil 経路で `exit 1` が出ても compose は puma を起こせて
  # しまい、`mastodon-web: service_healthy` 通過後に pytest 側だけが
  # `FileNotFoundError` で落ちる事故を防ぐ。
  if [ ! -s /mastodon-tokens/bob.token ]; then
    echo 'pytest token: /mastodon-tokens/bob.token was not produced; aborting' >&2
    exit 1
  fi
fi

exec bundle exec puma -C config/puma.rb
