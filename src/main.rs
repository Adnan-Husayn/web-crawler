use anyhow::{Result, anyhow};
use reqwest::Client;
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use scraper::{Html, Selector};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{OnceCell, mpsc};
use tokio::time::{sleep, sleep_until};
use url::Url;

const MAX_REDIRECTS: usize = 5;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct Task {
    url: Url,
    depth: usize,
}

#[derive(Serialize)]
struct Item {
    url: String,
    title: Option<String>,
    description: Option<String>,
    status: u16,
    depth: usize,
}

struct Config {
    seed: Url,
    max_pages: usize,
    concurrency: usize,
    max_depth: usize,
    timeout: Duration,
    retries: usize,
    output: String,
    append: bool,
    same_host_only: bool,
    respect_robots: bool,
    user_agent: String,
}

struct Selectors {
    links: Selector,
    title: Selector,
    description: Selector,
}

struct Fetched {
    status: u16,
    url: Url,
    body: Option<String>,
}

#[derive(Clone)]
struct Ctx {
    client: Arc<Client>,
    cfg: Arc<Config>,
    robots: Arc<RobotsCache>,
    visited: Arc<Mutex<HashSet<String>>>,
    throttle: Arc<Mutex<HashMap<String, Instant>>>,
    pending: Arc<AtomicUsize>,
    pages: Arc<AtomicUsize>,
    selectors: Arc<Selectors>,
    work_tx: async_channel::Sender<Task>,
    work_rx: async_channel::Receiver<Task>,
    item_tx: mpsc::Sender<Item>,
    seed_host: Option<String>,
}

/// Decrements the pending-task counter on drop and closes the queue when the
/// last task finishes, so the crawl terminates even if a task panics.
struct PendingGuard {
    pending: Arc<AtomicUsize>,
    tx: async_channel::Sender<Task>,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.tx.close();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Arc::new(parse_args()?);

    // Redirects are followed manually so every hop can be checked against the
    // host filter and robots.txt.
    let client = Arc::new(
        Client::builder()
            .user_agent(&cfg.user_agent)
            .timeout(cfg.timeout)
            .redirect(Policy::none())
            .build()?,
    );
    let robots_client = Client::builder()
        .user_agent(&cfg.user_agent)
        .timeout(cfg.timeout)
        .redirect(Policy::limited(MAX_REDIRECTS))
        .build()?;
    let robots = Arc::new(RobotsCache::new(
        robots_client,
        cfg.user_agent.clone(),
        cfg.respect_robots,
    ));

    let seed_host = cfg.seed.host_str().map(|s| s.to_string());

    // Unbounded on purpose: workers both consume and produce tasks, so a
    // bounded queue can deadlock when every worker blocks on `send`. The
    // visited set and `max_pages` already bound the total amount of work.
    let (work_tx, work_rx) = async_channel::unbounded::<Task>();
    let (item_tx, mut item_rx) = mpsc::channel::<Item>(1000);

    let output_file = cfg.output.clone();
    let append = cfg.append;
    let writer = tokio::spawn(async move {
        let mut options = OpenOptions::new();
        options.create(true);
        if append {
            options.append(true);
        } else {
            options.write(true).truncate(true);
        }
        let mut file = match options.open(&output_file).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("failed to open output file {output_file}: {e}");
                return;
            }
        };

