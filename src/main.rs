use anyhow::Result;
use reqwest::Client;
use scraper::{Html, Selector};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, mpsc};
use url::Url;

#[derive(Clone)]
struct Task {
    url: Url,
    depth: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = Arc::new(Client::builder().user_agent("RustCrawler/0.1").build()?);
    let url = "https://google.com/";
    let seed = Url::parse(url)?;
    let max_pages = 100usize;
    let concurrency = 10usize;

    let (tx, mut rx) = mpsc::channel::<Task>(1000);
    tx.send(Task {
        url: seed.clone(),
        depth: 0,
    })
    .await
    .unwrap();

    let visited = Arc::new(Mutex::new(std::collections::HashSet::<String>::new()));
    let selector = Selector::parse("a[href]").unwrap();
    let sem = Arc::new(Semaphore::new(concurrency));
    let pages_count = Arc::new(Mutex::new(0usize));

    let mut handles = Vec::new();
    let worker_count = 4;

    for _ in 0..worker_count {
        let client = client.clone();
        let mut rx = rx.clone();
        let visited = visited.clone();
        let sem = sem.clone();
        let selector = selector.clone();
        let pages_count = pages_count.clone();
        let tx = tx.clone();

        let handle = tokio::spawn(async move {
            while let Some(task) = rx.recv().await {
                {
                    let count = *pages_count.lock().await;
                    if count >= max_pages {
                        break;
                    }
                }

                let mut vis = visited.lock().await;
                let mut norm = task.url.clone();
                norm.set_fragment(None);
                let norm_str = norm.to_string();
                if vis.contains(&norm_str) {
                    continue;
                }
                vis.insert(norm_str.clone());
                drop(vis);

                let permit = sem.acquire().await.unwrap();

                let res = client.get(task.url.clone()).send().await;

                drop(permit);

                match res {
                    Ok(resp) => {
                        if resp.status().is_success() {
                            if let Ok(body) = resp.text().await {
                                // parse and enqueue children
                                let doc = Html::parse_document(&body);
                                for el in doc.select(&selector) {
                                    if let Some(href) = el.value().attr("href") {
                                        if let Ok(child_url) = task.url.join(href) {
                                            tx.send(Task {
                                                url: child_url,
                                                depth: task.depth + 1,
                                            })
                                            .await
                                            .ok();
                                        }
                                    }
                                }
                                let mut c = pages_count.lock().await;
                                *c += 1;
                                println!("Visited: {} total={}", task.url, *c);
                            }
                        }
                    }
                    Err(e) => eprintln!("Fetch error {}: {:?}", task.url, e),
                }
            }
        });
        handles.push(handle);
    }

    drop(tx);
    for h in handles {
        let _ = h.await;
    }
    println!("Crawl done");
    Ok(())
}
