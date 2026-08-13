//! pi.dev package catalog: crawls the server-rendered index at
//! https://pi.dev/packages (50 cards/page; there is no JSON API — /api/*
//! returns "reserved for future features"), parses the package cards, and
//! caches the result in memory. The web UI's create-agent flow uses this to
//! let operators pick pi extensions/packages to deploy with an agent.
//!
//! Each package maps to a pi install source for the manifest's
//! `[[extensions]] source = "…"` form: `npm:<pkg>` when the card links to
//! npm, else `git:github.com/<owner>/<repo>` when it links to GitHub.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Serialize;
use tokio::sync::Mutex;

const BASE: &str = "https://pi.dev/packages";
/// Concurrent page fetches during a crawl.
const CRAWL_CONCURRENCY: usize = 16;
/// How long a crawled catalog stays fresh.
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
/// Don't hammer pi.dev: a failed crawl backs off before retrying.
const ERROR_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize)]
pub struct PiPackage {
    pub name: String,
    pub description: String,
    pub author: String,
    /// Downloads as rendered by pi.dev (e.g. "479.6K/mo").
    pub downloads: String,
    /// Relative update time as rendered by pi.dev (e.g. "22d ago").
    pub updated: String,
    /// Resource badges (extension, skill, prompt, theme, …).
    pub types: Vec<String>,
    /// pi install source for `[[extensions]] source = "…"`; None when the
    /// card links neither npm nor GitHub (not deployable onto an agent).
    pub source: Option<String>,
    pub repo_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CatalogPage {
    pub packages: Vec<PiPackage>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
    pub pages: usize,
    /// Seconds since the catalog was last crawled.
    pub cache_age_secs: u64,
}

struct Cache {
    packages: Vec<PiPackage>,
    fetched_at: Instant,
}

pub struct Catalog {
    cache: Mutex<Option<Cache>>,
    /// Set when the last crawl failed (cache may be empty or stale);
    /// suppresses retry storms.
    last_error: Mutex<Option<(Instant, String)>>,
}

impl Catalog {
    pub fn shared() -> &'static Catalog {
        static CATALOG: OnceLock<Catalog> = OnceLock::new();
        CATALOG.get_or_init(|| Catalog {
            cache: Mutex::new(None),
            last_error: Mutex::new(None),
        })
    }

    /// Query the catalog with substring search, badge-type filter, and
    /// 1-based pagination. Crawls pi.dev when the cache is empty or stale.
    pub async fn query(
        &self,
        q: Option<&str>,
        type_filter: Option<&str>,
        page: usize,
        per_page: usize,
    ) -> Result<CatalogPage> {
        self.ensure_fresh().await?;
        let cache = self.cache.lock().await;
        let cache = cache.as_ref().context("catalog unavailable")?;

        let q = q.map(str::to_lowercase);
        let mut filtered: Vec<&PiPackage> = cache
            .packages
            .iter()
            .filter(|p| {
                type_filter.is_none_or(|t| p.types.iter().any(|x| x == t))
                    && q.as_ref().is_none_or(|needle| {
                        p.name.to_lowercase().contains(needle)
                            || p.description.to_lowercase().contains(needle)
                            || p.author.to_lowercase().contains(needle)
                    })
            })
            .collect();
        // Most-downloaded first: parse the compact suffix ("479.6K/mo").
        filtered.sort_by_key(|p| std::cmp::Reverse(downloads_key(&p.downloads)));

        let total = filtered.len();
        let per_page = per_page.clamp(1, 200);
        let pages = total.div_ceil(per_page).max(1);
        let page = page.clamp(1, pages);
        let start = (page - 1) * per_page;
        let packages: Vec<PiPackage> = filtered
            .into_iter()
            .skip(start)
            .take(per_page)
            .cloned()
            .collect();

        Ok(CatalogPage {
            packages,
            total,
            page,
            per_page,
            pages,
            cache_age_secs: cache.fetched_at.elapsed().as_secs(),
        })
    }

