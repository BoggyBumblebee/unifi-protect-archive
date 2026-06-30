use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{
    header::{self, HeaderMap, HeaderName, HeaderValue},
    Client, RequestBuilder, Response, StatusCode,
};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::Credentials;

#[derive(Debug, Clone, Deserialize)]
pub struct Camera {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "isConnected")]
    pub is_connected: Option<bool>,
    #[serde(default, rename = "isRecording")]
    pub is_recording: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveTask {
    #[serde(rename = "fileId")]
    pub file_id: Option<String>,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveVideoRequest {
    pub start: i64,
    pub end: i64,
    pub filename: String,
    pub lens: u8,
    pub destination: String,
    #[serde(rename = "cameraId")]
    pub camera_id: String,
    #[serde(rename = "type")]
    pub archive_type: String,
    pub channel: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<u64>,
    pub host: String,
    #[serde(rename = "sharedDrive")]
    pub shared_drive: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PendingArchive {
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PendingArchiveResponse {
    #[serde(default)]
    data: Vec<PendingArchive>,
}

#[derive(Debug, Deserialize)]
struct Bootstrap {
    #[serde(default)]
    cameras: Vec<Camera>,
}

#[derive(Debug, Clone)]
pub struct ProtectClient {
    http: Client,
    base_url: Url,
    uses_api_key: bool,
    csrf_token: Arc<Mutex<Option<HeaderValue>>>,
}

impl ProtectClient {
    pub fn new(controller: &str, verify_tls: bool, api_key: Option<String>) -> Result<Self> {
        let base_url = Url::parse(controller)
            .with_context(|| format!("controller URL is not valid: {controller}"))?;
        let mut default_headers = HeaderMap::new();
        let uses_api_key = api_key.is_some();

        if let Some(api_key) = api_key {
            default_headers.insert(
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(&api_key).context("API key contains invalid header bytes")?,
            );
        }

        let http = Client::builder()
            .cookie_store(true)
            .default_headers(default_headers)
            .danger_accept_invalid_certs(!verify_tls)
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            http,
            base_url,
            uses_api_key,
            csrf_token: Arc::new(Mutex::new(None)),
        })
    }

    pub fn uses_api_key(&self) -> bool {
        self.uses_api_key
    }

    pub async fn login(&self, credentials: &Credentials) -> Result<()> {
        let url = self.url("/api/auth/login")?;
        let response = self
            .http
            .post(url)
            .header(header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "username": credentials.username,
                "password": credentials.password,
                "rememberMe": true,
            }))
            .send()
            .await
            .context("failed to send Protect login request")?;

        self.store_csrf_token(&response)?;
        ensure_success(response, "login").await?;
        Ok(())
    }

    pub async fn cameras(&self) -> Result<Vec<Camera>> {
        let url = self.url("/proxy/protect/api/bootstrap")?;
        let response = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .context("failed to request Protect bootstrap")?;

        let response = ensure_success(response, "bootstrap").await?;
        let bootstrap = response
            .json::<Bootstrap>()
            .await
            .context("failed to parse Protect bootstrap response")?;
        Ok(bootstrap.cameras)
    }

    pub async fn archive_video_to_provider(
        &self,
        request: &ArchiveVideoRequest,
    ) -> Result<ArchiveTask> {
        let url = self.url("/proxy/protect/api/cloud-provider/video-archive")?;
        let response = self
            .with_csrf(self.http.post(url))?
            .header(header::ACCEPT, "application/json")
            .json(request)
            .send()
            .await
            .context("failed to request Protect video archive")?;

        self.store_csrf_token(&response)?;
        let response = ensure_success(response, "video archive").await?;
        response
            .json::<ArchiveTask>()
            .await
            .context("failed to parse Protect video archive response")
    }

    pub async fn delete_video_range(
        &self,
        camera_id: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<()> {
        let mut url = self.url("/proxy/protect/api/video")?;
        url.query_pairs_mut()
            .append_pair("camera", camera_id)
            .append_pair("start", &start_ms.to_string())
            .append_pair("end", &end_ms.to_string());

        let response = self
            .with_csrf(self.http.delete(url))?
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .context("failed to request Protect video deletion")?;

        self.store_csrf_token(&response)?;
        ensure_success(response, "delete video range").await?;
        Ok(())
    }

    pub async fn pending_archives(&self) -> Result<Vec<PendingArchive>> {
        let url = self.url("/proxy/protect/api/video-archive/fetch-pending")?;
        let response = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .context("failed to fetch pending Protect video archives")?;

        let response = ensure_success(response, "fetch pending video archives").await?;
        let body = response
            .json::<PendingArchiveResponse>()
            .await
            .context("failed to parse pending video archives response")?;
        Ok(body.data)
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .map_err(|error| anyhow!("failed to build Protect URL {path}: {error}"))
    }

    fn store_csrf_token(&self, response: &Response) -> Result<()> {
        let Some(value) = response
            .headers()
            .get("x-csrf-token")
            .or_else(|| response.headers().get("x-updated-csrf-token"))
        else {
            return Ok(());
        };

        let mut token = self
            .csrf_token
            .lock()
            .map_err(|_| anyhow!("CSRF token lock was poisoned"))?;
        *token = Some(value.clone());
        Ok(())
    }

    fn with_csrf(&self, builder: RequestBuilder) -> Result<RequestBuilder> {
        let token = self
            .csrf_token
            .lock()
            .map_err(|_| anyhow!("CSRF token lock was poisoned"))?
            .clone();

        Ok(match token {
            Some(token) => builder.header("x-csrf-token", token),
            None => builder,
        })
    }
}

