use anyhow::{Context, Result};
use serde::Serialize;
use std::time::Duration;

use crate::auth;
use crate::config;

#[cfg(not(test))]
const BASE_URL: &str = "https://www.googleapis.com/youtube/v3";
const MAX_RETRIES: u32 = 3;

pub struct Client {
    http: reqwest::Client,
    access_token: String,
    config: config::Config,
    base_url: String,
}

impl Client {
    #[cfg(not(test))]
    pub async fn new() -> Result<Self> {
        let config = config::load_config()?;
        let tokens = config::load_tokens().context("Not logged in. Run `youtube login` first.")?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            access_token: tokens.access_token,
            config,
            base_url: BASE_URL.to_string(),
        })
    }

    #[cfg(test)]
    fn for_test(base_url: String, access_token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("test client should build"),
            access_token: access_token.to_string(),
            config: config::Config::default(),
            base_url,
        }
    }

    async fn ensure_token(&mut self) -> Result<()> {
        // Try a lightweight call; if 401, refresh
        let resp = self
            .http
            .get(format!("{}/channels", self.base_url))
            .query(&[("part", "id"), ("mine", "true")])
            .bearer_auth(&self.access_token)
            .send()
            .await?;

        if resp.status() == 401 {
            let tokens = config::load_tokens()?;
            let new_tokens = auth::refresh_token(
                self.config.client_id(),
                self.config.client_secret(),
                &tokens.refresh_token,
            )
            .await?;
            self.access_token = new_tokens.access_token;
        }
        Ok(())
    }

    async fn send_with_retry<F>(&mut self, build_request: F) -> Result<serde_json::Value>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        self.ensure_token().await?;

        let mut last_error = None;
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << attempt);
                eprintln!("Retrying in {:?}...", delay);
                tokio::time::sleep(delay).await;
            }

            let resp = build_request(&self.http, &self.access_token).send().await;

            match resp {
                Ok(r) => {
                    let status = r.status();
                    let body: serde_json::Value = r.json().await?;
                    if status.is_success() {
                        return Ok(body);
                    }
                    let msg = body["error"]["message"].as_str().unwrap_or("Unknown error");
                    anyhow::bail!("API error {}: {}", status, msg);
                }
                Err(e) if e.is_timeout() => {
                    eprintln!(
                        "Request timed out (attempt {}/{})",
                        attempt + 1,
                        MAX_RETRIES
                    );
                    last_error = Some(e);
                }
                Err(e) => return Err(e).context("Request failed"),
            }
        }
        Err(last_error.unwrap()).context("Request failed after retries")
    }

    async fn get(&mut self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let url = format!("{}/{}", self.base_url, path);
        let params = params.to_vec();
        self.send_with_retry(|http, token| http.get(&url).query(&params).bearer_auth(token))
            .await
    }

    async fn post(
        &mut self,
        path: &str,
        params: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/{}", self.base_url, path);
        let params = params.to_vec();
        let body = body.clone();
        self.send_with_retry(|http, token| {
            http.post(&url)
                .query(&params)
                .bearer_auth(token)
                .json(&body)
        })
        .await
    }

    async fn delete(&mut self, path: &str, params: &[(&str, &str)]) -> Result<()> {
        self.ensure_token().await?;

        let resp = self
            .http
            .delete(format!("{}/{}", self.base_url, path))
            .query(params)
            .bearer_auth(&self.access_token)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body: serde_json::Value = resp.json().await?;
        let msg = body["error"]["message"].as_str().unwrap_or("Unknown error");
        anyhow::bail!("API error {}: {}", status, msg);
    }

    pub async fn search(
        &mut self,
        query: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> Result<SearchResult> {
        let max_str = max_results.to_string();
        let mut params = vec![
            ("part", "snippet"),
            ("type", "video"),
            ("q", query),
            ("maxResults", &max_str),
        ];
        if let Some(token) = page_token {
            params.push(("pageToken", token));
        }

        let data = self.get("search", &params).await?;
        let video_ids = extract_video_ids(&data);

        let items = if video_ids.is_empty() {
            vec![]
        } else {
            self.get_video_details(&video_ids).await?
        };

        Ok(SearchResult {
            items,
            next_page_token: data["nextPageToken"].as_str().map(String::from),
            total_results: data["pageInfo"]["totalResults"].as_u64().unwrap_or(0),
        })
    }

    pub async fn video_info(&mut self, video_id: &str) -> Result<VideoDetail> {
        let items = self.get_video_details(&[video_id.to_string()]).await?;
        items
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Video not found: {}", video_id))
    }

    async fn get_video_details(&mut self, video_ids: &[String]) -> Result<Vec<VideoDetail>> {
        let ids = video_ids.join(",");
        let data = self
            .get(
                "videos",
                &[("part", "snippet,contentDetails,statistics"), ("id", &ids)],
            )
            .await?;

        let items = data["items"]
            .as_array()
            .map(|arr| arr.iter().map(parse_video_detail).collect())
            .unwrap_or_default();
        Ok(items)
    }

    pub async fn list_playlists(&mut self, max_results: u32) -> Result<Vec<PlaylistInfo>> {
        let max_str = max_results.to_string();
        let data = self
            .get(
                "playlists",
                &[
                    ("part", "snippet,contentDetails"),
                    ("mine", "true"),
                    ("maxResults", &max_str),
                ],
            )
            .await?;

        let items = data["items"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|item| PlaylistInfo {
                        id: item["id"].as_str().unwrap_or("").to_string(),
                        title: item["snippet"]["title"].as_str().unwrap_or("").to_string(),
                        count: item["contentDetails"]["itemCount"].as_u64().unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(items)
    }

    pub async fn playlist_items(
        &mut self,
        playlist_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> Result<PlaylistItemsResult> {
        let max_str = max_results.to_string();
        let params = build_playlist_items_params(playlist_id, &max_str, page_token);
        let data = self.get("playlistItems", &params).await?;
        let items = extract_playlist_items(&data);

        Ok(PlaylistItemsResult {
            items,
            next_page_token: data["nextPageToken"].as_str().map(String::from),
            total_results: data["pageInfo"]["totalResults"].as_u64().unwrap_or(0),
        })
    }

    pub async fn playlist_add(
        &mut self,
        playlist_id: &str,
        video_ids: &[String],
    ) -> Result<Vec<String>> {
        let mut added = Vec::new();
        for vid in video_ids {
            let body = serde_json::json!({
                "snippet": {
                    "playlistId": playlist_id,
                    "resourceId": {
                        "kind": "youtube#video",
                        "videoId": vid
                    }
                }
            });
            self.post("playlistItems", &[("part", "snippet")], &body)
                .await
                .with_context(|| format!("Failed to add video {}", vid))?;
            added.push(vid.clone());
        }
        Ok(added)
    }

    pub async fn playlist_remove(&mut self, playlist_item_id: &str) -> Result<()> {
        self.delete("playlistItems", &[("id", playlist_item_id)])
            .await
    }
}

fn extract_video_ids(data: &serde_json::Value) -> Vec<String> {
    data["items"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item["id"]["videoId"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_video_detail(item: &serde_json::Value) -> VideoDetail {
    VideoDetail {
        id: item["id"].as_str().unwrap_or("").to_string(),
        title: item["snippet"]["title"].as_str().unwrap_or("").to_string(),
        channel: item["snippet"]["channelTitle"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        duration: item["contentDetails"]["duration"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        views: item["statistics"]["viewCount"]
            .as_str()
            .unwrap_or("0")
            .to_string(),
        published: item["snippet"]["publishedAt"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        description: item["snippet"]["description"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

fn build_playlist_items_params<'a>(
    playlist_id: &'a str,
    max_results: &'a str,
    page_token: Option<&'a str>,
) -> Vec<(&'static str, &'a str)> {
    let mut params = vec![
        ("part", "snippet"),
        ("playlistId", playlist_id),
        ("maxResults", max_results),
    ];
    if let Some(token) = page_token {
        params.push(("pageToken", token));
    }
    params
}

fn extract_playlist_items(data: &serde_json::Value) -> Vec<PlaylistItem> {
    data["items"]
        .as_array()
        .map(|arr| arr.iter().map(parse_playlist_item).collect())
        .unwrap_or_default()
}

fn parse_playlist_item(item: &serde_json::Value) -> PlaylistItem {
    PlaylistItem {
        playlist_item_id: item["id"].as_str().unwrap_or("").to_string(),
        video_id: item["snippet"]["resourceId"]["videoId"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        title: item["snippet"]["title"].as_str().unwrap_or("").to_string(),
        channel: item["snippet"]["videoOwnerChannelTitle"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub items: Vec<VideoDetail>,
    pub next_page_token: Option<String>,
    pub total_results: u64,
}

#[derive(Debug, Serialize)]
pub struct VideoDetail {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: String,
    pub views: String,
    pub published: String,
    pub description: String,
}

#[derive(Debug, Serialize)]
pub struct PlaylistInfo {
    pub id: String,
    pub title: String,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct PlaylistItem {
    pub playlist_item_id: String,
    pub video_id: String,
    pub title: String,
    pub channel: String,
}

#[derive(Debug)]
pub struct PlaylistItemsResult {
    pub items: Vec<PlaylistItem>,
    pub next_page_token: Option<String>,
    pub total_results: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[tokio::test]
    async fn search_fetches_details_and_preserves_pagination() {
        let server = MockYoutube::start(handler);
        let mut client = Client::for_test(server.base_url(), "token");

        let result = client.search("rust", 2, Some("page-1")).await.unwrap();

        assert_eq!(result.next_page_token.as_deref(), Some("page-2"));
        assert_eq!(result.total_results, 10);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].id, "v1");
        assert_eq!(result.items[1].title, "Second");
        let requests = server.requests();
        assert!(
            requests
                .iter()
                .any(|request| request.contains("GET /channels?part=id&mine=true"))
        );
        assert!(requests.iter().any(|request| {
            request.contains("GET /search?")
                && request.contains("q=rust")
                && request.contains("pageToken=page-1")
        }));
        assert!(
            requests
                .iter()
                .any(|request| request.contains("GET /videos?") && request.contains("id=v1%2Cv2"))
        );
    }

    #[tokio::test]
    async fn search_returns_empty_items_without_detail_call() {
        fn empty_search(line: &str, _body: &str) -> (&'static str, String) {
            if line.starts_with("GET /channels") {
                ("200 OK", "{}".to_string())
            } else if line.starts_with("GET /search") {
                (
                    "200 OK",
                    serde_json::json!({
                        "items": [],
                        "pageInfo": {"totalResults": 0}
                    })
                    .to_string(),
                )
            } else {
                ("500 Internal Server Error", error_body("unexpected"))
            }
        }
        let server = MockYoutube::start(empty_search);
        let mut client = Client::for_test(server.base_url(), "token");

        let result = client.search("none", 2, None).await.unwrap();

        assert!(result.items.is_empty());
        assert_eq!(result.total_results, 0);
        assert!(
            !server
                .requests()
                .iter()
                .any(|request| request.contains("GET /videos?"))
        );
    }

    #[tokio::test]
    async fn video_info_reports_missing_video() {
        let server = MockYoutube::start(handler);
        let mut client = Client::for_test(server.base_url(), "token");

        let err = client.video_info("missing").await.unwrap_err();

        assert!(err.to_string().contains("Video not found: missing"));
    }

    #[tokio::test]
    async fn playlists_and_items_parse_api_responses() {
        let server = MockYoutube::start(handler);
        let mut client = Client::for_test(server.base_url(), "token");

        let playlists = client.list_playlists(5).await.unwrap();
        let items = client
            .playlist_items("pl-1", 5, Some("items-page"))
            .await
            .unwrap();

        assert_eq!(playlists[0].id, "pl-1");
        assert_eq!(playlists[0].count, 2);
        assert_eq!(items.items[0].playlist_item_id, "pli-1");
        assert_eq!(items.next_page_token.as_deref(), Some("items-next"));
        assert_eq!(items.total_results, 2);
    }

    #[tokio::test]
    async fn playlist_add_and_remove_send_expected_requests() {
        let server = MockYoutube::start(handler);
        let mut client = Client::for_test(server.base_url(), "token");

        let added = client
            .playlist_add("pl-1", &["v1".to_string(), "v2".to_string()])
            .await
            .unwrap();
        client.playlist_remove("pli-1").await.unwrap();

        assert_eq!(added, vec!["v1", "v2"]);
        let requests = server.requests();
        assert!(requests.iter().any(|request| {
            request.starts_with("POST /playlistItems?part=snippet")
                && request.contains("\"playlistId\":\"pl-1\"")
                && request.contains("\"videoId\":\"v1\"")
        }));
        assert!(
            requests
                .iter()
                .any(|request| request.starts_with("DELETE /playlistItems?id=pli-1"))
        );
    }

    #[tokio::test]
    async fn api_error_uses_google_error_message() {
        fn failing(line: &str, _body: &str) -> (&'static str, String) {
            if line.starts_with("GET /channels") {
                ("200 OK", "{}".to_string())
            } else {
                ("403 Forbidden", error_body("quota exceeded"))
            }
        }
        let server = MockYoutube::start(failing);
        let mut client = Client::for_test(server.base_url(), "token");

        let err = client.list_playlists(5).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("API error 403 Forbidden: quota exceeded")
        );
    }

    fn handler(line: &str, body: &str) -> (&'static str, String) {
        if line.starts_with("GET /channels") {
            return ("200 OK", "{}".to_string());
        }
        if line.starts_with("GET /search") {
            return ("200 OK", search_response());
        }
        if line.starts_with("GET /videos") && line.contains("missing") {
            return ("200 OK", serde_json::json!({"items": []}).to_string());
        }
        if line.starts_with("GET /videos") {
            return ("200 OK", videos_response());
        }
        if line.starts_with("GET /playlists") {
            return ("200 OK", playlists_response());
        }
        if line.starts_with("GET /playlistItems") {
            return ("200 OK", playlist_items_response());
        }
        if line.starts_with("POST /playlistItems") {
            assert!(body.contains("\"kind\":\"youtube#video\""));
            return ("200 OK", "{}".to_string());
        }
        if line.starts_with("DELETE /playlistItems") {
            return ("204 No Content", String::new());
        }
        ("500 Internal Server Error", error_body("unexpected"))
    }

    fn search_response() -> String {
        serde_json::json!({
            "items": [
                {"id": {"videoId": "v1"}},
                {"id": {"videoId": "v2"}}
            ],
            "nextPageToken": "page-2",
            "pageInfo": {"totalResults": 10}
        })
        .to_string()
    }

    fn videos_response() -> String {
        serde_json::json!({
            "items": [
                video_detail("v1", "First"),
                video_detail("v2", "Second")
            ]
        })
        .to_string()
    }

    fn playlists_response() -> String {
        serde_json::json!({
            "items": [
                {"id": "pl-1", "snippet": {"title": "Playlist"}, "contentDetails": {"itemCount": 2}}
            ]
        })
        .to_string()
    }

    fn playlist_items_response() -> String {
        serde_json::json!({
            "items": [
                {
                    "id": "pli-1",
                    "snippet": {
                        "resourceId": {"videoId": "v1"},
                        "title": "First",
                        "videoOwnerChannelTitle": "Owner"
                    }
                }
            ],
            "nextPageToken": "items-next",
            "pageInfo": {"totalResults": 2}
        })
        .to_string()
    }

    fn video_detail(id: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "snippet": {
                "title": title,
                "channelTitle": "Channel",
                "publishedAt": "2026-01-02",
                "description": "Description"
            },
            "contentDetails": {"duration": "PT1M02S"},
            "statistics": {"viewCount": "123"}
        })
    }

    fn error_body(message: &str) -> String {
        serde_json::json!({"error": {"message": message}}).to_string()
    }

    struct MockYoutube {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockYoutube {
        fn start(handler: fn(&str, &str) -> (&'static str, String)) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_requests = Arc::clone(&requests);
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    handle_connection(stream, &thread_requests, handler);
                }
            });
            Self { addr, requests }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn handle_connection(
        mut stream: std::net::TcpStream,
        requests: &Arc<Mutex<Vec<String>>>,
        handler: fn(&str, &str) -> (&'static str, String),
    ) {
        let mut buffer = [0_u8; 16384];
        let bytes = stream.read(&mut buffer).unwrap();
        let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
        let request_line = request.lines().next().unwrap_or_default().to_string();
        requests.lock().unwrap().push(request.clone());
        let body = request
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let (status, response_body) = handler(&request_line, &body);
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream.write_all(response.as_bytes()).unwrap();
    }
}