    async fn ensure_fresh(&self) -> Result<()> {
        // Fast path: fresh cache.
        if let Some(c) = self.cache.lock().await.as_ref() {
            if c.fetched_at.elapsed() < CACHE_TTL {
                return Ok(());
            }
        }
        // Error backoff (only when there is no usable cache at all).
        if self.cache.lock().await.is_none() {
            if let Some((at, msg)) = self.last_error.lock().await.as_ref() {
                if at.elapsed() < ERROR_BACKOFF {
                    bail!("pi.dev catalog unavailable: {msg}");
                }
            }
        }
        match crawl().await {
            Ok(packages) => {
                *self.cache.lock().await = Some(Cache {
                    packages,
                    fetched_at: Instant::now(),
                });
                *self.last_error.lock().await = None;
                Ok(())
            }
            Err(err) => {
                let msg = format!("{err:#}");
                *self.last_error.lock().await = Some((Instant::now(), msg.clone()));
                // A stale cache beats no catalog.
                if self.cache.lock().await.is_some() {
                    tracing::warn!(error = %msg, "pi.dev recrawl failed; serving stale catalog");
                    Ok(())
                } else {
                    Err(err.context("crawling pi.dev package catalog"))
                }
            }
        }
    }
}

/// Fetch and parse the full catalog (all pages).
async fn crawl() -> Result<Vec<PiPackage>> {
    let client = reqwest::Client::builder()
        .user_agent("suzerain control plane (pi.dev catalog crawler)")
        .timeout(Duration::from_secs(20))
        .build()?;

    let first = fetch_page(&client, 1).await?;
    let (mut packages, last_page) = first;
    tracing::info!(last_page, "crawling pi.dev package catalog");

    // Remaining pages in concurrent batches.
    for chunk in (2..=last_page)
        .collect::<Vec<_>>()
        .chunks(CRAWL_CONCURRENCY)
    {
        let results =
            futures_util::future::join_all(chunk.iter().map(|&n| fetch_page(&client, n))).await;
        for (n, res) in chunk.iter().zip(results) {
            let (page_packages, _) = res.with_context(|| format!("fetching page {n}"))?;
            packages.extend(page_packages);
        }
    }
    tracing::info!(count = packages.len(), "pi.dev catalog crawl complete");
    Ok(packages)
}

/// Fetch one page; returns its packages and the highest page number seen in
/// the pagination nav (so page 1 reveals the total page count).
async fn fetch_page(client: &reqwest::Client, page: usize) -> Result<(Vec<PiPackage>, usize)> {
    let url = format!("{BASE}?page={page}");
    let html = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .text()
        .await?;
    Ok((parse_cards(&html), last_page(&html)))
}

fn last_page(html: &str) -> usize {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"/packages\?page=(\d+)"#).unwrap());
    re.captures_iter(html)
        .filter_map(|c| c[1].parse::<usize>().ok())
        .max()
        .unwrap_or(1)
}

