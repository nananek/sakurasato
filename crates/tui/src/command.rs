//! M13 PR5 (Issue #79): vim 風コマンドプロンプト (`:`)。
//!
//! Timeline focus 中に `:` を押すと開く 1 行入力欄。Enter で実行、Esc で
//! キャンセル。実行する slash-command は [`Command`] にパースして、
//! [`crate::runtime`] 側で派生処理 (follow / open profile / etc.) に分岐する。
//!
//! ## サポートするコマンド (M13 PR5)
//!
//! - `:follow <acct-or-url>` ── follow を投入 (`POST /api/v1/follow`)
//! - `:unfollow <acct-or-url>` ── follow を取り消し
//! - `:open <acct-or-url>` ── Profile 画面を push (= Misskey 照会 / Mastodon
//!   Lookup 相当)
//! - `:lookup <acct-or-url>` ── `:open` のエイリアス (Misskey 風呼称)
//! - `:me` ── 自分の Profile を push
//! - `:following` / `:followers` ── `FollowList` を push
//! - `:q` / `:quit` ── 終了
//! - `:help` / `:?` ── ヘルプ overlay を開く
//!
//! ## acct / URL の入力形式
//!
//! [`parse_lookup_target`] 参照。受理する形:
//! - `@user@host` / `user@host` → acct 経路
//! - `https://host/@user` / `https://host/@user@otherhost` → acct 経路に変換
//! - `https://host/users/xxx` 等 → AP URI 経路
//! - `http://` も同様 (= 開発環境互換、SSRF 防御は server 側 `net_guard` 任せ)
//!
//! 未知のコマンドは [`Command::Unknown`] で返し、runtime 側で「unknown
//! command: ...」を status に出してプロンプトは閉じる。

/// `:` プロンプトの 1 行入力 state。
#[derive(Debug, Clone, Default)]
pub struct CommandPrompt {
    pub buffer: String,
}

impl CommandPrompt {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_char(&mut self, c: char) {
        self.buffer.push(c);
    }

    pub fn backspace(&mut self) {
        self.buffer.pop();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.trim().is_empty()
    }
}

/// 解決対象。`:follow` / `:open` / `:lookup` / `:unfollow` で使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupTarget {
    /// `user@host` 形式。先頭 `@` は除いてある。
    Acct(String),
    /// AP URI (= `id` フィールドそのもの)。
    ApId(String),
}

/// `parse` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Follow(LookupTarget),
    Unfollow(LookupTarget),
    Open(LookupTarget),
    OpenSelf,
    ListFollowing,
    ListFollowers,
    Help,
    Quit,
    /// 引数不足 / 形式エラー。`reason` を status に出す。
    Invalid {
        reason: String,
    },
    /// 未知のコマンド (= 先頭ワードが不明)。
    Unknown {
        name: String,
    },
}

/// プロンプト buffer (`:` は含まない) → [`Command`]。空文字は `Invalid` 扱い。
pub fn parse(raw: &str) -> Command {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Command::Invalid {
            reason: "empty command".into(),
        };
    }
    let mut parts = trimmed.split_whitespace();
    let head = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest: Vec<&str> = parts.collect();
    match head.as_str() {
        "follow" => map_target(&rest, Command::Follow, "follow"),
        "unfollow" => map_target(&rest, Command::Unfollow, "unfollow"),
        // `:lookup` は Misskey で「照会」の英語ラベル。`:open` のエイリアス。
        "open" | "lookup" => map_target(&rest, Command::Open, "open"),
        "me" => no_arg(&rest, Command::OpenSelf, "me"),
        "following" => no_arg(&rest, Command::ListFollowing, "following"),
        "followers" => no_arg(&rest, Command::ListFollowers, "followers"),
        "help" | "?" => Command::Help,
        "q" | "quit" => Command::Quit,
        other => Command::Unknown { name: other.into() },
    }
}

fn map_target(rest: &[&str], ctor: fn(LookupTarget) -> Command, name: &str) -> Command {
    match parse_one_arg(rest, name).and_then(parse_lookup_target) {
        Ok(target) => ctor(target),
        Err(reason) => Command::Invalid { reason },
    }
}

fn no_arg(rest: &[&str], ok: Command, name: &str) -> Command {
    if rest.is_empty() {
        ok
    } else {
        Command::Invalid {
            reason: format!("`:{name}` takes no argument, got {}", rest.join(" ")),
        }
    }
}

fn parse_one_arg<'a>(rest: &[&'a str], name: &str) -> Result<&'a str, String> {
    match rest.len() {
        0 => Err(format!(
            "`:{name}` expects one acct/URL argument (e.g. @user@host or https://host/@user)"
        )),
        1 => Ok(rest[0]),
        n => Err(format!(
            "`:{name}` expects one argument, got {n} ({})",
            rest.join(" ")
        )),
    }
}

/// acct / URL 文字列 → [`LookupTarget`]。
///
/// 受理する形:
/// - `@user@host` / `user@host` → `Acct`
/// - `https://host/@user[/...]` → `Acct("user@host")`
/// - `https://host/@user@otherhost[/...]` → `Acct("user@otherhost")`
///   (federated permalink 形式、Misskey/Mastodon どちらも採用)
/// - `https://host/users/xxx[/...]` (= AP URI 風) → `ApId(<orig URL>)`
/// - その他の `http(s)://` → `ApId(<orig URL>)` ── 「acct には見えないが
///   何らかの actor URL かもしれない」を server に委ねる
pub fn parse_lookup_target(raw: &str) -> Result<LookupTarget, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty target".into());
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return parse_url_target(raw);
    }
    let body = raw.strip_prefix('@').unwrap_or(raw);
    if !body.contains('@') || body.starts_with('@') || body.ends_with('@') {
        return Err(format!("expected acct (`user@host`) or URL, got {raw:?}"));
    }
    // 軽い検証 ── 厳密 IDN / ASCII チェックは server 側 webfinger_guard が行う。
    Ok(LookupTarget::Acct(body.to_string()))
}