        while let Some(item) = item_rx.recv().await {
            match serde_json::to_vec(&item) {
                Ok(mut bytes) => {
                    bytes.push(b'\n');
                    if let Err(e) = file.write_all(&bytes).await {
                        eprintln!("Failed to write item: {e:?}");
                    }
                }
                Err(e) => eprintln!("Serialization error: {e:?}"),
            }
        }
        let _ = file.flush().await;
    });

    let selectors = Arc::new(Selectors {
        links: Selector::parse("a[href]").unwrap(),
        title: Selector::parse("title").unwrap(),
        description: Selector::parse("meta[name=description]").unwrap(),
    });

    let visited = Arc::new(Mutex::new(HashSet::<String>::new()));
    lock(&visited).insert(normalize_url(cfg.seed.clone()));

    let pending = Arc::new(AtomicUsize::new(1));

    let ctx = Ctx {
        client,
        cfg: cfg.clone(),
        robots,
        visited,
        throttle: Arc::new(Mutex::new(HashMap::new())),
        pending,
        pages: Arc::new(AtomicUsize::new(0)),
        selectors,
        work_tx: work_tx.clone(),
        work_rx: work_rx.clone(),
        item_tx,
        seed_host,
    };

    work_tx
        .send(Task {
            url: cfg.seed.clone(),
            depth: 0,
        })
        .await?;
    drop(work_tx);
    drop(work_rx);

    let mut handles = Vec::new();
    for _ in 0..cfg.concurrency {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move { worker(ctx).await }));
    }
    drop(ctx);

    for h in handles {
        let _ = h.await;
    }
    let _ = writer.await;

    println!("Crawl done");
    Ok(())
}

async fn worker(ctx: Ctx) {
    while let Ok(task) = ctx.work_rx.recv().await {
        // Each task runs in its own spawned task so a panic is isolated and
        // the worker keeps going (the PendingGuard still fires on unwind).
        let _ = tokio::spawn(handle(ctx.clone(), task)).await;
    }
}

