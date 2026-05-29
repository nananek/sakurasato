#!/bin/bash
# Fedibird entrypoint for federation tests.
#
# Mastodon fork なので mastodon-entrypoint.sh とほぼ同じだが、
# Fedibird 3.4.1 は OAuth password grant 未対応のため bob の access token を
# Doorkeeper 経由で直接発行して /tokens/ に書く (将来 setup-fedibird.sh 等で
# 参照する用途を想定)。
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

RAILS_ENV=production bundle exec bin/tootctl accounts create bob --email bob@fedibird --confirmed 2>/dev/null || true
bundle exec rails runner "
  account = Account.find_local('bob')
  if account
    user = account.user
    user.password = 'Password1234!'
    user.save!(validate: false)
  end
" 2>/dev/null || true

echo "Creating OAuth token for bob..."
mkdir -p /tokens
bundle exec rails runner "
  user = Account.find_local('bob')&.user
  if user
    app = Doorkeeper::Application.create_with(
      redirect_uri: 'urn:ietf:wg:oauth:2.0:oob',
      scopes: 'read write follow'
    ).find_or_create_by!(name: 'federation-test')
    token = Doorkeeper::AccessToken.create!(
      application: app,
      resource_owner_id: user.id,
      scopes: 'read write follow',
      expires_in: nil
    )
    File.write('/tokens/bob_token.txt', token.token)
    puts \"Token created: #{token.token[0..7]}...\"
  else
    puts 'ERROR: bob user not found'
    exit 1
  end
" || { echo "Failed to create token"; exit 1; }

exec bundle exec puma -C config/puma.rb
