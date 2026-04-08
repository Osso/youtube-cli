use anyhow::{Context, Result};
use serde::Serialize;
use std::time::Duration;

use crate::auth;
use crate::config;

const BASE_URL: &str = "https://www.googleapis.com/youtube/v3";
const MAX_RETRIES: u32 = 3;

pub struct Client {
    http: reqwest::Client,
    access_token: String,
    config: config::Config,
}

impl Client {
    pub async fn new() -> Result<Self> {
        let config = config::load_config()?;
        let tokens = config::load_tokens().context("Not logged in. Run `youtube login` first.")?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            access_token: tokens.access_token,
            config,
        })
    }

    async fn ensure_token(&mut self) -> Result<()> {
        // Try a lightweight call; if 401, refresh
        let resp = self
            .http
            .get(format!("{}/channels", BASE_URL))
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

    async fn send_with_retry<F>(
        &mut self,
        build_request: F,
    ) -> Result<serde_json::Value>
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
        let url = format!("{}/{}", BASE_URL, path);
        let params = params.to_vec();
        self.send_with_retry(|http, token| {
            http.get(&url).query(&params).bearer_auth(token)
        })
        .await
    }

    async fn post(
        &mut self,
        path: &str,
        params: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/{}", BASE_URL, path);
        let params = params.to_vec();
        let body = body.clone();
        self.send_with_retry(|http, token| {
            http.post(&url).query(&params).bearer_auth(token).json(&body)
        })
        .await
    }

    async fn delete(&mut self, path: &str, params: &[(&str, &str)]) -> Result<()> {
        self.ensure_token().await?;

        let resp = self
            .http
            .delete(format!("{}/{}", BASE_URL, path))
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
        let mut params = vec![
            ("part", "snippet"),
            ("playlistId", playlist_id),
            ("maxResults", &max_str),
        ];
        if let Some(token) = page_token {
            params.push(("pageToken", token));
        }

        let data = self.get("playlistItems", &params).await?;

        let items = data["items"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|item| PlaylistItem {
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
                    })
                    .collect()
            })
            .unwrap_or_default();

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
