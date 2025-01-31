use crate::{database::Database, ffi, IndexerConfig, IndexerError, IndexerResult};
use async_std::{
    fs::File,
    io::ReadExt,
    sync::{Arc, Mutex},
};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use fuel_core_client::client::{
    types::{TransactionResponse, TransactionStatus as GqlTransactionStatus},
    FuelClient, PageDirection, PaginatedResult, PaginationRequest,
};
use fuel_indexer_lib::{defaults::*, manifest::Manifest};
use fuel_indexer_schema::utils::serialize;
use fuel_indexer_types::{
    abi::{BlockData, TransactionData},
    tx::{TransactionStatus, TxId},
    Bytes32,
};
use futures::Future;
use std::{
    marker::{Send, Sync},
    path::Path,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
};
use thiserror::Error;
use tokio::{
    task::{spawn_blocking, JoinHandle},
    time::{sleep, Duration},
};
use tracing::{debug, error, info};
use wasmer::{
    imports, Instance, LazyInit, Memory, Module, NativeFunc, RuntimeError, Store,
    WasmerEnv,
};
use wasmer_compiler_cranelift::Cranelift;
use wasmer_engine_universal::Universal;

fn compiler() -> Cranelift {
    Cranelift::default()
}

#[derive(Debug, Clone)]
pub enum ExecutorSource {
    Manifest,
    Registry(Vec<u8>),
}

impl AsRef<[u8]> for ExecutorSource {
    fn as_ref(&self) -> &[u8] {
        match self {
            ExecutorSource::Manifest => &[],
            ExecutorSource::Registry(b) => b,
        }
    }
}

impl ExecutorSource {
    pub fn to_vec(self) -> Vec<u8> {
        match self {
            ExecutorSource::Manifest => vec![],
            ExecutorSource::Registry(bytes) => bytes,
        }
    }
}

pub fn run_executor<T: 'static + Executor + Send + Sync>(
    config: &IndexerConfig,
    manifest: &Manifest,
    mut executor: T,
    kill_switch: Arc<AtomicBool>,
) -> impl Future<Output = ()> {
    let start_block = manifest.start_block.expect("Failed to detect start_block.");
    let stop_idle_indexers = config.stop_idle_indexers;

    let fuel_node_addr = if config.indexer_net_config {
        manifest
            .fuel_client
            .clone()
            .unwrap_or(config.fuel_node.to_string())
    } else {
        config.fuel_node.to_string()
    };

    let mut next_cursor = if start_block > 1 {
        let decremented = start_block - 1;
        Some(decremented.to_string())
    } else {
        None
    };
    info!("Subscribing to Fuel node at {fuel_node_addr}");

    let client = FuelClient::from_str(&fuel_node_addr)
        .unwrap_or_else(|e| panic!("Node connection failed: {e}."));

    async move {
        let mut retry_count = 0;

        // If we're testing or running on CI, we don't want indexers to run forever. But in production
        // let the index operators decide if they want to stop idle indexers. Maybe we can eventually
        // make this MAX_EMPTY_BLOCK_REQUESTS value configurable
        let max_empty_block_reqs = if stop_idle_indexers {
            MAX_EMPTY_BLOCK_REQUESTS
        } else {
            usize::MAX
        };
        let mut num_empty_block_reqs = 0;

        loop {
            debug!("Fetching paginated results from {next_cursor:?}",);

            let PaginatedResult {
                cursor, results, ..
            } = client
                .blocks(PaginationRequest {
                    cursor: next_cursor.clone(),
                    results: NODE_GRAPHQL_PAGE_SIZE,
                    direction: PageDirection::Forward,
                })
                .await
                .unwrap_or_else(|e| {
                    error!("Failed to retrieve blocks: {e}",);
                    PaginatedResult {
                        cursor: None,
                        results: vec![],
                        has_next_page: false,
                        has_previous_page: false,
                    }
                });

            let mut block_info = Vec::new();
            for block in results.into_iter() {
                let producer = block.block_producer().map(|pk| pk.hash());

                let mut transactions = Vec::new();

                for trans in block.transactions {
                    // TODO: https://github.com/FuelLabs/fuel-indexer/issues/288
                    match client.transaction(&trans.id.to_string()).await {
                        Ok(result) => {
                            if let Some(TransactionResponse {
                                transaction,
                                status,
                            }) = result
                            {
                                let receipts = client
                                    .receipts(&trans.id.to_string())
                                    .await
                                    .unwrap_or_else(|e| {
                                        error!("Client communication error fetching receipts: {e:?}");
                                        Vec::new()
                                    });

                                // NOTE: https://github.com/FuelLabs/fuel-indexer/issues/286
                                let status = match status {
                                    GqlTransactionStatus::Success {
                                        block_id,
                                        time,
                                        ..
                                    } => TransactionStatus::Success {
                                        block_id,
                                        time: Utc
                                            .timestamp_opt(time.to_unix(), 0)
                                            .single()
                                            .unwrap(),
                                    },
                                    GqlTransactionStatus::Failure {
                                        block_id,
                                        time,
                                        reason,
                                        ..
                                    } => TransactionStatus::Failure {
                                        block_id,
                                        time: Utc
                                            .timestamp_opt(time.to_unix(), 0)
                                            .single()
                                            .unwrap(),
                                        reason,
                                    },
                                    GqlTransactionStatus::Submitted { submitted_at } => {
                                        TransactionStatus::Submitted {
                                            submitted_at: Utc
                                                .timestamp_opt(submitted_at.to_unix(), 0)
                                                .single()
                                                .unwrap(),
                                        }
                                    }
                                    GqlTransactionStatus::SqueezedOut { reason } => {
                                        TransactionStatus::SqueezedOut { reason }
                                    }
                                };

                                let tx_data = TransactionData {
                                    receipts,
                                    status,
                                    transaction,
                                    id: TxId::from(trans.id),
                                };
                                transactions.push(tx_data);
                            }
                        }
                        Err(e) => {
                            error!("Error fetching transactions: {e:?}.",)
                        }
                    };
                }

                let block = BlockData {
                    height: block.header.height.0,
                    id: Bytes32::from(block.id),
                    producer,
                    time: block.header.time.0.to_unix(),
                    transactions,
                };

                block_info.push(block);
            }

            let result = executor.handle_events(block_info).await;

            if let Err(e) = result {
                error!("Indexer executor failed {e:?}, retrying.");
                sleep(Duration::from_secs(DELAY_FOR_SERVICE_ERR)).await;
                retry_count += 1;
                if retry_count < INDEX_FAILED_CALLS {
                    continue;
                } else {
                    error!("Indexer failed after retries, giving up. <('.')>");
                    break;
                }
            }

            if cursor.is_none() {
                info!("No new blocks to process, sleeping.");
                sleep(Duration::from_secs(DELAY_FOR_EMPTY_PAGE)).await;

                num_empty_block_reqs += 1;

                if num_empty_block_reqs == max_empty_block_reqs {
                    error!("No blocks being produced, giving up. <('.')>");
                    break;
                }
            } else {
                next_cursor = cursor;
                num_empty_block_reqs = 0;
            }

            if kill_switch.load(Ordering::SeqCst) {
                break;
            }

            retry_count = 0;
        }
    }
}

