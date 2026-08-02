//! SQLSTATE error code mapping for PostgreSQL wire protocol.
//!
//! Provides exhaustive mapping from internal database errors to PostgreSQL
//! SQLSTATE codes. No wildcard arms — every error must map to a specific code.

use bytes::BufMut;
use tpt_archon_relational::database::DbError;
use tpt_archon_relational::executor;

/// PostgreSQL wire protocol error types
#[derive(Debug, thiserror::Error)]
pub enum PgWireError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("Authentication error: {0}")]
    Auth(String),
    #[error("Database error: {0}")]
    Database(DbError),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("Invalid message: {0}")]
    InvalidMessage(String),
    #[error("Codec error: {0}")]
    Codec(#[from] crate::codec::CodecError),
    #[error("UTF-8 conversion error: {0}")]
    FromUtf8(#[from] std::string::FromUtf8Error),
}

impl From<tpt_archon_relational::database::DbError> for PgWireError {
    fn from(err: tpt_archon_relational::database::DbError) -> Self {
        PgWireError::Database(err)
    }
}

pub type Result<T> = std::result::Result<T, PgWireError>;

/// PostgreSQL SQLSTATE error codes (5 characters each)
/// See: https://www.postgresql.org/docs/current/errcodes-appendix.html
pub mod sqlstate {
    // Class 00 — Successful Completion
    pub const SUCCESSFUL_COMPLETION: &str = "00000";

    // Class 01 — Warning
    pub const WARNING: &str = "01000";
    pub const DYNAMIC_RESULT_SETS_RETURNED: &str = "0100C";
    pub const IMPLICIT_ZERO_BIT_PADDING: &str = "01008";
    pub const NULL_VALUE_ELIMINATED_IN_SET_FUNCTION: &str = "01003";
    pub const PRIVILEGE_NOT_GRANTED: &str = "01007";
    pub const PRIVILEGE_NOT_REVOKED: &str = "01006";
    pub const STRING_DATA_RIGHT_TRUNCATION: &str = "01004";
    pub const DEPRECATED_FEATURE: &str = "01P01";

    // Class 02 — No Data
    pub const NO_DATA: &str = "02000";
    pub const NO_ADDITIONAL_DYNAMIC_RESULT_SETS_RETURNED: &str = "02001";

    // Class 03 — SQL Statement Not Yet Complete
    pub const SQL_STATEMENT_NOT_YET_COMPLETE: &str = "03000";

    // Class 08 — Connection Exception
    pub const CONNECTION_EXCEPTION: &str = "08000";
    pub const CONNECTION_DOES_NOT_EXIST: &str = "08003";
    pub const CONNECTION_FAILURE: &str = "08006";
    pub const SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION: &str = "08001";
    pub const SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION: &str = "08004";
    pub const TRANSACTION_RESOLUTION_UNKNOWN: &str = "08007";
    pub const PROTOCOL_VIOLATION: &str = "08P01";

    // Class 09 — Triggered Action Exception
    pub const TRIGGERED_ACTION_EXCEPTION: &str = "09000";

    // Class 0A — Feature Not Supported
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";

    // Class 0B — Invalid Transaction Initiation
    pub const INVALID_TRANSACTION_INITIATION: &str = "0B000";

    // Class 0F — Locator Exception
    pub const LOCATOR_EXCEPTION: &str = "0F000";
    pub const INVALID_LOCATOR_SPECIFICATION: &str = "0F001";

    // Class 0L — Invalid Grantor
    pub const INVALID_GRANTOR: &str = "0L000";
    pub const INVALID_GRANT_OPERATION: &str = "0LP01";

    // Class 0P — Invalid Role Specification
    pub const INVALID_ROLE_SPECIFICATION: &str = "0P000";

    // Class 20 — Case Not Found
    pub const CASE_NOT_FOUND: &str = "20000";

    // Class 21 — Cardinality Violation
    pub const CARDINALITY_VIOLATION: &str = "21000";

    // Class 22 — Data Exception
    pub const DATA_EXCEPTION: &str = "22000";
    pub const ARRAY_SUBSCRIPT_ERROR: &str = "2202E";
    pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
    pub const CARDINALITY_MISMATCH: &str = "2202G";
    pub const DATA_EXCEPTION_INVALID_ESCAPE_CHARACTER: &str = "22P06";
    pub const DATA_EXCEPTION_INVALID_ESCAPE_OCTET: &str = "22P07";
    pub const DATA_EXCEPTION_INVALID_ESCAPE_SEQUENCE: &str = "22P08";
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    pub const DIVISION_BY_ZERO: &str = "22012";
    pub const ERROR_IN_ASSIGNMENT: &str = "22005";
    pub const ESCAPE_CHARACTER_CONFLICT: &str = "2200B";
    pub const INDICATOR_OVERFLOW: &str = "22022";
    pub const INTERVAL_FIELD_OVERFLOW: &str = "22015";
    pub const INVALID_ARGUMENT_FOR_LOGARITHM: &str = "2201E";
    pub const INVALID_ARGUMENT_FOR_NTILE_FUNCTION: &str = "22014";
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
    pub const INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION: &str = "2201G";
    pub const INVALID_CHARACTER_VALUE_FOR_CAST: &str = "22018";
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    pub const INVALID_ESCAPE_CHARACTER: &str = "22019";
    pub const INVALID_ESCAPE_OCTET: &str = "2200D";
    pub const INVALID_ESCAPE_SEQUENCE: &str = "22025";
    pub const INVALID_INDICATOR_PARAMETER_VALUE: &str = "22010";
    pub const INVALID_LIMIT_VALUE: &str = "2201W";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const INVALID_PRECEDING_OR_FOLLOWING_SIZE: &str = "2201X";
    pub const INVALID_REGULAR_EXPRESSION: &str = "2201B";
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: &str = "2201V";
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: &str = "2201U";
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
    pub const INVALID_USE_OF_ESCAPE_CHARACTER: &str = "2200C";
    pub const MOST_SPECIFIC_TYPE_MISMATCH: &str = "2200G";
    pub const NULL_VALUE_NOT_ALLOWED: &str = "22004";
    pub const NULL_VALUE_NO_INDICATOR_PARAMETER: &str = "22002";
    pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
    pub const STRING_DATA_LENGTH_MISMATCH: &str = "22026";
    pub const STRING_DATA_RIGHT_TRUNCATION_22: &str = "22001";
    pub const SUBSTRING_ERROR: &str = "22011";
    pub const TRIM_ERROR: &str = "22027";
    pub const UNTERMINATED_C_STRING: &str = "22024";
    pub const ZERO_LENGTH_CHARACTER_STRING: &str = "2200F";

    // Class 23 — Integrity Constraint Violation
    pub const INTEGRITY_CONSTRAINT_VIOLATION: &str = "23000";
    pub const RESTRICT_VIOLATION: &str = "23001";
    pub const NOT_NULL_VIOLATION: &str = "23502";
    pub const FOREIGN_KEY_VIOLATION: &str = "23503";
    pub const UNIQUE_VIOLATION: &str = "23505";
    pub const CHECK_VIOLATION: &str = "23514";
    pub const EXCLUSION_VIOLATION: &str = "23P01";

    // Class 24 — Invalid Cursor State
    pub const INVALID_CURSOR_STATE: &str = "24000";

    // Class 25 — Invalid Transaction State
    pub const INVALID_TRANSACTION_STATE: &str = "25000";
    pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
    pub const BRANCH_TRANSACTION_ALREADY_ACTIVE: &str = "25002";
    pub const HELD_CURSOR_REQUIRES_SAME_BRANCH: &str = "25003";
    pub const INAPPROPRIATE_ACCESS_MODE_FOR_BRANCH_TRANSACTION: &str = "25004";
    pub const INAPPROPRIATE_ISOLATION_LEVEL_FOR_BRANCH_TRANSACTION: &str = "25005";
    pub const NO_ACTIVE_SQL_TRANSACTION_FOR_BRANCH_TRANSACTION: &str = "25006";
    pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
    pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";
    pub const IDLE_IN_TRANSACTION_SESSION_TIMEOUT: &str = "25P03";

    // Class 26 — Invalid SQL Statement Name
    pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";

    // Class 27 — Triggered Data Change Violation
    pub const TRIGGERED_DATA_CHANGE_VIOLATION: &str = "27000";

    // Class 28 — Invalid Authorization Specification
    pub const INVALID_AUTHORIZATION_SPECIFICATION: &str = "28000";
    pub const INVALID_PASSWORD: &str = "28P01";

    // Class 2B — Dependent Privilege Descriptors Still Exist
    pub const DEPENDENT_PRIVILEGE_DESCRIPTORS_STILL_EXIST: &str = "2B000";
    pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";

    // Class 2D — Invalid Transaction Termination
    pub const INVALID_TRANSACTION_TERMINATION: &str = "2D000";

    // Class 2F — SQL Routine Exception
    pub const SQL_ROUTINE_EXCEPTION: &str = "2F000";
    pub const FUNCTION_EXECUTED_NO_RETURN_STATEMENT: &str = "2F005";
    pub const MODIFYING_SQL_DATA_NOT_PERMITTED: &str = "2F002";
    pub const PROHIBITED_SQL_STATEMENT_ATTEMPTED: &str = "2F003";
    pub const READING_SQL_DATA_NOT_PERMITTED: &str = "2F004";

    // Class 34 — Invalid Cursor Name
    pub const INVALID_CURSOR_NAME: &str = "34000";

    // Class 38 — External Routine Exception
    pub const EXTERNAL_ROUTINE_EXCEPTION: &str = "38000";
    pub const CONTAINING_SQL_NOT_PERMITTED: &str = "38001";
    pub const MODIFYING_SQL_DATA_NOT_PERMITTED_38: &str = "38002";
    pub const PROHIBITED_SQL_STATEMENT_ATTEMPTED_38: &str = "38003";
    pub const READING_SQL_DATA_NOT_PERMITTED_38: &str = "38004";

    // Class 39 — External Routine Invocation Exception
    pub const EXTERNAL_ROUTINE_INVOCATION_EXCEPTION: &str = "39000";
    pub const INVALID_SQLSTATE_RETURNED: &str = "39001";
    pub const NULL_VALUE_NOT_ALLOWED_39: &str = "39004";
    pub const TRIGGER_PROTOCOL_VIOLATION: &str = "39P01";
    pub const SRF_PROTOCOL_VIOLATION: &str = "39P02";
    pub const EVENT_TRIGGER_PROTOCOL_VIOLATION: &str = "39P03";

    // Class 3B — Savepoint Exception
    pub const SAVEPOINT_EXCEPTION: &str = "3B000";
    pub const INVALID_SAVEPOINT_SPECIFICATION: &str = "3B001";

    // Class 3D — Invalid Catalog Name
    pub const INVALID_CATALOG_NAME: &str = "3D000";

    // Class 3F — Invalid Schema Name
    pub const INVALID_SCHEMA_NAME: &str = "3F000";

    // Class 40 — Transaction Rollback
    pub const TRANSACTION_ROLLBACK: &str = "40000";
    pub const TRANSACTION_INTEGRITY_CONSTRAINT_VIOLATION: &str = "40002";
    pub const SERIALIZATION_FAILURE: &str = "40001";
    pub const STATEMENT_COMPLETION_UNKNOWN: &str = "40003";
    pub const DEADLOCK_DETECTED: &str = "40P01";

    // Class 42 — Syntax Error or Access Rule Violation
    pub const SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION: &str = "42000";
    pub const SYNTAX_ERROR: &str = "42601";
    pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
    pub const CANNOT_COERCE: &str = "42846";
    pub const GROUPING_ERROR: &str = "42803";
    pub const INAPPROPRIATE_USE_OF_WINDOW_FUNCTION: &str = "42P20";
    pub const INVALID_COLUMN_REFERENCE: &str = "42P22";
    pub const INVALID_CURSOR_DEFINITION: &str = "42P21";
    pub const INVALID_DATABASE_DEFINITION: &str = "42P12";
    pub const INVALID_FUNCTION_DEFINITION: &str = "42P13";
    pub const INVALID_PREPARED_STATEMENT_DEFINITION: &str = "42P19";
    pub const INVALID_SCHEMA_DEFINITION: &str = "42P14";
    pub const INVALID_TABLE_DEFINITION: &str = "42P16";
    pub const INVALID_OBJECT_DEFINITION: &str = "42P23";
    pub const INVALID_COLUMN_DEFINITION: &str = "42P24";
    pub const INVALID_TYPE_DEFINITION: &str = "42P17";
    pub const INVALID_XML_DEFINITION: &str = "42P18";
    pub const MULTIPLE_DISTINCTIONS: &str = "42P06";
    pub const NON_UNIQUE_TABLE_DEFINITION: &str = "42723";
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "42P15";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_DATABASE: &str = "42P04";
    pub const UNDEFINED_FUNCTION: &str = "42883";
    pub const UNDEFINED_OBJECT: &str = "42704";
    pub const UNDEFINED_PARAMETER: &str = "42P02";
    pub const UNDEFINED_SCHEMA: &str = "42P03";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const UNDEFINED_TYPE: &str = "42P05";
    pub const UNDEFINED_VALUE: &str = "42P24";
    pub const DUPLICATE_COLUMN: &str = "42701";
    pub const DUPLICATE_CURSOR: &str = "42P07";
    pub const DUPLICATE_DATABASE: &str = "42P04";
    pub const DUPLICATE_FUNCTION: &str = "42723";
    pub const DUPLICATE_PREPARED_STATEMENT: &str = "42P07";
    pub const DUPLICATE_SCHEMA: &str = "42P06";
    pub const DUPLICATE_TABLE: &str = "42P07";
    pub const DUPLICATE_ALIAS: &str = "42712";
    pub const DUPLICATE_OBJECT: &str = "42710";
    pub const AMBIGUOUS_COLUMN: &str = "42702";
    pub const AMBIGUOUS_FUNCTION: &str = "42725";
    pub const AMBIGUOUS_PARAMETER: &str = "42P08";
    pub const AMBIGUOUS_ALIAS: &str = "42P09";
    pub const INVALID_COLUMN_DEFINITION_42: &str = "42P24";
    pub const INVALID_TABLE_DEFINITION_42: &str = "42P16";
    pub const INVALID_OBJECT_DEFINITION_42: &str = "42P23";

    // Class 44 — WITH CHECK OPTION Violation
    pub const WITH_CHECK_OPTION_VIOLATION: &str = "44000";

    // Class 53 — Insufficient Resources
    pub const INSUFFICIENT_RESOURCES: &str = "53000";
    pub const DISK_FULL: &str = "53100";
    pub const OUT_OF_MEMORY: &str = "53200";
    pub const TOO_MANY_CONNECTIONS: &str = "53300";
    pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
    pub const STATEMENT_TOO_COMPLEX: &str = "54001";
    pub const TOO_MANY_COLUMNS: &str = "54011";
    pub const TOO_MANY_ARGUMENTS: &str = "54023";

    // Class 55 — Object Not In Prerequisite State
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE_55: &str = "55000";
    pub const OBJECT_IN_USE: &str = "55006";
    pub const CANT_CHANGE_RUNTIME_PARAM: &str = "55P02";
    pub const LOCK_NOT_AVAILABLE: &str = "55P03";

    // Class 57 — Operator Intervention
    pub const OPERATOR_INTERVENTION: &str = "57000";
    pub const QUERY_CANCELED: &str = "57014";
    pub const ADMIN_SHUTDOWN: &str = "57P01";
    pub const CRASH_SHUTDOWN: &str = "57P02";
    pub const CANNOT_CONNECT_NOW: &str = "57P03";
    pub const DATABASE_DROPPED: &str = "57P04";

    // Class 58 — System Error
    pub const SYSTEM_ERROR: &str = "58000";
    pub const IO_ERROR: &str = "58030";
    pub const UNDEFINED_FILE: &str = "58P01";
    pub const DUPLICATE_FILE: &str = "58P02";

    // Class 72 — Snapshot Isolation Failure
    pub const SNAPSHOT_ISOLATION_FAILURE: &str = "72000";

    // Class F0 — Configuration File Error
    pub const CONFIG_FILE_ERROR: &str = "F0000";
    pub const LOCK_FILE_EXISTS: &str = "F0001";

    // Class HV — Foreign Data Wrapper Error
    pub const FDW_ERROR: &str = "HV000";
    pub const FDW_COLUMN_NAME_NOT_FOUND: &str = "HV005";
    pub const FDW_DYNAMIC_PARAMETER_VALUE_NEEDED: &str = "HV002";
    pub const FDW_FUNCTION_SEQUENCE_ERROR: &str = "HV004";
    pub const FDW_INCONSISTENT_DESCRIPTOR_INFORMATION: &str = "HV003";
    pub const FDW_INVALID_ATTRIBUTE_VALUE: &str = "HV007";
    pub const FDW_INVALID_COLUMN_NAME: &str = "HV006";
    pub const FDW_INVALID_COLUMN_NUMBER: &str = "HV008";
    pub const FDW_INVALID_DATA_TYPE: &str = "HV001";
    pub const FDW_INVALID_DATA_TYPE_DESCRIPTORS: &str = "HV009";
    pub const FDW_INVALID_DESCRIPTOR_FIELD_IDENTIFIER: &str = "HV00A";
    pub const FDW_INVALID_HANDLE: &str = "HV00B";
    pub const FDW_INVALID_OPTION_INDEX: &str = "HV00C";
    pub const FDW_INVALID_OPTION_NAME: &str = "HV00D";
    pub const FDW_INVALID_STRING_LENGTH_OR_BUFFER_LENGTH: &str = "HV00E";
    pub const FDW_INVALID_STRING_FORMAT: &str = "HV00F";
    pub const FDW_INVALID_USE_OF_NULL_POINTER: &str = "HV00G";
    pub const FDW_TOO_MANY_HANDLES: &str = "HV00H";
    pub const FDW_OUT_OF_MEMORY: &str = "HV00J";
    pub const FDW_NO_SCHEMAS: &str = "HV00K";
    pub const FDW_OPTION_NAME_NOT_FOUND: &str = "HV00L";
    pub const FDW_REPLY_HANDLE: &str = "HV00M";
    pub const FDW_SCHEMA_NOT_FOUND: &str = "HV00N";
    pub const FDW_TABLE_NOT_FOUND: &str = "HV00O";
    pub const FDW_UNABLE_TO_CREATE_EXECUTION: &str = "HV00P";
    pub const FDW_UNABLE_TO_CREATE_REPLY: &str = "HV00Q";
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION: &str = "HV00R";
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION_WITH_FDW: &str = "HV00S";
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION_TO_FOREIGN_SERVER: &str = "HV00T";
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION_TO_LOCAL_SERVER: &str = "HV00U";
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION_TO_REMOTE_SERVER: &str = "HV00V";
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION_TO_SERVER: &str = "HV00W";
}

/// Map a DbError to a PostgreSQL SQLSTATE code.
/// This is an exhaustive match - no wildcard arm allowed.
pub fn db_error_to_sqlstate(err: &DbError) -> &'static str {
    use sqlstate::*;
    use DbError::*;

    match err {
        // Transaction errors
        TransactionError(msg) if msg.contains("conflict") => SERIALIZATION_FAILURE,
        TransactionError(msg) if msg.contains("already in progress") => ACTIVE_SQL_TRANSACTION,
        TransactionError(msg) if msg.contains("no active transaction") => NO_ACTIVE_SQL_TRANSACTION,
        TransactionError(_) => INVALID_TRANSACTION_STATE,

        // Schema/table errors
        UnknownTable(_) => UNDEFINED_TABLE,
        UnknownColumn(_) => UNDEFINED_COLUMN,
        TableAlreadyExists(_) => DUPLICATE_TABLE,
        ViewAlreadyExists(_) => DUPLICATE_TABLE,

        // Data errors
        ArityMismatch => DATA_EXCEPTION,
        ColumnCountMismatch => DATA_EXCEPTION,
        RowNotFound(_) => NO_DATA,
        CorruptRow(_) => DATA_EXCEPTION,

        // Type errors
        TypeMismatch => DATA_EXCEPTION,
        ColumnTypeMismatch(_) => DATA_EXCEPTION,
        NotAVectorColumn(_) => DATA_EXCEPTION,
        MissingParam => DATA_EXCEPTION,
        SubqueryCardinality(_) => DATA_EXCEPTION,

        // Not yet implemented features
        Unsupported(_) => FEATURE_NOT_SUPPORTED,

        // IO errors
        Exec(executor::ExecError::UnknownColumn(_)) => UNDEFINED_COLUMN,
        Exec(executor::ExecError::TypeMismatch) => DATA_EXCEPTION,
        Exec(executor::ExecError::GroupByColumnNotFound(_)) => UNDEFINED_COLUMN,
        Exec(executor::ExecError::UnresolvedSubquery) => FEATURE_NOT_SUPPORTED,

        // Internal errors
        UnknownView(_) => UNDEFINED_TABLE,
        RecursiveView(_) => INVALID_OBJECT_DEFINITION,
    }
}

