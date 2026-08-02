//! PostgreSQL compatibility shims for the wire protocol.
//!
//! Handles statement splitting, comment stripping, SET/SHOW/RESET commands,
//! SELECT <const> health checks, and transaction keyword synonyms.

use crate::error::PgWireError;
use tpt_archon_relational::parser::Statement;

/// Strip PostgreSQL-style comments from SQL text
/// Handles -- line comments and /* block comments */ (including nested)
pub fn strip_comments(sql: &str) -> String {
    let mut result = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_line_comment = false;
    let mut block_comment_depth = 0;

    while let Some(c) = chars.next() {
        if block_comment_depth > 0 {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next(); // consume '/'
                block_comment_depth -= 1;
            } else if c == '/' && chars.peek() == Some(&'*') {
                chars.next(); // consume '*'
                block_comment_depth += 1;
            }
            // Inside block comment - skip character
        } else if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                result.push(c); // Keep the newline
            }
            // Inside line comment - skip character
        } else if c == '-' && chars.peek() == Some(&'-') {
            chars.next(); // consume second '-'
            in_line_comment = true;
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next(); // consume '*'
            block_comment_depth = 1;
        } else {
            result.push(c);
        }
    }

    result
}

/// Split SQL text into individual statements by semicolon
/// Respects string literals and dollar-quoted strings
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_dollar_quote = false;
    // Dollar tag stores just the tag name (e.g., "" for $$, "tag" for $tag$)
    let mut dollar_tag = String::new();

    while let Some(c) = chars.next() {
        if in_dollar_quote {
            current.push(c);
            // Check for end of dollar quote
            if c == '$' {
                let mut temp_chars = chars.clone();

                // For $$ closing, check if next char is $ and dollar_tag is empty
                // For $tag$ closing, collect tag and check for closing $
                let mut tag_check = String::new();
                let mut is_empty_closing = false;

                if temp_chars.peek() == Some(&'$') && dollar_tag.is_empty() {
                    // It's $$ closing - the next char is $
                    is_empty_closing = true;
                } else {
                    // Collect tag for $tag$ closing
                    for tc in temp_chars.by_ref() {
                        if tc.is_ascii_alphanumeric() || tc == '_' {
                            tag_check.push(tc);
                        } else {
                            break;
                        }
                    }
                }

                if is_empty_closing {
                    // Consume the closing $
                    chars.next();
                    current.push('$');
                    in_dollar_quote = false;
                    dollar_tag.clear();
                } else if temp_chars.peek() == Some(&'$') && tag_check == dollar_tag {
                    // Found matching end tag - consume it
                    for _ in 0..tag_check.len() {
                        if let Some(cc) = chars.next() {
                            current.push(cc);
                        }
                    }
                    if let Some(cc) = chars.next() {
                        current.push(cc); // closing $
                    }
                    in_dollar_quote = false;
                    dollar_tag.clear();
                }
            }
        } else if in_single_quote {
            current.push(c);
            if c == '\'' && chars.peek() == Some(&'\'') {
                chars.next(); // escaped quote
                current.push('\'');
            } else if c == '\'' {
                in_single_quote = false;
            }
        } else if in_double_quote {
            current.push(c);
            if c == '"' && chars.peek() == Some(&'"') {
                chars.next(); // escaped quote
                current.push('"');
            } else if c == '"' {
                in_double_quote = false;
            }
        } else if c == '\'' {
            in_single_quote = true;
            current.push(c);
        } else if c == '"' {
            in_double_quote = true;
            current.push(c);
        } else if c == '$' {
            // Check for dollar quote start - could be $$ or $tag$
            let mut temp_chars = chars.clone();
            let mut tag = String::new();

            // Peek at the next character
            if let Some(&next_c) = temp_chars.peek() {
                if next_c == '$' {
                    // It's $$ - empty tag
                    dollar_tag.clear(); // empty string for $$
                    current.push('$');
                    current.push('$');
                    chars.next(); // consume the second $
                    in_dollar_quote = true;
                } else {
                    // Collect tag until we hit $ or non-alphanumeric
                    for tc in temp_chars.by_ref() {
                        if tc.is_ascii_alphanumeric() || tc == '_' {
                            tag.push(tc);
                        } else {
                            break;
                        }
                    }
                    if temp_chars.peek() == Some(&'$') {
                        // It's a $tag$ dollar quote
                        dollar_tag = tag.clone();
                        current.push('$');
                        current.push_str(&tag);
                        current.push('$');
                        // Consume the tag and closing $
                        for _ in 0..tag.len() {
                            if let Some(cc) = chars.next() {
                                current.push(cc);
                            }
                        }
                        if let Some(cc) = chars.next() {
                            current.push(cc);
                        }
                        in_dollar_quote = true;
                    } else {
                        // Not a dollar quote, just a $ character
                        current.push(c);
                    }
                }
            } else {
                // End of input after $
                current.push(c);
            }
        } else if c == ';' {
            // End of statement (not in any quote)
            let stmt = current.trim().to_string();
            if !stmt.is_empty() {
                statements.push(stmt);
            }
            current.clear();
        } else {
            current.push(c);
        }
    }

    // Don't forget the last statement if no trailing semicolon
    let stmt = current.trim().to_string();
    if !stmt.is_empty() {
        statements.push(stmt);
    }

    statements
}