#[async_trait]
pub trait Executor
where
    Self: Sized,
{
    async fn handle_events(&mut self, blocks: Vec<BlockData>) -> IndexerResult<()>;
}

#[derive(Error, Debug)]
pub enum TxError {
    #[error("WASM Runtime Error {0:?}")]
    WasmRuntimeError(#[from] RuntimeError),
}

#[derive(WasmerEnv, Clone)]
pub struct IndexEnv {
    #[wasmer(export)]
    memory: LazyInit<Memory>,
    #[wasmer(export(name = "alloc_fn"))]
    alloc: LazyInit<NativeFunc<u32, u32>>,
    #[wasmer(export(name = "dealloc_fn"))]
    dealloc: LazyInit<NativeFunc<(u32, u32), ()>>,
    pub db: Arc<Mutex<Database>>,
}

impl IndexEnv {
    pub async fn new(db_url: String) -> IndexerResult<IndexEnv> {
        let db = Arc::new(Mutex::new(Database::new(&db_url).await?));
        Ok(IndexEnv {
            memory: Default::default(),
            alloc: Default::default(),
            dealloc: Default::default(),
            db,
        })
    }
}

// TODO: Use mutex
unsafe impl<F: Future<Output = IndexerResult<()>> + Send> Sync
    for NativeIndexExecutor<F>
{
}
unsafe impl<F: Future<Output = IndexerResult<()>> + Send> Send
    for NativeIndexExecutor<F>
{
}

pub struct NativeIndexExecutor<F>
where
    F: Future<Output = IndexerResult<()>> + Send,
{
    db: Arc<Mutex<Database>>,
    #[allow(unused)]
    manifest: Manifest,
    handle_events_fn: fn(Vec<BlockData>, Arc<Mutex<Database>>) -> F,
}

impl<F> NativeIndexExecutor<F>
where
    F: Future<Output = IndexerResult<()>> + Send,
{
    pub async fn new(
        config: &IndexerConfig,
        manifest: &Manifest,
        handle_events_fn: fn(Vec<BlockData>, Arc<Mutex<Database>>) -> F,
    ) -> IndexerResult<Self> {
        let db_url = config.database.to_string();
        let db = Arc::new(Mutex::new(Database::new(&db_url).await?));
        db.lock().await.load_schema(manifest, None).await?;
        Ok(Self {
            db,
            manifest: manifest.to_owned(),
            handle_events_fn,
        })
    }

    pub async fn create<T: Future<Output = IndexerResult<()>> + Send + 'static>(
        config: &IndexerConfig,
        manifest: &Manifest,
        handle_events: fn(Vec<BlockData>, Arc<Mutex<Database>>) -> T,
    ) -> IndexerResult<(JoinHandle<()>, ExecutorSource, Arc<AtomicBool>)> {
        let executor = NativeIndexExecutor::new(config, manifest, handle_events).await?;
        let kill_switch = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(run_executor(
            config,
            manifest,
            executor,
            kill_switch.clone(),
        ));
        Ok((handle, ExecutorSource::Manifest, kill_switch))
    }
}

