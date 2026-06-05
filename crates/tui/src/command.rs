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
//! - `:lock` ── 鍵アカ運用に切替 (M12 Issue #66)
//! - `:unlock` ── 鍵アカ解除 (pending は auto-accept されない)
//! - `:requests` ── 承認待ち follow 一覧画面 (`a` = approve / `x` = reject)
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
//!
//! ## Tab 補完 (Issue #116)
//!
//! 先頭ワード (= head) のみ前方一致補完する。引数 (`@acct@host` / URL) の補完
//! は scope 外。[`COMMAND_HEADS`] が静的候補一覧、[`ARG_TAKING_HEADS`] は補完
//! 確定時に末尾スペースを付ける head 集合 (= 引数を 1 個取るもの)。
//!
//! 動作 ([`CommandPrompt::complete`] 参照):
//! - 0 件: buffer も suggestions も触らない
//! - 1 件: buffer をその head に置換、arg 取るものは末尾 ` ` 付き、suggestions 空
//! - 複数件: buffer を最長共通接頭辞まで伸ばし、suggestions に候補を入れる
//!
//! buffer に空白が含まれる (= 既に引数領域に入っている) ときは Tab を noop。

/// Tab 補完で候補にする `:` コマンドの head 一覧 (アルファベット順)。
/// `parse` の `match` に追加した head はここにも足す ── grep で見つけやすい
/// よう、両方を 1 ファイル内に置く。`?` は単一記号のため補完対象外。
pub const COMMAND_HEADS: &[&str] = &[
    "follow",
    "followers",
    "following",
    "help",
    "lock",
    "lookup",
    "me",
    "notifications",
    "open",
    "q",
    "quit",
    "renote",
    "requests",
    "unfollow",
    "unlock",
    "unrenote",
];

/// 引数を 1 個取る head ── 補完確定時に末尾 space を足して引数入力を促す。
const ARG_TAKING_HEADS: &[&str] = &["follow", "unfollow", "open", "lookup"];

/// `:` プロンプトの 1 行入力 state。
#[derive(Debug, Clone, Default)]
pub struct CommandPrompt {
    pub buffer: String,
    /// Tab 補完で複数候補が当たったときに UI に出す候補。1 件以下なら空。
    /// 入力 (`insert_char` / `backspace`) が走ると自動でクリアされる。
    pub suggestions: Vec<&'static str>,
}

impl CommandPrompt {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_char(&mut self, c: char) {
        self.buffer.push(c);
        self.suggestions.clear();
    }

    pub fn backspace(&mut self) {
        self.buffer.pop();
        self.suggestions.clear();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.trim().is_empty()
    }

    /// Tab 補完。buffer の head 部 (= 最初の空白までの prefix) を [`COMMAND_HEADS`]
    /// と前方一致させ、buffer / suggestions を更新する。詳細はモジュール doc 参照。
    pub fn complete(&mut self) {
        // 既に引数領域 (= 空白後) に入っているなら触らない。
        if self.buffer.contains(char::is_whitespace) {
            self.suggestions.clear();
            return;
        }
        let prefix = self.buffer.to_ascii_lowercase();
        let matches: Vec<&'static str> = COMMAND_HEADS
            .iter()
            .copied()
            .filter(|h| h.starts_with(&prefix))
            .collect();
        match matches.len() {
            0 => {
                // 補完不能 ── popup は出さない (= 既存 suggestions は消す)。
                self.suggestions.clear();
            }
            1 => {
                let head = matches[0];
                self.buffer.clear();
                self.buffer.push_str(head);
                if ARG_TAKING_HEADS.contains(&head) {
                    self.buffer.push(' ');
                }
                self.suggestions.clear();
            }
            _ => {
                let lcp = longest_common_prefix(&matches);
                if lcp.len() > self.buffer.len() {
                    self.buffer.clear();
                    self.buffer.push_str(lcp);
                }
                self.suggestions = matches;
            }
        }
    }
}

