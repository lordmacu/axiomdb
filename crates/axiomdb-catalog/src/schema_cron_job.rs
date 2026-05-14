/// Persistent definition for a scheduled cron job — one row in `axiom_cron_jobs` (Phase 22b.1).
///
/// ## On-disk format
/// ```text
/// [name_len:1][name UTF-8]
/// [schedule_len:1][schedule UTF-8]   (5-field cron expression, max 255 bytes)
/// [cmd_len:2 LE][command UTF-8]      (SQL statement, max 65535 bytes)
/// [db_len:1][database_name UTF-8]
/// [enabled:1]                        (0 = disabled, 1 = enabled)
/// [next_run_ms:8 LE]                 (i64 Unix ms, 0 = not yet computed)
/// [last_run_ms:8 LE]                 (i64 Unix ms, 0 = never run)
/// [status_len:1][last_status UTF-8]  (last run outcome: "" | "ok" | error message)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronJobDef {
    pub name: String,
    pub schedule: String,
    pub command: String,
    pub database_name: String,
    pub enabled: bool,
    pub next_run_ms: i64,
    pub last_run_ms: i64,
    pub last_status: String,
}

impl CronJobDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let sched = self.schedule.as_bytes();
        let cmd = self.command.as_bytes();
        let db = self.database_name.as_bytes();
        let status = self.last_status.as_bytes();
        debug_assert!(name.len() <= 255, "cron job name too long");
        debug_assert!(sched.len() <= 255, "cron schedule too long");
        debug_assert!(cmd.len() <= 65535, "cron command too long");
        debug_assert!(db.len() <= 255, "database name too long");
        debug_assert!(status.len() <= 255, "last_status too long");

        let mut buf = Vec::with_capacity(
            1 + name.len() + 1 + sched.len() + 2 + cmd.len() + 1 + db.len() + 1 + 8 + 8 + 1
                + status.len(),
        );
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(sched.len() as u8);
        buf.extend_from_slice(sched);
        buf.extend_from_slice(&(cmd.len() as u16).to_le_bytes());
        buf.extend_from_slice(cmd);
        buf.push(db.len() as u8);
        buf.extend_from_slice(db);
        buf.push(u8::from(self.enabled));
        buf.extend_from_slice(&self.next_run_ms.to_le_bytes());
        buf.extend_from_slice(&self.last_run_ms.to_le_bytes());
        buf.push(status.len() as u8);
        buf.extend_from_slice(status);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated CronJobDef bytes".into(),
            position: None,
        };
        let mut pos = 0usize;

        macro_rules! read_str {
            ($len_bytes:expr) => {{
                if bytes.len() < pos + $len_bytes {
                    return Err(err());
                }
                let len = if $len_bytes == 1 {
                    bytes[pos] as usize
                } else {
                    u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize
                };
                pos += $len_bytes;
                if bytes.len() < pos + len {
                    return Err(err());
                }
                let s = std::str::from_utf8(&bytes[pos..pos + len]).map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in CronJobDef".into(),
                    position: None,
                })?
                .to_string();
                pos += len;
                s
            }};
        }

        let name = read_str!(1);
        let schedule = read_str!(1);
        let command = read_str!(2);
        let database_name = read_str!(1);

        if bytes.len() < pos + 1 {
            return Err(err());
        }
        let enabled = bytes[pos] != 0;
        pos += 1;

        if bytes.len() < pos + 16 {
            return Err(err());
        }
        let next_run_ms = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let last_run_ms = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let last_status = read_str!(1);

        Ok((
            Self {
                name,
                schedule,
                command,
                database_name,
                enabled,
                next_run_ms,
                last_run_ms,
                last_status,
            },
            pos,
        ))
    }
}
