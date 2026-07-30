// The REST surface (§3, §10).
//
// Only what a native client actually needs: discover the server, mint a session
// token, fetch the network roster, log out. Note the shape difference from the
// WebSocket — REST payloads are **snake_case** while WS frames are camelCase.

use std::collections::HashMap;

use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};

/// Which flavour of server we are talking to (§1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    /// Self-hosted. Has `/api/api-tokens`, `/mcp` and `/uploads/*`.
    #[default]
    Standalone,
    /// A hosted lurker.chat cell behind the control plane.
    Node,
}

/// `GET /api/config` — unauthenticated, and the first call a client should
/// make (§1).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    #[serde(default)]
    pub edition: Edition,
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub min_protocol_version: u32,
    /// Whether this instance offers voice/video calls (operator opt-in + a
    /// configured LiveKit SFU). When false, the client hides all call UI.
    #[serde(default)]
    pub voice_enabled: bool,
}

impl ServerConfig {
    /// Whether this client's protocol version is still accepted.
    ///
    /// Checking up front turns a mid-session parse disaster into a clear
    /// "update your client" at startup.
    pub fn is_compatible(&self) -> bool {
        lurker_proto::PROTOCOL_VERSION >= self.min_protocol_version
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenResponse {
    pub token: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
}

/// A row from `GET /api/networks`.
///
/// This is the only place the network's display name and host appear — the WS
/// snapshot deliberately omits them (§5.1) — so the client fetches this before
/// opening the socket, where it doubles as a token-validity check.
#[derive(Clone, Debug, Deserialize)]
pub struct NetworkRow {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub autoconnect: bool,
    /// Passwords are never returned; only whether one is set.
    #[serde(default)]
    pub has_password: bool,
    #[serde(default)]
    pub has_sasl_password: bool,
}

#[derive(Deserialize)]
struct NetworksEnvelope {
    #[serde(default)]
    networks: Vec<NetworkRow>,
}

/// One entry in the self-describing settings registry
/// (`shared/settingsRegistry.ts`). §10: build the settings UI from this rather
/// than hardcoding keys — the server validates writes against the same data.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingOption {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub description: String,
    /// `string | color | secret | int | bool | enum | string-list`.
    #[serde(default, rename = "type")]
    pub setting_type: String,
    #[serde(default)]
    pub default: serde_json::Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub choices: Option<Vec<String>>,
    #[serde(default)]
    pub choice_labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub self_hosted_only: bool,
}

/// `GET /api/settings/bootstrap` → the registry plus this account's values.
#[derive(Debug, Default)]
pub struct SettingsBootstrap {
    pub registry: Vec<SettingOption>,
    pub values: HashMap<String, serde_json::Value>,
}

/// Reply to `POST /api/uploads`.
#[derive(Clone, Debug, Deserialize)]
pub struct UploadResponse {
    pub url: String,
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

/// Reply to `POST /api/voice/token`. Everything the client needs to join a
/// LiveKit room: the connection URL, the room name (server-derived), and the
/// short-lived room-scoped access token.
#[derive(Clone, Debug, Deserialize)]
pub struct VoiceToken {
    pub url: String,
    pub room: String,
    pub token: String,
}

/// Reply to `GET`/`PUT /api/voice/policy`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoicePolicy {
    /// `none` | `voice` | `halfop` | `op`. Unknown values are the server's
    /// problem — it normalizes to `none`.
    #[serde(default)]
    pub min_join_mode: String,
}

/// Percent-encode a query-string value. Channel targets start with `#`, which
/// would otherwise be read as a URL fragment and silently truncate the target.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One active call in the `GET /api/voice/presence` snapshot.
#[derive(Clone, Debug, Deserialize)]
pub struct VoiceCall {
    pub target: String,
    #[serde(default)]
    pub count: u32,
}

#[derive(Deserialize)]
struct VoicePresenceEnvelope {
    #[serde(default)]
    calls: Vec<VoiceCall>,
}

#[derive(Deserialize)]
struct BootstrapEnvelope {
    #[serde(default)]
    registry: Vec<serde_json::Value>,
    #[serde(default)]
    values: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ValuesEnvelope {
    #[serde(default)]
    values: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    error: Option<String>,
}

/// A REST client bound to one Lurker instance.
#[derive(Clone)]
pub struct Rest {
    base: Url,
    http: reqwest::Client,
    token: Option<String>,
}

impl Rest {
    pub fn new(base: Url) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("lurker-desktop/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { base, http, token: None })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base.join(path).map_err(|_| Error::BadBaseUrl)
    }

