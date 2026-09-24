# web_crawler

A small, concurrent, polite web crawler written in Rust. Give it a seed URL and
it follows links breadth-first, extracting each page's title and meta
description into a JSON Lines file.

## Features

- **Concurrent** — a pool of async workers (tokio) pulling from a shared queue.
- **Polite** — honors `robots.txt` (per origin, with `Allow`/`Disallow`,
  `*` and `$` wildcards, user-agent groups, and `Crawl-delay`).
- **Safe by default** — stays on the seed's host, hard `--max-pages` and
  `--max-depth` limits, per-request timeout, 2 MB body cap, HTML-only parsing.
- **Resilient** — retries network errors, `429` and `5xx` with exponential
  backoff; other `4xx` responses are not retried.
- **Deduplicated** — URLs are normalized (fragment removed, host lowercased,
  default ports dropped, trailing slash trimmed, query params sorted) before
  the visited check.
- **Redirect-aware** — redirects are followed manually (max 5 hops) and each
  hop is re-checked against the host filter and `robots.txt`; links are
  resolved against the final URL.

## Build & run

```bash
cargo build --release
./target/release/web_crawler https://example.com
```

Or directly with cargo:

```bash
cargo run --release -- https://example.com --max-pages 50 --max-depth 2
```

## Options

| Flag | Default | Description |
| --- | --- | --- |
| `--max-pages <N>` | `100` | Hard limit on pages fetched |
| `--concurrency <N>` | `10` | Number of workers (= max in-flight requests) |
| `--max-depth <N>` | `3` | Maximum link depth from the seed |
| `--timeout <SECS>` | `15` | Per-request timeout |
| `--retries <N>` | `2` | Retries for network errors, 429 and 5xx |
| `--output <FILE>` | `scraped.jsonl` | Output file (overwritten unless `--append`) |
| `--append` | off | Append to the output file instead of overwriting |
| `--user-agent <UA>` | `RustCrawler/0.1` | User-Agent header, also used for robots matching |
| `--all-hosts` | off | Follow links to other hosts |
| `--ignore-robots` | off | Do not fetch or enforce `robots.txt` |
| `-h`, `--help` | | Show usage |

## Output

One JSON object per line:

```json
{"url":"https://example.com/","title":"Example Domain","description":null,"status":200,"depth":0}
```

| Field | Meaning |
| --- | --- |
| `url` | The URL that was requested |
| `title` | Contents of `<title>`, if any |
| `description` | `<meta name="description">` content, if any |
| `status` | Final HTTP status; `0` means no response (network failure) |
| `depth` | Link distance from the seed |

Pages blocked by `robots.txt` are skipped and not written. Non-HTML responses
and failed fetches are written with null `title`/`description`. Every page
written counts toward `--max-pages`.

## How it works

```
seed ──► work queue (unbounded) ──► N workers ──► fetch ──► parse ──► new links ─┐
              ▲                                        │                          │
              └────────────────────────────────────────┴──── visited set ◄───────┘
                                                       │
                                                       ▼
                                              writer task ──► JSONL file
```

- The queue is unbounded because workers both consume and produce tasks; a
  bounded queue can deadlock when every worker blocks on a full queue. The
  visited set and `--max-pages` keep total work bounded.
- A page slot is reserved atomically before each fetch, so `--max-pages` is
  exact even with many workers.
- A `pending` counter tracks queued + in-flight tasks. When it reaches zero the
  queue closes and workers exit. A drop guard decrements it, so a panicking
  task can't hang the crawl.
- `Crawl-delay` is enforced per origin across all workers via a shared
  "next allowed request time".

## Limitations

- No JavaScript rendering — only links present in the served HTML are found.
- Response bodies are decoded as UTF-8 (lossily); other charsets may garble
  titles.
- `<base href>` and `rel=nofollow` are not handled.
- A `robots.txt` that fails to load (including 5xx) is treated as "allow all".
- Trailing-slash normalization assumes `/a` and `/a/` are the same page, which
  is true for most sites but not all.

## Tests

```bash
cargo test
```

Unit tests cover URL normalization and `robots.txt` parsing/matching.

Please only crawl sites you have permission to crawl, and keep the default
robots.txt handling on.

## License

MIT — see [LICENSE](LICENSE).