fn parse_url_target(raw: &str) -> Result<LookupTarget, String> {
    let url = url::Url::parse(raw).map_err(|e| format!("invalid URL {raw:?}: {e}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL {raw:?} has no host"))?
        .to_string();
    // path 先頭 segment が `@user` (`+`/`%40` 等は今は無視) なら acct 経路に変換。
    // Mastodon / Misskey / GoToSocial / Pleroma / Mitra すべて `/@user` 形式の
    // permalink を持つ。
    let mut segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<&str>>)
        .unwrap_or_default();
    // 先頭の空 segment (= `/` 直後の "") を捨てる。
    if segments.first() == Some(&"") {
        segments.remove(0);
    }
    if let Some(first) = segments.first()
        && let Some(stripped) = first.strip_prefix('@')
        && !stripped.is_empty()
    {
        // `@user@otherhost` 形式 (federated permalink) もここで吸収。
        let acct = if stripped.contains('@') {
            stripped.to_string()
        } else {
            format!("{stripped}@{host}")
        };
        return Ok(LookupTarget::Acct(acct));
    }
    // それ以外は AP URI として server に投げる ── server `?ap_id=` 経路で
    // `id == ap_id` チェックが効くので、HTML ページが返っても拒否される。
    Ok(LookupTarget::ApId(url.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quit_aliases() {
        assert_eq!(parse("q"), Command::Quit);
        assert_eq!(parse("quit"), Command::Quit);
        assert_eq!(parse("  quit  "), Command::Quit);
    }

    #[test]
    fn parse_help_aliases() {
        assert_eq!(parse("help"), Command::Help);
        assert_eq!(parse("?"), Command::Help);
    }

    #[test]
    fn parse_me_no_args() {
        assert_eq!(parse("me"), Command::OpenSelf);
        assert!(matches!(parse("me extra"), Command::Invalid { .. }));
    }

    #[test]
    fn parse_follow_strips_at_prefix() {
        assert_eq!(
            parse("follow @bob@example.test"),
            Command::Follow(LookupTarget::Acct("bob@example.test".into())),
        );
        // `@` 省略でも通る。
        assert_eq!(
            parse("follow bob@example.test"),
            Command::Follow(LookupTarget::Acct("bob@example.test".into())),
        );
    }

    #[test]
    fn parse_follow_missing_host_invalid() {
        assert!(matches!(parse("follow bob"), Command::Invalid { .. }));
    }

    #[test]
    fn parse_lookup_alias_open() {
        // Misskey 「照会」相当のエイリアス。
        assert_eq!(
            parse("lookup @bob@example.test"),
            Command::Open(LookupTarget::Acct("bob@example.test".into())),
        );
    }

    #[test]
    fn parse_open_accepts_html_permalink_url() {
        // Mastodon / Misskey 形式の `/@user` permalink。
        assert_eq!(
            parse("open https://misskey.io/@alice"),
            Command::Open(LookupTarget::Acct("alice@misskey.io".into())),
        );
        // 末尾スラッシュ + パス余り。
        assert_eq!(
            parse("open https://mastodon.social/@bob/"),
            Command::Open(LookupTarget::Acct("bob@mastodon.social".into())),
        );
    }

    #[test]
    fn parse_open_accepts_federated_permalink() {
        // `https://aggregator.example/@user@home.example` ── Mastodon Web UI が
        // 他鯖の actor を見るときに出すリンク形式。
        assert_eq!(
            parse("open https://aggregator.test/@bob@home.test"),
            Command::Open(LookupTarget::Acct("bob@home.test".into())),
        );
    }

    #[test]
    fn parse_open_accepts_canonical_actor_uri() {
        // Misskey の `/users/xxx` 系。permalink ではないので AP URI として渡す。
        let cmd = parse("open https://misskey.io/users/abcd1234");
        match cmd {
            Command::Open(LookupTarget::ApId(uri)) => {
                assert!(uri.starts_with("https://misskey.io/users/"));
            }
            other => panic!("expected ApId, got {other:?}"),
        }
    }

    #[test]
    fn parse_unfollow_accepts_url_and_acct() {
        assert!(matches!(
            parse("unfollow @bob@example.test"),
            Command::Unfollow(LookupTarget::Acct(_))
        ));
        assert!(matches!(
            parse("unfollow https://misskey.io/@bob"),
            Command::Unfollow(LookupTarget::Acct(_))
        ));
    }

    #[test]
    fn parse_lists() {
        assert_eq!(parse("following"), Command::ListFollowing);
        assert_eq!(parse("followers"), Command::ListFollowers);
    }

    #[test]
    fn parse_unknown_command() {
        assert!(matches!(parse("nuke"), Command::Unknown { .. }));
    }

    #[test]
    fn parse_empty_is_invalid() {
        assert!(matches!(parse(""), Command::Invalid { .. }));
        assert!(matches!(parse("   "), Command::Invalid { .. }));
    }

    #[test]
    fn parse_lookup_target_rejects_garbage() {
        assert!(parse_lookup_target("bob").is_err());
        assert!(parse_lookup_target("@@bob").is_err());
        assert!(parse_lookup_target("bob@").is_err());
    }
}