    /// Turn a response into an error that distinguishes the cases §3.4 says to
    /// treat differently.
    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        // §3.4: any 401 means *dead session* — the server deliberately never
        // uses 401 for downstream failures (upload provider errors are
        // 502/400), so this can be trusted to mean "clear the token".
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            return Err(Error::RateLimited { retry_after });
        }
        let body = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<ErrorEnvelope>(&body)
            .ok()
            .and_then(|e| e.error)
            .unwrap_or_else(|| format!("HTTP {status}"));
        Err(Error::Api { status: status.as_u16(), message })
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// `GET /api/config` — no auth. Discover edition and protocol range.
    pub async fn config(&self) -> Result<ServerConfig> {
        let resp = self.http.get(self.url("api/config")?).send().await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// `POST /api/auth/login/token` — mint a 30-day session token (§3.1).
    ///
    /// Mint is **password-only**: a passkey-only account cannot mint a native
    /// token until it sets a password, and this endpoint just returns 401 in
    /// that case — so surface [`Error::Unauthorized`] as "wrong credentials, or
    /// this account has no password set" rather than only the former.
    ///
    /// There is no refresh token; re-login to renew.
    pub async fn login_token(&self, username: &str, password: &str) -> Result<TokenResponse> {
        let resp = self
            .http
            .post(self.url("api/auth/login/token")?)
            .json(&serde_json::json!({ "username": username, "password": password }))
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// `GET /api/networks` — the roster, and a token-validity check.
    pub async fn networks(&self) -> Result<Vec<NetworkRow>> {
        let resp = self.authed(self.http.get(self.url("api/networks")?)).send().await?;
        let env: NetworksEnvelope = Self::check(resp).await?.json().await?;
        Ok(env.networks)
    }

    /// `GET /api/settings/bootstrap` — registry + values.
    ///
    /// Registry entries are salvaged row-by-row (the `alt`/`speakers` lesson):
    /// one odd future entry must cost that entry, not the whole settings UI.
    pub async fn settings_bootstrap(&self) -> Result<SettingsBootstrap> {
        let resp =
            self.authed(self.http.get(self.url("api/settings/bootstrap")?)).send().await?;
        let env: BootstrapEnvelope = Self::check(resp).await?.json().await?;
        let registry = env
            .registry
            .into_iter()
            .filter_map(|value| match serde_json::from_value::<SettingOption>(value.clone()) {
                Ok(opt) if !opt.key.is_empty() => Some(opt),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "dropping one undecodable registry entry");
                    None
                }
            })
            .collect();
        Ok(SettingsBootstrap { registry, values: env.values })
    }

    /// `PATCH /api/settings` with `{changes}` → the authoritative values.
    /// Other devices learn via the `settings` WS frame.
    pub async fn patch_settings(
        &self,
        changes: HashMap<String, serde_json::Value>,
    ) -> Result<HashMap<String, serde_json::Value>> {
        let resp = self
            .authed(self.http.patch(self.url("api/settings")?))
            .json(&serde_json::json!({ "changes": changes }))
            .send()
            .await?;
        let env: ValuesEnvelope = Self::check(resp).await?.json().await?;
        Ok(env.values)
    }

    /// `DELETE /api/settings/:key` — reset one setting to its default.
    pub async fn reset_setting(
        &self,
        key: &str,
    ) -> Result<HashMap<String, serde_json::Value>> {
        let resp = self
            .authed(self.http.delete(self.url(&format!("api/settings/{key}"))?))
            .send()
            .await?;
        let env: ValuesEnvelope = Self::check(resp).await?.json().await?;
        Ok(env.values)
    }

