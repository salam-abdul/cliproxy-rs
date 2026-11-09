//! History rows and their Parquet files: `<dir>/date=YYYY-MM-DD/<HHMMSS>-<pid>-<seq>.parquet`
//! (UTC), written as `.partial` and renamed once the footer is on disk, so a reader
//! globbing `*.parquet` never sees a file without one.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parquet::basic::{Compression, ZstdLevel};
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;
use parquet::file::writer::SerializedFileWriter;
use parquet::record::Field;
use parquet::schema::types::ColumnPath;

/// Which server path sent the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Proxy,
    Native,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Native => "native",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [Self::Proxy, Self::Native].into_iter().find(|v| v.as_str() == s)
    }
}

/// How one upstream send ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    #[default]
    Complete,
    Failed,
    Failover,
    StreamError,
    TransportLost,
    Cancelled,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Failover => "failover",
            Self::StreamError => "stream_error",
            Self::TransportLost => "transport_lost",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            Self::Complete,
            Self::Failed,
            Self::Failover,
            Self::StreamError,
            Self::TransportLost,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

/// One upstream send. Every field is chosen here; nothing is copied wholesale from
/// usage records or client facts, which carry keys.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub exchange_id: String,
    pub turn: i32,
    pub send: i32,
    pub started_at_us: i64,
    pub ended_at_us: Option<i64>,
    pub source: Source,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub provider: Option<String>,
    pub model_requested: Option<String>,
    pub model: Option<String>,
    pub response_model: Option<String>,
    pub account: Option<String>,
    pub auth_index: Option<String>,
    pub session_id: Option<String>,
    pub user_agent: Option<String>,
    pub stream: bool,
    pub status: Option<i32>,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cache_write_5m_tokens: Option<i64>,
    pub cache_write_1h_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub upstream_request_id: Option<String>,
    pub request_body: Option<String>,
    pub request_body_raw: Option<Vec<u8>>,
    pub response_body: Option<String>,
    pub response_body_raw: Option<Vec<u8>>,
    pub request_frames: Option<Vec<String>>,
    pub response_frames: Option<Vec<String>>,
    pub request_bytes: i64,
    pub response_bytes: i64,
    pub request_truncated: bool,
    pub response_truncated: bool,
}

impl Row {
    /// Token columns from a usage detail; all left empty when it reports no counts.
    pub(crate) fn set_tokens(&mut self, detail: &crate::usage_record::Detail) {
        let counts = [
            detail.input,
            detail.output,
            detail.reasoning,
            detail.cached,
            detail.cache_read,
            detail.cache_creation,
            detail.total,
        ];
        if counts.iter().all(|n| *n == 0) && detail.cache_creation_5m.is_none() && detail.cache_creation_1h.is_none() {
            return;
        }
        self.input_tokens = Some(detail.input);
        self.output_tokens = Some(detail.output);
        self.reasoning_tokens = Some(detail.reasoning);
        self.cache_read_tokens = Some(if detail.cache_read > 0 {
            detail.cache_read
        } else {
            detail.cached
        });
        self.cache_write_tokens = Some(detail.cache_creation);
        self.cache_write_5m_tokens = detail.cache_creation_5m;
        self.cache_write_1h_tokens = detail.cache_creation_1h;
        self.total_tokens = Some(detail.total);
    }
}

/// A body as a string column when it is UTF-8, else as the raw column: one of the two.
/// A `truncated` body cut inside a character loses that character's start, so a cut
/// text body stays text.
pub(crate) fn body_columns(mut bytes: Vec<u8>, truncated: bool) -> (Option<String>, Option<Vec<u8>>) {
    if truncated
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    match String::from_utf8(bytes) {
        Ok(text) => (Some(text), None),
        Err(error) => (None, Some(error.into_bytes())),
    }
}

enum Get {
    Str(fn(&Row) -> Option<&str>),
    Bin(fn(&Row) -> Option<&[u8]>),
    I32(fn(&Row) -> Option<i32>),
    I64(fn(&Row) -> Option<i64>),
    Ts(fn(&Row) -> Option<i64>),
    Bool(fn(&Row) -> Option<bool>),
    List(fn(&Row) -> Option<&[String]>),
}

struct Column {
    name: &'static str,
    get: Get,
    set: fn(&mut Row, Field) -> io::Result<()>,
}

