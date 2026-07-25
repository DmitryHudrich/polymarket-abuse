//! Gamma REST client. Cold path only: resolving a pasted link / slug /
//! condition id into tradable markets and free-text search. Never used in the
//! hot book-streaming loop.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::state::{Category, Market};

#[derive(Debug, Deserialize)]
struct Tag {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    slug: String,
}

pub struct Gamma {
    http: reqwest::Client,
    base: String,
}

impl Gamma {
    pub fn new(base: String, timeout_secs: u64) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent("polymarket-yesno-arb/0.1")
            .build()?;
        Ok(Self { http, base })
    }

    /// Resolve arbitrary user input (Polymarket URL, market slug, event slug or
    /// condition id) into the tradable YES/NO markets it refers to.
    pub async fn resolve(&self, input: &str) -> Result<Vec<Market>> {
        let input = input.trim();
        if input.starts_with("0x") && input.len() >= 40 {
            return self.markets_by_condition(input).await;
        }

        let slugs = slug_candidates(input);
        anyhow::ensure!(!slugs.is_empty(), "could not extract a slug from input");

        for slug in &slugs {
            let markets = self.markets_by_slug(slug).await?;
            if !markets.is_empty() {
                return Ok(markets);
            }
        }
        for slug in &slugs {
            let markets = self.event_markets(slug).await?;
            if !markets.is_empty() {
                return Ok(markets);
            }
        }
        Ok(Vec::new())
    }

    /// Free-text search returning candidate markets from matching events.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Market>> {
        let url = format!(
            "{}/public-search?q={}&limit_per_type={}&events_status=active",
            self.base,
            urlencode(query),
            limit
        );
        let resp: SearchResponse = self.get_json(&url).await.context("gamma search failed")?;
        let mut out = Vec::new();
        for e in resp.events {
            for m in e.markets {
                if let Some(mk) = m.into_market() {
                    out.push(mk);
                }
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Resolve a category link/slug (e.g. `polymarket.com/iran` or `iran`) into
    /// its Polymarket tag. Returns `None` if no candidate slug maps to a tag.
    pub async fn resolve_category(&self, input: &str) -> Result<Option<Category>> {
        for slug in slug_candidates(input) {
            let url = format!("{}/tags/slug/{}", self.base, urlencode(&slug));
            if let Ok(tag) = self.get_json::<Tag>(&url).await {
                if !tag.id.is_empty() {
                    return Ok(Some(Category {
                        slug: tag.slug,
                        label: tag.label,
                        tag_id: tag.id,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// Active, order-book-enabled markets carrying `tag_id`, ordered by volume
    /// desc, above `min_volume`, up to `cap`.
    ///
    /// Gamma rejects offset pagination past ~2000 rows (HTTP 422, "offset too
    /// large"), so a very large category is capped to its highest-volume markets
    /// — exactly the ones worth arbing. A page error *after* we already have
    /// rows is treated as that cap and returns what we have; an error on the
    /// first page is a real failure and is propagated so the caller does not
    /// drop markets it merely failed to refresh.
    pub async fn markets_by_tag(
        &self,
        tag_id: &str,
        min_volume: f64,
        cap: usize,
    ) -> Result<Vec<Market>> {
        const OFFSET_LIMIT: usize = 2000;
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            let url = format!(
                "{}/markets?tag_id={}&active=true&closed=false&limit=100&offset={}&order=volumeNum&ascending=false",
                self.base, urlencode(tag_id), offset
            );
            let raw: Vec<GammaMarket> = match self.get_json(&url).await {
                Ok(r) => r,
                Err(e) if !out.is_empty() => {
                    tracing::debug!("tag {tag_id} pagination stopped at offset {offset}: {e}");
                    break;
                }
                Err(e) => return Err(e),
            };
            if raw.is_empty() {
                break;
            }
            offset += raw.len();
            for m in raw {
                if let Some(mk) = m.into_market() {
                    if mk.volume >= min_volume {
                        out.push(mk);
                    }
                }
            }
            if out.len() >= cap || offset >= OFFSET_LIMIT {
                break;
            }
        }
        out.truncate(cap);
        Ok(out)
    }

    async fn markets_by_slug(&self, slug: &str) -> Result<Vec<Market>> {
        let url = format!("{}/markets?slug={}", self.base, urlencode(slug));
        let raw: Vec<GammaMarket> = self.get_json(&url).await?;
        Ok(raw
            .into_iter()
            .filter_map(GammaMarket::into_market)
            .collect())
    }

    async fn markets_by_condition(&self, condition_id: &str) -> Result<Vec<Market>> {
        let url = format!(
            "{}/markets?condition_ids={}",
            self.base,
            urlencode(condition_id)
        );
        let raw: Vec<GammaMarket> = self.get_json(&url).await?;
        Ok(raw
            .into_iter()
            .filter_map(GammaMarket::into_market)
            .collect())
    }

    async fn event_markets(&self, slug: &str) -> Result<Vec<Market>> {
        let url = format!("{}/events?slug={}", self.base, urlencode(slug));
        let events: Vec<GammaEvent> = self.get_json(&url).await?;
        Ok(events
            .into_iter()
            .flat_map(|e| e.markets)
            .filter_map(GammaMarket::into_market)
            .collect())
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        self.http
            .get(url)
            .send()
            .await
            .context("gamma request failed")?
            .error_for_status()
            .context("gamma error status")?
            .json()
            .await
            .context("gamma decode failed")
    }
}

/// Extract slug candidates (most specific first) from a URL or bare slug.
fn slug_candidates(input: &str) -> Vec<String> {
    let path = input
        .rsplit("://")
        .next()
        .unwrap_or(input)
        .split('?')
        .next()
        .unwrap_or(input);
    let mut segs: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty() && !matches!(*s, "event" | "market" | "markets"))
        .filter(|s| !s.contains('.'))
        .map(|s| s.to_string())
        .collect();
    segs.reverse();
    segs
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    events: Vec<GammaEvent>,
}

#[derive(Debug, Deserialize)]
struct GammaEvent {
    #[serde(default)]
    markets: Vec<GammaMarket>,
}

#[derive(Debug, Deserialize)]
struct GammaMarket {
    question: Option<String>,
    slug: Option<String>,
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<String>,
    outcomes: Option<String>,
    #[serde(rename = "enableOrderBook")]
    enable_order_book: Option<bool>,
    #[serde(rename = "acceptingOrders")]
    accepting_orders: Option<bool>,
    closed: Option<bool>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    #[serde(rename = "volumeNum")]
    volume_num: Option<f64>,
}

impl GammaMarket {
    fn into_market(self) -> Option<Market> {
        // Only markets that are live and can actually take orders. This drops
        // resolved/expired markets (e.g. finished hourly BTC up/down) whose frozen
        // books would otherwise produce phantom, unexecutable arbs.
        if !self.enable_order_book.unwrap_or(false)
            || !self.accepting_orders.unwrap_or(true)
            || self.closed.unwrap_or(false)
        {
            return None;
        }
        let ids: Vec<String> = serde_json::from_str(self.clob_token_ids.as_deref()?).ok()?;
        let outcomes: Vec<String> = serde_json::from_str(self.outcomes.as_deref()?).ok()?;
        if ids.len() != 2 || outcomes.len() != 2 {
            return None;
        }
        let yes_idx = outcomes
            .iter()
            .position(|o| o.eq_ignore_ascii_case("yes"))
            .unwrap_or(0);
        let no_idx = 1 - yes_idx;
        Some(Market {
            question: self
                .question
                .or_else(|| self.slug.clone())
                .unwrap_or_default(),
            slug: self.slug.unwrap_or_default(),
            condition_id: self.condition_id.unwrap_or_default(),
            yes_token: ids[yes_idx].clone(),
            no_token: ids[no_idx].clone(),
            volume: self.volume_num.unwrap_or(0.0),
            end_date: self.end_date,
            fee_bps: None,
        })
    }
}