#[async_trait]
impl<F> Executor for NativeIndexExecutor<F>
where
    F: Future<Output = IndexerResult<()>> + Send,
{
    async fn handle_events(&mut self, blocks: Vec<BlockData>) -> IndexerResult<()> {
        self.db.lock().await.start_transaction().await?;
        let res = (self.handle_events_fn)(blocks, self.db.clone()).await;
        if let Err(e) = res {
            error!("NativeIndexExecutor handle_events failed: {}.", e);
            self.db.lock().await.revert_transaction().await?;
            return Err(IndexerError::NativeExecutionRuntimeError);
        } else {
            self.db.lock().await.commit_transaction().await?;
        }
        Ok(())
    }
}

/// Responsible for loading a single indexer module, triggering events.
#[derive(Debug)]
pub struct WasmIndexExecutor {
    instance: Instance,
    _module: Module,
    _store: Store,
    db: Arc<Mutex<Database>>,
}

impl WasmIndexExecutor {
    pub async fn new(
        config: &IndexerConfig,
        manifest: &Manifest,
        wasm_bytes: impl AsRef<[u8]>,
    ) -> IndexerResult<Self> {
        let db_url = config.database.to_string();
        let store = Store::new(&Universal::new(compiler()).engine());
        let module = Module::new(&store, &wasm_bytes)?;

        let mut import_object = imports! {};

        let mut env = IndexEnv::new(db_url).await?;
        let exports = ffi::get_exports(&env, &store);

        import_object.register("env", exports);

        let instance = Instance::new(&module, &import_object)?;
        env.init_with_instance(&instance)?;
        env.db
            .lock()
            .await
            .load_schema(manifest, Some(&instance))
            .await?;

        if !instance
            .exports
            .contains(ffi::MODULE_ENTRYPOINT.to_string())
        {
            return Err(IndexerError::MissingHandler);
        }

        Ok(WasmIndexExecutor {
            instance,
            _module: module,
            _store: store,
            db: env.db.clone(),
        })
    }

    /// Restore index from wasm file
    pub async fn from_file(
        p: impl AsRef<Path>,
        config: Option<IndexerConfig>,
    ) -> IndexerResult<Self> {
        let config = config.unwrap_or_default();
        let manifest = Manifest::from_file(p)?;
        let bytes = manifest.module_bytes()?;
        Self::new(&config, &manifest, bytes).await
    }

    pub async fn create(
        config: &IndexerConfig,
        manifest: &Manifest,
        exec_source: ExecutorSource,
    ) -> IndexerResult<(JoinHandle<()>, ExecutorSource, Arc<AtomicBool>)> {
        let killer = Arc::new(AtomicBool::new(false));

        match &exec_source {
            ExecutorSource::Manifest => match &manifest.module {
                crate::Module::Wasm(ref module) => {
                    let mut bytes = Vec::<u8>::new();
                    let mut file = File::open(module).await?;
                    file.read_to_end(&mut bytes).await?;

                    let executor =
                        WasmIndexExecutor::new(config, manifest, bytes.clone()).await?;
                    let handle = tokio::spawn(run_executor(
                        config,
                        manifest,
                        executor,
                        killer.clone(),
                    ));

                    Ok((handle, ExecutorSource::Registry(bytes), killer))
                }
                crate::Module::Native => {
                    Err(IndexerError::NativeExecutionInstantiationError)
                }
            },
            ExecutorSource::Registry(bytes) => {
                let executor = WasmIndexExecutor::new(config, manifest, bytes).await?;
                let handle = tokio::spawn(run_executor(
                    config,
                    manifest,
                    executor,
                    killer.clone(),
                ));

                Ok((handle, exec_source, killer))
            }
        }
    }
}

#[async_trait]
impl Executor for WasmIndexExecutor {
    /// Trigger a WASM event handler, passing in a serialized event struct.
    async fn handle_events(&mut self, blocks: Vec<BlockData>) -> IndexerResult<()> {
        let bytes = serialize(&blocks);
        let arg = ffi::WasmArg::new(&self.instance, bytes)?;

        let fun = self
            .instance
            .exports
            .get_native_function::<(u32, u32), ()>(ffi::MODULE_ENTRYPOINT)?;

        self.db.lock().await.start_transaction().await?;

        let ptr = arg.get_ptr();
        let len = arg.get_len();

        let res = spawn_blocking(move || fun.call(ptr, len)).await?;

        if let Err(e) = res {
            error!("WasmIndexExecutor handle_events failed: {}.", e.message());
            let frames = e.trace();
            for (i, frame) in frames.iter().enumerate() {
                println!(
                    "Frame #{}: {:?}::{:?}",
                    i,
                    frame.module_name(),
                    frame.function_name()
                );
            }

            self.db.lock().await.revert_transaction().await?;
            return Err(IndexerError::RuntimeError(e));
        } else {
            self.db.lock().await.commit_transaction().await?;
        }
        Ok(())
    }
}
