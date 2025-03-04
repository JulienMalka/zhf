//! Find the failed dependency storepath basenames of a build

use anyhow::{anyhow, Result};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use select::node::Node;
use select::predicate::{And, Attr, Class, Name, Predicate};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use wg::AsyncWaitGroup;

#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    env_logger::builder().format_timestamp(None).init();
    // Handle args
    let argv: Vec<u64> = std::env::args()
        .skip(1)
        .map(|x| x.parse::<u64>().unwrap())
        .collect();
    log::info!("Will crawl evaluations: {:?}", argv);

    // Prepare directories
    let mut data_dir = std::env::current_dir()?;
    data_dir.push("data");
    let mut most_important_dir = data_dir.clone();
    most_important_dir.push("mostimportantcache");
    create_dir_all(&most_important_dir)?;

    // Find all build IDs
    let mut evals = HashMap::new();
    for eval in &argv {
        let mut build_ids = vec![];
        let mut cache_loc = most_important_dir.clone();
        cache_loc.push(format!("{eval}.cache"));
        if cache_loc.exists() {
            log::info!("Skipping {eval} because it's already cached");
            continue;
        }

        let mut eval_loc = data_dir.clone();
        eval_loc.push("evalcache");
        eval_loc.push(format!("{eval}.cache"));
        let lines = read_to_string(eval_loc)?;
        let lines: Vec<&str> = lines.split('\n').collect();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(5, ' ').collect();
            if parts[4] != "Dependency failed" {
                continue;
            }
            build_ids.push(parts[1].parse::<u64>()?);
        }
        evals.insert(eval, build_ids);
    }
    let num_build_ids: usize = evals.values().map(Vec::len).sum();
    log::info!("Found {} builds with failed dependencies", num_build_ids);

    // Spawn tasks for getting the failed dependencies and writing them to files
    if num_build_ids > 0 {
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(10);
        let http_client = ClientBuilder::new(reqwest::Client::new())
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();
        let wg = AsyncWaitGroup::new();
        for (eval_id, build_ids) in evals {
            let mut cache_loc = most_important_dir.clone();
            cache_loc.push(format!("{eval_id}.cache.new"));
            let file_to_write = Arc::new(Mutex::new(File::create(&cache_loc).await?));
            for build_id in build_ids {
                let http_client = http_client.clone();
                let t_wg = wg.add(1);
                tokio::spawn(fetch_failed_deps_of_wrapped(
                    build_id,
                    file_to_write.clone(),
                    http_client,
                    t_wg,
                ));
            }
            // Move file to final destination
            let mut final_cache_loc = most_important_dir.clone();
            final_cache_loc.push(format!("{eval_id}.cache"));
            std::fs::rename(cache_loc, final_cache_loc)?;
        }
        let sleep_time = Duration::from_secs(5);
        loop {
            sleep(sleep_time).await;
            log::info!("Remaining: {} of {num_build_ids}", wg.waitings());
            if wg.waitings() == 0 {
                break;
            }
        }
    }

    // Clean cache
    log::info!("Cleaning cache");
    for path in std::fs::read_dir(most_important_dir)? {
        let path = path?;
        // Ignore none-cache entries
        if !path
            .file_name()
            .to_str()
            .ok_or_else(|| anyhow!("Cache entry has no filename"))?
            .ends_with(".cache")
        {
            continue;
        }
        // Ignore entries we know about
        let id = if let Ok(id) = path
            .file_name()
            .to_str()
            .ok_or_else(|| anyhow!("Cache entry has no filename"))?
            .strip_suffix(".cache")
            .ok_or_else(|| anyhow!("Cache entry lost its suffix"))?
            .parse::<u64>()
        {
            id
        } else {
            // Invalid entry
            continue;
        };
        if !argv.contains(&id) {
            log::info!("Purging cache of eval {id}");
            std::fs::remove_file(path.path())?;
        }
    }

    Ok(())
}

/// Little error handling wrapper for `fetch_failed_deps_of`
async fn fetch_failed_deps_of_wrapped(
    build_id: u64,
    file_to_write: Arc<Mutex<File>>,
    http_client: ClientWithMiddleware,
    wg_t: AsyncWaitGroup,
) {
    if let Err(e) =
        fetch_failed_deps_of(build_id, file_to_write, http_client).await
    {
        log::error!("Failed fetching dependencies of build #{build_id}: {e}");
    }
    wg_t.done();
}

/// Fetches the failed dependencies of a given build
async fn fetch_failed_deps_of(
    build_id: u64,
    file_to_write: Arc<Mutex<File>>,
    http_client: ClientWithMiddleware,
) -> Result<()> {
    let mut lines_to_write = HashMap::new();
    {
        let res = http_client
            .get(format!("https://hydra.nixos.org/build/{build_id}"))
            .send()
            .await?
            .text()
            .await?;
        let doc = select::document::Document::from(&res[..]);

        // Find architecture
        let arch = doc
            .find(Class("info-table").descendant(Name("tt")))
            .take(1)
            .next()
            .ok_or_else(|| anyhow!("No architecture found"))?
            .text();
        log::debug!("Detected architecture {arch}");

        // Find all failed steps
        let rows = doc
            .find(
                Attr("id", "tabs-buildsteps")
                    .descendant(And(Name("table"), Class("clickable-rows"))),
            )
            .next()
            .ok_or_else(|| anyhow!("No build steps found"))?
            .find(Name("tr"));
        for row in rows {
            let cols: Vec<Node> = row.find(Name("td")).collect();
            if cols.len() != 5 {
                continue;
            }
            // Ignore non-failed steps
            let status = cols[4].text();
            if !status.contains("Failed") && !status.contains("Cached") {
                continue;
            }
            // Find all links
            let mut link_to_return = None;
            for link in cols[4].find(Name("a")) {
                // Use the log link
                if link_to_return.is_none() && link.text() == "log" {
                    link_to_return = link.attr("href");
                }
                // Prefer the propagated build link
                if link.text().starts_with("build ") {
                    link_to_return = link.attr("href");
                }
            }
            if link_to_return.is_none() {
                // This happens when a build is retried
                continue;
            }
            // Calculate things to return
            let store_path = cols[1]
                .find(Name("tt"))
                .next()
                .ok_or_else(|| anyhow!("No store path found"))?
                .text();
            let store_path = store_path.split(',').next().unwrap();
            let path_name = store_path[44..].to_owned();
            let build_id = link_to_return
                .ok_or_else(|| anyhow!("logic error"))?
                .split('/')
                .nth(4)
                .ok_or_else(|| anyhow!("No build ID found"))?;

            lines_to_write.insert(
                store_path.to_owned(),
                format!("{path_name};{arch};{build_id}"),
            );
        }
    }

    // Handle store path deduplication logic and write to file. We do this deduplication so we
    // don't count the same build failing because of the same dependency multiple times twice. This
    // would happen if a whole evaluation is restarted.
    for line in lines_to_write.values() {
        file_to_write
            .lock()
            .await
            .write_all(format!("{line}\n").as_ref())
            .await?;
    }

    Ok(())
}
