use crate::{config::IndexerConfig, defaults};
use anyhow::Result;
use fuel_indexer_types::Bytes32;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::canonicalize,
    future::Future,
    net::{SocketAddr, ToSocketAddrs},
    path::Path,
    process::Command,
    str::FromStr,
};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};
use tracing_subscriber::filter::EnvFilter;

const RUST_LOG: &str = "RUST_LOG";
const HUMAN_LOGGING: &str = "HUMAN_LOGGING";

// Testing assets use relative paths, while production assets will use absolute paths
//
// If we can successfully find the local project root, then we're in the repository,
// and should prefix all relative asset paths with the project root. If we can't find
// the project root, then it's assumed we're not in a local repository, thus no prefix.
//
// This is specifically required for the trybuild test suite.
pub fn local_repository_root() -> Option<String> {
    let curr_filepath = canonicalize(file!()).unwrap();
    let mut curr_dir = Path::new(&curr_filepath);

    // 4 = -> src (1) -> fuel-indexer-lib (2) -> packages -> (3) -> fuel-indexer (4)
    let mut depth = 4;
    while depth > 0 {
        match curr_dir.parent() {
            Some(p) => {
                curr_dir = p;
                depth -= 1;
            }
            None => {
                return None;
            }
        }
    }

    if !curr_dir.is_dir()
        || curr_dir.file_name().unwrap() != defaults::ROOT_DIRECTORY_NAME
    {
        return None;
    }

    let root_dir = curr_dir.as_os_str().to_str().unwrap().to_string();

    Some(root_dir)
}

#[derive(Debug)]
pub struct AssetReloadRequest {
    pub namespace: String,
    pub identifier: String,
}

#[derive(Debug)]
pub struct IndexStopRequest {
    pub namespace: String,
    pub identifier: String,
}

#[derive(Debug)]
pub struct IndexRevertRequest {
    pub penultimate_asset_id: i64,
    pub penultimate_asset_bytes: Vec<u8>,
    pub namespace: String,
    pub identifier: String,
}

#[derive(Debug)]
pub enum ServiceRequest {
    AssetReload(AssetReloadRequest),
    IndexStop(IndexStopRequest),
    IndexRevert(IndexRevertRequest),
}

pub fn sha256_digest<T: AsRef<[u8]>>(blob: &T) -> String {
    let mut hasher = Sha256::new();
    hasher.update(blob);
    format!("{:x}", hasher.finalize())
}

pub fn trim_opt_env_key(key: &str) -> &str {
    // Abmiguous key: $FOO, non-ambiguous key: ${FOO}
    let not_ambiguous = key.starts_with("${");
    match not_ambiguous {
        false => &key[1..],
        true => &key[2..key.len() - 1],
    }
}

pub fn is_opt_env_var(key: &str) -> bool {
    key.starts_with('$') || (key.starts_with("${") && key.ends_with('}'))
}

pub fn derive_socket_addr(host: &str, port: &str) -> SocketAddr {
    let host = format!("{host}:{port}");
    SocketAddr::from_str(&host).unwrap_or_else(|e| {
            warn!(
                "Failed to parse '{}' as a SocketAddr due to '{}'. Retrying using ToSocketAddrs.",
                host, e
            );

            let mut addrs: Vec<_> = host
                .to_socket_addrs()
                .expect("Unable to resolve domain.")
                .collect();

            let addr = addrs.pop().expect("Could not derive SocketAddr from '{}'");

            info!("Parsed SocketAddr '{:?}' from '{}'", addr, host);

            addr
        })
}

