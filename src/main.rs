use anyhow::Result;
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::{HashSet, VecDeque};
use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::builder().user_agent("RustCrawler/0.1").build()?;
    let url = "https://google.com/";
    let seed = Url::parse(url)?;
    let max_pages = 50;

    let mut frontier: VecDeque<(Url, usize)> = VecDeque::new();
    frontier.push_back((seed.clone(), 0));
    let mut visited: HashSet<String> = HashSet::new();

    let selector = Selector::parse("a[href]").unwrap();

    while let Some((url, depth)) = frontier.pop_front() {
        if visited.len() >= max_pages {
            break;
        }
        let norm = {
            let mut temp = url.clone();
            temp.set_fragment(None);
            temp.to_string()
        };
        if visited.contains(&norm) {
            continue;
        }
        println!("Fetching {} (depth = {})", url, depth);

        let body = match client.get(url.clone()).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    eprintln!("Non-success: {}", resp.status())
                }
                match resp.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("Read error: {:?}", e);
                        continue;
                    }
                }
            }
            Err(e) => {
                eprintln!("Request error: {:?}", e);
                continue;
            }
        };

        visited.insert(norm);

        let document = Html::parse_document(&body);
        for el in document.select(&selector) {
            if let Some(href) = el.value().attr("href") {
                if let Ok(child) = url.join(href) {
                    let mut child_norm = child.clone();
                    child_norm.set_fragment(None);
                    let child_str = child_norm.to_string();
                    if !visited.contains(&child_str) {
                        frontier.push_back((child, depth + 1));
                    }
                }
            }
        }
    }

    println!("Done. Visited {} pages", visited.len());
    Ok(())
}

fn resolve_and_normalize(base: &Url, href: &str) -> Option<Url> {
    match base.join(href) {
        Ok(mut u) => {
            u.set_fragment(None);
            Some(u)
        }
        Err(_) => None,
    }
}
