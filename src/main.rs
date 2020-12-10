#[macro_use]
extern crate log;

use anyhow::Result;
use jheap_histo::HistogramWithTimestamp;
use std::path::PathBuf;
use structopt::StructOpt;

use ::serde::*;
use chrono::*;
use elasticsearch::auth::Credentials;
use elasticsearch::http::transport::{
    CloudConnectionPool, SingleNodeConnectionPool, TransportBuilder,
};
use elasticsearch::*;

use indicatif::{ProgressBar, ProgressStyle};
use serde_json::{to_string_pretty, Value};

const INDEX_NAME_BASE: &str = "jheap-histo-ingest";

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let progress_style = ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
        .progress_chars("##-");

    let options: Opt = Opt::from_args();
    debug!("Options [{:?}]", options);

    let es_client = build_client(&options)?;

    let index_name = options.index_name.unwrap_or_else(|| {
        format!(
            "{}_{}",
            INDEX_NAME_BASE,
            Utc::now().to_rfc3339().to_lowercase().replace(':', "-")
        )
    });

    println!("Histograms will be indexed into [{}]", index_name);

    if let Some(ref file) = options.file {
        println!("Opening file [{:?}]", file);
        let parsed = HistogramWithTimestamp::from_path(file).await?;
        ingest_to_es(
            &es_client,
            options.chunk_size,
            &index_name,
            parsed,
            progress_style.clone(),
        )
        .await?;
    }

    if let Some(ref dir) = options.dir {
        println!("Opening dir [{:?}]", dir);
        let mut files_in_dir = tokio::fs::read_dir(dir).await?;

        while let Some(file) = files_in_dir.next_entry().await? {
            if file.path().is_file() {
                let parse_result = HistogramWithTimestamp::from_path(file.path()).await;
                match parse_result {
                    Ok(parsed) => {
                        println!("Parsed [{:?}] as histogram, sending ...", file.path());
                        ingest_to_es(
                            &es_client,
                            options.chunk_size,
                            &index_name,
                            parsed,
                            progress_style.clone(),
                        )
                        .await?
                    }
                    Err(e) => warn!(
                        "Failed to parse [{:?}] into a histogram due to [{:?}], skipping",
                        file.path(),
                        e
                    ),
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug, StructOpt)]
#[structopt(
    name = "jhhi [jheap-histo-ingest]",
    about = "Ingests Java heap histograms from the jmap util to Elasticsearch"
)]
struct Opt {
    /// Histogram file to ingest. Should be passed if dir is not.
    #[structopt(
        short,
        long,
        parse(from_os_str),
        env = "JHHI_HISTO_FILE",
        required_unless("dir")
    )]
    file: Option<PathBuf>,

    /// Directory holding histogram files to ingest. Should be passed if file is not.
    #[structopt(
        short,
        long,
        parse(from_os_str),
        env = "JHHI_HISTO_DIR",
        required_unless("file")
    )]
    dir: Option<PathBuf>,

    // Credentials
    /// Basic auth: username
    #[structopt(long, env = "JHHI_ES_USER", requires("password"), conflicts_with_all(& ["api_key_id", "api_key"]))]
    user: Option<String>,
    /// Basic auth: password
    #[structopt(long, env = "JHHI_ES_PASSWORD", requires("user"), conflicts_with_all(& ["api_key_id", "api_key"]))]
    password: Option<String>,

    /// API key auth: API Key Id
    #[structopt(long, env = "JHHI_ES_API_KEY_id", requires("api-key"), conflicts_with_all(& ["password", "user"]))]
    api_key_id: Option<String>,

    /// API key auth: API Key
    #[structopt(long, env = "JHHI_ES_API_KEY", requires("api-key-id"), conflicts_with_all(& ["password", "user"]))]
    api_key: Option<String>,

    /// Cloud Id for the cluster to send data to
    #[structopt(long, env = "JHHI_ES_CLOUD_ID", conflicts_with("url"))]
    cloud_id: Option<String>,

    /// Url for the cluster to send data to
    #[structopt(long, env = "JHHI_ES_URL", conflicts_with("cloud-id"))]
    url: Option<String>,

    /// Ingest bulk size
    #[structopt(long, env = "JHHI_ES_BULK_SIZE", default_value("500"))]
    chunk_size: usize,

    /// Target index name
    #[structopt(long, env = "JHHI_ES_INDEX_NAME")]
    index_name: Option<String>,
}