fn bad(name: &str, field: &Field) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("history column {name}: unexpected {field}"),
    )
}

fn opt_str(name: &str, field: Field) -> io::Result<Option<String>> {
    match field {
        Field::Null => Ok(None),
        Field::Str(s) => Ok(Some(s)),
        other => Err(bad(name, &other)),
    }
}

fn opt_bytes(name: &str, field: Field) -> io::Result<Option<Vec<u8>>> {
    match field {
        Field::Null => Ok(None),
        Field::Bytes(b) => Ok(Some(b.data().to_vec())),
        other => Err(bad(name, &other)),
    }
}

fn opt_i32(name: &str, field: Field) -> io::Result<Option<i32>> {
    match field {
        Field::Null => Ok(None),
        Field::Int(v) => Ok(Some(v)),
        other => Err(bad(name, &other)),
    }
}

fn opt_i64(name: &str, field: Field) -> io::Result<Option<i64>> {
    match field {
        Field::Null => Ok(None),
        Field::Long(v) | Field::TimestampMicros(v) => Ok(Some(v)),
        other => Err(bad(name, &other)),
    }
}

fn opt_bool(name: &str, field: Field) -> io::Result<Option<bool>> {
    match field {
        Field::Null => Ok(None),
        Field::Bool(v) => Ok(Some(v)),
        other => Err(bad(name, &other)),
    }
}

fn opt_list(name: &str, field: Field) -> io::Result<Option<Vec<String>>> {
    match field {
        Field::Null => Ok(None),
        Field::ListInternal(list) => list
            .elements()
            .iter()
            .map(|e| match e {
                Field::Str(s) => Ok(s.clone()),
                other => Err(bad(name, other)),
            })
            .collect::<io::Result<Vec<_>>>()
            .map(Some),
        other => Err(bad(name, &other)),
    }
}

macro_rules! column {
    ($name:ident, Str) => {
        Column {
            name: stringify!($name),
            get: Get::Str(|r| r.$name.as_deref()),
            set: |r, f| {
                r.$name = opt_str(stringify!($name), f)?;
                Ok(())
            },
        }
    };
    ($name:ident, Bin) => {
        Column {
            name: stringify!($name),
            get: Get::Bin(|r| r.$name.as_deref()),
            set: |r, f| {
                r.$name = opt_bytes(stringify!($name), f)?;
                Ok(())
            },
        }
    };
    ($name:ident, OptI32) => {
        Column {
            name: stringify!($name),
            get: Get::I32(|r| r.$name),
            set: |r, f| {
                r.$name = opt_i32(stringify!($name), f)?;
                Ok(())
            },
        }
    };
    ($name:ident, I32) => {
        Column {
            name: stringify!($name),
            get: Get::I32(|r| Some(r.$name)),
            set: |r, f| {
                r.$name = opt_i32(stringify!($name), f)?.unwrap_or_default();
                Ok(())
            },
        }
    };
    ($name:ident, OptI64) => {
        Column {
            name: stringify!($name),
            get: Get::I64(|r| r.$name),
            set: |r, f| {
                r.$name = opt_i64(stringify!($name), f)?;
                Ok(())
            },
        }
    };
    ($name:ident, I64) => {
        Column {
            name: stringify!($name),
            get: Get::I64(|r| Some(r.$name)),
            set: |r, f| {
                r.$name = opt_i64(stringify!($name), f)?.unwrap_or_default();
                Ok(())
            },
        }
    };
    ($name:ident, Bool) => {
        Column {
            name: stringify!($name),
            get: Get::Bool(|r| Some(r.$name)),
            set: |r, f| {
                r.$name = opt_bool(stringify!($name), f)?.unwrap_or_default();
                Ok(())
            },
        }
    };
    ($name:ident, List) => {
        Column {
            name: stringify!($name),
            get: Get::List(|r| r.$name.as_deref()),
            set: |r, f| {
                r.$name = opt_list(stringify!($name), f)?;
                Ok(())
            },
        }
    };
}

