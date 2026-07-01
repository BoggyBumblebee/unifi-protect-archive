use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

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

#[derive(Debug, Clone)]
pub struct EventQuery {
    pub start_ms: i64,
    pub end_ms: i64,
    pub camera_ids: Vec<String>,
    pub event_types: Vec<String>,
    pub smart_detect_types: Vec<String>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProtectEvent {
    #[serde(default, rename = "camera", alias = "cameraId")]
    pub camera_id: String,
    pub start: i64,
    #[serde(default)]
    pub end: Option<i64>,
    #[serde(default, rename = "type")]
    pub event_type: Option<String>,
    #[serde(default, rename = "smartDetectTypes")]
    pub smart_detect_types: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PendingArchiveResponse {
    #[serde(default)]
    data: Vec<PendingArchive>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EventsResponse {
    Data { data: Vec<ProtectEvent> },
    List(Vec<ProtectEvent>),
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
            .timeout(Duration::from_secs(60))
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

    pub async fn events(&self, query: &EventQuery) -> Result<Vec<ProtectEvent>> {
        let mut url = self.url("/proxy/protect/api/events")?;
        {
            let mut query_pairs = url.query_pairs_mut();
            query_pairs
                .append_pair("start", &query.start_ms.to_string())
                .append_pair("end", &query.end_ms.to_string())
                .append_pair("limit", &query.limit.to_string())
                .append_pair("offset", &query.offset.to_string())
                .append_pair("orderDirection", "ASC")
                .append_pair("withoutDescriptions", "true");

            for camera_id in &query.camera_ids {
                query_pairs.append_pair("cameras", camera_id);
            }

            for event_type in &query.event_types {
                query_pairs.append_pair("types", event_type);
            }

            for smart_detect_type in &query.smart_detect_types {
                query_pairs.append_pair("smartDetectTypes", smart_detect_type);
            }
        }

        let response = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .context("failed to fetch Protect events")?;

        let response = ensure_success(response, "fetch events").await?;
        let body = response
            .json::<EventsResponse>()
            .await
            .context("failed to parse Protect events response")?;
        Ok(match body {
            EventsResponse::Data { data } | EventsResponse::List(data) => data,
        })
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
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread::{self, JoinHandle},
        time::Duration,
    };

    use super::*;

    struct MockResponse {
        status: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
    }

    impl MockResponse {
        fn ok_json(body: &'static str) -> Self {
            Self {
                status: "200 OK",
                headers: vec![("content-type", "application/json")],
                body,
            }
        }

        fn no_content() -> Self {
            Self {
                status: "204 No Content",
                headers: Vec::new(),
                body: "",
            }
        }

        fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
            self.headers.push((name, value));
            self
        }
    }

    struct MockProtectServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl MockProtectServer {
        fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_requests = Arc::clone(&requests);
            let handle = thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let request = read_http_request(&mut stream);
                    thread_requests.lock().unwrap().push(request);
                    write_http_response(&mut stream, response);
                }
            });