    /// `POST /api/uploads` — multipart upload; the file field is named
    /// **`image`** (exact field names matter, §10) though it accepts any file
    /// kind the instance's uploader allows. Returns the public URL to paste.
    pub async fn upload(
        &self,
        filename: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<UploadResponse> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|_| Error::Api { status: 0, message: "bad mime type".into() })?;
        let form = reqwest::multipart::Form::new().part("image", part);
        let resp = self
            .authed(self.http.post(self.url("api/uploads")?))
            .multipart(form)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// `POST /api/voice/token` — mint a LiveKit access token for a call in
    /// `target` (a channel like `#dev` or a nick for a DM) on `network_id`. The
    /// server derives the room name and checks ownership/membership; the client
    /// only ever holds the returned token, never the LiveKit API secret.
    pub async fn voice_token(&self, network_id: i64, target: &str) -> Result<VoiceToken> {
        let resp = self
            .authed(self.http.post(self.url("api/voice/token")?))
            .json(&serde_json::json!({ "networkId": network_id, "target": target }))
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// `GET /api/voice/presence?networkId=` — snapshot of which channels on a
    /// network currently have an active voice call, for hydrating presence on
    /// (re)connect (Lurker #680). The live `call-presence` frame carries deltas
    /// only, so a client attaching mid-call needs this to see the badge.
    pub async fn voice_presence(&self, network_id: i64) -> Result<Vec<VoiceCall>> {
        let url = self.url(&format!("api/voice/presence?networkId={network_id}"))?;
        let resp = self.authed(self.http.get(url)).send().await?;
        let env: VoicePresenceEnvelope = Self::check(resp).await?.json().await?;
        Ok(env.calls)
    }

    /// `POST /api/voice/moderate` — mute or remove a participant from a call
    /// (Lurker #680). Server-gated on channel op status (`q`/`a`/`o`/`h`); the
    /// client gates the *menu* on the same rule so a rejected action is rare.
    /// `identity` is the participant's LiveKit identity — their IRC nick.
    pub async fn voice_moderate(
        &self,
        network_id: i64,
        target: &str,
        identity: &str,
        action: &str,
    ) -> Result<()> {
        let resp = self
            .authed(self.http.post(self.url("api/voice/moderate")?))
            .json(&serde_json::json!({
                "networkId": network_id,
                "target": target,
                "identity": identity,
                "action": action,
            }))
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }

    /// `GET /api/voice/policy` — the minimum channel status required to join
    /// this channel's call (`none`/`voice`/`halfop`/`op`). Readable by any member.
    pub async fn voice_policy(&self, network_id: i64, target: &str) -> Result<String> {
        let url = self.url(&format!(
            "api/voice/policy?networkId={network_id}&target={}",
            urlencode(target)
        ))?;
        let resp = self.authed(self.http.get(url)).send().await?;
        let env: VoicePolicy = Self::check(resp).await?.json().await?;
        Ok(env.min_join_mode)
    }

    /// `PUT /api/voice/policy` — set that minimum. Ops only (`q`/`a`/`o`).
    pub async fn set_voice_policy(
        &self,
        network_id: i64,
        target: &str,
        min_join_mode: &str,
    ) -> Result<String> {
        let resp = self
            .authed(self.http.put(self.url("api/voice/policy")?))
            .json(&serde_json::json!({
                "networkId": network_id,
                "target": target,
                "minJoinMode": min_join_mode,
            }))
            .send()
            .await?;
        let env: VoicePolicy = Self::check(resp).await?.json().await?;
        Ok(env.min_join_mode)
    }

    /// `POST /api/auth/logout` — deletes this session row (per-device revoke on
    /// standalone).
    pub async fn logout(&self) -> Result<()> {
        let resp = self.authed(self.http.post(self.url("api/auth/logout")?)).send().await?;
        Self::check(resp).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_the_channel_sigil() {
        // A bare `#` in a query string is a fragment delimiter — unescaped,
        // `?target=#dev` sends an EMPTY target and the request 400s.
        assert_eq!(urlencode("#dev"), "%23dev");
        assert_eq!(urlencode("#foo[bar]"), "%23foo%5Bbar%5D");
        assert_eq!(urlencode("plain-nick_1.x~"), "plain-nick_1.x~", "unreserved kept as-is");
    }

    #[test]
    fn parses_camel_case_config() {
        // The REST surface is MIXED: /api/config and the token mint reply are
        // camelCase, while /api/networks is snake_case. Verified against a live
        // v1.1.5 server. Getting this wrong is silent, because these are
        // numeric/Option fields that quietly default.
        let cfg: ServerConfig = serde_json::from_str(
            r#"{"edition":"standalone","protocolVersion":1,"minProtocolVersion":1}"#,
        )
        .unwrap();
        assert_eq!(cfg.edition, Edition::Standalone);
        assert_eq!(cfg.protocol_version, 1, "camelCase protocolVersion must parse");
        assert_eq!(cfg.min_protocol_version, 1);
    }

    #[test]
    fn network_row_is_snake_case_and_never_carries_a_password() {
        let row: NetworkRow = serde_json::from_str(
            r#"{"id":1,"name":"Libera","host":"irc.libera.chat","tls":true,
                "has_password":false,"has_sasl_password":true}"#,
        )
        .unwrap();
        assert!(row.has_sasl_password);
        assert_eq!(row.name, "Libera");
    }

    #[test]
    fn compatibility_is_checked_against_min_protocol_version() {
        let ok = ServerConfig {
            edition: Edition::Standalone,
            protocol_version: 2,
            min_protocol_version: 1,
            voice_enabled: false,
        };
        assert!(ok.is_compatible());
        let too_old = ServerConfig {
            edition: Edition::Standalone,
            protocol_version: 3,
            min_protocol_version: 3,
            voice_enabled: false,
        };
        assert!(!too_old.is_compatible());
    }
}
