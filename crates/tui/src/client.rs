//! ローカル API クライアント (UDS or TCP + Bearer + JSON)。
//!
//! `server::local_api` が提供する `/api/v1/{whoami,timeline/home,notes,stream,...}`
//! を叩く。本クレートは HTTPS 経由の公開 API は扱わない。
//!
//! # 接続
//!
//! 接続方式は構築時に [`Endpoint`] で選ぶ:
//!
//! - **UDS** (`Endpoint::Unix(path)`): [`hyperlocal::UnixConnector`] +
//!   `hyper_util::client::legacy::Client`。`Host` ヘッダ相当の authority は
//!   `sakurasato.local` 固定。ホスト同居運用 (= TUI が server と同じマシン)。
//! - **TCP** (`Endpoint::Tcp(base_url)`): `HttpConnector`。Tailscale tailnet
//!   越しに別端末から TUI を動かすケース用 (#69)。
//!
//! # 認証
//!
//! すべてのリクエストに `Authorization: Bearer <token>` を付与する。UDS なら
//! socket 0o600 + Bearer の二重壁、TCP なら Bearer + tailnet ACL (= Tailscale
//! 側のアクセス制御) の二重壁。
//!
//! # エラー
//!
//! [`ApiError`] が transport / status / 1XX 系を一律に表現する。401/404 は
//! 起動時の whoami で検出して止め、TUI 本体には流さない方針。

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as UnixUri};
use serde::{Deserialize, Serialize};

/// Auth / Host のための固定 authority (UDS 時)。Unix socket の場合 hyper は
/// authority を要求するが実際には DNS 解決されないので何でもよい。
const UNIX_AUTHORITY: &str = "sakurasato.local";

/// 1 リクエスト全体のタイムアウト。TUI 内でハングしないように低めに切る。
/// SSE はこの上限を適用しない (= 別経路で長時間維持)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// レスポンス body の上限。タイムライン 1 ページ最大 80 件 × 6KiB ≒ 480KiB
/// を想定し、攻撃的な誤動作対策で 4 MiB に上限を切る。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// クライアントの接続先指定。`LocalApi::new` に渡す。
///
/// TOML の `server.local_api_listen` URI と 1 対 1 で対応するが、CLI 側で
/// 別途構築する (TUI は `--socket` または `--api-url` で受ける)。
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// Unix domain socket への接続。`PathBuf` は server 側 listener と同じパス。
    Unix(PathBuf),
    /// TCP HTTP origin への接続。`base` は `http://host:port` 形式 (末尾 `/` 無し)。
    Tcp { base: String },
}

impl Endpoint {
    /// 診断ログ / エラーメッセージ向けの表示文字列。
    pub fn display(&self) -> String {
        match self {
            Self::Unix(p) => format!("unix:{}", p.display()),
            Self::Tcp { base } => base.clone(),
        }
    }
}

/// 共有可能なローカル API クライアント。`Clone` 可で各 task が安全に使える。
#[derive(Debug, Clone)]
pub struct LocalApi {
    backend: Backend,
    endpoint: Endpoint,
    token: String,
}

