use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use cloud_llm_client::WebSearchResponse;
use futures::FutureExt as _;
use gpui::{App, Task};
use language_model::{LanguageModelProviderId, LanguageModelToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use util::markdown::MarkdownInlineCode;
use web_search::{WebSearchRegistry, WebSearchRequest};

/// Search the web for information using your query.
/// Use this when you need real-time information, facts, or data that might not be in your training.
/// Results will include snippets and links from relevant web pages.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WebSearchToolInput {
    /// The search term or question to query on the web.
    query: String,
    /// Why this search is being run — the underlying goal or task the results
    /// will be used for. Improves ranking against your intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<String>,
    /// Country code for geo-targeted results (e.g. `US`, `DE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    /// Language code for results (e.g. `en`, `ru`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    /// Restrict results to these domains (e.g. `["docs.rs", "github.com"]`).
    #[serde(default)]
    include_domains: Vec<String>,
    /// Exclude results from these domains.
    #[serde(default)]
    exclude_domains: Vec<String>,
    /// Type of search. One of `web` (default) or `news`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    domain_type: Option<String>,
    /// Only return results published on or after this date, in `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_date: Option<String>,
    /// Only return results published on or before this date, in `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before_date: Option<String>,
    /// Only return results from the past N minutes (1 to 5,256,000). Cannot be
    /// combined with `after_date` or `before_date`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recency_minutes: Option<u32>,
    /// Page number for pagination, starting from 0 (max 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
}

impl From<WebSearchToolInput> for WebSearchRequest {
    fn from(input: WebSearchToolInput) -> Self {
        Self {
            query: input.query,
            purpose: input.purpose,
            location: input.location,
            language: input.language,
            include_domains: input.include_domains,
            exclude_domains: input.exclude_domains,
            domain_type: input.domain_type,
            after_date: input.after_date,
            before_date: input.before_date,
            recency_minutes: input.recency_minutes,
            page: input.page,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WebSearchToolOutput {
    Success(WebSearchResponse),
    Error { error: String },
}

impl From<WebSearchToolOutput> for LanguageModelToolResultContent {
    fn from(value: WebSearchToolOutput) -> Self {
        match value {
            WebSearchToolOutput::Success(response) => serde_json::to_string(&response)
                .unwrap_or_else(|e| format!("Failed to serialize web search response: {e}"))
                .into(),
            WebSearchToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct WebSearchTool;

impl AgentTool for WebSearchTool {
    type Input = WebSearchToolInput;
    type Output = WebSearchToolOutput;

    const NAME: &'static str = "search_web";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Fetch
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Searching the Web".into()
    }

    /// Web search is backed by a provider-independent service, so the tool is
    /// available regardless of which language model is selected.
    fn supports_provider(_provider: &LanguageModelProviderId) -> bool {
        true
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| WebSearchToolOutput::Error {
                    error: e.to_string(),
                })?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.query.clone()]);
                event_stream.authorize(
                    format!("Search the web for {}", MarkdownInlineCode(&input.query)),
                    context,
                    cx,
                )
            });
            authorize
                .await
                .map_err(|e| WebSearchToolOutput::Error { error: e.to_string() })?;

            let search_task = cx.update(|cx| {
                let Some(provider) = WebSearchRegistry::read_global(cx).active_provider() else {
                    return Err(WebSearchToolOutput::Error {
                        error: "Web search is not available.".to_string(),
                    });
                };
                Ok(provider.search(input.into(), cx))
            })?;

            let response = futures::select! {
                result = search_task.fuse() => {
                    match result {
                        Ok(response) => response,
                        Err(err) => {
                            event_stream
                                .update_fields(acp::ToolCallUpdateFields::new().title("Web Search Failed"));
                            return Err(WebSearchToolOutput::Error { error: err.to_string() });
                        }
                    }
                }
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(WebSearchToolOutput::Error { error: "Web search cancelled by user".to_string() });
                }
            };

            emit_update(&response, &event_stream);
            Ok(WebSearchToolOutput::Success(response))
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        if let WebSearchToolOutput::Success(response) = &output {
            emit_update(response, &event_stream);
        }
        Ok(())
    }
}

fn emit_update(response: &WebSearchResponse, event_stream: &ToolCallEventStream) {
    let result_text = if response.results.len() == 1 {
        "1 result".to_string()
    } else {
        format!("{} results", response.results.len())
    };
    event_stream.update_fields(
        acp::ToolCallUpdateFields::new()
            .title(format!("Searched the web: {result_text}"))
            .content(
                response
                    .results
                    .iter()
                    .map(|result| {
                        acp::ToolCallContent::Content(acp::Content::new(
                            acp::ContentBlock::ResourceLink(
                                acp::ResourceLink::new(result.title.clone(), result.url.clone())
                                    .title(result.title.clone())
                                    .description(result.text.clone()),
                            ),
                        ))
                    })
                    .collect::<Vec<_>>(),
            ),
    );
}