async fn handle(ctx: Ctx, task: Task) {
    let _guard = PendingGuard {
        pending: ctx.pending.clone(),
        tx: ctx.work_tx.clone(),
    };

    if !url_allowed(&ctx, &task.url).await {
        return;
    }

    // Reserve a page slot before fetching so `max_pages` is a hard limit.
    let max = ctx.cfg.max_pages;
    let Ok(prev) = ctx
        .pages
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
            (c < max).then_some(c + 1)
        })
    else {
        return;
    };
    let count = prev + 1;

    let fetched = fetch(&ctx, &task.url).await;
    if fetched.url != task.url {
        lock(&ctx.visited).insert(normalize_url(fetched.url.clone()));
    }

    let (title, description, children) = match &fetched.body {
        Some(body) => parse_page(&ctx, &fetched.url, body),
        None => (None, None, Vec::new()),
    };

    println!("[{count}/{max}] {} ({})", task.url, fetched.status);

    if fetched.body.is_some() && task.depth < ctx.cfg.max_depth {
        for child in children {
            if ctx.pages.load(Ordering::SeqCst) >= max {
                break;
            }
            if !host_ok(&ctx, &child) {
                continue;
            }
            let is_new = lock(&ctx.visited).insert(normalize_url(child.clone()));
            if is_new {
                ctx.pending.fetch_add(1, Ordering::SeqCst);
                let sent = ctx
                    .work_tx
                    .send(Task {
                        url: child,
                        depth: task.depth + 1,
                    })
                    .await;
                if sent.is_err() {
                    ctx.pending.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
    }

    let _ = ctx
        .item_tx
        .send(Item {
            url: task.url.to_string(),
            title,
            description,
            status: fetched.status,
            depth: task.depth,
        })
        .await;
}

fn host_ok(ctx: &Ctx, url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if !ctx.cfg.same_host_only {
        return true;
    }
    match (ctx.seed_host.as_deref(), url.host_str()) {
        (Some(seed), Some(host)) => seed.eq_ignore_ascii_case(host),
        _ => false,
    }
}

async fn url_allowed(ctx: &Ctx, url: &Url) -> bool {
    host_ok(ctx, url) && ctx.robots.get(url).await.allowed(url)
}

/// Waits so requests to one origin are at least `crawl-delay` apart, even
/// across concurrent workers.
async fn throttle(ctx: &Ctx, url: &Url) {
    let Some(delay) = ctx.robots.get(url).await.crawl_delay else {
        return;
    };
    let origin = url.origin().ascii_serialization();
    let slot = {
        let mut map = lock(&ctx.throttle);
        let now = Instant::now();
        let slot = map
            .get(&origin)
            .copied()
            .filter(|t| *t > now)
            .unwrap_or(now);
        map.insert(origin, slot + delay);
        slot
    };
    sleep_until(tokio::time::Instant::from_std(slot)).await;
}

async fn fetch(ctx: &Ctx, start: &Url) -> Fetched {
    let mut last_status = 0u16;
    for attempt in 0..=ctx.cfg.retries {
        match fetch_once(ctx, start).await {
            Ok(fetched) => return fetched,
            Err(status) => {
                last_status = status;
                if attempt < ctx.cfg.retries {
                    // Exponential backoff: 500ms, 1s, 2s, ...
                    let shift = attempt.min(6) as u32;
                    sleep(Duration::from_millis(500u64 << shift)).await;
                }
            }
        }
    }
    Fetched {
        status: last_status,
        url: start.clone(),
        body: None,
    }
}

/// One fetch attempt, following redirects manually. `Err(status)` means the
/// failure is worth retrying (network error, 429, 5xx); 0 means no response.
async fn fetch_once(ctx: &Ctx, start: &Url) -> Result<Fetched, u16> {
    let mut url = start.clone();
    let mut status = 0u16;

    for _ in 0..=MAX_REDIRECTS {
        throttle(ctx, &url).await;

        let mut resp = match ctx.client.get(url.clone()).send().await {
            Ok(resp) => resp,
            Err(e) => {
                eprintln!("Fetch error {url}: {e}");
                return Err(0);
            }
        };
        let code = resp.status();
        status = code.as_u16();

        if code.is_redirection() {
            let next = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| url.join(loc).ok());
            match next {
                Some(next) if url_allowed(ctx, &next).await => {
                    url = next;
                    continue;
                }
                _ => {
                    return Ok(Fetched {
                        status,
                        url,
                        body: None,
                    });
                }
            }
        }

        if status == 429 || code.is_server_error() {
            eprintln!("Retryable status {url}: {code}");
            return Err(status);
        }
        if !code.is_success() {
            eprintln!("Non-success {url}: {code}");
            return Ok(Fetched {
                status,
                url,
                body: None,
            });
        }

        let is_html = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.to_ascii_lowercase().contains("html"))
            .unwrap_or(true);
        if !is_html {
            return Ok(Fetched {
                status,
                url,
                body: None,
            });
        }

        let mut buf: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let room = MAX_BODY_BYTES - buf.len();
                    if chunk.len() >= room {
                        buf.extend_from_slice(&chunk[..room]);
                        break;
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("Error reading body {url}: {e}");
                    return Err(status);
                }
            }
        }
        let body = String::from_utf8_lossy(&buf).into_owned();
        return Ok(Fetched {
            status,
            url,
            body: Some(body),
        });
    }

    eprintln!("Too many redirects starting at {start}");
    Ok(Fetched {
        status,
        url,
        body: None,
    })
}

fn parse_page(ctx: &Ctx, base: &Url, body: &str) -> (Option<String>, Option<String>, Vec<Url>) {
    let doc = Html::parse_document(body);

    let title = doc
        .select(&ctx.selectors.title)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty());

    let description = doc
        .select(&ctx.selectors.description)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut urls = Vec::new();
    for el in doc.select(&ctx.selectors.links) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty()
            || href.starts_with('#')
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
        {
            continue;
        }
        if let Ok(mut child) = base.join(href)
            && matches!(child.scheme(), "http" | "https")
        {
            child.set_fragment(None);
            urls.push(child);
        }
    }

    (title, description, urls)
}

/// Canonical form used as the dedupe key. The `url` crate already lowercases
/// the scheme/host and drops default ports; on top of that this strips the
/// fragment, removes trailing slashes (except the root), and sorts query
/// parameters.
fn normalize_url(mut url: Url) -> String {
    url.set_fragment(None);

    let path = url.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        url.set_path(path.trim_end_matches('/'));
    }

    if url.query().is_some() {
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if pairs.is_empty() {
            url.set_query(None);
        } else {
            pairs.sort();
            url.query_pairs_mut().clear().extend_pairs(pairs);
        }
    }

    url.into()
}