/// Internal error SQLSTATE (for errors that don't map cleanly)
pub const INTERNAL_ERROR: &str = "XX000";

/// Convert a PgWireError to SQLSTATE
pub fn pgwire_error_to_sqlstate(err: &PgWireError) -> &'static str {
    use sqlstate::*;
    use PgWireError::*;

    match err {
        Io(_) => IO_ERROR,
        Protocol(_) => PROTOCOL_VIOLATION,
        Auth(_) => INVALID_AUTHORIZATION_SPECIFICATION,
        Database(e) => db_error_to_sqlstate(e),
        Utf8(_) => DATA_EXCEPTION,
        InvalidMessage(_) => PROTOCOL_VIOLATION,
        Codec(_) => PROTOCOL_VIOLATION,
        FromUtf8(_) => DATA_EXCEPTION,
    }
}

/// Build an ErrorResponse message for a given error
pub fn build_error_response(err: &PgWireError) -> Vec<u8> {
    use crate::codec::message::ErrorFieldType;
    use crate::codec::{write_cstring, write_message};
    use bytes::{BufMut, BytesMut};

    let sqlstate = pgwire_error_to_sqlstate(err);
    let mut payload = BytesMut::new();

    // Severity
    payload.put_u8(ErrorFieldType::Severity as u8);
    write_cstring(&mut payload, "ERROR");

    // SQLSTATE
    payload.put_u8(ErrorFieldType::SqlState as u8);
    write_cstring(&mut payload, sqlstate);

    // Message
    payload.put_u8(ErrorFieldType::Message as u8);
    write_cstring(&mut payload, &err.to_string());

    // Terminator
    payload.put_u8(0);

    let mut buf = BytesMut::new();
    write_message(&mut buf, b'E', &payload);
    buf.to_vec()
}

