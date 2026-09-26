use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use cloud_llm_client::{WebSearchResponse, WebSearchResult};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Task};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use serde::Deserialize;
use serde_json::json;
use web_search::{WebSearchProvider, WebSearchProviderId, WebSearchRequest};

pub const TINYFISH_WEB_SEARCH_PROVIDER_ID: &str = "tinyfish";

const MCP_ENDPOINT: &str = "https://agent.tinyfish.ai/mcp";
const ACCESS_MODE_HEADER: &str = "X-TinyFish-Access-Mode";
const KEYLESS_ACCESS_MODE: &str = "keyless";

/// Searches the web through TinyFish's public MCP endpoint. The endpoint accepts
/// an accountless `keyless` access mode, so no API key is required; Search is
/// free either way.
pub struct TinyFishWebSearchProvider {
    http_client: Arc<dyn HttpClient>,
}

impl TinyFishWebSearchProvider {
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self { http_client }
    }
}

impl WebSearchProvider for TinyFishWebSearchProvider {
    fn id(&self) -> WebSearchProviderId {
        WebSearchProviderId(TINYFISH_WEB_SEARCH_PROVIDER_ID.into())
    }

    fn search(&self, request: WebSearchRequest, cx: &mut App) -> Task<Result<WebSearchResponse>> {
        let http_client = self.http_client.clone();
        cx.background_spawn(async move { search(http_client, request).await })
    }
}

async fn search(
    http_client: Arc<dyn HttpClient>,
    request: WebSearchRequest,
) -> Result<WebSearchResponse> {
    let arguments = build_arguments(&request)?;
    let body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": arguments,
        },
    }))
    .context("Failed to serialize the TinyFish search request")?;

    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(MCP_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(ACCESS_MODE_HEADER, KEYLESS_ACCESS_MODE)
        .body(AsyncBody::from(body))
        .context("Failed to build the TinyFish search request")?;

    let mut response = http_client
        .send(request)
        .await
        .context("Failed to reach TinyFish")?;

    let status = response.status();
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .context("Failed to read the TinyFish response")?;

    if !status.is_success() {
        bail!(
            "TinyFish search failed with status {status}: {}",
            body.trim()
        );
    }

    let response = parse_body(&body).context("Failed to parse the TinyFish response")?;

    if let Some(error) = response.error {
        bail!("TinyFish search failed: {} ({})", error.message, error.code);
    }

    let result = response
        .result
        .ok_or_else(|| anyhow!("TinyFish returned an empty response"))?;

    let text = result
        .content
        .iter()
        .find_map(|content| content.text.as_deref())
        .ok_or_else(|| anyhow!("TinyFish returned no search results"))?;

    if result.is_error {
        bail!("TinyFish search failed: {}", text.trim());
    }

    let results: SearchResults =
        serde_json::from_str(text.trim()).context("Failed to parse the TinyFish search results")?;

    Ok(WebSearchResponse {
        results: results
            .results
            .into_iter()
            .map(|result| WebSearchResult {
                title: result
                    .title
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| result.url.clone()),
                url: result.url,
                text: result.snippet.unwrap_or_default(),
            })
            .collect(),
    })
}

/// Maps a search request onto the arguments accepted by TinyFish's MCP `search`
/// tool. Unset fields are omitted so the server applies its own defaults.
fn build_arguments(request: &WebSearchRequest) -> Result<serde_json::Value> {
    if request.recency_minutes.is_some()
        && (request.after_date.is_some() || request.before_date.is_some())
    {
        bail!("`recency_minutes` cannot be combined with `after_date` or `before_date`");
    }

    let mut arguments = serde_json::Map::new();
    arguments.insert("query".into(), json!(request.query.as_str()));
    if let Some(purpose) = non_empty(request.purpose.as_deref()) {
        arguments.insert("purpose".into(), json!(purpose));
    }
    if let Some(location) = non_empty(request.location.as_deref()) {
        arguments.insert("location".into(), json!(location));
    }
    if let Some(language) = non_empty(request.language.as_deref()) {
        arguments.insert("language".into(), json!(language));
    }
    if !request.include_domains.is_empty() {
        arguments.insert(
            "include_domains".into(),
            json!(request.include_domains.join(",")),
        );
    }
    if !request.exclude_domains.is_empty() {
        arguments.insert(
            "exclude_domains".into(),
            json!(request.exclude_domains.join(",")),
        );
    }
    if let Some(domain_type) = non_empty(request.domain_type.as_deref()) {
        arguments.insert("domain_type".into(), json!(domain_type));
    }
    if let Some(after_date) = non_empty(request.after_date.as_deref()) {
        arguments.insert("after_date".into(), json!(after_date));
    }
    if let Some(before_date) = non_empty(request.before_date.as_deref()) {
        arguments.insert("before_date".into(), json!(before_date));
    }
    if let Some(recency_minutes) = request.recency_minutes {
        arguments.insert("recency_minutes".into(), json!(recency_minutes));
    }
    if let Some(page) = request.page {
        arguments.insert("page".into(), json!(page));
    }

    Ok(serde_json::Value::Object(arguments))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// The MCP endpoint answers either with a plain JSON body or an SSE stream of
/// `data: ` lines, depending on how the server negotiates the response.
fn parse_body(body: &str) -> Result<JsonRpcResponse> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
            if response.result.is_some() || response.error.is_some() {
                return Ok(response);
            }
        }
    }

    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(payload) {
            if response.result.is_some() || response.error.is_some() {
                return Ok(response);
            }
        }
    }

    bail!("TinyFish returned an unexpected response")
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<ToolResult>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct ToolResult {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct SearchResults {
    #[serde(default)]
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
}