            Self {
                base_url,
                requests,
                handle: Some(handle),
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MockProtectServer {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.join().unwrap();
            }
        }
    }

    fn read_http_request(stream: &mut impl Read) -> String {
        let mut data = Vec::new();
        let mut buffer = [0_u8; 1024];

        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..read]);

            if request_is_complete(&data) {
                break;
            }
        }

        String::from_utf8(data).unwrap()
    }

    fn request_is_complete(data: &[u8]) -> bool {
        let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&data[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);

        data.len() >= header_end + 4 + content_length
    }

    fn write_http_response(stream: &mut impl Write, response: MockResponse) {
        let mut raw = format!(
            "HTTP/1.1 {}\r\ncontent-length: {}\r\nconnection: close\r\n",
            response.status,
            response.body.len()
        );
        for (name, value) in response.headers {
            raw.push_str(name);
            raw.push_str(": ");
            raw.push_str(value);
            raw.push_str("\r\n");
        }
        raw.push_str("\r\n");
        raw.push_str(response.body);

        stream.write_all(raw.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

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
    fn client_rejects_api_keys_that_are_invalid_header_values() {
        let error = ProtectClient::new(
            "https://unifi-console.example.invalid",
            true,
            Some("bad\nkey".to_string()),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("API key contains invalid header bytes"));
    }

    #[test]
    fn client_builds_protect_urls_from_controller_base() {
        let client =
            ProtectClient::new("https://unifi-console.example.invalid", true, None).unwrap();

        let url = client.url("/proxy/protect/api/bootstrap").unwrap();

        assert_eq!(
            url.as_str(),
            "https://unifi-console.example.invalid/proxy/protect/api/bootstrap"
        );
    }

    #[tokio::test]
    async fn client_methods_send_expected_requests_and_parse_responses() {
        let server = MockProtectServer::start(vec![
            MockResponse::ok_json("{}").with_header("x-csrf-token", "login-token"),
            MockResponse::ok_json(
                r#"{"cameras":[{"id":"camera-1","name":"Front","isConnected":true,"isRecording":true}]}"#,
            ),
            MockResponse::ok_json(r#"{"fileId":"archive-1","filename":"clip.mp4"}"#)
                .with_header("x-updated-csrf-token", "archive-token"),
            MockResponse::no_content(),
            MockResponse::ok_json(
                r#"{"data":[{"id":"archive-1","status":"uploading","filename":"clip.mp4"}]}"#,
            ),
            MockResponse::ok_json(
                r#"{"data":[{"camera":"camera-1","start":1000,"end":2000,"type":"motion"}]}"#,
            ),
        ]);
        let client = ProtectClient::new(&server.base_url, true, None).unwrap();

        client
            .login(&Credentials {
                username: "service".to_string(),
                password: "secret".to_string(),
            })
            .await
            .unwrap();
        let cameras = client.cameras().await.unwrap();
        let task = client
            .archive_video_to_provider(&ArchiveVideoRequest {
                start: 1000,
                end: 2000,
                filename: "clip.mp4".to_string(),
                lens: 0,
                destination: "NAS".to_string(),
                camera_id: "camera-1".to_string(),
                archive_type: "rotating".to_string(),
                channel: 0,
                fps: None,
                host: "nas.example.invalid".to_string(),
                shared_drive: "ProtectArchive".to_string(),
            })
            .await
            .unwrap();
        client
            .delete_video_range("camera-1", 1000, 2000)
            .await
            .unwrap();
        let pending = client.pending_archives().await.unwrap();
        let events = client
            .events(&EventQuery {
                start_ms: 1000,
                end_ms: 2000,
                camera_ids: vec!["camera-1".to_string(), "camera-2".to_string()],
                event_types: vec!["motion".to_string()],
                smart_detect_types: vec!["person".to_string()],
                limit: 50,
                offset: 10,
            })
            .await
            .unwrap();

        assert_eq!(cameras[0].name, "Front");
        assert_eq!(task.file_id.as_deref(), Some("archive-1"));
        assert_eq!(pending[0].status.as_deref(), Some("uploading"));
        assert_eq!(events[0].event_type.as_deref(), Some("motion"));

        let requests = server.requests();
        assert!(requests[0].starts_with("POST /api/auth/login "));
        assert!(requests[0].contains(r#""username":"service""#));
        assert!(requests[1].starts_with("GET /proxy/protect/api/bootstrap "));
        assert!(requests[2].starts_with("POST /proxy/protect/api/cloud-provider/video-archive "));
        assert!(requests[2].contains("x-csrf-token: login-token"));
        assert!(requests[2].contains(r#""cameraId":"camera-1""#));
        assert!(requests[3]
            .starts_with("DELETE /proxy/protect/api/video?camera=camera-1&start=1000&end=2000 "));
        assert!(requests[3].contains("x-csrf-token: archive-token"));
        assert!(requests[4].starts_with("GET /proxy/protect/api/video-archive/fetch-pending "));
        assert!(requests[5]
            .starts_with("GET /proxy/protect/api/events?start=1000&end=2000&limit=50&offset=10"));
        assert!(requests[5].contains("cameras=camera-1"));
        assert!(requests[5].contains("cameras=camera-2"));
        assert!(requests[5].contains("types=motion"));
        assert!(requests[5].contains("smartDetectTypes=person"));
    }

    #[tokio::test]
    async fn client_reports_permission_errors_without_parsing_body() {
        let server = MockProtectServer::start(vec![MockResponse {
            status: "403 Forbidden",
            headers: vec![("content-type", "application/json")],
            body: r#"{"error":"forbidden"}"#,
        }]);
        let client = ProtectClient::new(&server.base_url, true, None).unwrap();

        let error = client.cameras().await.unwrap_err();

        assert!(error.to_string().contains("rejected by Protect"));
    }

    #[tokio::test]
    async fn login_reports_mfa_requirement() {
        let server = MockProtectServer::start(vec![MockResponse {
            status: "500 Internal Server Error",
            headers: vec![("content-type", "application/json")],
            body: r#"{"error":"MFA_AUTH_REQUIRED"}"#,
        }]);
        let client = ProtectClient::new(&server.base_url, true, None).unwrap();

        let error = client
            .login(&Credentials {
                username: "service".to_string(),
                password: "secret".to_string(),
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("login requires MFA/SSO"));
    }

    #[tokio::test]
    async fn login_redacts_failed_auth_response_body() {
        let server = MockProtectServer::start(vec![MockResponse {
            status: "500 Internal Server Error",
            headers: vec![("content-type", "application/json")],
            body: r#"{"meta":{"rc":"error"},"data":{"token":"secret"}}"#,
        }]);
        let client = ProtectClient::new(&server.base_url, true, None).unwrap();

        let error = client
            .login(&Credentials {
                username: "service".to_string(),
                password: "secret".to_string(),
            })
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("login failed with HTTP 500"));
        assert!(!message.contains("secret"));
    }

    #[tokio::test]
    async fn non_auth_errors_include_response_body() {
        let server = MockProtectServer::start(vec![MockResponse {
            status: "502 Bad Gateway",
            headers: vec![("content-type", "application/json")],
            body: r#"{"error":"bad gateway"}"#,
        }]);
        let client = ProtectClient::new(&server.base_url, true, None).unwrap();

        let error = client.pending_archives().await.unwrap_err();

        assert!(error.to_string().contains("HTTP 502 Bad Gateway"));
        assert!(error.to_string().contains("bad gateway"));
    }

    #[tokio::test]
    async fn empty_error_body_reports_status_only() {
        let server = MockProtectServer::start(vec![MockResponse {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "",
        }]);
        let client = ProtectClient::new(&server.base_url, true, None).unwrap();

        let error = client.pending_archives().await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "fetch pending video archives failed with HTTP 500 Internal Server Error"
        );
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
    fn archive_video_request_serializes_optional_fps_when_present() {
        let request = ArchiveVideoRequest {
            start: 1,
            end: 2,
            filename: "clip.mp4".to_string(),
            lens: 0,
            destination: "NAS".to_string(),
            camera_id: "camera-1".to_string(),
            archive_type: "rotating".to_string(),
            channel: 0,
            fps: Some(15),
            host: "nas.example.invalid".to_string(),
            shared_drive: "ProtectArchive".to_string(),
        };

        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["fps"], 15);
    }

    #[test]
    fn archive_task_deserializes_protect_field_names() {
        let task = serde_json::from_str::<ArchiveTask>(
            r#"{"fileId":"archive-file","filename":"clip.mp4"}"#,
        )
        .unwrap();

        assert_eq!(task.file_id.as_deref(), Some("archive-file"));
        assert_eq!(task.filename.as_deref(), Some("clip.mp4"));
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
    fn pending_archive_response_deserializes_data_items() {
        let response = serde_json::from_str::<PendingArchiveResponse>(
            r#"{"data":[{"id":"archive-1","status":"uploading","filename":"clip.mp4"}]}"#,
        )
        .unwrap();

        assert_eq!(response.data.len(), 1);
        assert_eq!(response.data[0].id, "archive-1");
        assert_eq!(response.data[0].status.as_deref(), Some("uploading"));
        assert_eq!(response.data[0].filename.as_deref(), Some("clip.mp4"));
    }

    #[test]
    fn events_response_deserializes_data_wrapper() {
        let response = serde_json::from_str::<EventsResponse>(
            r#"{
                "data": [
                    {
                        "camera": "camera-1",
                        "start": 1000,
                        "end": 2000,
                        "type": "smartDetectZone",
                        "smartDetectTypes": ["person"]
                    }
                ]
            }"#,
        )
        .unwrap();

        let EventsResponse::Data { data } = response else {
            panic!("expected wrapped events");
        };
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].camera_id, "camera-1");
        assert_eq!(data[0].event_type.as_deref(), Some("smartDetectZone"));
        assert_eq!(data[0].smart_detect_types, vec!["person"]);
    }

    #[test]
    fn events_response_deserializes_list_and_camera_id_alias() {
        let response = serde_json::from_str::<EventsResponse>(
            r#"[
                {
                    "cameraId": "camera-1",
                    "start": 1000
                }
            ]"#,
        )
        .unwrap();

        let EventsResponse::List(data) = response else {
            panic!("expected event list");
        };
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].camera_id, "camera-1");
        assert_eq!(data[0].end, None);
        assert_eq!(data[0].event_type, None);
        assert!(data[0].smart_detect_types.is_empty());
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