/// Normalize a statement for compatibility handling
/// Handles: SET, SHOW, RESET, SELECT <const>, transaction synonyms
pub fn normalize_statement(stmt: &str) -> Result<NormalizedStatement, PgWireError> {
    let trimmed = stmt.trim();
    let upper = trimmed.to_uppercase();

    // Check for SET command
    if upper.starts_with("SET ") {
        // Use the uppercased version for keyword matching but trimmed for value extraction
        let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
        if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("SESSION") {
            // SET SESSION name = value
            let rest = parts[2].trim();
            if let Some(eq_pos) = rest.find('=') {
                let name = rest[..eq_pos].trim().to_string();
                let value = rest[eq_pos + 1..].trim().to_string();
                return Ok(NormalizedStatement::SetParameter { name, value });
            }
        } else if parts.len() >= 2 {
            // SET name = value - need to find the rest after the name
            // The value might contain spaces, so we need the full remainder
            let after_set = &trimmed[4..].trim(); // Skip "SET "
            if let Some(eq_pos) = after_set.find('=') {
                let name = after_set[..eq_pos].trim().to_string();
                let value = after_set[eq_pos + 1..].trim().to_string();
                return Ok(NormalizedStatement::SetParameter { name, value });
            }
        }
        // Could also be SET TRANSACTION ...
        return Ok(NormalizedStatement::Passthrough(stmt.to_string()));
    }

    // Check for SHOW command
    if upper.starts_with("SHOW ") {
        let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
        if parts.len() == 2 {
            let name = parts[1].trim().trim_matches(';').to_string();
            return Ok(NormalizedStatement::ShowParameter { name });
        }
    }

    // Check for RESET ALL (must come before RESET parameter)
    if upper == "RESET ALL" || upper == "RESET ALL;" {
        return Ok(NormalizedStatement::ResetAll);
    }

    // Check for RESET command
    if upper.starts_with("RESET ") {
        let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
        if parts.len() == 2 {
            let name = parts[1].trim().trim_matches(';').to_string();
            return Ok(NormalizedStatement::ResetParameter { name });
        }
    }

    // Check for SELECT <const> (no FROM) - common health check
    if upper.starts_with("SELECT ") && !upper.contains(" FROM ") && !upper.contains(" WHERE ") {
        // Could be SELECT 1, SELECT 'hello', SELECT current_timestamp, etc.
        return Ok(NormalizedStatement::SelectConstant(trimmed.to_string()));
    }

    // Transaction keyword synonyms
    if upper == "START TRANSACTION" || upper == "START TRANSACTION;" {
        return Ok(NormalizedStatement::Begin);
    }

    if upper == "END"
        || upper == "END;"
        || upper == "END TRANSACTION"
        || upper == "END TRANSACTION;"
    {
        return Ok(NormalizedStatement::Commit);
    }

    if upper == "ABORT"
        || upper == "ABORT;"
        || upper == "ABORT TRANSACTION"
        || upper == "ABORT TRANSACTION;"
    {
        return Ok(NormalizedStatement::Rollback);
    }

    // Pass through to normal parser
    Ok(NormalizedStatement::Passthrough(stmt.to_string()))
}

/// Result of normalizing a statement
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedStatement {
    /// SET parameter = value
    SetParameter { name: String, value: String },
    /// SHOW parameter
    ShowParameter { name: String },
    /// RESET parameter
    ResetParameter { name: String },
    /// RESET ALL
    ResetAll,
    /// SELECT <constant> (health check)
    SelectConstant(String),
    /// BEGIN synonym
    Begin,
    /// COMMIT synonym
    Commit,
    /// ROLLBACK synonym
    Rollback,
    /// Pass through to normal SQL parser
    Passthrough(String),
}