fn build_client(options: &Opt) -> Result<Elasticsearch> {
    let es_transport = {
        let builder = if let Some(ref cloud_id) = options.cloud_id {
            debug!("Using Cloud Id [{}]", cloud_id);
            TransportBuilder::new(CloudConnectionPool::new(&cloud_id)?)
        } else if let Some(url) = &options.url {
            debug!("Using URL [{}]", url);
            TransportBuilder::new(SingleNodeConnectionPool::new(url.parse()?))
        } else {
            debug!("Using default transport with no URL, defaulting to localhost");
            TransportBuilder::default()
        };

        let api_key_credentials = (options.api_key_id.as_ref())
            .zip(options.api_key.as_ref())
            .map(|(key_id, key)| {
                debug!("Using API key auth");
                Credentials::ApiKey(key_id.clone(), key.clone())
            });
        let basic_credentials =
            options
                .user
                .as_ref()
                .zip(options.password.as_ref())
                .map(|(user, password)| {
                    debug!("Using Basic auth");
                    Credentials::Basic(user.clone(), password.clone())
                });
        if let Some(credentials) = api_key_credentials.or(basic_credentials) {
            builder.auth(credentials).build()
        } else {
            debug!("Using no auth");
            builder.build()
        }?
    };
    Ok(Elasticsearch::new(es_transport))
}

// Simple typed representation of the messages we'll send to ES
#[derive(Serialize, Deserialize)]
struct HistoEntry {
    timestamp: DateTime<Utc>,
    rank: usize,
    instances_count: usize,
    bytes: usize,
    class_name: String,
}

async fn ingest_to_es(
    client: &Elasticsearch,
    chunk_size: usize,
    index_name: &str,
    histo_with_timestamp: HistogramWithTimestamp,
    progress_style: ProgressStyle,
) -> Result<()> {
    let pb = ProgressBar::new(histo_with_timestamp.histogram.0.len() as u64);
    pb.set_style(progress_style);

    let bulk_op_chunks = histo_with_timestamp
        .histogram
        .0
        .as_slice()
        .chunks(chunk_size)
        .map(|chunk| {
            let ops: Vec<BulkOperation<_>> = chunk
                .iter()
                .map(|entry| {
                    let entry = HistoEntry {
                        timestamp: histo_with_timestamp.timestamp,
                        rank: entry.rank,
                        instances_count: entry.instances_count,
                        bytes: entry.bytes,
                        class_name: entry.class_name.to_owned(),
                    };
                    BulkOperation::index(entry).into()
                })
                .collect();
            ops
        });
    for bulk_ops in bulk_op_chunks {
        let chunk_items_count = bulk_ops.len();
        pb.set_message(&format!(
            "Sending chunk with [{}] entries",
            chunk_items_count
        ));
        let response = client
            .bulk(BulkParts::Index(index_name))
            .body(bulk_ops)
            .send()
            .await?;
        let response_code_successful = response.status_code().is_success();
        let response_body = response.json::<Value>().await?;
        let errors_in_body = response_body["errors"].as_bool().unwrap_or(false);
        if !response_code_successful || errors_in_body {
            anyhow::bail!(format!(
                "Bulk insert was not ok [{}]",
                to_string_pretty(&response_body)?
            ));
        }
        pb.inc(chunk_items_count as u64)
    }
    pb.finish_with_message("Done");
    Ok(())
}