/// The file's columns, in schema order. Body and frame columns are listed in
/// [`UNINDEXED`].
static COLUMNS: [Column; 41] = [
    Column {
        name: "exchange_id",
        get: Get::Str(|r| Some(r.exchange_id.as_str())),
        set: |r, f| {
            r.exchange_id = opt_str("exchange_id", f)?.unwrap_or_default();
            Ok(())
        },
    },
    column!(turn, I32),
    column!(send, I32),
    Column {
        name: "started_at",
        get: Get::Ts(|r| Some(r.started_at_us)),
        set: |r, f| {
            r.started_at_us = opt_i64("started_at", f)?.unwrap_or_default();
            Ok(())
        },
    },
    Column {
        name: "ended_at",
        get: Get::Ts(|r| r.ended_at_us),
        set: |r, f| {
            r.ended_at_us = opt_i64("ended_at", f)?;
            Ok(())
        },
    },
    Column {
        name: "source",
        get: Get::Str(|r| Some(r.source.as_str())),
        set: |r, f| {
            let value = opt_str("source", f)?.unwrap_or_default();
            r.source = Source::parse(&value).ok_or_else(|| bad("source", &Field::Str(value)))?;
            Ok(())
        },
    },
    column!(endpoint, Str),
    column!(transport, Str),
    column!(provider, Str),
    column!(model_requested, Str),
    column!(model, Str),
    column!(response_model, Str),
    column!(account, Str),
    column!(auth_index, Str),
    column!(session_id, Str),
    column!(user_agent, Str),
    column!(stream, Bool),
    column!(status, OptI32),
    Column {
        name: "outcome",
        get: Get::Str(|r| Some(r.outcome.as_str())),
        set: |r, f| {
            let value = opt_str("outcome", f)?.unwrap_or_default();
            r.outcome = Outcome::parse(&value).ok_or_else(|| bad("outcome", &Field::Str(value)))?;
            Ok(())
        },
    },
    column!(error, Str),
    column!(input_tokens, OptI64),
    column!(output_tokens, OptI64),
    column!(reasoning_tokens, OptI64),
    column!(cache_read_tokens, OptI64),
    column!(cache_write_tokens, OptI64),
    column!(cache_write_5m_tokens, OptI64),
    column!(cache_write_1h_tokens, OptI64),
    column!(total_tokens, OptI64),
    column!(latency_ms, OptI64),
    column!(ttft_ms, OptI64),
    column!(upstream_request_id, Str),
    column!(request_body, Str),
    column!(request_body_raw, Bin),
    column!(response_body, Str),
    column!(response_body_raw, Bin),
    column!(request_frames, List),
    column!(response_frames, List),
    column!(request_bytes, I64),
    column!(response_bytes, I64),
    column!(request_truncated, Bool),
    column!(response_truncated, Bool),
];

/// Columns whose values are whole bodies: no dictionary (every value is distinct) and
/// no statistics (min/max of a body would copy prompt text into the footer).
const UNINDEXED: [&[&str]; 6] = [
    &["request_body"],
    &["request_body_raw"],
    &["response_body"],
    &["response_body_raw"],
    &["request_frames", "list", "element"],
    &["response_frames", "list", "element"],
];

fn schema() -> parquet::errors::Result<parquet::schema::types::TypePtr> {
    schema_of(COLUMNS.iter())
}

/// The columns a delete matches on: enough to decide without reading bodies.
const KEY_COLUMNS: [&str; 5] = ["started_at", "session_id", "account", "model", "model_requested"];

fn schema_of<'a>(
    columns: impl Iterator<Item = &'a Column>,
) -> parquet::errors::Result<parquet::schema::types::TypePtr> {
    let mut text = String::from("message history {\n");
    for column in columns {
        let name = column.name;
        let line = match column.get {
            Get::Str(_) => format!("optional binary {name} (STRING);"),
            Get::Bin(_) => format!("optional binary {name};"),
            Get::I32(_) => format!("optional int32 {name};"),
            Get::I64(_) => format!("optional int64 {name};"),
            Get::Ts(_) => format!("optional int64 {name} (TIMESTAMP(MICROS,true));"),
            Get::Bool(_) => format!("optional boolean {name};"),
            Get::List(_) => {
                format!(
                    "optional group {name} (LIST) {{ repeated group list {{ optional binary element (STRING); }} }}"
                )
            }
        };
        text.push_str(&line);
        text.push('\n');
    }
    text.push('}');
    Ok(Arc::new(parquet::schema::parser::parse_message_type(&text)?))
}

