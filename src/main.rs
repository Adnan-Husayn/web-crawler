use anyhow::Result;
use reqwest::Client;
use scraper::{Html, Selector};
use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::builder().user_agent("RustCrawler/0.1").build()?;

    let url = "https://google.com/";
    let resp = client.get(url).send().await?;
    let body = resp.text().await?;

    let document = Html::parse_document(&body);
    let selector = Selector::parse("a[href]").unwrap();

    let mut links = Vec::new();
    for elements in document.select(&selector) {
        if let Some(href) = elements.value().attr("href") {
            links.push(href.to_string())
        }
    }

    let base = Url::parse(url)?;

    println!("Links found : {}", links.len());
    for raw in links {
        if let Some(abs) = resolve_and_normalize(&base, &raw) {
            println!(" {}", abs)
        }
    }
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