#[derive(Clone, Default)]
struct Robots {
    /// `(is_allow, pattern)`
    rules: Vec<(bool, String)>,
    crawl_delay: Option<Duration>,
}

#[derive(Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<(bool, String)>,
    delay: Option<Duration>,
}

impl Robots {
    fn parse(body: &str, user_agent: &str) -> Robots {
        let mut groups: Vec<Group> = Vec::new();
        let mut last_was_agent = false;

        for raw in body.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();

            if key == "user-agent" {
                // Consecutive user-agent lines share one group.
                if !last_was_agent || groups.is_empty() {
                    groups.push(Group::default());
                }
                groups
                    .last_mut()
                    .unwrap()
                    .agents
                    .push(value.to_ascii_lowercase());
                last_was_agent = true;
                continue;
            }

            last_was_agent = false;
            let Some(group) = groups.last_mut() else {
                continue;
            };
            match key.as_str() {
                "disallow" if !value.is_empty() => group.rules.push((false, value.to_string())),
                "allow" if !value.is_empty() => group.rules.push((true, value.to_string())),
                "crawl-delay" => {
                    group.delay = value
                        .parse::<f64>()
                        .ok()
                        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
                        .map(|d| d.min(MAX_CRAWL_DELAY));
                }
                _ => {}
            }
        }

        // A group naming our product token beats the `*` group.
        let token = user_agent
            .split('/')
            .next()
            .unwrap_or(user_agent)
            .to_ascii_lowercase();
        let specific: Vec<&Group> = groups
            .iter()
            .filter(|g| {
                g.agents
                    .iter()
                    .any(|a| a != "*" && !a.is_empty() && token.contains(a.as_str()))
            })
            .collect();
        let chosen: Vec<&Group> = if specific.is_empty() {
            groups
                .iter()
                .filter(|g| g.agents.iter().any(|a| a == "*"))
                .collect()
        } else {
            specific
        };

        let mut robots = Robots::default();
        for group in chosen {
            robots.rules.extend(group.rules.iter().cloned());
            if group.delay.is_some() {
                robots.crawl_delay = group.delay;
            }
        }
        robots
    }

    fn allowed(&self, url: &Url) -> bool {
        let path = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };
        let target = match url.query() {
            Some(query) => format!("{path}?{query}"),
            None => path.to_string(),
        };

        // Longest matching pattern wins; on a tie, allow wins.
        let mut best: Option<(usize, bool)> = None;
        for (is_allow, pattern) in &self.rules {
            if pattern_matches(pattern, &target) {
                let candidate = (pattern.len(), *is_allow);
                best = match best {
                    Some(current) if current.0 > candidate.0 => Some(current),
                    Some(current) if current.0 == candidate.0 => {
                        Some((current.0, current.1 || candidate.1))
                    }
                    _ => Some(candidate),
                };
            }
        }
        best.map(|(_, allow)| allow).unwrap_or(true)
    }
}

