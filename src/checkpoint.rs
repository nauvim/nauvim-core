use nvim_rpc_ui::{Checkpoint, CheckpointError, Limits};

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior};

use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;
use std::{fmt, fs, io};

const FILE_NAME: &str = "ui-checkpoints.sqlite3";
const SCHEMA_VERSION: i64 = 1;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_millis(100);

pub(crate) struct Store(Connection);

impl Store {
    pub(crate) fn open(data_dir: &Path) -> Result<Self, Error> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(FILE_NAME);

        // The checkpoint can contain editor text. Give SQLite a private file
        // so it creates the corresponding journal with the same permissions.
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;

        let mut db = Connection::open(path)?;
        db.busy_timeout(BUSY_TIMEOUT)?;

        let version = schema_version(&db)?;
        if version == SCHEMA_VERSION {
            return Ok(Self(db));
        }
        if version != 0 {
            return Err(Error::Schema(version));
        }

        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match schema_version(&tx)? {
            0 => {
                tx.execute_batch(
                    "CREATE TABLE ui_checkpoints (
                        host TEXT PRIMARY KEY,
                        session BLOB NOT NULL,
                        checkpoint BLOB NOT NULL
                    ) STRICT;",
                )?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            version => return Err(Error::Schema(version)),
        }
        tx.commit()?;

        Ok(Self(db))
    }

    pub(crate) fn load(
        &self,
        host: &str,
        session: &[u8],
        limits: Limits,
    ) -> Result<Option<Checkpoint>, Error> {
        let row = self
            .0
            .query_row(
                "SELECT session,
                    CASE WHEN session = ?2 AND length(checkpoint) <= ?3 THEN checkpoint END
                 FROM ui_checkpoints WHERE host = ?1",
                (host, session, MAX_BYTES as i64),
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()?;

        let Some((stored_session, bytes)) = row else {
            return Ok(None);
        };
        if stored_session != session {
            self.0.execute(
                "DELETE FROM ui_checkpoints WHERE host = ?1 AND session = ?2",
                (host, stored_session),
            )?;
            return Ok(None);
        }

        let bytes = bytes.ok_or(Error::Size)?;
        Ok(Some(Checkpoint::decode(&bytes, limits, MAX_BYTES)?))
    }

    pub(crate) fn save(
        &self,
        host: &str,
        session: &[u8],
        checkpoint: &Checkpoint,
    ) -> Result<(), Error> {
        let bytes = checkpoint.encode()?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::Size);
        }

        self.0.execute(
            "INSERT INTO ui_checkpoints (host, session, checkpoint) VALUES (?1, ?2, ?3)
             ON CONFLICT (host) DO UPDATE SET
                 session = excluded.session, checkpoint = excluded.checkpoint",
            (host, session, bytes),
        )?;

        Ok(())
    }
}

fn schema_version(db: &Connection) -> rusqlite::Result<i64> {
    db.pragma_query_value(None, "user_version", |row| row.get(0))
}