/// Attempt to connect to a database, retrying a number of times if a connection
/// can't be made. This function takes a closure with a database connection
/// function as an argument; said function should return a future that
/// resolves to a final value of type Result<T, sqlx::Error>.
pub async fn attempt_database_connection<F, Fut, T, U>(mut fut: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, U>>,
    U: std::error::Error,
{
    let mut remaining_retries = defaults::MAX_DATABASE_CONNECTION_ATTEMPTS;
    let mut delay = defaults::INITIAL_RETRY_DELAY_SECS;
    loop {
        match fut().await {
            Ok(t) => break t,
            Err(_) => {
                if remaining_retries > 0 {
                    warn!(
                            "Could not connect to database backend, retrying in {} seconds...",
                            delay
                        );
                    remaining_retries -= 1;
                    sleep(Duration::from_secs(delay)).await;
                    delay *= 2;
                } else {
                    panic!(
                        "Retry attempts exceeded; could not connect to database backend!"
                    )
                }
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServiceStatus {
    OK,
    NotOk,
}

impl From<FuelNodeHealthResponse> for ServiceStatus {
    fn from(r: FuelNodeHealthResponse) -> Self {
        match r.up {
            true => ServiceStatus::OK,
            _ => ServiceStatus::NotOk,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct FuelNodeHealthResponse {
    up: bool,
}

pub async fn init_logging(config: &IndexerConfig) -> anyhow::Result<()> {
    let level = env::var_os(RUST_LOG)
        .map(|x| x.into_string().unwrap())
        .unwrap_or("info".to_string());

    if !config.verbose {
        std::env::set_var(RUST_LOG, format!("{level},wasmer_compiler_cranelift=warn"));
    }

    let filter = match env::var_os(RUST_LOG) {
        Some(_) => {
            EnvFilter::try_from_default_env().expect("Invalid `RUST_LOG` provided")
        }
        None => EnvFilter::new("info"),
    };

    let human_logging = env::var_os(HUMAN_LOGGING)
        .map(|s| {
            bool::from_str(s.to_str().unwrap())
                .expect("Expected `true` or `false` to be provided for `HUMAN_LOGGING`")
        })
        .unwrap_or(true);

    let sub = tracing_subscriber::fmt::Subscriber::builder()
        .with_writer(std::io::stderr)
        .with_env_filter(filter);

    if human_logging {
        sub.with_ansi(true)
            .with_level(true)
            .with_line_number(true)
            .init();
    } else {
        sub.with_ansi(false)
            .with_level(true)
            .with_line_number(true)
            .json()
            .init();
    }
    Ok(())
}

pub mod indexer_utils {
    use fuel_indexer_types::SizedAsciiString;

    use super::{sha256_digest, Bytes32};

    pub fn u64_id(d: &[u8; 8]) -> u64 {
        u64::from_le_bytes(*d)
    }

    pub fn first8_bytes_to_u64(data: impl AsRef<[u8]>) -> u64 {
        let data = sha256_digest(&data);
        let mut buff = [0u8; 8];
        buff.copy_from_slice(&data.as_bytes()[..8]);
        u64_id(&buff)
    }

    pub fn first32_bytes_to_bytes32(data: impl AsRef<[u8]>) -> Bytes32 {
        let data = sha256_digest(&data);
        let mut buff = [0u8; 32];
        buff.copy_from_slice(&data.as_bytes()[..32]);
        Bytes32::from(buff)
    }

    pub fn u64_id_from_inputs(id: &[u8; 32], inputs: Vec<u8>) -> u64 {
        let inputs = [id.to_vec(), inputs].concat();
        first8_bytes_to_u64(inputs)
    }

    pub fn bytes32_from_inputs(id: &[u8; 32], inputs: Vec<u8>) -> Bytes32 {
        let inputs = [id.to_vec(), inputs].concat();
        first32_bytes_to_bytes32(inputs)
    }

    pub fn trim_sized_ascii_string<const LEN: usize>(
        s: &SizedAsciiString<LEN>,
    ) -> String {
        let mut s = s.to_string();
        let n = s.trim_end_matches(' ').len();
        s.truncate(n);
        s
    }
}

pub fn format_exec_msg(exec_name: &str, path: Option<String>) -> String {
    if let Some(path) = path {
        rightpad_whitespace(&path, defaults::MESSAGE_PADDING)
    } else {
        rightpad_whitespace(
            &format!("Can't locate {exec_name}."),
            defaults::MESSAGE_PADDING,
        )
    }
}

pub fn find_executable_with_msg(exec_name: &str) -> (String, Option<String>, String) {
    let (emoji, path) = find_executable(exec_name);
    let p = path.clone();
    (emoji, path, format_exec_msg(exec_name, p))
}

pub fn find_executable(exec_name: &str) -> (String, Option<String>) {
    match Command::new("which").arg(exec_name).output() {
        Ok(o) => {
            let path = String::from_utf8_lossy(&o.stdout)
                .strip_suffix('\n')
                .map(|x| x.to_string())
                .unwrap_or_else(String::new);

            if !path.is_empty() {
                (
                    center_align("✅", defaults::SUCCESS_EMOJI_PADDING),
                    Some(path),
                )
            } else {
                (center_align("⛔️", defaults::FAIL_EMOJI_PADDING - 2), None)
            }
        }
        Err(_e) => (center_align("⛔️", defaults::FAIL_EMOJI_PADDING), None),
    }
}

pub fn center_align(s: &str, n: usize) -> String {
    format!("{s: ^n$}")
}

pub fn rightpad_whitespace(s: &str, n: usize) -> String {
    format!("{s:0n$}")
}

// IMPORTANT: rustc is required for this functionality.
pub fn host_triple() -> String {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("Failed to get rustc version output.");

    String::from_utf8_lossy(&output.stdout)
        .split('\n')
        .filter_map(|x| {
            if x.to_lowercase().starts_with("host") {
                Some(x.to_string())
            } else {
                None
            }
        })
        .collect::<Vec<String>>()
        .first()
        .expect("Failed to determine host triple via rustc.")[6..]
        .to_owned()
}