/// robots.txt pattern match: prefix match with `*` wildcards and an optional
/// trailing `$` end anchor.
fn pattern_matches(pattern: &str, target: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.len() - 1;
    let mut pos = 0;

    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !target.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == last && anchored {
            return target.len() >= pos + part.len() && target.ends_with(part);
        } else {
            match target[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }

    !anchored || pos == target.len()
}

/// Per-origin robots.txt, fetched lazily once and cached.
struct RobotsCache {
    client: Client,
    user_agent: String,
    enabled: bool,
    cells: Mutex<HashMap<String, Arc<OnceCell<Arc<Robots>>>>>,
}

impl RobotsCache {
    fn new(client: Client, user_agent: String, enabled: bool) -> Self {
        Self {
            client,
            user_agent,
            enabled,
            cells: Mutex::new(HashMap::new()),
        }
    }

    async fn get(&self, url: &Url) -> Arc<Robots> {
        if !self.enabled {
            return Arc::new(Robots::default());
        }
        let origin = url.origin().ascii_serialization();
        let cell = lock(&self.cells).entry(origin).or_default().clone();
        cell.get_or_init(|| async { Arc::new(self.fetch(url).await) })
            .await
            .clone()
    }

    async fn fetch(&self, url: &Url) -> Robots {
        let mut robots_url = url.clone();
        robots_url.set_path("/robots.txt");
        robots_url.set_query(None);
        robots_url.set_fragment(None);

        match self.client.get(robots_url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(body) => Robots::parse(&body, &self.user_agent),
                Err(_) => Robots::default(),
            },
            _ => Robots::default(),
        }
    }
}

fn parse_args() -> Result<Config> {
    let mut seed: Option<String> = None;
    let mut max_pages = 100usize;
    let mut concurrency = 10usize;
    let mut max_depth = 3usize;
    let mut timeout_secs = 15u64;
    let mut retries = 2usize;
    let mut output = "scraped.jsonl".to_string();
    let mut append = false;
    let mut same_host_only = true;
    let mut respect_robots = true;
    let mut user_agent = "RustCrawler/0.1".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--max-pages" => max_pages = parse_num(&mut args, "--max-pages")?,
            "--concurrency" => concurrency = parse_num(&mut args, "--concurrency")?,
            "--max-depth" => max_depth = parse_num(&mut args, "--max-depth")?,
            "--timeout" => timeout_secs = parse_num(&mut args, "--timeout")?,
            "--retries" => retries = parse_num(&mut args, "--retries")?,
            "--output" => output = next_value(&mut args, "--output")?,
            "--user-agent" => user_agent = next_value(&mut args, "--user-agent")?,
            "--append" => append = true,
            "--all-hosts" => same_host_only = false,
            "--ignore-robots" => respect_robots = false,
            other if other.starts_with('-') => {
                return Err(anyhow!("unknown flag: {other} (try --help)"));
            }
            other => {
                if seed.is_some() {
                    return Err(anyhow!("unexpected extra argument: {other}"));
                }
                seed = Some(other.to_string());
            }
        }
    }

    let seed = seed.ok_or_else(|| anyhow!("missing seed URL (try --help)"))?;
    let seed = Url::parse(&seed)?;
    if !matches!(seed.scheme(), "http" | "https") {
        return Err(anyhow!("seed URL must be http or https"));
    }
    if concurrency == 0 {
        return Err(anyhow!("concurrency must be greater than 0"));
    }

    Ok(Config {
        seed,
        max_pages,
        concurrency,
        max_depth,
        timeout: Duration::from_secs(timeout_secs),
        retries,
        output,
        append,
        same_host_only,
        respect_robots,
        user_agent,
    })
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("missing value for {flag}"))
}

fn parse_num<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let raw = next_value(args, flag)?;
    raw.parse::<T>()
        .map_err(|e| anyhow!("invalid value for {flag}: {raw} ({e})"))
}