fn properties() -> parquet::errors::Result<WriterProperties> {
    let mut builder = WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?));
    for path in UNINDEXED {
        let path = ColumnPath::new(path.iter().map(|p| (*p).to_owned()).collect());
        builder = builder
            .set_column_dictionary_enabled(path.clone(), false)
            .set_column_statistics_enabled(path, EnabledStatistics::None);
    }
    Ok(builder.build())
}

fn io_error(error: parquet::errors::ParquetError) -> io::Error {
    match error {
        parquet::errors::ParquetError::External(inner) => io::Error::other(inner),
        other => io::Error::other(other),
    }
}

/// Definition levels for an optional leaf: 1 present, 0 null.
fn levels<T>(values: impl Iterator<Item = Option<T>>) -> (Vec<T>, Vec<i16>) {
    let mut present = Vec::new();
    let mut defs = Vec::new();
    for value in values {
        defs.push(i16::from(value.is_some()));
        present.extend(value);
    }
    (present, defs)
}

fn write_column(
    writer: &mut parquet::file::writer::SerializedColumnWriter<'_>,
    get: &Get,
    rows: &[Row],
) -> parquet::errors::Result<()> {
    match get {
        Get::Str(get) => {
            let (values, defs) = levels(
                rows.iter()
                    .map(|r| get(r).map(|s| ByteArray::from(s.as_bytes().to_vec()))),
            );
            writer
                .typed::<ByteArrayType>()
                .write_batch(&values, Some(&defs), None)?;
        }
        Get::Bin(get) => {
            let (values, defs) = levels(rows.iter().map(|r| get(r).map(|b| ByteArray::from(b.to_vec()))));
            writer
                .typed::<ByteArrayType>()
                .write_batch(&values, Some(&defs), None)?;
        }
        Get::I32(get) => {
            let (values, defs) = levels(rows.iter().map(get));
            writer.typed::<Int32Type>().write_batch(&values, Some(&defs), None)?;
        }
        Get::I64(get) | Get::Ts(get) => {
            let (values, defs) = levels(rows.iter().map(get));
            writer.typed::<Int64Type>().write_batch(&values, Some(&defs), None)?;
        }
        Get::Bool(get) => {
            let (values, defs) = levels(rows.iter().map(get));
            writer.typed::<BoolType>().write_batch(&values, Some(&defs), None)?;
        }
        Get::List(get) => {
            // Three-level list: 0 null list, 1 empty list, 3 present element.
            let (mut values, mut defs, mut reps) = (Vec::new(), Vec::new(), Vec::new());
            for row in rows {
                match get(row) {
                    None => {
                        defs.push(0);
                        reps.push(0);
                    }
                    Some([]) => {
                        defs.push(1);
                        reps.push(0);
                    }
                    Some(items) => {
                        for (i, item) in items.iter().enumerate() {
                            values.push(ByteArray::from(item.as_bytes().to_vec()));
                            defs.push(3);
                            reps.push(i16::from(i > 0));
                        }
                    }
                }
            }
            writer
                .typed::<ByteArrayType>()
                .write_batch(&values, Some(&defs), Some(&reps))?;
        }
    }
    Ok(())
}

/// The first of `<stem><ext>`, `<stem>-1<ext>`, ... that does not exist.
fn free_name(partition: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut path = partition.join(format!("{stem}{ext}"));
    let mut n = 0u32;
    while path.exists() {
        n += 1;
        path = partition.join(format!("{stem}-{n}{ext}"));
    }
    path
}

pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// One history file being written.
pub(crate) struct ParquetFile {
    writer: Option<SerializedFileWriter<File>>,
    partition: PathBuf,
    stem: String,
    partial: PathBuf,
    /// Set when this file replaces an existing one.
    replaces: Option<PathBuf>,
}

impl ParquetFile {
    pub(crate) fn create(dir: &Path, now: DateTime<Utc>, seq: u32) -> io::Result<Self> {
        private_dir(dir)?;
        let partition = dir.join(format!("date={}", now.format("%Y-%m-%d")));
        private_dir(&partition)?;
        let stem = format!("{}-{}-{seq:04}", now.format("%H%M%S"), std::process::id());
        Self::open(partition, stem, None)
    }