/// 内部で hyper クライアントを transport 別に保持する。
#[derive(Debug, Clone)]
enum Backend {
    Unix(Client<UnixConnector, Full<Bytes>>),
    Tcp(Client<HttpConnector, Full<Bytes>>),
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("local api request timed out after {0:?}")]
    Timeout(Duration),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("HTTP {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("invalid JSON in response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid header value: {0}")]
    Header(#[from] http::header::InvalidHeaderValue),
}

impl LocalApi {
    /// 新しいクライアントを構築する。`endpoint` で UDS / TCP を選び、`token`
    /// は Bearer トークン。
    ///
    /// 空トークンは構築自体は通すが、最初の認証付きリクエストで `server::token`
    /// の SHA-256 lookup が空文字に対しても比較するため 401 を返す。CLI 側
    /// (`main.rs`) で `resolve_token` がそもそも空文字を弾くので、
    /// ここでは追加検査しない。
    pub fn new(endpoint: Endpoint, token: String) -> Self {
        let backend = match &endpoint {
            Endpoint::Unix(_) => Backend::Unix(Client::unix()),
            Endpoint::Tcp { .. } => {
                Backend::Tcp(Client::builder(TokioExecutor::new()).build_http())
            }
        };
        Self {
            backend,
            endpoint,
            token,
        }
    }

    /// 後方互換用ショートカット: UDS パスから構築。既存呼び出し元を一気に
    /// 書き換えずに済むよう残してある。
    pub fn from_socket(socket: PathBuf, token: String) -> Self {
        Self::new(Endpoint::Unix(socket), token)
    }

    /// 接続先 socket (UDS 時のみ)。診断ログ用。TCP のときは `None`。
    pub fn socket(&self) -> Option<&Path> {
        match &self.endpoint {
            Endpoint::Unix(p) => Some(p.as_path()),
            Endpoint::Tcp { .. } => None,
        }
    }

    /// 診断ログ向け表示文字列 (UDS / TCP どちらにも対応)。
    pub fn endpoint_display(&self) -> String {
        self.endpoint.display()
    }

    /// `GET /api/v1/whoami`
    pub async fn whoami(&self) -> Result<Whoami, ApiError> {
        self.get_json("/api/v1/whoami").await
    }

    /// `GET /api/v1/timeline/home`
    ///
    /// `before_ts_ms` は前ページ応答の `next_before_ts_ms` (= epoch ミリ秒)。
    /// home timeline は note + renote を `published_at` で混在ページングする。
    pub async fn timeline_home(
        &self,
        before_ts_ms: Option<i64>,
        limit: i64,
    ) -> Result<TimelineResponse, ApiError> {
        use std::fmt::Write as _;
        let mut path = format!("/api/v1/timeline/home?limit={limit}");
        if let Some(b) = before_ts_ms {
            // i64 は format 上 ASCII 安全 (= クエリエンコード不要)。
            write!(&mut path, "&before_ts_ms={b}").expect("write to String");
        }
        self.get_json(&path).await
    }

    /// `POST /api/v1/notes`
    pub async fn create_note(
        &self,
        req: &CreateNoteRequest,
    ) -> Result<CreateNoteResponse, ApiError> {
        let body = serde_json::to_vec(req)?;
        let request = self
            .request_builder(Method::POST, "/api/v1/notes")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `POST /api/v1/media?kind=...[&alt=...]` ── M7 アップロード経路。
    ///
    /// raw バイト列を送り、server 側で media-proxy サニタイズ → versitygw
    /// 格納 → DB 登録までを行う。戻り値は `media.id` 等を含む JSON。
    ///
    /// **`kind`**: `"avatar"` / `"header"` / `"attachment"` のいずれか。
    /// **`alt`**: 添付時の代替テキスト (a11y)。`None` で省略可。
    /// **`content_type`**: server 側の image/video 経路分岐に使うヒント
    /// (server は権威判定せず、実際のフォーマット検証は media-proxy が行う)。
    /// 画像は従来通り `application/octet-stream` を渡す。
    pub async fn upload_media(
        &self,
        kind: &str,
        alt: Option<&str>,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<MediaResponse, ApiError> {
        // クエリ文字列は **必ず await 前に確定** させる ── `Serializer` は
        // 内部に `Cow<'_, [u8]>` を持つため `Send` ではない。スコープを
        // 限定して `String` だけを残すよう block で囲む。
        let path = {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("kind", kind);
            if let Some(a) = alt
                && !a.is_empty()
            {
                query.append_pair("alt", a);
            }
            format!("/api/v1/media?{}", query.finish())
        };
        let request = self
            .request_builder(Method::POST, &path)?
            .header(CONTENT_TYPE, content_type)
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `PATCH /api/v1/actor/profile` ── M7 プロフィール更新。
    ///
    /// `display_name` / `summary` / `icon_media_id` / `image_media_id` を
    /// 個別に設定するか、`clear_*` フラグで明示クリアする。サーバ側で
    /// Update Activity が followers に送出される。
    pub async fn patch_profile(&self, req: &ProfileUpdate) -> Result<ProfileResponse, ApiError> {
        let body = serde_json::to_vec(req)?;
        let request = self
            .request_builder(Method::PATCH, "/api/v1/actor/profile")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `GET /api/v1/media/proxy?url=...&variant=...` ── server 経由 (=
    /// media-proxy 経由) でアバター等の画像を取得する (M6 / Issue #36)。
    ///
    /// 戻り値の `Bytes` は media-proxy が再エンコードした WebP。TUI は受信
    /// バイト列を信頼してデコード/プロトコル変換するだけで OK ──
    /// `image` crate の脆弱性が万一あっても、(1) media-proxy 側で 1 度デコード
    /// 済み、(2) WebP の単純な形に再エンコード済み、(3) サイズ上限と画素数
    /// 上限を強制済み、という多層防御が掛かっている。
    pub async fn fetch_proxy_image(&self, url: &str, variant: &str) -> Result<Bytes, ApiError> {
        // `url` の値には `?` / `&` / `=` / `%` 等が含まれうるので
        // form_urlencoded で必ず percent-encode する ── 生のまま format!
        // すると同パラメータが分解されたり 400 を貰ったりする。
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("url", url)
            .append_pair("variant", variant)
            .finish();
        let path = format!("/api/v1/media/proxy?{query}");
        let request = self
            .request_builder(Method::GET, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        let bytes = read_body_bytes(resp.into_body()).await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            return Err(ApiError::Status { status, body });
        }
        Ok(bytes)
    }

    /// `POST /api/v1/reactions` ── ローカル user が自分の Note にリアクション
    /// を付ける (M8 PR3)。`content` は `:foo:` 形式 (ローカル emoji) または
    /// Unicode emoji。失敗時は `ApiError::Status` (400/404 など) を伝播する。
    pub async fn create_reaction(
        &self,
        note_id: i64,
        content: &str,
    ) -> Result<ReactionResponse, ApiError> {
        let body = serde_json::to_vec(&CreateReactionRequest {
            note_id,
            content: content.to_string(),
        })?;
        let request = self
            .request_builder(Method::POST, "/api/v1/reactions")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `GET /api/v1/actor?acct=...` ── M13 PR4 (Issue #79)。
    ///
    /// acct (`user@host` 形式) を server 側 `webfinger_guard` + media-proxy で
    /// 解決し、actor を DB upsert した上で local actor との relationship と
    /// あわせて返す。`:follow @user@host` や Profile 画面の起動経路で使う。
    pub async fn lookup_actor_by_acct(
        &self,
        acct: &str,
    ) -> Result<ActorWithRelationship, ApiError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("acct", acct)
            .finish();
        let path = format!("/api/v1/actor?{query}");
        self.get_json(&path).await
    }

    /// `GET /api/v1/actor?ap_id=...` ── M13 PR4 (Issue #79)。
    ///
    /// AP URI 直指定で actor を DB upsert した上で relationship を返す。
    /// `WebFinger` を経由しないため、actor URI が既知の場面 (= 別経路で取得した
    /// JSON / Move target / debug、PR5 `:me` で自分の `whoami.ap_id` を直引き)
    /// 向け。
    pub async fn lookup_actor_by_ap_id(
        &self,
        ap_id: &str,
    ) -> Result<ActorWithRelationship, ApiError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("ap_id", ap_id)
            .finish();
        let path = format!("/api/v1/actor?{query}");
        self.get_json(&path).await
    }

    /// `GET /api/v1/actor/{id}` ── M13 PR4 (Issue #79)。
    ///
    /// 既知 actor を DB id で取得する高速経路。remote fetch を伴わない (= 同じ
    /// Profile を再描画する用)。
    pub async fn get_actor(&self, actor_id: i64) -> Result<ActorOnly, ApiError> {
        let path = format!("/api/v1/actor/{actor_id}");
        self.get_json(&path).await
    }

    /// `GET /api/v1/actor/{id}/relationship` ── M13 PR4 (Issue #79)。
    ///
    /// follow toggle 後に relationship だけを再取得して画面に反映する。
    pub async fn get_relationship(&self, actor_id: i64) -> Result<Relationship, ApiError> {
        let path = format!("/api/v1/actor/{actor_id}/relationship");
        self.get_json(&path).await
    }

    /// `GET /api/v1/following?limit=&before_id=` ── M13 PR5 (Issue #79)。
    ///
    /// `FollowList` screen の "following" タブが叩く。state=accepted のみ、
    /// `follow.id DESC` 順 (= 最近 follow 順)。
    pub async fn list_following(
        &self,
        before_id: Option<i64>,
        limit: i64,
    ) -> Result<FollowListResponse, ApiError> {
        use std::fmt::Write as _;
        let mut path = format!("/api/v1/following?limit={limit}");
        if let Some(b) = before_id {
            write!(&mut path, "&before_id={b}").expect("write to String");
        }
        self.get_json(&path).await
    }

    /// `GET /api/v1/followers?limit=&before_id=` ── M13 PR5 (Issue #79)。
    pub async fn list_followers(
        &self,
        before_id: Option<i64>,
        limit: i64,
    ) -> Result<FollowListResponse, ApiError> {
        use std::fmt::Write as _;
        let mut path = format!("/api/v1/followers?limit={limit}");
        if let Some(b) = before_id {
            write!(&mut path, "&before_id={b}").expect("write to String");
        }
        self.get_json(&path).await
    }

    /// `GET /api/v1/actor/{id}/notes?limit=&before_id=` ── M13 PR4 (Issue #79)。
    ///
    /// Profile 画面下部の「最近の投稿」用。visibility filter は viewer (=
    /// local actor) 視点で server 側 SQL が評価するため、TUI は素直に表示する。
    pub async fn list_actor_notes(
        &self,
        actor_id: i64,
        before_id: Option<i64>,
        limit: i64,
    ) -> Result<AuthorNotesResponse, ApiError> {
        use std::fmt::Write as _;
        let mut path = format!("/api/v1/actor/{actor_id}/notes?limit={limit}");
        if let Some(b) = before_id {
            write!(&mut path, "&before_id={b}").expect("write to String");
        }
        self.get_json(&path).await
    }

    /// `POST /api/v1/follow` ── M13 PR2 (Issue #79)。
    ///
    /// `target` で `acct` / `actor_uri` / `actor_id` のいずれか 1 つを指定する
    /// (= 排他)。`acct` と `actor_uri` は server 側で `WebFinger` host 一致検証
    /// と remote actor fetch を経由する。`actor_id` は `GET /api/v1/actor` で
    /// 既に取り込み済みの actor を素早く follow するための高速経路。
    ///
    /// 既存 `accepted` 行の再叩きは冪等 ── 200 + `already_accepted=true` で返る。
    pub async fn follow(&self, target: &FollowTarget) -> Result<FollowResponse, ApiError> {
        let body = serde_json::to_vec(target)?;
        let request = self
            .request_builder(Method::POST, "/api/v1/follow")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `DELETE /api/v1/follow/{id}` ── M13 PR2 (Issue #79)。
    ///
    /// `follow_id` (= `follow.id`、Profile relationship エンドポイントが返す
    /// `follow_state` を保持する行) に対して Undo Follow を送出し、ローカル
    /// follow 行を削除する。本人 (= local actor) が follower の行のみ削除可能
    /// (= 他人の follow を消そうとすると 403)。
    pub async fn unfollow(&self, follow_id: i64) -> Result<UnfollowResponse, ApiError> {
        let path = format!("/api/v1/follow/{follow_id}");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `POST /api/v1/block` ── ユーザーブロック PR6。body 形式は `follow` と
    /// 同じ `FollowTarget` (acct/actor_uri/actor_id の排他 3 択) を再利用する。
    pub async fn block(&self, target: &FollowTarget) -> Result<BlockResponse, ApiError> {
        let body = serde_json::to_vec(target)?;
        let request = self
            .request_builder(Method::POST, "/api/v1/block")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `DELETE /api/v1/block/{id}` ── ユーザーブロック PR6。`block_id` は
    /// `Relationship::block_id` (= `is_blocked` のときのみ `Some`) から取る。
    pub async fn unblock(&self, block_id: i64) -> Result<UnblockResponse, ApiError> {
        let path = format!("/api/v1/block/{block_id}");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `GET /api/v1/blocks` ── ブロック中の actor 一覧 (ユーザーブロック PR6)。
    pub async fn list_blocks(&self) -> Result<BlockListResponse, ApiError> {
        self.get_json("/api/v1/blocks").await
    }

    /// `GET /api/v1/domains` ── 連合ドメインブロック PR7。既知ドメイン一覧 +
    /// actor 数 + 現在の moderation state。
    pub async fn list_domains(&self) -> Result<DomainListResponse, ApiError> {
        self.get_json("/api/v1/domains").await
    }

    /// `GET /api/v1/domains/{host}` ── 統計 + moderation state +
    /// following/followers 一覧 (連合ドメインブロック PR7)。
    pub async fn domain_detail(&self, host: &str) -> Result<DomainDetailResponse, ApiError> {
        let encoded_host: String =
            url::form_urlencoded::byte_serialize(host.as_bytes()).collect();
        let path = format!("/api/v1/domains/{encoded_host}");
        self.get_json(&path).await
    }

    /// `POST /api/v1/domains/{host}/silence` ── 連合ドメインブロック PR7。
    pub async fn domain_silence(
        &self,
        host: &str,
        reason: Option<&str>,
    ) -> Result<DomainActionResponse, ApiError> {
        self.domain_moderate(host, "silence", reason).await
    }

    /// `POST /api/v1/domains/{host}/suspend` ── 連合ドメインブロック PR7。
    /// 破壊的操作 (対象ドメインの全フォロー関係を強制解除)。TUI 側は
    /// [`crate::confirm::ConfirmPrompt`] で確認を挟んでからここを呼ぶ。
    pub async fn domain_suspend(
        &self,
        host: &str,
        reason: Option<&str>,
    ) -> Result<DomainActionResponse, ApiError> {
        self.domain_moderate(host, "suspend", reason).await
    }

    async fn domain_moderate(
        &self,
        host: &str,
        action: &str,
        reason: Option<&str>,
    ) -> Result<DomainActionResponse, ApiError> {
        let encoded_host: String =
            url::form_urlencoded::byte_serialize(host.as_bytes()).collect();
        let path = format!("/api/v1/domains/{encoded_host}/{action}");
        let body = serde_json::to_vec(&DomainActionRequest {
            reason: reason.map(str::to_string),
        })?;
        let request = self
            .request_builder(Method::POST, &path)?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `DELETE /api/v1/domains/{host}` ── 措置解除 (連合ドメインブロック PR7)。
    /// server は `204 No Content` を返す。
    pub async fn domain_unset(&self, host: &str) -> Result<(), ApiError> {
        let encoded_host: String =
            url::form_urlencoded::byte_serialize(host.as_bytes()).collect();
        let path = format!("/api/v1/domains/{encoded_host}");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
    }

    /// `GET /api/v1/lists` ── リスト一覧 (Mastodon/Misskey 互換のリスト機能)。
    pub async fn list_lists(&self) -> Result<ListsResponse, ApiError> {
        self.get_json("/api/v1/lists").await
    }

    /// `POST /api/v1/lists { title }` ── リスト作成。
    pub async fn create_list(&self, title: &str) -> Result<ListSummary, ApiError> {
        let body = serde_json::to_vec(&CreateListRequest { title })?;
        let request = self
            .request_builder(Method::POST, "/api/v1/lists")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `GET /api/v1/lists/{id}` ── リスト詳細 (メンバー込み)。
    pub async fn show_list(&self, id: i64) -> Result<ListDetail, ApiError> {
        let path = format!("/api/v1/lists/{id}");
        self.get_json(&path).await
    }

    /// `PATCH /api/v1/lists/{id} { title }` ── リストリネーム。
    pub async fn rename_list(&self, id: i64, title: &str) -> Result<ListSummary, ApiError> {
        let body = serde_json::to_vec(&RenameListRequest { title })?;
        let path = format!("/api/v1/lists/{id}");
        let request = self
            .request_builder(Method::PATCH, &path)?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `DELETE /api/v1/lists/{id}` ── リスト削除。
    pub async fn delete_list(&self, id: i64) -> Result<(), ApiError> {
        let path = format!("/api/v1/lists/{id}");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
    }

    /// `POST /api/v1/lists/{id}/members { actor_id }` ── メンバー追加。
    /// 追加できるのは `follow.state = 'accepted'` の相手のみ (server 側制約)。
    pub async fn add_list_member(&self, id: i64, actor_id: i64) -> Result<(), ApiError> {
        let body = serde_json::to_vec(&AddListMemberRequest { actor_id })?;
        let path = format!("/api/v1/lists/{id}/members");
        let request = self
            .request_builder(Method::POST, &path)?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
    }

    /// `DELETE /api/v1/lists/{id}/members/{actor_id}` ── メンバー削除。
    pub async fn remove_list_member(&self, id: i64, actor_id: i64) -> Result<(), ApiError> {
        let path = format!("/api/v1/lists/{id}/members/{actor_id}");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
    }

    /// `GET /api/v1/timeline/list/{id}` ── リストタイムライン。
    /// `timeline_home` と同じ `before_ts_ms` カーソル方式。
    pub async fn timeline_list(
        &self,
        list_id: i64,
        before_ts_ms: Option<i64>,
        limit: i64,
    ) -> Result<TimelineResponse, ApiError> {
        use std::fmt::Write as _;
        let mut path = format!("/api/v1/timeline/list/{list_id}?limit={limit}");
        if let Some(b) = before_ts_ms {
            write!(&mut path, "&before_ts_ms={b}").expect("write to String");
        }
        self.get_json(&path).await
    }

    /// `GET /api/v1/emojis?prefix=...&limit=...` ── ローカル絵文字候補
    /// (Issue #101)。reaction prompt の shortcode サジェスト popup で使う。
    ///
    /// `prefix` は server 側で `is_valid_shortcode` の文字集合 (ASCII
    /// alphanumeric + `_` + `-`) に縛られるので URL エンコード不要。本クライ
    /// アント側でも事前に同集合で trim/filter してから渡すこと。
    pub async fn list_emojis(
        &self,
        prefix: &str,
        limit: i64,
    ) -> Result<EmojiListResponse, ApiError> {
        let path = format!("/api/v1/emojis?prefix={prefix}&limit={limit}");
        self.get_json(&path).await
    }

    /// `GET /api/v1/emojis?q=&limit=` ── 絵文字管理画面 Local タブの部分一致
    /// 検索。既存 [`Self::list_emojis`] (emoji サジェスト用、前方一致固定)
    /// とは別に、ユーザー入力をそのまま `q` (ILIKE 部分一致) で投げる。
    pub async fn search_local_emojis(
        &self,
        q: &str,
        limit: i64,
    ) -> Result<EmojiListResponse, ApiError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", q)
            .append_pair("limit", &limit.to_string())
            .finish();
        let path = format!("/api/v1/emojis?{query}");
        self.get_json(&path).await
    }

    /// `POST /api/v1/emojis/import` ── 絵文字管理画面からの Misskey 形式 zip
    /// アップロード。raw body で zip バイト列をそのまま送る (`upload_media`
    /// と同じ raw POST パターン)。
    pub async fn import_emoji_zip(&self, body: Vec<u8>) -> Result<EmojiImportSummary, ApiError> {
        let request = self
            .request_builder(Method::POST, "/api/v1/emojis/import")?
            .header(CONTENT_TYPE, "application/zip")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `GET /api/v1/emojis/remote?q=&limit=` ── DB にキャッシュ済みの
    /// リモート絵文字を検索する (新規に外部インスタンスへ fetch はしない)。
    pub async fn search_remote_emojis(
        &self,
        q: &str,
        limit: i64,
    ) -> Result<RemoteEmojiListResponse, ApiError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", q)
            .append_pair("limit", &limit.to_string())
            .finish();
        let path = format!("/api/v1/emojis/remote?{query}");
        self.get_json(&path).await
    }

    /// `POST /api/v1/emojis/local/from-remote` ── リモート絵文字をローカルに
    /// コピーする (shortcode はリネームせず元のまま)。
    pub async fn copy_remote_emoji_to_local(
        &self,
        remote_emoji_id: i64,
    ) -> Result<EmojiItem, ApiError> {
        let body = serde_json::to_vec(&CopyRemoteEmojiRequest { remote_emoji_id })?;
        let request = self
            .request_builder(Method::POST, "/api/v1/emojis/local/from-remote")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `POST /api/v1/actor/lock` ── 鍵アカ運用に切替 (Issue #66 / M12)。
    pub async fn actor_lock(&self) -> Result<LockResponse, ApiError> {
        self.post_json_no_body("/api/v1/actor/lock").await
    }

    /// `POST /api/v1/actor/unlock` ── 鍵アカ運用を解除。**pending follow は
    /// auto-accept されない** ── 明示的に approve/reject する必要がある。
    pub async fn actor_unlock(&self) -> Result<LockResponse, ApiError> {
        self.post_json_no_body("/api/v1/actor/unlock").await
    }

    /// `GET /api/v1/follow-requests` ── pending 一覧 (Issue #66 / M12)。
    pub async fn list_follow_requests(&self) -> Result<FollowRequestList, ApiError> {
        self.get_json("/api/v1/follow-requests").await
    }

    /// `POST /api/v1/follow-requests/{id}/approve` ── Accept 配送 + state 遷移。
    pub async fn approve_follow_request(
        &self,
        id: i64,
    ) -> Result<FollowRequestMutateResponse, ApiError> {
        self.post_json_no_body(&format!("/api/v1/follow-requests/{id}/approve"))
            .await
    }

    /// `POST /api/v1/follow-requests/{id}/reject` ── Reject 配送 + state 遷移。
    pub async fn reject_follow_request(
        &self,
        id: i64,
    ) -> Result<FollowRequestMutateResponse, ApiError> {
        self.post_json_no_body(&format!("/api/v1/follow-requests/{id}/reject"))
            .await
    }

    /// `GET /api/v1/notifications` ── in-app 通知一覧 (#206 PR3)。
    pub async fn list_notifications(&self) -> Result<NotificationsResponse, ApiError> {
        self.get_json("/api/v1/notifications").await
    }

    /// `POST /api/v1/notifications/mark-all-read` ── 全件既読化 (204 No Content)。
    pub async fn mark_all_notifications_read(&self) -> Result<(), ApiError> {
        let request = self
            .request_builder(Method::POST, "/api/v1/notifications/mark-all-read")?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
    }

    /// `DELETE /api/v1/reactions/{id}` ── 自分のリアクションを取り消す (M8 PR3)。
    pub async fn delete_reaction(&self, reaction_id: i64) -> Result<(), ApiError> {
        let path = format!("/api/v1/reactions/{reaction_id}");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
    }

    /// `POST /api/v1/notes/{id}/renote` ── 自分が **元 Note を boost / renote**
    /// する (#151)。visibility = public / unlisted の Note にのみ有効、それ以外
    /// は server が 400 で弾く。同一 Note への 2 回目以降は idempotent (= 既存
    /// announce 行を返し、`queued_deliveries = 0`)。
    pub async fn create_renote(&self, note_id: i64) -> Result<AnnounceResponse, ApiError> {
        let path = format!("/api/v1/notes/{note_id}/renote");
        let request = self
            .request_builder(Method::POST, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `DELETE /api/v1/notes/{id}/renote` ── 自分の renote を取り消し (#151)。
    /// path は **元 Note の id** (= announce 行の id ではない)。サーバ側で
    /// `(note_id, local_actor.id)` の組み合わせを引いて Undo Announce を配送
    /// する。renote していない Note への DELETE は 404。
    pub async fn delete_renote(&self, note_id: i64) -> Result<(), ApiError> {
        let path = format!("/api/v1/notes/{note_id}/renote");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
    }

    /// `GET /api/v1/stream` を生 Incoming のまま返す。SSE は呼び出し側
    /// ([`crate::sse`]) で `eventsource-stream` に流す。
    pub async fn open_stream(&self) -> Result<hyper::Response<Incoming>, ApiError> {
        let request = self
            .request_builder(Method::GET, "/api/v1/stream")?
            .header(http::header::ACCEPT, "text/event-stream")
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        // SSE は長時間維持なので REQUEST_TIMEOUT を適用しない。
        let resp = self
            .raw_request(request)
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(resp)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, ApiError> {
        let request = self
            .request_builder(Method::GET, path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// body 不要の POST → JSON。M12 の鍵アカ管理系 (`actor/lock` /
    /// `follow-requests/{id}/approve` 等) で共通利用する。
    async fn post_json_no_body<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, ApiError> {
        let request = self
            .request_builder(Method::POST, path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    fn request_builder(
        &self,
        method: Method,
        path: &str,
    ) -> Result<http::request::Builder, ApiError> {
        let (uri, host_header) = match &self.endpoint {
            Endpoint::Unix(socket) => {
                let uri: http::Uri = UnixUri::new(socket, path).into();
                (uri, UNIX_AUTHORITY.to_string())
            }
            Endpoint::Tcp { base } => {
                let raw = format!("{base}{path}");
                let uri: http::Uri = raw.parse().map_err(|e: http::uri::InvalidUri| {
                    ApiError::Transport(format!("invalid TCP URI {raw:?}: {e}"))
                })?;
                let host = uri
                    .authority()
                    .map_or_else(|| UNIX_AUTHORITY.to_string(), |a| a.as_str().to_string());
                (uri, host)
            }
        };
        let bearer = format!("Bearer {}", self.token);
        Ok(Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::HOST, host_header)
            .header(AUTHORIZATION, http::HeaderValue::from_str(&bearer)?))
    }

    async fn send(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<hyper::Response<Incoming>, ApiError> {
        let fut = self.raw_request(request);
        match tokio::time::timeout(REQUEST_TIMEOUT, fut).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(ApiError::Transport(e.to_string())),
            Err(_) => Err(ApiError::Timeout(REQUEST_TIMEOUT)),
        }
    }

    /// `Backend` 越しに raw `request` を撃つ薄いヘルパ。`send` (タイムアウト
    /// 付き) と `open_stream` (タイムアウト無し) の両方から呼ぶ。
    async fn raw_request(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<hyper::Response<Incoming>, hyper_util::client::legacy::Error> {
        match &self.backend {
            Backend::Unix(client) => client.request(request).await,
            Backend::Tcp(client) => client.request(request).await,
        }
    }
}

async fn decode_json<T: for<'de> Deserialize<'de>>(
    resp: hyper::Response<Incoming>,
) -> Result<T, ApiError> {
    let status = resp.status();
    let bytes = read_body_bytes(resp.into_body()).await?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).into_owned();
        return Err(ApiError::Status { status, body });
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn read_body_bytes(body: Incoming) -> Result<Bytes, ApiError> {
    use http_body_util::Limited;
    let limited = Limited::new(body, MAX_BODY_BYTES);
    let collected = limited
        .collect()
        .await
        .map_err(|e| ApiError::Transport(format!("read body: {e}")))?;
    Ok(collected.to_bytes())
}

async fn read_body_string(body: Incoming) -> Result<String, ApiError> {
    let bytes = read_body_bytes(body).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ── DTO 群: server::local_api の `*Response` と JSON 形を合わせる ────

#[derive(Debug, Clone, Deserialize)]
pub struct Whoami {
    pub ap_id: String,
    pub preferred_username: String,
    pub host: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    pub inbox: String,
    #[serde(default)]
    pub outbox: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelineResponse {
    pub notes: Vec<TimelineNote>,
    /// 次ページのカーソル (= 最後のエントリの並び時刻、epoch ミリ秒)。home
    /// timeline は note と renote を `published_at` で混在ページングするため、
    /// id ではなく時刻カーソルを使う。`timeline_home` にそのまま渡す。
    #[serde(default)]
    pub next_before_ts_ms: Option<i64>,
}

/// プロフィール画面の note 一覧レスポンス。home timeline と違い renote は
/// 混ざらず id 降順なので、カーソルは従来どおり `before_id` (note id)。
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorNotesResponse {
    pub notes: Vec<TimelineNote>,
    #[serde(default)]
    pub next_before_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelineNote {
    pub id: i64,
    pub ap_id: String,
    #[serde(default)]
    pub url: Option<String>,
    pub actor_id: i64,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    #[serde(default)]
    pub actor_display_name: Option<String>,
    /// 投稿主のアバター URL (M5 PR2 で server が timeline 応答に載せる)。
    /// TUI は本フィールドを使って画像 fetch をキックする。
    #[serde(default)]
    pub actor_icon_url: Option<String>,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    #[serde(default)]
    pub in_reply_to_ap_id: Option<String>,
    #[serde(default)]
    pub in_reply_to_note_id: Option<i64>,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub is_local: bool,
    /// M8 PR3: 受領したリアクション集計 (`content` 単位)。古い server (M7 以前)
    /// と通信した場合は `default` で空 Vec になる。
    #[serde(default)]
    pub reactions: Vec<ReactionSummary>,
    /// Issue #133 (4): 添付メディアの一覧。Timeline の `📎 N` バッジと
    /// 詳細モーダルのプレビューに使う。SSE / 旧 server で欠ける場合は空。
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Issue #133 (5): 本文中 `:shortcode:` に対応する custom emoji の
    /// shortcode + URL 一覧。詳細モーダルでギャラリー表示。SSE / 旧 server で
    /// 欠ける場合は空。
    #[serde(default)]
    pub emojis: Vec<Emoji>,
    /// #151: この Note が何回 boost / renote されたか。受信した Announce +
    /// 自分の renote の合算 (= `announce` テーブル全体での count)。
    #[serde(default)]
    pub announce_count: i64,
    /// #151: viewer (= ローカル actor) 自身が renote 済みか。Timeline 描画の
    /// 「↻ you renoted」マーカーに使う。
    #[serde(default)]
    pub viewer_renoted: bool,
    /// このエントリが **renote (boost) として流れてきた** 場合の renoter 情報。
    /// `Some` のとき本体フィールド (author / content …) は **元 note** を表し、
    /// 描画時に「🔁 <renoter> がリノート」ヘッダを出して元 note を描く。通常の
    /// note では `None`。
    #[serde(default)]
    pub renote: Option<RenoteMeta>,
}

/// renote として流れてきたエントリの「誰がいつ renote したか」。
/// `server::local_api::timeline::RenoteMeta` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct RenoteMeta {
    pub announce_id: i64,
    pub announce_ap_id: String,
    pub renoter_actor_id: i64,
    pub renoter_ap_id: String,
    pub renoter_preferred_username: String,
    #[serde(default)]
    pub renoter_display_name: Option<String>,
    #[serde(default)]
    pub renoter_icon_url: Option<String>,
    pub renoted_at: chrono::DateTime<chrono::Utc>,
}

/// `TimelineNote.attachments` の 1 要素。`server::local_api::timeline::AttachmentDto`
/// と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    pub url: String,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub alt: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// `TimelineNote.emojis` の 1 要素。`server::local_api::timeline::EmojiDto`
/// と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct Emoji {
    pub shortcode: String,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub is_local: Option<bool>,
}

/// `TimelineNote.reactions` の 1 要素。`server::local_api::timeline::ReactionSummaryDto`
/// と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct ReactionSummary {
    pub content: String,
    pub count: i64,
    #[serde(default)]
    pub emoji_image_url: Option<String>,
    #[serde(default)]
    pub emoji_media_type: Option<String>,
    /// `Some(true)` = local emoji、`Some(false)` = remote、`None` = Unicode。
    #[serde(default)]
    pub emoji_is_local: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateNoteRequest {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to_ap_id: Option<String>,
    /// M7: 添付メディア `media.id` の配列。空配列は省略する。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<i64>,
}

/// `POST /api/v1/media` のレスポンス。`server::local_api::media::MediaResponse`
/// と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaResponse {
    pub id: i64,
    pub storage_key: String,
    pub url: String,
    pub media_type: String,
    pub width: i32,
    pub height: i32,
    pub byte_size: i64,
    pub kind: String,
    #[serde(default)]
    pub alt_text: Option<String>,
}

/// `PATCH /api/v1/actor/profile` のリクエスト。`server::local_api::profile::ProfileUpdate`
/// と対称形。`clear_*` フラグは「明示的に NULL を書く」指示。
#[derive(Debug, Clone, Default, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "4 clear_* フラグは optional field の null 指示 ── server 側と同形を保つ"
)]
pub struct ProfileUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_display_name: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_summary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_media_id: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_icon: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_media_id: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_image: bool,
}

/// `PATCH /api/v1/actor/profile` のレスポンス。
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileResponse {
    pub ap_id: String,
    pub preferred_username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    pub queued_deliveries: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateReactionRequest {
    pub note_id: i64,
    pub content: String,
}

/// `POST /api/v1/follow` の body。3 つの指定方式は排他 ── 1 つだけ Some に
/// する。`#[serde(skip_serializing_if = "Option::is_none")]` で None を JSON
/// から落とすことで「acct と `actor_id` を同時送信して 400」を踏まない設計。
///
/// `TUI` 内では `for_actor_id` / `for_acct` のような builder で組み立てる
/// ことを推奨 ── 3 つ全部 None で送ると server 側で 400 になる。
#[derive(Debug, Clone, Serialize, Default)]
pub struct FollowTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acct: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<i64>,
}

impl FollowTarget {
    /// `GET /api/v1/actor` で取り込み済みの actor を follow する経路。
    /// remote fetch を伴わないので最速 (= TUI Profile 画面 `f` トグルの常用形)。
    #[must_use]
    pub fn for_actor_id(id: i64) -> Self {
        Self {
            actor_id: Some(id),
            ..Self::default()
        }
    }
    /// `acct` (= `user@host`) から `WebFinger` 解決 + remote fetch を経由する。
    /// `:follow @bob@example` 等のコマンドで使う。
    #[must_use]
    pub fn for_acct(acct: impl Into<String>) -> Self {
        Self {
            acct: Some(acct.into()),
            ..Self::default()
        }
    }
    /// actor URI 直接指定 (`WebFinger` をスキップ、remote fetch は通る)。
    /// 上級者向け / 障害切り分け用。
    #[must_use]
    pub fn for_actor_uri(uri: impl Into<String>) -> Self {
        Self {
            actor_uri: Some(uri.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FollowResponse {
    pub follow_id: i64,
    pub ap_id: String,
    pub state: String,
    pub target_actor_id: i64,
    pub target_ap_id: String,
    #[serde(default)]
    pub delivery_queue_id: Option<i64>,
    #[serde(default)]
    pub inbox_url: Option<String>,
    pub already_accepted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnfollowResponse {
    pub follow_id: i64,
    pub target_ap_id: String,
    pub delivery_queue_id: i64,
    pub inbox_url: String,
}

/// `POST /api/v1/block` の応答 (ユーザーブロック PR6)。
#[derive(Debug, Clone, Deserialize)]
pub struct BlockResponse {
    pub block_id: i64,
    pub ap_id: String,
    pub target_actor_id: i64,
    pub target_ap_id: String,
    pub delivery_queue_id: i64,
    pub inbox_url: String,
}

/// `DELETE /api/v1/block/{id}` の応答。
#[derive(Debug, Clone, Deserialize)]
pub struct UnblockResponse {
    pub block_id: i64,
    pub target_ap_id: String,
    pub delivery_queue_id: i64,
    pub inbox_url: String,
}

/// `GET /api/v1/blocks` の 1 件分。
#[derive(Debug, Clone, Deserialize)]
pub struct BlockListEntry {
    pub block_id: i64,
    pub block_created_at: chrono::DateTime<chrono::Utc>,
    pub actor: ActorProfile,
}

/// `GET /api/v1/blocks` のレスポンス body。
#[derive(Debug, Clone, Deserialize)]
pub struct BlockListResponse {
    pub entries: Vec<BlockListEntry>,
}

/// `GET /api/v1/domains` の 1 件分 (連合ドメインブロック PR7)。
#[derive(Debug, Clone, Deserialize)]
pub struct DomainSummary {
    pub host: String,
    pub actor_count: i64,
    #[serde(default)]
    pub severity: Option<String>,
}

/// `GET /api/v1/domains` のレスポンス body。
#[derive(Debug, Clone, Deserialize)]
pub struct DomainListResponse {
    pub domains: Vec<DomainSummary>,
}

/// `GET /api/v1/domains/{host}` の following/followers 1 件分。
#[derive(Debug, Clone, Deserialize)]
pub struct DomainFollowEntry {
    pub follow_id: i64,
    pub follow_state: String,
    pub follow_created_at: chrono::DateTime<chrono::Utc>,
    pub actor: ActorProfile,
}

/// `GET /api/v1/domains/{host}` のレスポンス body。
#[derive(Debug, Clone, Deserialize)]
pub struct DomainDetailResponse {
    pub host: String,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    pub known_actor_count: i64,
    pub accepted_following_count: i64,
    pub accepted_followers_count: i64,
    pub pending_following_count: i64,
    pub pending_followers_count: i64,
    pub following: Vec<DomainFollowEntry>,
    pub followers: Vec<DomainFollowEntry>,
}

/// `POST /api/v1/domains/{host}/silence|suspend` の body。
#[derive(Debug, Clone, Serialize, Default)]
struct DomainActionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// `POST /api/v1/domains/{host}/silence` の応答。
#[derive(Debug, Clone, Deserialize)]
pub struct DomainActionResponse {
    pub host: String,
    pub severity: String,
    /// `suspend` のときのみ、強制解除した follow 行数。`silence` は常に 0。
    #[serde(default)]
    pub forced_unfollow_count: u64,
}

/// `GET /api/v1/emojis` の各要素。
/// `server::local_api::emojis::EmojiItem` と JSON 形を合わせる。
///
/// `kind` で custom (画像 emoji) と unicode (Unicode emoji ─
/// `sakurasato_core::unicode_emoji`) を区別する。サーバが返すのは
/// 現状 `EmojiKind::Custom` のみで、`EmojiKind::Unicode` は TUI 側で
/// 静的テーブルから注入する。
#[derive(Debug, Clone, Deserialize)]
pub struct EmojiItem {
    /// `"custom"` | `"unicode"`。デフォルトは `Custom` で、古い server
    /// (= `kind` フィールド未対応) と通信した場合も画像 emoji として扱う。
    #[serde(default)]
    pub kind: EmojiKind,
    pub shortcode: String,
    /// custom emoji の画像 URL。unicode は空文字 (= 画像 fetch しない)。
    pub url: String,
    /// custom emoji の MIME type。unicode は空文字。
    pub media_type: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Unicode emoji の codepoint 文字列 (ZWJ シーケンス含む可)。`Custom`
    /// では `None`。配信時の AP `content` はこの値をそのまま流す。
    #[serde(default)]
    pub codepoint: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmojiKind {
    #[default]
    Custom,
    Unicode,
}

impl EmojiItem {
    /// AP `content` (= リアクション送信 / `:foo:` 挿入のときに使う文字列) を返す。
    /// Custom は `:shortcode:` 形式、Unicode は emoji の生 codepoint。
    ///
    /// **Invariant**: Unicode entry は必ず `codepoint = Some(_)` で構築される
    /// (build.rs 経由で gemoji 由来、`EmojiSuggestState::open` 内で組み立て、
    /// server 側は Unicode を返さない)。fallback の `shortcode.clone()` は
    /// 「`:foo:` が AP `content` に流れて Misskey / Mastodon 非互換」になる
    /// 経路なので、debug build では `debug_assert!` で早期検出する
    /// ([review #122] minor 2 対応)。release build では fallback を実行して
    /// **panic はしない** ── リアクション 1 件のために TUI を落とすより、
    /// 相手側の reaction parser に拒否させた方が被害が小さい。
    #[must_use]
    pub fn content_token(&self) -> String {
        match self.kind {
            EmojiKind::Custom => format!(":{}:", self.shortcode),
            EmojiKind::Unicode => {
                debug_assert!(
                    self.codepoint.is_some(),
                    "Unicode EmojiItem must have codepoint (shortcode={})",
                    self.shortcode,
                );
                self.codepoint
                    .clone()
                    .unwrap_or_else(|| self.shortcode.clone())
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmojiListResponse {
    pub items: Vec<EmojiItem>,
}

/// `POST /api/v1/emojis/import` のレスポンス。
/// `server::emoji_import::ImportSummary` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct EmojiImportSummary {
    pub imported: usize,
    pub skipped_not_downloaded: usize,
    pub skipped_invalid: usize,
    pub failed: usize,
}

/// `GET /api/v1/emojis/remote` の各要素。
/// `server::local_api::emoji_admin::RemoteEmojiItem` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteEmojiItem {
    pub id: i64,
    pub shortcode: String,
    pub host: String,
    pub url: String,
    pub media_type: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteEmojiListResponse {
    pub items: Vec<RemoteEmojiItem>,
}

#[derive(Debug, Serialize)]
struct CopyRemoteEmojiRequest {
    remote_emoji_id: i64,
}

/// `POST /api/v1/actor/{lock,unlock}` のレスポンス。
/// `server::local_api::actor_admin::LockResponse` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct LockResponse {
    pub ap_id: String,
    pub manually_approves_followers: bool,
    pub queued_deliveries: usize,
    pub changed: bool,
    pub enqueue_failures: usize,
}

/// `GET /api/v1/follow-requests` の各要素。
/// `server::local_api::follow_request::PendingFollow` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct PendingFollow {
    pub id: i64,
    pub ap_id: String,
    pub follower_ap_id: String,
    /// 表示用 acct: local なら `user`、remote なら `user@host`。
    /// フィールド追加 (フォローリクエスト情報表示) より前の server 版と組み
    /// 合わせても一覧が deserialize 失敗しないよう `#[serde(default)]`。
    /// (TUI は ghcr 発行対象外でローカルビルドのため、server とバージョンが
    /// ズレることがある — `TimelineNote.actor_icon_url` と同じ流儀)
    #[serde(default)]
    pub follower_acct: String,
    #[serde(default)]
    pub follower_display_name: Option<String>,
    /// HTML のまま。プレーン化は描画側 (`crate::content::to_plain_text`)。
    #[serde(default)]
    pub follower_summary: Option<String>,
    pub received_at: String,
    pub state: String,
}

/// `GET /api/v1/follow-requests` のレスポンス全体。
#[derive(Debug, Clone, Deserialize)]
pub struct FollowRequestList {
    pub items: Vec<PendingFollow>,
}

/// `POST /api/v1/follow-requests/{id}/{approve,reject}` のレスポンス。
#[derive(Debug, Clone, Deserialize)]
pub struct FollowRequestMutateResponse {
    pub id: i64,
    pub new_state: String,
}

/// `GET /api/v1/notifications` の各要素。
/// `server::local_api::notifications::NotificationItem` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationItem {
    pub id: i64,
    /// `reaction` / `follow` / `mention` / `direct` / `quote` / `renote` /
    /// `follow_request`。
    pub event_type: String,
    pub is_read: bool,
    pub created_at: String,
    /// 通知を起こした相手 (`user` or `user@host`)。
    pub notifier_acct: Option<String>,
    pub notifier_display_name: Option<String>,
    pub note_id: Option<i64>,
    /// 対象 note 本文の plain text プレビュー。
    pub note_preview: Option<String>,
    pub reaction: Option<String>,
}

/// `GET /api/v1/notifications` のレスポンス全体。
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationsResponse {
    pub items: Vec<NotificationItem>,
    pub unread_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReactionResponse {
    pub id: i64,
    pub ap_id: String,
    pub note_id: i64,
    pub content: String,
    #[serde(default)]
    pub emoji_id: Option<i64>,
    pub queued_deliveries: usize,
}

/// `POST /api/v1/notes/{id}/renote` の成功レスポンス (#151)。
#[derive(Debug, Clone, Deserialize)]
pub struct AnnounceResponse {
    /// `announce` テーブルの行 id ── 取り消しのときは TUI 側 hint として
    /// `last_renote_ids` に覚えておく。
    pub id: i64,
    /// この renote の Activity URI (= `Announce` activity の `id`)。
    pub ap_id: String,
    /// 元 Note の id。
    pub note_id: i64,
    pub queued_deliveries: usize,
}

/// `GET /api/v1/actor` のレスポンス body。
/// `server::local_api::actor::ActorWithRelationship` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct ActorWithRelationship {
    pub actor: ActorProfile,
    pub relationship: Relationship,
}

/// `GET /api/v1/actor/{id}` のレスポンス body。
#[derive(Debug, Clone, Deserialize)]
pub struct ActorOnly {
    pub actor: ActorProfile,
}

/// Profile 画面が表示する actor のサブセット。`ActorRow` のうち TUI が触る
/// フィールドだけを受け取る (= ed25519 鍵 / counts 等は触らない)。サーバ側
/// は `ActorRow` をそのまま JSON にしているので `#[serde(default)]` で未来
/// の追加フィールドを無視できる。
#[derive(Debug, Clone, Deserialize)]
pub struct ActorProfile {
    pub id: i64,
    pub ap_id: String,
    pub preferred_username: String,
    pub host: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub moved_to_ap_id: Option<String>,
    pub is_local: bool,
    pub actor_type: String,
    #[serde(default)]
    pub manually_approves_followers: bool,
}

/// `GET /api/v1/actor/{id}/relationship` の応答 + `ActorWithRelationship` 内側。
#[derive(Debug, Clone, Deserialize)]
pub struct Relationship {
    pub following: bool,
    #[serde(default)]
    pub follow_state: Option<String>,
    pub followed_by: bool,
    /// local → target の follow 行 id (`pending` / `accepted` のときだけ Some)。
    /// Profile `f` toggle で unfollow 経路を撃つときに使う ── relationship が
    /// `rejected` / 行無しのときは `None` で、その状態では unfollow ボタンが
    /// 表面に出ない (= UI 側で「現在 not following」を出す)。
    #[serde(default)]
    pub follow_id: Option<i64>,
    /// ローカル actor が target をブロックしている (ユーザーブロック PR6)。
    #[serde(default)]
    pub is_blocked: bool,
    /// local → target の `block.id`。`is_blocked` のときのみ `Some`。
    /// Profile `b` toggle で unblock 経路 (`DELETE /api/v1/block/{id}`) を
    /// 撃つときに使う (`follow_id` と同じ役割)。
    #[serde(default)]
    pub block_id: Option<i64>,
    /// target がローカル actor をブロックしている (ユーザーブロック PR6)。
    #[serde(default)]
    pub is_blocked_by: bool,
}

impl Relationship {
    /// 自己プロフィール用のニュートラル値。サーバが空 actor (= 自分) のときに
    /// 返す `Relationship` と一致する。
    #[must_use]
    pub fn neutral() -> Self {
        Self {
            following: false,
            follow_state: None,
            followed_by: false,
            follow_id: None,
            is_blocked: false,
            block_id: None,
            is_blocked_by: false,
        }
    }
}

/// `GET /api/v1/following` / `/api/v1/followers` の 1 件分。
/// `server::local_api::follow_list::FollowListEntry` と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct FollowListEntry {
    pub follow_id: i64,
    pub follow_state: String,
    pub follow_created_at: chrono::DateTime<chrono::Utc>,
    pub actor: ActorProfile,
}

/// `GET /api/v1/following` / `/api/v1/followers` のレスポンス body。
#[derive(Debug, Clone, Deserialize)]
pub struct FollowListResponse {
    pub entries: Vec<FollowListEntry>,
    #[serde(default)]
    pub next_before_id: Option<i64>,
}

/// リスト機能 (Mastodon/Misskey 互換)。`server::local_api::user_list::UserListDto`
/// と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct ListSummary {
    pub id: i64,
    pub title: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub member_count: i64,
}

/// `GET /api/v1/lists` のレスポンス body。
#[derive(Debug, Clone, Deserialize)]
pub struct ListsResponse {
    pub items: Vec<ListSummary>,
}

/// `GET /api/v1/lists/{id}` のレスポンス body (= メンバー込み)。
/// `server::local_api::user_list::UserListDetailDto` と対応。
#[derive(Debug, Clone, Deserialize)]
pub struct ListDetail {
    pub id: i64,
    pub title: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub members: Vec<ActorProfile>,
}

#[derive(Debug, Serialize)]
struct CreateListRequest<'a> {
    title: &'a str,
}

#[derive(Debug, Serialize)]
struct RenameListRequest<'a> {
    title: &'a str,
}

#[derive(Debug, Serialize)]
struct AddListMemberRequest {
    actor_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateNoteResponse {
    pub id: i64,
    pub ap_id: String,
    pub url: String,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub queued_deliveries: usize,
}

/// SSE 経由で配られるタイムラインイベント。`server::local_api::stream::TimelineEvent`
/// に対応する。`kind` フィールドで分岐する内部 tag 形式。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    NoteCreated(Box<NoteCreatedPayload>),
}

/// `note.created` の payload。`TimelineNote` とほぼ同形だが `actor_id` 等の
/// JSON 命名と order は server 側に合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct NoteCreatedPayload {
    pub id: i64,
    pub ap_id: String,
    pub actor_id: i64,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    #[serde(default)]
    pub actor_display_name: Option<String>,
    #[serde(default)]
    pub actor_icon_url: Option<String>,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    #[serde(default)]
    pub url: Option<String>,
    pub published_at: chrono::DateTime<chrono::Utc>,
}

impl NoteCreatedPayload {
    /// SSE から TUI のタイムライン Vec に挿入するための変換。`is_local` と
    /// `actor_id` / `in_reply_to_*` の有無は SSE payload には載らない (= 既知の
    /// 範囲だけ写す)。`is_local` は SSE 側で判別できないので false 既定で
    /// 受け、`whoami.ap_id` と一致したら true に上書きする運用 ([[`crate::app`]])。
    pub fn into_timeline_note(self) -> TimelineNote {
        TimelineNote {
            id: self.id,
            ap_id: self.ap_id,
            url: self.url,
            actor_id: self.actor_id,
            actor_ap_id: self.actor_ap_id,
            actor_preferred_username: self.actor_preferred_username,
            actor_display_name: self.actor_display_name,
            actor_icon_url: self.actor_icon_url,
            content: self.content,
            summary: self.summary,
            language: None,
            visibility: self.visibility,
            sensitive: self.sensitive,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            published_at: self.published_at,
            is_local: false,
            // SSE は reactions を運ばない (= 新規 Note は初期状態リアクション 0)。
            // 既存 Note へのリアクション増減は M9 で SSE 拡張する想定。
            reactions: Vec::new(),
            // SSE 経路は attachments / emojis も運ばない (= 自分が posted
            // した瞬間の新 Note は本人 TUI 既に持っているので、後続の
            // GET /timeline で正しい値が再フェッチされる)。
            attachments: Vec::new(),
            emojis: Vec::new(),
            // 新規 Note は初期状態 boost 0 / 自分も renote していない。
            announce_count: 0,
            viewer_renoted: false,
            // SSE の note.created は常に通常 note (= renote ではない)。
            renote: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_response_roundtrip() {
        let src = r#"{
            "notes": [
                {
                    "id": 7,
                    "ap_id": "https://x.test/notes/7",
                    "url": "https://x.test/notes/7",
                    "actor_id": 3,
                    "actor_ap_id": "https://x.test/users/me",
                    "actor_preferred_username": "me",
                    "actor_display_name": "Me",
                    "content": "hello",
                    "summary": null,
                    "language": "en",
                    "visibility": "public",
                    "sensitive": false,
                    "in_reply_to_ap_id": null,
                    "in_reply_to_note_id": null,
                    "published_at": "2026-05-30T12:34:56Z",
                    "is_local": true
                }
            ],
            "next_before_ts_ms": 1748608496000
        }"#;
        let parsed: TimelineResponse = serde_json::from_str(src).unwrap();
        assert_eq!(parsed.notes.len(), 1);
        assert_eq!(parsed.next_before_ts_ms, Some(1_748_608_496_000));
        assert_eq!(parsed.notes[0].content, "hello");
        // 通常 note は renote メタを持たない。
        assert!(parsed.notes[0].renote.is_none());
    }

    #[test]
    fn timeline_response_parses_renote_entry() {
        // renote として流れてきたエントリ: 本体は元 note、`renote` に renoter。
        let src = r#"{
            "notes": [
                {
                    "id": 42,
                    "ap_id": "https://x.test/notes/42",
                    "actor_id": 9,
                    "actor_ap_id": "https://remote.test/users/author",
                    "actor_preferred_username": "author",
                    "content": "boosted body",
                    "visibility": "public",
                    "sensitive": false,
                    "published_at": "2026-05-30T10:00:00Z",
                    "is_local": false,
                    "renote": {
                        "announce_id": 5,
                        "announce_ap_id": "https://remote.test/announces/5",
                        "renoter_actor_id": 3,
                        "renoter_ap_id": "https://x.test/users/me",
                        "renoter_preferred_username": "me",
                        "renoter_display_name": "Me",
                        "renoted_at": "2026-05-30T12:00:00Z"
                    }
                }
            ],
            "next_before_ts_ms": 1748602800000
        }"#;
        let parsed: TimelineResponse = serde_json::from_str(src).unwrap();
        let r = parsed.notes[0]
            .renote
            .as_ref()
            .expect("renote meta present");
        assert_eq!(r.announce_id, 5);
        assert_eq!(r.renoter_preferred_username, "me");
        // 本体は元 note (= author の投稿)。
        assert_eq!(parsed.notes[0].content, "boosted body");
        assert_eq!(parsed.notes[0].actor_preferred_username, "author");
    }

    #[test]
    fn stream_event_note_created_deserializes() {
        let src = r#"{
            "kind": "note_created",
            "id": 11,
            "ap_id": "https://x.test/notes/11",
            "actor_id": 1,
            "actor_ap_id": "https://x.test/users/me",
            "actor_preferred_username": "me",
            "actor_display_name": null,
            "content": "stream test",
            "summary": null,
            "visibility": "public",
            "sensitive": false,
            "url": "https://x.test/notes/11",
            "published_at": "2026-05-30T12:34:56Z"
        }"#;
        let evt: StreamEvent = serde_json::from_str(src).unwrap();
        let StreamEvent::NoteCreated(payload) = evt;
        assert_eq!(payload.id, 11);
        let tn = payload.into_timeline_note();
        assert_eq!(tn.id, 11);
        assert!(!tn.is_local);
    }

    #[test]
    fn create_note_request_skips_none() {
        let req = CreateNoteRequest {
            content: "hi".into(),
            summary: None,
            visibility: Some("public".into()),
            sensitive: None,
            language: None,
            in_reply_to_ap_id: None,
            attachment_ids: Vec::new(),
        };
        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        // 設定したフィールドだけ JSON に乗る。空 Vec の attachment_ids も省略。
        assert!(obj.contains_key("content"));
        assert!(obj.contains_key("visibility"));
        assert!(!obj.contains_key("summary"));
        assert!(!obj.contains_key("sensitive"));
        assert!(!obj.contains_key("language"));
        assert!(!obj.contains_key("in_reply_to_ap_id"));
        assert!(!obj.contains_key("attachment_ids"));
    }

    #[test]
    fn create_note_request_includes_attachment_ids() {
        let req = CreateNoteRequest {
            content: "hi".into(),
            summary: None,
            visibility: None,
            sensitive: None,
            language: None,
            in_reply_to_ap_id: None,
            attachment_ids: vec![1, 2],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["attachment_ids"], serde_json::json!([1, 2]));
    }

    #[test]
    fn profile_update_serializes_only_set_fields() {
        let req = ProfileUpdate {
            display_name: Some("ありす".into()),
            ..ProfileUpdate::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(
            obj.get("display_name").and_then(|v| v.as_str()),
            Some("ありす")
        );
        assert!(!obj.contains_key("clear_display_name"));
        assert!(!obj.contains_key("summary"));
        assert!(!obj.contains_key("icon_media_id"));
    }

    #[test]
    fn profile_update_emits_clear_flags() {
        let req = ProfileUpdate {
            clear_icon: true,
            ..ProfileUpdate::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["clear_icon"], serde_json::json!(true));
        assert!(json.as_object().unwrap().get("icon_media_id").is_none());
    }
}
