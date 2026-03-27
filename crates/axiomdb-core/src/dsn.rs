use std::collections::BTreeMap;
use std::path::PathBuf;

use percent_encoding::percent_decode_str;
use url::{form_urlencoded, Url};

use crate::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedDsn {
    Local(LocalPathDsn),
    Wire(WireEndpointDsn),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalScheme {
    PlainPath,
    File,
    AxiomDbLocal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPathDsn {
    pub original_scheme: LocalScheme,
    pub path: PathBuf,
    pub query: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireScheme {
    AxiomDb,
    MySql,
    Postgres,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireEndpointDsn {
    pub original_scheme: WireScheme,
    pub user: Option<String>,
    pub password: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub query: BTreeMap<String, String>,
}

pub fn parse_dsn(input: &str) -> Result<ParsedDsn, DbError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(invalid_dsn("DSN cannot be empty"));
    }

    if looks_like_windows_path(input) || !looks_like_uri_candidate(input) {
        return Ok(ParsedDsn::Local(LocalPathDsn {
            original_scheme: LocalScheme::PlainPath,
            path: PathBuf::from(input),
            query: BTreeMap::new(),
        }));
    }

    let url = Url::parse(input).map_err(|e| invalid_dsn(format!("malformed DSN URI: {e}")))?;

    match url.scheme() {
        "file" => parse_file_dsn(&url),
        "axiomdb" => parse_axiomdb_dsn(&url),
        "mysql" => parse_wire_dsn(&url, WireScheme::MySql),
        "postgres" | "postgresql" => parse_wire_dsn(&url, WireScheme::Postgres),
        other => Err(invalid_dsn(format!("unsupported DSN scheme '{other}'"))),
    }
}

fn parse_file_dsn(url: &Url) -> Result<ParsedDsn, DbError> {
    let path = url
        .to_file_path()
        .map_err(|_| invalid_dsn("file: DSN must point to a local filesystem path"))?;
    Ok(ParsedDsn::Local(LocalPathDsn {
        original_scheme: LocalScheme::File,
        path,
        query: parse_query(url)?,
    }))
}

fn parse_axiomdb_dsn(url: &Url) -> Result<ParsedDsn, DbError> {
    if url.host_str().is_none()
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
    {
        let path = decode_component(url.path())?;
        if path.is_empty() || path == "/" {
            return Err(invalid_dsn(
                "axiomdb local DSN must include a filesystem path",
            ));
        }
        return Ok(ParsedDsn::Local(LocalPathDsn {
            original_scheme: LocalScheme::AxiomDbLocal,
            path: PathBuf::from(path),
            query: parse_query(url)?,
        }));
    }

    parse_wire_dsn(url, WireScheme::AxiomDb)
}

fn parse_wire_dsn(url: &Url, original_scheme: WireScheme) -> Result<ParsedDsn, DbError> {
    let host = url
        .host_str()
        .ok_or_else(|| invalid_dsn("wire-endpoint DSN must include a host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let user = if url.username().is_empty() {
        None
    } else {
        Some(decode_component(url.username())?)
    };
    let password = url.password().map(decode_component).transpose()?;
    let database = if url.path().is_empty() || url.path() == "/" {
        None
    } else {
        Some(decode_component(url.path().trim_start_matches('/'))?)
    };

    Ok(ParsedDsn::Wire(WireEndpointDsn {
        original_scheme,
        user,
        password,
        host,
        port: url.port(),
        database,
        query: parse_query(url)?,
    }))
}

fn parse_query(url: &Url) -> Result<BTreeMap<String, String>, DbError> {
    let mut query = BTreeMap::new();
    if let Some(raw) = url.query() {
        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            let key = key.into_owned();
            let value = value.into_owned();
            if query.insert(key.clone(), value).is_some() {
                return Err(invalid_dsn(format!("duplicate query parameter '{key}'")));
            }
        }
    }
    Ok(query)
}

fn decode_component(raw: &str) -> Result<String, DbError> {
    percent_decode_str(raw)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| invalid_dsn("DSN contains invalid UTF-8 percent-encoding"))
}

fn looks_like_uri_candidate(input: &str) -> bool {
    input.starts_with("file:")
        || input.starts_with("axiomdb:")
        || input.starts_with("mysql:")
        || input.starts_with("postgres:")
        || input.starts_with("postgresql:")
        || input.contains("://")
}

fn looks_like_windows_path(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn invalid_dsn(reason: impl Into<String>) -> DbError {
    DbError::InvalidDsn {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{parse_dsn, LocalPathDsn, LocalScheme, ParsedDsn, WireEndpointDsn, WireScheme};

    #[test]
    fn parses_mysql_wire_dsn() {
        let parsed = parse_dsn("mysql://ana:secret@127.0.0.1:3306/app").unwrap();
        assert_eq!(
            parsed,
            ParsedDsn::Wire(WireEndpointDsn {
                original_scheme: WireScheme::MySql,
                user: Some("ana".into()),
                password: Some("secret".into()),
                host: "127.0.0.1".into(),
                port: Some(3306),
                database: Some("app".into()),
                query: Default::default(),
            })
        );
    }

    #[test]
    fn parses_postgres_aliases_as_wire() {
        for dsn in ["postgres://db.internal/app", "postgresql://db.internal/app"] {
            let parsed = parse_dsn(dsn).unwrap();
            assert!(matches!(
                parsed,
                ParsedDsn::Wire(WireEndpointDsn {
                    original_scheme: WireScheme::Postgres,
                    host,
                    database,
                    ..
                }) if host == "db.internal" && database.as_deref() == Some("app")
            ));
        }
    }

    #[test]
    fn parses_axiomdb_local_uri() {
        let parsed = parse_dsn("axiomdb:///tmp/myapp").unwrap();
        assert_eq!(
            parsed,
            ParsedDsn::Local(LocalPathDsn {
                original_scheme: LocalScheme::AxiomDbLocal,
                path: PathBuf::from("/tmp/myapp"),
                query: Default::default(),
            })
        );
    }

    #[test]
    fn parses_ipv6_host() {
        let parsed = parse_dsn("mysql://[2001:db8::1]:3307/app").unwrap();
        assert!(matches!(
            parsed,
            ParsedDsn::Wire(WireEndpointDsn {
                host,
                port: Some(3307),
                ..
            }) if host == "2001:db8::1"
        ));
    }

    #[test]
    fn percent_decodes_credentials_database_and_query() {
        let parsed =
            parse_dsn("mysql://ana%20maria:s%40cret@localhost/app%2Fdb?data_dir=%2Ftmp%2Fmy%20db")
                .unwrap();
        assert!(matches!(
            parsed,
            ParsedDsn::Wire(WireEndpointDsn {
                user: Some(user),
                password: Some(password),
                database: Some(database),
                query,
                ..
            }) if user == "ana maria"
                && password == "s@cret"
                && database == "app/db"
                && query.get("data_dir").map(String::as_str) == Some("/tmp/my db")
        ));
    }

    #[test]
    fn rejects_duplicate_query_keys() {
        let err = parse_dsn("mysql://localhost/app?x=1&x=2").unwrap_err();
        assert!(matches!(
            err,
            crate::DbError::InvalidDsn { reason } if reason.contains("duplicate query parameter 'x'")
        ));
    }

    #[test]
    fn plain_paths_remain_local() {
        let parsed = parse_dsn("./data/app").unwrap();
        assert!(matches!(
            parsed,
            ParsedDsn::Local(LocalPathDsn {
                original_scheme: LocalScheme::PlainPath,
                path,
                ..
            }) if path == std::path::Path::new("./data/app")
        ));
    }
}