/// Build a NoticeResponse message
pub fn build_notice_response(severity: &str, sqlstate: &str, message: &str) -> Vec<u8> {
    use crate::codec::message::ErrorFieldType;
    use crate::codec::{write_cstring, write_message};
    use bytes::BytesMut;

    let mut payload = BytesMut::new();

    payload.put_u8(ErrorFieldType::Severity as u8);
    write_cstring(&mut payload, severity);

    payload.put_u8(ErrorFieldType::SqlState as u8);
    write_cstring(&mut payload, sqlstate);

    payload.put_u8(ErrorFieldType::Message as u8);
    write_cstring(&mut payload, message);

    payload.put_u8(0);

    let mut buf = BytesMut::new();
    write_message(&mut buf, b'N', &payload);
    buf.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpt_archon_relational::database::DbError;

    #[test]
    fn test_transaction_conflict_maps_to_serialization_failure() {
        let err = DbError::TransactionError("commit conflict on table 't'".to_string());
        assert_eq!(db_error_to_sqlstate(&err), sqlstate::SERIALIZATION_FAILURE);
    }

    #[test]
    fn test_unknown_table_maps_to_undefined_table() {
        let err = DbError::UnknownTable("nonexistent".to_string());
        assert_eq!(db_error_to_sqlstate(&err), sqlstate::UNDEFINED_TABLE);
    }

    #[test]
    fn test_unknown_column_maps_to_undefined_column() {
        let err = DbError::UnknownColumn("bad_col".to_string());
        assert_eq!(db_error_to_sqlstate(&err), sqlstate::UNDEFINED_COLUMN);
    }

    #[test]
    fn test_arity_mismatch_maps_to_data_exception() {
        let err = DbError::ArityMismatch;
        assert_eq!(db_error_to_sqlstate(&err), sqlstate::DATA_EXCEPTION);
    }
}
