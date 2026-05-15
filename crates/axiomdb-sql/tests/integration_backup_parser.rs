use axiomdb_sql::{
    ast::{BackupStmt, RestoreStmt, Stmt},
    parse,
};

fn parse_ok(sql: &str) -> Stmt {
    parse(sql, None).expect("parse failed")
}

fn parse_backup(sql: &str) -> BackupStmt {
    match parse_ok(sql) {
        Stmt::Backup(b) => b,
        other => panic!("expected Backup, got {other:?}"),
    }
}

fn parse_restore(sql: &str) -> RestoreStmt {
    match parse_ok(sql) {
        Stmt::Restore(r) => r,
        other => panic!("expected Restore, got {other:?}"),
    }
}

#[test]
fn test_parse_backup_full() {
    let b = parse_backup("BACKUP DATABASE TO '/tmp/full.axbk'");
    assert_eq!(b.dest, "/tmp/full.axbk");
    assert!(b.incremental_from.is_none());
}

#[test]
fn test_parse_backup_full_lowercase() {
    let b = parse_backup("backup database to '/var/backups/db.axbk'");
    assert_eq!(b.dest, "/var/backups/db.axbk");
    assert!(b.incremental_from.is_none());
}

#[test]
fn test_parse_backup_incremental() {
    let b = parse_backup("BACKUP DATABASE TO '/tmp/inc.axbk' INCREMENTAL FROM '/tmp/full.axbk'");
    assert_eq!(b.dest, "/tmp/inc.axbk");
    assert_eq!(b.incremental_from.as_deref(), Some("/tmp/full.axbk"));
}

#[test]
fn test_parse_backup_incremental_mixed_case() {
    let b = parse_backup("backup database to '/tmp/inc.axbk' incremental from '/tmp/full.axbk'");
    assert_eq!(b.incremental_from.as_deref(), Some("/tmp/full.axbk"));
}

#[test]
fn test_parse_restore_full() {
    let r = parse_restore("RESTORE DATABASE FROM '/tmp/full.axbk' TO '/tmp/restored.db'");
    assert_eq!(r.source, "/tmp/full.axbk");
    assert_eq!(r.dest_path, "/tmp/restored.db");
}

#[test]
fn test_parse_restore_lowercase() {
    let r = parse_restore("restore database from '/tmp/full.axbk' to '/tmp/out.db'");
    assert_eq!(r.source, "/tmp/full.axbk");
    assert_eq!(r.dest_path, "/tmp/out.db");
}

#[test]
fn test_parse_backup_missing_to_errors() {
    assert!(
        parse("BACKUP DATABASE '/tmp/x.axbk'", None).is_err(),
        "BACKUP without TO must fail"
    );
}

#[test]
fn test_parse_restore_missing_to_errors() {
    assert!(
        parse("RESTORE DATABASE FROM '/tmp/x.axbk'", None).is_err(),
        "RESTORE without TO must fail"
    );
}