#[derive(Debug)]
pub(crate) enum Error {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    Schema(i64),
    Checkpoint(CheckpointError),
    Size,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Sqlite(e) => e.fmt(f),
            Self::Schema(version) => {
                write!(f, "unsupported UI checkpoint database schema {version}")
            }
            Self::Checkpoint(e) => e.fmt(f),
            Self::Size => write!(
                f,
                "UI checkpoint exceeds the {MAX_BYTES}-byte storage limit"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<CheckpointError> for Error {
    fn from(e: CheckpointError) -> Self {
        Self::Checkpoint(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nvim_rpc_plugin::mp::decode::{Object, ObjectParser};
    use nvim_rpc_plugin::mp::encode::WriterSyncExt as _;
    use nvim_rpc_ui::Model;
    use rusqlite::ErrorCode;
    use tempfile::tempdir;

    use std::os::unix::fs::PermissionsExt as _;

    fn flushed(title: &str) -> Checkpoint {
        let mut bytes = Vec::new();
        bytes
            .write_primitive((("set_title", (title,)), ("flush", ())))
            .consume();

        let mut parser = ObjectParser::new(bytes.as_slice());
        let Object::Array(params) = parser.next_object().unwrap() else {
            panic!("expected a redraw argument array");
        };
        let mut model = Model::default();
        assert!(model.apply_notification(b"redraw", params, |_| {}).unwrap());
        model.checkpoint().unwrap()
    }

    #[test]
    fn restores_latest_checkpoint_for_each_host_after_reopening() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data");
        let limits = Limits::default();
        let db = Store::open(&path).unwrap();
        assert!(db.load("local", b"first", limits).unwrap().is_none());
        db.save("local", b"first", &flushed("old")).unwrap();
        db.save("local", b"first", &flushed("latest")).unwrap();
        db.save("ssh", b"second", &flushed("remote")).unwrap();
        drop(db);

        let db = Store::open(&path).unwrap();
        for (host, session, title) in [
            ("local", b"first".as_slice(), "latest"),
            ("ssh", b"second".as_slice(), "remote"),
        ] {
            let checkpoint = db.load(host, session, limits).unwrap().unwrap();
            assert_eq!(checkpoint.state().title.as_ref(), title);
        }

        let mode = fs::metadata(path.join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn discards_checkpoint_for_a_replaced_session() {
        let dir = tempdir().unwrap();
        let db = Store::open(dir.path()).unwrap();
        let limits = Limits::default();
        db.save("local", b"old", &flushed("old")).unwrap();

        assert!(db.load("local", b"replacement", limits).unwrap().is_none());
        assert!(db.load("local", b"old", limits).unwrap().is_none());

        db.save("local", b"replacement", &flushed("new")).unwrap();
        assert_eq!(
            db.load("local", b"replacement", limits)
                .unwrap()
                .unwrap()
                .state()
                .title
                .as_ref(),
            "new"
        );
    }

    #[test]
    fn rejects_invalid_and_incompatible_checkpoint_blobs() {
        let dir = tempdir().unwrap();
        let db = Store::open(dir.path()).unwrap();
        let limits = Limits::default();

        for (bytes, cause) in [
            (b"broken".as_slice(), CheckpointError::Invalid("header")),
            (
                b"NRUI\xff".as_slice(),
                CheckpointError::IncompatibleVersion(255),
            ),
        ] {
            db.0.execute(
                "INSERT OR REPLACE INTO ui_checkpoints VALUES (?1, ?2, ?3)",
                ("local", b"session".as_slice(), bytes),
            )
            .unwrap();
            match (db.load("local", b"session", limits), cause) {
                (
                    Err(Error::Checkpoint(CheckpointError::Invalid(actual))),
                    CheckpointError::Invalid(expected),
                ) => assert_eq!(actual, expected),
                (
                    Err(Error::Checkpoint(CheckpointError::IncompatibleVersion(actual))),
                    CheckpointError::IncompatibleVersion(expected),
                ) => assert_eq!(actual, expected),
                (actual, expected) => panic!("expected {expected}, got {actual:?}"),
            }
        }

        db.save("local", b"session", &flushed("saved again"))
            .unwrap();
        let checkpoint = db.load("local", b"session", limits).unwrap().unwrap();
        assert_eq!(checkpoint.state().title.as_ref(), "saved again");
    }

    #[test]
    fn rejects_incompatible_and_unreadable_databases() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let db = Store::open(dir.path()).unwrap();
        db.0.pragma_update(None, "user_version", 2).unwrap();
        drop(db);

        assert!(matches!(Store::open(dir.path()), Err(Error::Schema(2))));
        assert_eq!(
            schema_version(&Connection::open(&path).unwrap()).unwrap(),
            2
        );

        fs::write(path, b"not a SQLite database").unwrap();
        assert!(matches!(
            Store::open(dir.path()),
            Err(Error::Sqlite(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == ErrorCode::NotADatabase
        ));
    }

    #[test]
    fn failed_write_leaves_previous_checkpoint_intact() {
        let dir = tempdir().unwrap();
        let db = Store::open(dir.path()).unwrap();
        db.save("local", b"session", &flushed("existing")).unwrap();

        let mut locking = Connection::open(dir.path().join(FILE_NAME)).unwrap();
        let tx = locking
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(matches!(
            db.save("local", b"session", &flushed("new")),
            Err(Error::Sqlite(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == ErrorCode::DatabaseBusy
        ));
        tx.commit().unwrap();

        let checkpoint = db
            .load("local", b"session", Limits::default())
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.state().title.as_ref(), "existing");
    }
}
