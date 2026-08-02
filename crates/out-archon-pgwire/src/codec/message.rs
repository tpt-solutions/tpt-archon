//! PostgreSQL wire protocol message types (frontend → backend and backend → frontend).

use alloc::string::String;
use alloc::vec::Vec;

/// A frontend message type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontendTag(pub u8);

impl FrontendTag {
    pub const STARTUP: Self = Self(0x00);
    pub const QUERY: Self = Self(b'Q');
    pub const TERMINATE: Self = Self(b'X');
    pub const PARSE: Self = Self(b'P');
    pub const BIND: Self = Self(b'B');
    pub const DESCRIBE: Self = Self(b'D');
    pub const EXECUTE: Self = Self(b'E');
    pub const SYNC: Self = Self(b'S');
    pub const CLOSE: Self = Self(b'C');
    pub const PASSWORD: Self = Self(b'p');
    pub const SSL: Self = Self(b'\x00');
}

/// A backend message type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendTag(pub u8);

impl BackendTag {
    pub const AUTH: Self = Self(b'R');
    pub const BACKEND_KEY: Self = Self(b'K');
    pub const PARAM_STATUS: Self = Self(b'S');
    pub const READY_FOR_QUERY: Self = Self(b'Z');
    pub const ROW_DESCRIPTION: Self = Self(b'T');
    pub const DATA_ROW: Self = Self(b'D');
    pub const COMMAND_COMPLETE: Self = Self(b'C');
    pub const ERROR_RESPONSE: Self = Self(b'E');
    pub const NOTICE: Self = Self(b'N');
    pub const NO_DATA: Self = Self(b'n');
    pub const PARSE_COMPLETE: Self = Self(b'1');
    pub const BIND_COMPLETE: Self = Self(b'2');
    pub const CLOSE_COMPLETE: Self = Self(b'3');
    pub const PORTAL_SUSPEND: Self = Self(b's');
    pub const AUTH_SASL: Self = Self(b'R'); // SASL authentication (sub-type in payload)
    pub const AUTH_SASL_CONTINUE: Self = Self(b'R');
    pub const AUTH_SASL_FINAL: Self = Self(b'R');
}

/// Startup message payload: key-value pairs (no leading type byte, length prefix only).
#[derive(Debug, Clone, Default)]
pub struct StartupMessage {
    pub protocol_major: i32,
    pub protocol_minor: i32,
    pub params: Vec<(String, String)>,
}

impl StartupMessage {
    pub const PROTOCOL_VERSION: i32 = 196608; // 3.0
}

/// A single frontend message: type byte + length + payload.
#[derive(Debug, Clone)]
pub enum FrontendMessage {
    Startup(StartupMessage),
    Query(String),
    Terminate,
    Parse,
    Bind,
    Describe,
    Execute,
    Sync,
    Close,
    Password(String),
    SslRequest,
}

impl FrontendMessage {
    pub fn tag(&self) -> FrontendTag {
        match self {
            FrontendMessage::Startup(_) => FrontendTag::STARTUP,
            FrontendMessage::Query(_) => FrontendTag::QUERY,
            FrontendMessage::Terminate => FrontendTag::TERMINATE,
            FrontendMessage::Parse => FrontendTag::PARSE,
            FrontendMessage::Bind => FrontendTag::BIND,
            FrontendMessage::Describe => FrontendTag::DESCRIBE,
            FrontendMessage::Execute => FrontendTag::EXECUTE,
            FrontendMessage::Sync => FrontendTag::SYNC,
            FrontendMessage::Close => FrontendTag::CLOSE,
            FrontendMessage::Password(_) => FrontendTag::PASSWORD,
            FrontendMessage::SslRequest => FrontendTag::SSL,
        }
    }
}

/// Backend response messages.
#[derive(Debug, Clone)]
pub enum BackendMessage {
    AuthOk,
    AuthPlain,
    BackendKeyData {
        pid: i32,
        secret: i32,
    },
    ParameterStatus {
        name: String,
        value: String,
    },
    ReadyForQuery {
        txn_status: TxnStatus,
    },
    RowDescription(Vec<ColumnDesc>),
    DataRow(Vec<Option<Vec<u8>>>),
    CommandComplete {
        tag: String,
    },
    ErrorResponse {
        fields: Vec<(u8, String)>,
        sqlstate: Option<[u8; 5]>,
    },
    NoticeResponse {
        fields: Vec<(u8, String)>,
    },
    NoData,
    ParseComplete,
    BindComplete,
    CloseComplete,
    PortalSuspended,
}

/// Column description for RowDescription messages.
#[derive(Debug, Clone)]
pub struct ColumnDesc {
    pub name: String,
    pub table_oid: i32,
    pub column_attr: i16,
    pub type_oid: i32,
    pub type_size: i16,
    pub type_mod: i32,
    pub format: i16, // 0=text, 1=binary
}

/// Transaction status for ReadyForQuery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStatus {
    Idle = b'I' as isize,
    InTransaction = b'T' as isize,
    Failed = b'E' as isize,
}

impl TxnStatus {
    pub fn as_byte(&self) -> u8 {
        *self as u8
    }
}

/// Error field types for ErrorResponse and NoticeResponse messages.
/// See: https://www.postgresql.org/docs/current/protocol-message-formats.html
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorFieldType {
    Severity = b'S',
    SqlState = b'C',
    Message = b'M',
    Detail = b'D',
    Hint = b'H',
    Position = b'P',
    InternalPosition = b'p',
    InternalQuery = b'q',
    Where = b'W',
    Schema = b's',
    Table = b't',
    Column = b'c',
    DataType = b'd',
    Constraint = b'n',
    File = b'F',
    Line = b'L',
    Routine = b'R',
}
