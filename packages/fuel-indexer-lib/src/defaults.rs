pub const FUEL_NODE_HOST: &str = "localhost";
pub const FUEL_NODE_PORT: &str = "4000";

pub const GRAPHQL_API_HOST: &str = "localhost";
pub const GRAPHQL_API_PORT: &str = "29987";

pub const DATABASE: &str = "postgres";
pub const POSTGRES_DATABASE: &str = "postgres";
pub const POSTGRES_USER: &str = "postgres";
pub const POSTGRES_HOST: &str = "localhost";
pub const POSTGRES_PORT: &str = "5432";
pub const POSTGRES_PASSWORD: &str = "postgres";

pub const INDEX_FAILED_CALLS: usize = 10;
pub const STOP_IDLE_INDEXERS: bool = false;

pub const MAX_BODY_SIZE: usize = 5242880; // 5MB

pub const SERVICE_REQUEST_CHANNEL_SIZE: usize = 100;
pub const IDLE_SERVICE_WAIT_SECS: u64 = 3;

pub const MAX_DATABASE_CONNECTION_ATTEMPTS: usize = 5;
pub const INITIAL_RETRY_DELAY_SECS: u64 = 2;
pub const MAX_EMPTY_BLOCK_REQUESTS: usize = 10;

pub const DELAY_FOR_SERVICE_ERR: u64 = 5;
pub const DELAY_FOR_EMPTY_PAGE: u64 = 1;

pub const RUN_MIGRATIONS: bool = true;
pub const USE_METRICS: bool = true;

pub const FUEL_HOME_DIR: &str = ".fuel";
pub const INDEXER_CONFIG_DIR: &str = "indexer";

pub const ROOT_DIRECTORY_NAME: &str = "fuel-indexer";
pub const MESSAGE_PADDING: usize = 55;
pub const SUCCESS_EMOJI_PADDING: usize = 3;
pub const FAIL_EMOJI_PADDING: usize = 6;
pub const HEADER_PADDING: usize = 20;

pub const FORC_INDEX: &str = "forc-index";

pub const AUTH_ENABLED: bool = false;
pub const JWT_EXPIRY_SECS: usize = 2592000; // 30 days

pub const ACCOUNT_INDEX: &str = "0";

pub const VERBOSE_LOGGING: bool = false;
pub const VERBOSE_DB_LOGGING: &str = "false";

pub const NODE_GRAPHQL_PAGE_SIZE: usize = 10;

pub const LOCAL_FUEL_NODE: bool = false;

pub const INDEXER_NET_CONFIG: bool = false;
