use rusqlite::Connection;
fn test(conn: &mut Connection) {
    let tx = conn.transaction().unwrap();
}
