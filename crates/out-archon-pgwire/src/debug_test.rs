use out_archon_pgwire::compat::split_statements;
#[test]
fn debug_dollar() {
    let sql = "SELECT \$\$hello; world\$\$; SELECT 2;";
    let result = split_statements(sql);
    eprintln!("result: {:?}", result);
    eprintln!("len: {}", result.len());
    for (i, s) in result.iter().enumerate() {
        eprintln!("[{}]: {:?}", i, s);
    }
}
