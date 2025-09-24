use anyhow::Result;
use reqwest::Client;

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::builder().user_agent("RustCrawler/0.1").build()?;

    let url = "https://google.com/";
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    println!("status: {}", status);
    println!("body length: {}", body.len());
    println!("first 400 chars:\n{}", &body[..body.len().min(400)]);
    Ok(())
}