/// 入力文字列スライス群の longest common prefix を返す。
/// 入力が空なら `""`。ASCII 前提 (= [`COMMAND_HEADS`] は全 ASCII)。
fn longest_common_prefix<'a>(items: &[&'a str]) -> &'a str {
    let first = match items.first() {
        Some(s) => *s,
        None => return "",
    };
    let mut end = first.len();
    for s in &items[1..] {
        let bytes = s.as_bytes();
        let f = first.as_bytes();
        let mut i = 0;
        while i < end && i < bytes.len() && bytes[i] == f[i] {
            i += 1;
        }
        end = i;
        if end == 0 {
            break;
        }
    }
    &first[..end]
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
    /// M12 (#66): 鍵アカ運用に切替 (`manually_approves_followers = true`)。
    Lock,
    /// M12 (#66): 鍵アカ運用を解除。pending follow は auto-accept されない。
    Unlock,
    /// M12 (#66): 承認待ち follow 一覧画面を開く。`a` で approve / `x` で reject。
    OpenRequests,
    /// #206 PR3: in-app 通知一覧画面を開く。`m` で全件既読 / `r` で再取得。
    OpenNotifications,
    /// #151: 選択中の Note を renote (boost) する。`b` キーと同じ動作。
    Renote,
    /// #151: 選択中の Note への自分の renote を取り消し。`B` キーと同じ動作。
    Unrenote,
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
        // M12 (#66): 鍵アカ管理。`requests` は承認待ち一覧画面を開く。
        "lock" => no_arg(&rest, Command::Lock, "lock"),
        "unlock" => no_arg(&rest, Command::Unlock, "unlock"),
        "requests" => no_arg(&rest, Command::OpenRequests, "requests"),
        "notifications" => no_arg(&rest, Command::OpenNotifications, "notifications"),
        "renote" => no_arg(&rest, Command::Renote, "renote"),
        "unrenote" => no_arg(&rest, Command::Unrenote, "unrenote"),
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
    fn parse_lock_unlock_no_args() {
        assert_eq!(parse("lock"), Command::Lock);
        assert_eq!(parse("unlock"), Command::Unlock);
        assert!(matches!(parse("lock extra"), Command::Invalid { .. }));
        assert!(matches!(parse("unlock extra"), Command::Invalid { .. }));
    }

    #[test]
    fn parse_requests_no_args() {
        assert_eq!(parse("requests"), Command::OpenRequests);
        assert!(matches!(parse("requests extra"), Command::Invalid { .. }));
    }

    #[test]
    fn parse_renote_unrenote_no_args() {
        assert_eq!(parse("renote"), Command::Renote);
        assert_eq!(parse("unrenote"), Command::Unrenote);
        assert!(matches!(parse("renote extra"), Command::Invalid { .. }));
        assert!(matches!(parse("unrenote extra"), Command::Invalid { .. }));
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

    // ── Issue #116: Tab 補完テスト ────────────────────────────────────────

    /// 候補一覧が `parse` の `match` と漏れなく一致していることを安全側で
    /// 担保する。`parse` は単独の不明 head を `Command::Unknown` で返すので、
    /// [`COMMAND_HEADS`] の各 entry が `Unknown` 以外に解決することを確認。
    #[test]
    fn command_heads_are_recognized_by_parse() {
        for head in COMMAND_HEADS {
            let parsed = parse(head);
            assert!(
                !matches!(parsed, Command::Unknown { .. }),
                "{head:?} should be recognized by parse, got {parsed:?}"
            );
        }
    }

    #[test]
    fn complete_unique_match_fills_buffer() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("req");
        p.complete();
        assert_eq!(p.buffer, "requests");
        assert!(p.suggestions.is_empty());
    }

    #[test]
    fn complete_arg_taking_appends_space() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("foll");
        p.complete();
        // `follow` / `followers` / `following` で longest common prefix = `follow`、
        // suggestions が出る。
        assert_eq!(p.buffer, "follow");
        assert_eq!(p.suggestions, vec!["follow", "followers", "following"]);

        let mut p = CommandPrompt::new();
        p.buffer.push_str("lookup");
        p.complete();
        // `lookup` 自体が unique なので `lookup ` (trailing space) に確定。
        assert_eq!(p.buffer, "lookup ");
        assert!(p.suggestions.is_empty());
    }

    #[test]
    fn complete_no_arg_does_not_append_space() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("lo");
        p.complete();
        // `lock` / `lookup` で lcp = `lo`、suggestions 表示。
        assert_eq!(p.buffer, "lo");
        assert_eq!(p.suggestions, vec!["lock", "lookup"]);

        let mut p = CommandPrompt::new();
        p.buffer.push_str("loc");
        p.complete();
        // `lock` のみ。arg を取らないので末尾 space 無し。
        assert_eq!(p.buffer, "lock");
        assert!(p.suggestions.is_empty());
    }

    #[test]
    fn complete_extends_to_longest_common_prefix() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("un");
        p.complete();
        // `unfollow` / `unlock` / `unrenote` で lcp = `un`。
        assert_eq!(p.buffer, "un");
        assert_eq!(p.suggestions, vec!["unfollow", "unlock", "unrenote"]);
    }

    #[test]
    fn complete_empty_buffer_lists_all_heads() {
        let mut p = CommandPrompt::new();
        p.complete();
        // 空 prefix なら全 head が候補。lcp は空文字なので buffer は不変。
        assert!(p.buffer.is_empty());
        assert_eq!(p.suggestions.len(), COMMAND_HEADS.len());
    }

    #[test]
    fn complete_no_match_keeps_buffer_clears_suggestions() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("nuke");
        p.suggestions = vec!["stale"];
        p.complete();
        // 不一致は静かに何もしない (= buffer 触らず、過去の suggestions だけ消す)。
        assert_eq!(p.buffer, "nuke");
        assert!(p.suggestions.is_empty());
    }

    #[test]
    fn complete_is_noop_after_space() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("follow ");
        p.suggestions = vec!["stale"];
        p.complete();
        // 引数領域に入ったら Tab は no-op (suggestions だけクリアする)。
        assert_eq!(p.buffer, "follow ");
        assert!(p.suggestions.is_empty());
    }

    #[test]
    fn complete_case_insensitive() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("LOC");
        p.complete();
        assert_eq!(p.buffer, "lock");
        assert!(p.suggestions.is_empty());
    }

    #[test]
    fn insert_char_clears_suggestions() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("foll");
        p.complete();
        // `foll` → lcp `follow` まで伸びる + suggestions に 3 件。
        assert_eq!(p.buffer, "follow");
        assert!(!p.suggestions.is_empty());
        p.insert_char('o');
        assert!(p.suggestions.is_empty());
        assert_eq!(p.buffer, "followo");
    }

    #[test]
    fn backspace_clears_suggestions() {
        let mut p = CommandPrompt::new();
        p.buffer.push_str("foll");
        p.complete();
        assert_eq!(p.buffer, "follow");
        assert!(!p.suggestions.is_empty());
        p.backspace();
        assert!(p.suggestions.is_empty());
        assert_eq!(p.buffer, "follo");
    }

    #[test]
    fn longest_common_prefix_basic() {
        assert_eq!(longest_common_prefix(&["foo", "foobar"]), "foo");
        assert_eq!(longest_common_prefix(&["foo", "bar"]), "");
        assert_eq!(longest_common_prefix(&["foo"]), "foo");
        assert_eq!(longest_common_prefix(&[]), "");
    }
}
