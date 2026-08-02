//! SQLSTATE error codes: exhaustive mapping from [`DbError`] to the 5-character
//! SQLSTATE class/subclass defined by the SQL standard and PostgreSQL.

use tpt_archon_relational::database::DbError;

/// A 5-byte SQLSTATE code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlState(pub [u8; 5]);

impl SqlState {
    pub const SUCCESSFUL_COMPLETION: Self = Self(*b"00000");
    pub const WARNING: Self = Self(*b"01000");
    pub const NO_DATA: Self = Self(*b"02000");
    pub const FEATURE_NOT_SUPPORTED: Self = Self(*b"0A000");
    pub const DUPLICATE_TABLE: Self = Self(*b"42P07");
    pub const UNDEFINED_TABLE: Self = Self(*b"42P01");
    pub const UNDEFINED_COLUMN: Self = Self(*b"42703");
    pub const SYNTAX_ERROR: Self = Self(*b"42601");
    pub const DATA_EXCEPTION: Self = Self(*b"22000");
    pub const INTERNAL_ERROR: Self = Self(*b"XX000");
    pub const ACTIVE_SQL_TRANSACTION: Self = Self(*b"25001");
    pub const NO_ACTIVE_SQL_TRANSACTION: Self = Self(*b"25000");
    pub const IN_FAILED_SQL_TRANSACTION: Self = Self(*b"25P02");

    pub fn as_bytes(&self) -> &[u8; 5] {
        &self.0
    }
}

/// Maps a [`DbError`] to its SQLSTATE code. Every variant is matched exhaustively
/// so no wildcard or generic `XX000` leak goes unnoticed during review.
pub fn sqlstate_for_db_error(err: &DbError) -> SqlState {
    match err {
        DbError::UnknownColumn(_) => SqlState::UNDEFINED_COLUMN,
        DbError::TypeMismatch => SqlState::DATA_EXCEPTION,
        DbError::ColumnTypeMismatch(_) => SqlState::DATA_EXCEPTION,
        DbError::ArityMismatch => SqlState::DATA_EXCEPTION,
        DbError::NotAVectorColumn(_) => SqlState::DATA_EXCEPTION,
        DbError::MissingParam => SqlState::DATA_EXCEPTION,
        DbError::RowNotFound(_) => SqlState::INTERNAL_ERROR,
        DbError::CorruptRow(_) => SqlState::DATA_EXCEPTION,
        DbError::UnknownTable(_) => SqlState::UNDEFINED_TABLE,
        DbError::UnknownView(_) => SqlState::UNDEFINED_TABLE,
        DbError::TransactionError(_) => SqlState::ACTIVE_SQL_TRANSACTION,
        DbError::TableAlreadyExists(_) => SqlState::DUPLICATE_TABLE,
        DbError::ViewAlreadyExists(_) => SqlState::DUPLICATE_TABLE,
        DbError::RecursiveView(_) => SqlState::SYNTAX_ERROR,
        DbError::Unsupported(_) => SqlState::FEATURE_NOT_SUPPORTED,
        DbError::ColumnCountMismatch => SqlState::DATA_EXCEPTION,
        DbError::SubqueryCardinality(_) => SqlState::DATA_EXCEPTION,
        DbError::Exec(_) => SqlState::INTERNAL_ERROR,
    }
}