    /// A file that replaces the finished file at `target` when it is finished.
    pub(crate) fn replacing(target: &Path) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a history file: {}", target.display()),
            )
        };
        let partition = target.parent().ok_or_else(invalid)?.to_path_buf();
        let name = target.file_name().and_then(|n| n.to_str()).ok_or_else(invalid)?;
        if !is_history_file(name) {
            return Err(invalid());
        }
        let stem = name.strip_suffix(".parquet").ok_or_else(invalid)?;
        let (time, rest) = stem.split_once('-').ok_or_else(invalid)?;
        let (pid, seq) = rest.split_once('-').ok_or_else(invalid)?;
        let stem = format!("{time}-{}-{seq}-{pid}", std::process::id());
        Self::open(partition, stem, Some(target.to_path_buf()))
    }

    fn open(partition: PathBuf, stem: String, replaces: Option<PathBuf>) -> io::Result<Self> {
        let partial = free_name(&partition, &stem, ".partial");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&partial)?;
        // Held until the file is published: startup cleanup removes only unlocked
        // `.partial` files, whatever process wrote them.
        if !super::lock::try_lock(&file)? {
            return Err(io::Error::other(format!("{} is locked", partial.display())));
        }
        let writer = SerializedFileWriter::new(
            file,
            schema().map_err(io_error)?,
            Arc::new(properties().map_err(io_error)?),
        )
        .map_err(io_error)?;
        Ok(Self {
            writer: Some(writer),
            partition,
            stem,
            partial,
            replaces,
        })
    }

    /// Writes `rows` as one row group.
    pub(crate) fn write_group(&mut self, rows: &[Row]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("history file already finished"))?;
        let mut group = writer.next_row_group().map_err(io_error)?;
        for column in &COLUMNS {
            let mut out = group
                .next_column()
                .map_err(io_error)?
                .ok_or_else(|| io::Error::other("history schema has fewer columns than expected"))?;
            write_column(&mut out, &column.get, rows).map_err(io_error)?;
            out.close().map_err(io_error)?;
        }
        group.close().map_err(io_error)?;
        Ok(())
    }

    pub(crate) fn bytes_written(&self) -> u64 {
        self.writer.as_ref().map_or(0, |w| w.bytes_written() as u64)
    }

    /// Writes the footer, syncs, and renames the file to its `.parquet` name.
    pub(crate) fn finish(mut self) -> io::Result<PathBuf> {
        let writer = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("history file already finished"))?;
        let finished = self.publish(writer);
        if finished.is_err() {
            let _ = std::fs::remove_file(&self.partial);
        }
        finished
    }

    fn publish(&mut self, writer: SerializedFileWriter<File>) -> io::Result<PathBuf> {
        let file = writer.into_inner().map_err(io_error)?;
        file.sync_all()?;
        // Published while still open, so still locked: no cleanup sees it unowned.
        let target = match self.replaces.take() {
            Some(target) => {
                std::fs::rename(&self.partial, &target)?;
                target
            }
            None => self.link_free_name()?,
        };
        sync_dir(&self.partition)?;
        drop(file);
        Ok(target)
    }

    /// Links the finished file under the first free `.parquet` name and drops the
    /// `.partial` name. A link never replaces an existing file, so another writer that
    /// took the same name in the meantime keeps its file. A filesystem without links
    /// gets a rename to a name that was free a moment before.
    fn link_free_name(&self) -> io::Result<PathBuf> {
        loop {
            let target = free_name(&self.partition, &self.stem, ".parquet");
            match std::fs::hard_link(&self.partial, &target) {
                Ok(()) => {
                    std::fs::remove_file(&self.partial)?;
                    return Ok(target);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => {
                    std::fs::rename(&self.partial, &target)?;
                    return Ok(target);
                }
            }
        }
    }
}