/// Process a raw SQL string through the compatibility layer
/// Returns a vector of parsed Statements ready for execution
pub fn process_sql(raw_sql: &str) -> Result<Vec<Statement>, PgWireError> {
    // Strip comments
    let cleaned = strip_comments(raw_sql);

    // Split into statements
    let statements = split_statements(&cleaned);

    let mut parsed = Vec::new();

    for stmt_text in statements {
        // Try compatibility normalization first
        match normalize_statement(&stmt_text)? {
            NormalizedStatement::SetParameter { name, value } => {
                parsed.push(Statement::SetParameter(
                    tpt_archon_relational::parser::SetParameterStatement { name, value },
                ));
            }
            NormalizedStatement::ShowParameter { name } => {
                parsed.push(Statement::ShowParameter(
                    tpt_archon_relational::parser::ShowParameterStatement { name },
                ));
            }
            NormalizedStatement::ResetParameter { name } => {
                parsed.push(Statement::ResetParameter(
                    tpt_archon_relational::parser::ResetParameterStatement { name },
                ));
            }
            NormalizedStatement::ResetAll => {
                parsed.push(Statement::ResetAll(
                    tpt_archon_relational::parser::ResetAllStatement,
                ));
            }
            NormalizedStatement::SelectConstant(sql) => {
                // Parse as regular SELECT
                let stmts = crate::compat::process_sql(&sql)?;
                parsed.extend(stmts);
            }
            NormalizedStatement::Begin => {
                parsed.push(Statement::Begin);
            }
            NormalizedStatement::Commit => {
                parsed.push(Statement::Commit);
            }
            NormalizedStatement::Rollback => {
                parsed.push(Statement::Rollback);
            }
            NormalizedStatement::Passthrough(sql) => {
                let stmts = crate::compat::process_sql(&sql)?;
                parsed.extend(stmts);
            }
        }
    }

    Ok(parsed)
}

/// Statement types for compatibility commands
/// These need to be added to the parser's Statement enum
pub enum CompatStatement {
    SetParameter { name: String, value: String },
    ShowParameter { name: String },
    ResetParameter { name: String },
    ResetAll,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_line_comments() {
        let sql = "SELECT 1; -- this is a comment\nSELECT 2;";
        let result = strip_comments(sql);
        assert_eq!(result, "SELECT 1; \nSELECT 2;");
    }

    #[test]
    fn test_strip_block_comments() {
        let sql = "SELECT /* comment */ 1;";
        let result = strip_comments(sql);
        assert_eq!(result, "SELECT  1;");
    }

    #[test]
    fn test_strip_nested_block_comments() {
        let sql = "SELECT /* outer /* inner */ outer */ 1;";
        let result = strip_comments(sql);
        assert_eq!(result, "SELECT  1;");
    }

    #[test]
    fn test_split_statements_simple() {
        let sql = "SELECT 1; SELECT 2; SELECT 3";
        let result = split_statements(sql);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "SELECT 1");
        assert_eq!(result[1], "SELECT 2");
        assert_eq!(result[2], "SELECT 3");
    }

    #[test]
    fn test_split_statements_with_quotes() {
        let sql = "SELECT 'hello; world'; SELECT 2;";
        let result = split_statements(sql);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "SELECT 'hello; world'");
        assert_eq!(result[1], "SELECT 2");
    }

    #[test]
    fn test_split_statements_dollar_quotes() {
        let sql = "SELECT $$hello; world$$; SELECT 2;";
        let result = split_statements(sql);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "SELECT $$hello; world$$");
        assert_eq!(result[1], "SELECT 2");
    }

    #[test]
    fn test_normalize_set() {
        let result = normalize_statement("SET timezone = 'UTC'").unwrap();
        assert_eq!(
            result,
            NormalizedStatement::SetParameter {
                name: "timezone".to_string(),
                value: "'UTC'".to_string(),
            }
        );
    }

    #[test]
    fn test_normalize_show() {
        let result = normalize_statement("SHOW timezone").unwrap();
        assert_eq!(
            result,
            NormalizedStatement::ShowParameter {
                name: "timezone".to_string(),
            }
        );
    }

    #[test]
    fn test_normalize_reset() {
        let result = normalize_statement("RESET timezone").unwrap();
        assert_eq!(
            result,
            NormalizedStatement::ResetParameter {
                name: "timezone".to_string(),
            }
        );
    }

    #[test]
    fn test_normalize_reset_all() {
        let result = normalize_statement("RESET ALL").unwrap();
        assert_eq!(result, NormalizedStatement::ResetAll);
    }

    #[test]
    fn test_normalize_select_const() {
        let result = normalize_statement("SELECT 1").unwrap();
        assert_eq!(
            result,
            NormalizedStatement::SelectConstant("SELECT 1".to_string())
        );
    }

    #[test]
    fn test_normalize_start_transaction() {
        let result = normalize_statement("START TRANSACTION").unwrap();
        assert_eq!(result, NormalizedStatement::Begin);
    }

    #[test]
    fn test_normalize_end() {
        let result = normalize_statement("END").unwrap();
        assert_eq!(result, NormalizedStatement::Commit);
    }

    #[test]
    fn test_normalize_abort() {
        let result = normalize_statement("ABORT").unwrap();
        assert_eq!(result, NormalizedStatement::Rollback);
    }
}
