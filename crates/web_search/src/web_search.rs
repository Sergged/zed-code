use std::sync::Arc;

use anyhow::Result;
use cloud_llm_client::WebSearchResponse;
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};

pub fn init(cx: &mut App) {
    let registry = cx.new(|_cx| WebSearchRegistry::default());
    cx.set_global(GlobalWebSearchRegistry(registry));
}

#[derive(Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
pub struct WebSearchProviderId(pub SharedString);

/// A web search request. Providers use the fields they support and ignore the
/// rest, so adding a parameter here does not require every provider to handle it.
#[derive(Clone, Debug, Default)]
pub struct WebSearchRequest {
    pub query: String,
    pub purpose: Option<String>,
    pub location: Option<String>,
    pub language: Option<String>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    /// Either `"web"` or `"news"`.
    pub domain_type: Option<String>,
    /// `YYYY-MM-DD`.
    pub after_date: Option<String>,
    /// `YYYY-MM-DD`.
    pub before_date: Option<String>,
    pub recency_minutes: Option<u32>,
    pub page: Option<u32>,
}

pub trait WebSearchProvider {
    fn id(&self) -> WebSearchProviderId;
    fn search(&self, request: WebSearchRequest, cx: &mut App) -> Task<Result<WebSearchResponse>>;
}

struct GlobalWebSearchRegistry(Entity<WebSearchRegistry>);

impl Global for GlobalWebSearchRegistry {}

#[derive(Default)]
pub struct WebSearchRegistry {
    providers: HashMap<WebSearchProviderId, Arc<dyn WebSearchProvider>>,
    active_provider: Option<Arc<dyn WebSearchProvider>>,
}

impl WebSearchRegistry {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalWebSearchRegistry>().0.clone()
    }

    pub fn read_global(cx: &App) -> &Self {
        cx.global::<GlobalWebSearchRegistry>().0.read(cx)
    }

    pub fn providers(&self) -> impl Iterator<Item = &Arc<dyn WebSearchProvider>> {
        self.providers.values()
    }

    pub fn active_provider(&self) -> Option<Arc<dyn WebSearchProvider>> {
        self.active_provider.clone()
    }

    pub fn set_active_provider(&mut self, provider: Arc<dyn WebSearchProvider>) {
        self.active_provider = Some(provider.clone());
        self.providers.insert(provider.id(), provider);
    }

    pub fn register_provider<T: WebSearchProvider + 'static>(
        &mut self,
        provider: T,
        _cx: &mut Context<Self>,
    ) {
        let id = provider.id();
        let provider = Arc::new(provider);
        self.providers.insert(id, provider.clone());
        if self.active_provider.is_none() {
            self.active_provider = Some(provider);
        }
    }

    pub fn unregister_provider(&mut self, id: WebSearchProviderId) {
        self.providers.remove(&id);
        if self.active_provider.as_ref().map(|provider| provider.id()) == Some(id) {
            self.active_provider = None;
        }
    }
}