fn print_usage() {
    println!(
        "Usage: web_crawler <SEED_URL> [OPTIONS]\n\
         \n\
         Options:\n\
           --max-pages <N>      Maximum pages to crawl (default: 100)\n\
           --concurrency <N>    Concurrent workers / in-flight requests (default: 10)\n\
           --max-depth <N>      Maximum link depth (default: 3)\n\
           --timeout <SECS>     Per-request timeout in seconds (default: 15)\n\
           --retries <N>        Retries for network errors, 429 and 5xx (default: 2)\n\
           --output <FILE>      Output JSONL file (default: scraped.jsonl)\n\
           --append             Append to the output file instead of overwriting it\n\
           --user-agent <UA>    User-Agent header (default: RustCrawler/0.1)\n\
           --all-hosts          Follow links to other hosts (default: same host only)\n\
           --ignore-robots      Do not fetch or enforce robots.txt\n\
           -h, --help           Show this help"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        normalize_url(Url::parse(s).unwrap())
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn normalize_strips_fragment_and_default_port() {
        assert_eq!(norm("HTTPS://Example.com:443/a#b"), "https://example.com/a");
    }

    #[test]
    fn normalize_keeps_non_default_port() {
        assert_eq!(
            norm("http://example.com:8080/a"),
            "http://example.com:8080/a"
        );
    }

    #[test]
    fn normalize_trims_trailing_slash_but_not_root() {
        assert_eq!(norm("https://x.com/a/"), "https://x.com/a");
        assert_eq!(norm("https://x.com/"), "https://x.com/");
    }

    #[test]
    fn normalize_sorts_query_and_drops_empty_query() {
        assert_eq!(norm("https://x.com/a?b=2&a=1"), "https://x.com/a?a=1&b=2");
        assert_eq!(norm("https://x.com/a?"), "https://x.com/a");
    }

    #[test]
    fn robots_allow_overrides_disallow_by_length() {
        let body = "User-agent: *\nDisallow: /private\nAllow: /private/public\n";
        let robots = Robots::parse(body, "RustCrawler/0.1");
        assert!(!robots.allowed(&url("https://x.com/private/a")));
        assert!(robots.allowed(&url("https://x.com/private/public/a")));
        assert!(robots.allowed(&url("https://x.com/open")));
    }

    #[test]
    fn robots_ignores_other_user_agents() {
        let body = "User-agent: Googlebot\nDisallow: /\n";
        let robots = Robots::parse(body, "RustCrawler/0.1");
        assert!(robots.allowed(&url("https://x.com/anything")));
    }

    #[test]
    fn robots_parses_crawl_delay() {
        let body = "User-agent: *\nCrawl-delay: 1.5\n";
        let robots = Robots::parse(body, "RustCrawler/0.1");
        assert_eq!(robots.crawl_delay, Some(Duration::from_millis(1500)));
    }

    #[test]
    fn robots_ignores_invalid_crawl_delay() {
        let body = "User-agent: *\nCrawl-delay: -3\n";
        let robots = Robots::parse(body, "RustCrawler/0.1");
        assert_eq!(robots.crawl_delay, None);
    }

    #[test]
    fn robots_groups_consecutive_user_agents() {
        let body = "User-agent: *\nUser-agent: Googlebot\nDisallow: /x\n";
        let robots = Robots::parse(body, "RustCrawler/0.1");
        assert!(!robots.allowed(&url("https://x.com/x")));
    }

    #[test]
    fn robots_specific_group_beats_wildcard() {
        let body = "User-agent: *\nDisallow: /\n\nUser-agent: RustCrawler\nAllow: /\n";
        let robots = Robots::parse(body, "RustCrawler/0.1");
        assert!(robots.allowed(&url("https://x.com/anything")));
    }

    #[test]
    fn robots_supports_wildcards_and_end_anchor() {
        let body = "User-agent: *\nDisallow: /*.pdf$\nDisallow: /tmp*/cache\n";
        let robots = Robots::parse(body, "RustCrawler/0.1");
        assert!(!robots.allowed(&url("https://x.com/a/b.pdf")));
        assert!(robots.allowed(&url("https://x.com/a/b.pdf.html")));
        assert!(!robots.allowed(&url("https://x.com/tmp123/cache")));
        assert!(robots.allowed(&url("https://x.com/other/cache")));
    }

    #[test]
    fn pattern_matching_basics() {
        assert!(pattern_matches("/a", "/abc"));
        assert!(!pattern_matches("/a$", "/abc"));
        assert!(pattern_matches("/a$", "/a"));
        assert!(pattern_matches("/*/x", "/foo/x/y"));
        assert!(!pattern_matches("/*/x$", "/foo/x/y"));
    }
}