impl Drop for ParquetFile {
    /// A file abandoned before its footer is unreadable; it goes rather than leaving a
    /// `.partial` for the next start to find.
    fn drop(&mut self) {
        if self.writer.take().is_some() {
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}

/// Every row of a history file, in order.
#[cfg(test)]
pub(crate) fn read_rows(path: &Path) -> io::Result<Vec<Row>> {
    let reader = SerializedFileReader::new(File::open(path)?).map_err(io_error)?;
    reader
        .get_row_iter(None)
        .map_err(io_error)?
        .map(|record| to_row(record.map_err(io_error)?))
        .collect()
}

/// Every row of a history file with only the columns a delete matches on set.
pub(crate) fn read_keys(path: &Path) -> io::Result<Vec<Row>> {
    let reader = SerializedFileReader::new(File::open(path)?).map_err(io_error)?;
    let projection = schema_of(COLUMNS.iter().filter(|c| KEY_COLUMNS.contains(&c.name))).map_err(io_error)?;
    reader
        .get_row_iter(Some((*projection).clone()))
        .map_err(io_error)?
        .map(|record| to_row(record.map_err(io_error)?))
        .collect()
}

fn to_row(record: parquet::record::Row) -> io::Result<Row> {
    let mut row = Row::default();
    for (name, field) in record.into_columns() {
        let column = COLUMNS
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unknown history column {name}")))?;
        (column.set)(&mut row, field)?;
    }
    Ok(row)
}

/// Rewrites the file at `path` without the rows `remove` matches, one row group at a
/// time, so memory stays near one group whatever the file's size. Returns the rows
/// removed; the file is left untouched when there are none and deleted when nothing
/// is left.
pub(crate) fn rewrite_without(path: &Path, remove: impl Fn(&Row) -> bool) -> io::Result<u64> {
    let reader = SerializedFileReader::new(File::open(path)?).map_err(io_error)?;
    let mut out = ParquetFile::replacing(path)?;
    let (mut removed, mut kept) = (0u64, 0u64);
    for group in 0..reader.num_row_groups() {
        let rows = reader
            .get_row_group(group)
            .map_err(io_error)?
            .get_row_iter(None)
            .map_err(io_error)?
            .map(|record| to_row(record.map_err(io_error)?))
            .collect::<io::Result<Vec<Row>>>()?;
        let before = rows.len();
        let rows: Vec<Row> = rows.into_iter().filter(|row| !remove(row)).collect();
        removed += (before - rows.len()) as u64;
        kept += rows.len() as u64;
        if !rows.is_empty() {
            out.write_group(&rows)?;
        }
    }
    // Closed before the file is replaced: Windows does not replace a file still open.
    drop(reader);
    if removed == 0 {
        return Ok(0);
    }
    if kept == 0 {
        drop(out);
        std::fs::remove_file(path)?;
    } else {
        out.finish()?;
    }
    Ok(removed)
}

/// A finished history file under a history folder.
pub(crate) struct HistoryFile {
    pub(crate) partition: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
}

/// The finished history files under `dir`, oldest first by partition date and name.
pub(crate) fn history_files(dir: &Path) -> io::Result<Vec<HistoryFile>> {
    let mut out = Vec::new();
    for partition in std::fs::read_dir(dir)? {
        let partition = partition?;
        let partition_name = partition.file_name().to_string_lossy().into_owned();
        if !partition_name.starts_with("date=") || !partition.file_type()?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(partition.path())? {
            let file = file?;
            let name = file.file_name().to_string_lossy().into_owned();
            if !is_history_file(&name) {
                continue;
            }
            let len = file.metadata()?.len();
            out.push(HistoryFile {
                partition: partition_name.clone(),
                name,
                path: file.path(),
                len,
            });
        }
    }
    out.sort_by(|a, b| (&a.partition, &a.name).cmp(&(&b.partition, &b.name)));
    Ok(out)
}

/// Removes `date=` partitions under `dir` that hold nothing.
pub(crate) fn remove_empty_partitions(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("date=") {
            // Fails while the partition still holds anything, which is the point.
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

/// The PID in an in-progress file's name.
pub(crate) fn partial_pid(name: &str) -> Option<u32> {
    parse_name(name.strip_suffix(".partial")?)
}

/// Whether `name` is a finished history file this module wrote.
pub(crate) fn is_history_file(name: &str) -> bool {
    name.strip_suffix(".parquet").and_then(parse_name).is_some()
}

fn parse_name(stem: &str) -> Option<u32> {
    let mut parts = stem.split('-');
    let time = parts.next()?;
    let pid = parts.next()?.parse().ok()?;
    let seq = parts.next()?;
    let ok = time.len() == 6
        && time.bytes().all(|b| b.is_ascii_digit())
        && !seq.is_empty()
        && seq.bytes().all(|b| b.is_ascii_digit())
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then_some(pid)
}

#[cfg(test)]
#[path = "file_tests.rs"]
pub(crate) mod tests;