/// Parse the `<article>` cards of the "All packages" list.
fn parse_cards(html: &str) -> Vec<PiPackage> {
    static ARTICLE: OnceLock<Regex> = OnceLock::new();
    static NAME: OnceLock<Regex> = OnceLock::new();
    static DESC: OnceLock<Regex> = OnceLock::new();
    static META: OnceLock<Regex> = OnceLock::new();
    static SPAN: OnceLock<Regex> = OnceLock::new();
    static BADGE: OnceLock<Regex> = OnceLock::new();
    static NPM: OnceLock<Regex> = OnceLock::new();
    static GITHUB: OnceLock<Regex> = OnceLock::new();

    let article = ARTICLE.get_or_init(|| Regex::new(r"(?s)<article[^>]*>(.*?)</article>").unwrap());
    let name = NAME.get_or_init(|| {
        Regex::new(r#"(?s)<h3 class="packages-name"><a[^>]*>([^<]+)</a>"#).unwrap()
    });
    let desc =
        DESC.get_or_init(|| Regex::new(r#"(?s)<p class="packages-desc">(.*?)</p>"#).unwrap());
    let meta =
        META.get_or_init(|| Regex::new(r#"(?s)<div class="packages-meta">(.*?)</div>"#).unwrap());
    let span = SPAN.get_or_init(|| Regex::new(r"(?s)<span[^>]*>(.*?)</span>").unwrap());
    let badge = BADGE.get_or_init(|| Regex::new(r#"data-type="([^"]+)""#).unwrap());
    let npm =
        NPM.get_or_init(|| Regex::new(r#"https://www\.npmjs\.com/package/([^"<>\s]+)"#).unwrap());
    let github = GITHUB.get_or_init(|| {
        Regex::new(r#"https://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)"#).unwrap()
    });

    article
        .captures_iter(html)
        .filter_map(|card| {
            let card = &card[1];
            let pkg_name = decode_entities(name.captures(card)?[1].trim());

            let description = desc
                .captures(card)
                .map(|c| decode_entities(strip_tags(&c[1]).trim()))
                .unwrap_or_default();

            let meta_spans: Vec<String> = meta
                .captures(card)
                .map(|m| {
                    span.captures_iter(&m[1])
                        .map(|s| decode_entities(strip_tags(&s[1]).trim()))
                        .collect()
                })
                .unwrap_or_default();
            let author = meta_spans.first().cloned().unwrap_or_default();
            let downloads = meta_spans.get(1).cloned().unwrap_or_default();
            let updated = meta_spans.get(2).cloned().unwrap_or_default();

            let mut types: Vec<String> = badge
                .captures_iter(card)
                .map(|b| b[1].to_string())
                .collect();
            types.sort();
            types.dedup();

            let repo_url = github
                .captures(card)
                .map(|c| format!("https://github.com/{}", &c[1]));
            // Prefer npm (pinning/versioning is cleaner); fall back to the
            // GitHub repo as a git: source.
            let source = npm
                .captures(card)
                .map(|c| format!("npm:{}", &c[1]))
                .or_else(|| {
                    repo_url
                        .as_ref()
                        .map(|u| format!("git:{}", &u["https://".len()..]))
                });

            Some(PiPackage {
                name: pkg_name,
                description,
                author,
                downloads,
                updated,
                types,
                source,
                repo_url,
            })
        })
        .collect()
}

fn strip_tags(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    re.replace_all(s, "").into_owned()
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Sortable key for pi.dev's compact download counts ("479.6K/mo", "1.2M/mo").
fn downloads_key(s: &str) -> u64 {
    let num = s.trim_end_matches("/mo");
    let (mult, digits) = match num.chars().last() {
        Some('K') => (1_000.0, &num[..num.len() - 1]),
        Some('M') => (1_000_000.0, &num[..num.len() - 1]),
        Some('B') => (1_000_000_000.0, &num[..num.len() - 1]),
        _ => (1.0, num),
    };
    (digits.parse::<f64>().unwrap_or(0.0) * mult) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = r#"
<article class="packages-card"><div class="packages-card-body">
<h3 class="packages-name"><a href="/packages/@vigolium/piolium">@vigolium/piolium</a></h3>
<p class="packages-desc">Multi-phase security audits &amp; reports.</p>
<div class="packages-meta"><span>j3ssie</span><span>479.6K/mo</span><span>22d ago</span></div>
<div class="packages-badges"><span class="meta-chip packages-badge" data-type="extension">extension</span><span class="meta-chip packages-badge" data-type="skill">skill</span></div>
<div class="packages-links"><a href="https://www.npmjs.com/package/@vigolium/piolium">npm</a>
<a href="https://github.com/vigolium/piolium">GitHub</a></div>
</div></article>"#;

    #[test]
    fn parses_a_card() {
        let pkgs = parse_cards(CARD);
        assert_eq!(pkgs.len(), 1);
        let p = &pkgs[0];
        assert_eq!(p.name, "@vigolium/piolium");
        assert_eq!(p.description, "Multi-phase security audits & reports.");
        assert_eq!(p.author, "j3ssie");
        assert_eq!(p.downloads, "479.6K/mo");
        assert_eq!(p.updated, "22d ago");
        assert_eq!(p.types, vec!["extension", "skill"]);
        assert_eq!(p.source.as_deref(), Some("npm:@vigolium/piolium"));
        assert_eq!(
            p.repo_url.as_deref(),
            Some("https://github.com/vigolium/piolium")
        );
    }

    #[test]
    fn falls_back_to_git_source() {
        let card = CARD.replace(
            r#"<a href="https://www.npmjs.com/package/@vigolium/piolium">npm</a>"#,
            "",
        );
        let pkgs = parse_cards(&card);
        assert_eq!(
            pkgs[0].source.as_deref(),
            Some("git:github.com/vigolium/piolium")
        );
    }

    #[test]
    fn parses_last_page() {
        let html = r#"<a href="/packages?page=2">2</a> … <a href="/packages?page=112">112</a>"#;
        assert_eq!(last_page(html), 112);
    }

    #[test]
    fn parses_download_counts() {
        assert_eq!(downloads_key("479.6K/mo"), 479_600);
        assert_eq!(downloads_key("1.2M/mo"), 1_200_000);
        assert_eq!(downloads_key("42/mo"), 42);
    }
}
