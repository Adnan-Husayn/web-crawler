use anyhow::Result;
use reqwest::Client;
use scraper::{Html, Selector};
use tokio::io::AsyncWriteExt;
use std::f32::consts::E;
use std::{sync::Arc, time::Duration};
use tokio::fs::OpenOptions;
use tokio::{sync::{mpsc, Mutex, Semaphore}, time::sleep};
use url::{Host, Url};
use serde::Serialize;

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

#[tokio::main]
async fn main() -> Result<()> {
    let url = "https://google.com/";
    let seed = Url::parse(url)?;
    let max_pages = 100usize;
    let concurrency = 10usize;
    let worker_count = 4usize;
    let max_depth = 3usize;
    let same_host_only = true;
    let timeout = Duration::from_secs(15);
    let max_retries = 2usize;
    let output_file = "scraped.jsonl";
    
    let client = Arc::new(Client::builder().user_agent("RustCrawler/0.1").timeout(timeout).build()?);
    
    let (tx, mut rx) = mpsc::channel::<Task>(1000);
    let (item_tx, mut item_rx) = mpsc::channel::<Item>(1000);
    
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
    
    let (work_tx, work_rx) = async_channel::bounded::<Task>(1000);
    let work_rx = Arc::new(work_rx);
    
    let work_tx_clone = work_tx.clone();
    let distributor = tokio::spawn(async move {
        while let Some(task) = rx.recv().await {
            if work_tx_clone.send(task).await.is_err() {
                break;
            }
        }
    });

    let writer_handle = {
        let output_file = output_file.to_string();
        tokio::spawn(async move {
            let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_file)
            .await
            .expect("failed to open output file");
            
            while let Some(item) = item_rx.recv().await {
                match serde_json::to_vec(&item) {
                    Ok(mut bytes) => {
                        bytes.push(b'\n');
                        if let Err(e) = file.write_all(&bytes).await {
                            eprintln!("Failed to write item: {:?}", e)
                        }
                    },
                    Err(e) => {
                        eprintln!("Serialization error: {:?}", e);
                    }
                }
            }
            let _ = file.flush().await;
            println!("Writer task finished");
        })
    };
    
    let mut handles = Vec::new();
    
    for _ in 0..worker_count {
        let client = client.clone();
        let work_rx = work_rx.clone();
        let visited = visited.clone();
        let sem = sem.clone();
        let selector = selector.clone();
        let pages_count = pages_count.clone();
        let tx = tx.clone();
        let seed_host = seed.host_str().map(|s| s.to_string());
        let same_host_only = same_host_only;
        let max_pages = max_pages;
        let max_depth = max_depth;
        let max_retries = max_retries;
        
        let handle = tokio::spawn(async move {
            while let Ok(task) = work_rx.recv().await {
                {
                    let count = *pages_count.lock().await;
                    if count >= max_pages {
                        break;
                    }
                }

                let mut norm_url = task.url.clone();
                norm_url.set_fragment(None);
                let norm_str = normalize_url(norm_url.clone());

                {
                    let mut vis = visited.lock().await;
                    if vis.contains(&norm_str) {
                        continue;
                    }
                    vis.insert(norm_str.clone());
                }
                
                let permit = sem.acquire().await.unwrap();
                
                let mut attempt = 0;
                let mut maybe_body: Option<String> = None;
                let mut status_code: u16 = 0;
                loop {
                    attempt += 1;
                    match client.get(task.url.clone()).send().await {
                        Ok(resp) => {
                            status_code = resp.status().as_u16();
                            if resp.status().is_success() {
                                match resp.text().await {
                                    Ok(text) => {
                                        maybe_body = Some(text);
                                        break;
                                    }
                                    Err(e) => {
                                        eprintln!("Error reading body {}: {:?}", task.url, e);
                                    }
                                }
                            } else {
                                eprintln!("Non-success {}: {}", task.url, resp.status());
                            }
                        }
                        Err(e) => {
                            eprintln!("Fetch error {}: {:?}", task.url, e);
                        }
                    }
                    if attempt > max_retries {
                        break;
                    }
                    let backoff = Duration::from_millis(500 * attempt as u64);
                    sleep(backoff).await;
                }

                drop(permit);

                if let Some(body) = maybe_body {
                    
                    let child_urls = {
                        let doc = Html::parse_document(&body);
                        let mut urls = Vec::new();

                        for el in doc.select(&selector) {
                            if let Some(href) = el.value().attr("href") {
                                let href_l = href.trim();
                                if href_l.starts_with('#')
                                    || href_l.starts_with("javascript:")
                                    || href_l.starts_with("mailto:")
                                    || href_l.starts_with("tel:")
                                {
                                    continue;
                                }
                                if let Ok(child_url) = task.url.join(href_l) {
                                    match child_url.scheme() {
                                        "http" | "https" => urls.push(child_url),
                                        _ => {}
                                    }
                                }
                            }
                        }
                        urls
                    };

                    if task.depth < max_depth {
                        for child_url in child_urls {
                            if same_host_only {
                                if let (Some(seed_h), Some(child_h)) =
                                    (seed_host.as_ref(), child_url.host_str())
                                {
                                    if seed_h != child_h {
                                        continue;
                                    }
                                } else {
                                    continue;
                                }
                            }

                            tx.send(Task {
                                url: child_url,
                                depth: task.depth + 1,
                            })
                            .await
                            .ok();
                        }
                    }

                    {
                        let mut c = pages_count.lock().await;
                        *c += 1;
                        println!("Visited: {} total={}", task.url, *c);
                    }
                }
            }
        });

        handles.push(handle);
    }

    drop(tx);
    drop(work_tx);

    let _ = distributor.await;

    for h in handles {
        let _ = h.await;
    }

    println!("Crawl done");
    Ok(())
}

fn normalize_url(mut url : Url) -> String {
    url.set_fragment(None);
    if let Some(host) = url.host_str() {
        let mut parts = url.clone().into_string();

        if let Ok(mut reparsed) = Url::parse(&parts) {
            if let Some(h) = reparsed.host_str() {
                let low = h.to_ascii_lowercase();
                let _ = reparsed.set_host(Some(&low));
            }
            if (reparsed.scheme() == "http" && reparsed.port_or_known_default() == Some(80))
                || (reparsed.scheme() == "https" && reparsed.port_or_known_default() == Some(443))
            {
                let _ = reparsed.set_port(None);
            }
            let s = reparsed.into_string();
            return s;
        }
    }

    url.to_string()
}