async fn ensure_success(response: Response, operation: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        bail!("{operation} was rejected by Protect; check credentials and user permissions");
    }

    let body = response.text().await.unwrap_or_default();
    if body.trim().is_empty() {
        bail!("{operation} failed with HTTP {status}");
    }

    if operation == "login" && body.contains("MFA_AUTH_REQUIRED") {
        bail!(
            "login requires MFA/SSO; use a local service account without MFA or an API key with the required site/app access"
        );
    }

    if operation == "login" {
        bail!(
            "{operation} failed with HTTP {status}: {}",
            redact_auth_body(&body)
        );
    }

    bail!("{operation} failed with HTTP {status}: {body}");
}

fn redact_auth_body(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return "<redacted auth response>".to_string();
    };

    if let Some(object) = value.as_object_mut() {
        object.remove("data");
    }

    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_tracks_whether_api_key_was_configured() {
        let without_key =
            ProtectClient::new("https://unifi-console.example.invalid", true, None).unwrap();
        let with_key = ProtectClient::new(
            "https://unifi-console.example.invalid",
            true,
            Some("test-key".to_string()),
        )
        .unwrap();

        assert!(!without_key.uses_api_key());
        assert!(with_key.uses_api_key());
    }

    #[test]
    fn client_rejects_invalid_controller_url() {
        let error = ProtectClient::new("not a url", true, None).unwrap_err();

        assert!(error.to_string().contains("controller URL is not valid"));
    }

    #[test]
    fn archive_video_request_uses_protect_field_names() {
        let request = ArchiveVideoRequest {
            start: 1,
            end: 2,
            filename: "clip.mp4".to_string(),
            lens: 0,
            destination: "NAS".to_string(),
            camera_id: "camera-1".to_string(),
            archive_type: "rotating".to_string(),
            channel: 0,
            fps: None,
            host: "nas.example.invalid".to_string(),
            shared_drive: "ProtectArchive".to_string(),
        };

        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["cameraId"], "camera-1");
        assert_eq!(json["sharedDrive"], "ProtectArchive");
        assert_eq!(json["type"], "rotating");
        assert!(json.get("fps").is_none());
    }

    #[test]
    fn bootstrap_deserializes_camera_defaults() {
        let bootstrap = serde_json::from_str::<Bootstrap>(
            r#"{
                "cameras": [
                    {
                        "id": "camera-1",
                        "name": "Front",
                        "isConnected": true
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(bootstrap.cameras.len(), 1);
        assert_eq!(bootstrap.cameras[0].id, "camera-1");
        assert_eq!(bootstrap.cameras[0].is_connected, Some(true));
        assert_eq!(bootstrap.cameras[0].is_recording, None);
    }

    #[test]
    fn pending_archive_response_defaults_missing_data() {
        let response = serde_json::from_str::<PendingArchiveResponse>("{}").unwrap();

        assert!(response.data.is_empty());
    }

    #[test]
    fn redact_auth_body_removes_sensitive_data_field() {
        let redacted = redact_auth_body(r#"{"meta":{"rc":"error"},"data":{"token":"secret"}}"#);

        assert_eq!(redacted, r#"{"meta":{"rc":"error"}}"#);
    }

    #[test]
    fn redact_auth_body_handles_non_json() {
        assert_eq!(redact_auth_body("not json"), "<redacted auth response>");
    }
}
