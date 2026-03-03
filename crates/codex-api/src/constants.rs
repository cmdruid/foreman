pub const CLIENT_NAME: &str = "foreman";
pub const CLIENT_TITLE: &str = "Codex Foreman";

pub const APP_SERVER_COMMAND: &str = "app-server";
pub const CONNECT_RETRY_ATTEMPTS: usize = 3;
pub const CONNECT_RETRY_DELAY_MS: u64 = 250;
pub const DEFAULT_INITIALIZE_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_INITIALIZED: &str = "initialized";
pub const METHOD_MODEL_LIST: &str = "model/list";
pub const METHOD_THREAD_START: &str = "thread/start";
pub const METHOD_TURN_START: &str = "turn/start";
pub const METHOD_TURN_STEER: &str = "turn/steer";
pub const METHOD_TURN_INTERRUPT: &str = "turn/interrupt";
pub const METHOD_THREAD_INTERRUPT: &str = "thread/interrupt";
pub const REQUEST_STATUS_UNHANDLED: &str = "unhandled";

pub const REQUEST_NOT_HANDLED_ERROR_CODE: i64 = -32601;
pub const REQUEST_NOT_HANDLED_ERROR_MESSAGE: &str = "Request not handled by foreman";

pub const SERVER_REQUEST_METHOD_COMMAND_EXECUTION: &str = "item/commandExecution/requestApproval";
pub const SERVER_REQUEST_METHOD_FILE_CHANGE: &str = "item/fileChange/requestApproval";
pub const SERVER_REQUEST_METHOD_REQUEST_USER_INPUT: &str = "item/requestUserInput";
pub const SERVER_REQUEST_METHOD_TOOL_REQUEST_USER_INPUT: &str = "item/tool/requestUserInput";
pub const SERVER_REQUEST_METHOD_TOOL_CALL: &str = "item/tool/call";
