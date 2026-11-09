//! History Parquet files: the column codec, and files written as `.partial` and
//! published once the footer is on disk, so a reader globbing `*.parquet` never sees a
//! file without one.

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

use super::dir::is_history_file;
use super::row::{Outcome, Row, Source, Transport};

enum Get {
    Str(fn(&Row) -> Option<&str>),
    Bin(fn(&Row) -> Option<&[u8]>),
    I32(fn(&Row) -> Option<i32>),
    I64(fn(&Row) -> Option<i64>),
    Ts(fn(&Row) -> Option<i64>),
    Bool(fn(&Row) -> Option<bool>),
    List(fn(&Row) -> Option<&[String]>),
}

/// What a column is for beyond holding its value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Plain,
    /// Read by a delete to decide which rows match, without reading bodies.
    Key,
    /// A whole body: no dictionary (every value is distinct) and no statistics (min/max
    /// of a body would copy prompt text into the footer).
    Body,
}

struct Column {
    name: &'static str,
    get: Get,
    set: fn(&mut Row, Field) -> io::Result<()>,
    role: Role,
}

impl Column {
    const fn key(self) -> Self {
        Self {
            role: Role::Key,
            ..self
        }
    }

    const fn body(self) -> Self {
        Self {
            role: Role::Body,
            ..self
        }
    }

    /// The leaf's path in the schema.
    fn leaf(&self) -> ColumnPath {
        let mut path = vec![self.name.to_owned()];
        if matches!(self.get, Get::List(_)) {
            path.extend(["list".to_owned(), "element".to_owned()]);
        }
        ColumnPath::new(path)
    }
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
    ($name:ident, Enum($ty:ident)) => {
        Column {
            name: stringify!($name),
            get: Get::Str(|r| Some(r.$name.as_str())),
            set: |r, f| {
                let value = opt_str(stringify!($name), f)?.unwrap_or_default();
                r.$name = $ty::parse(&value).ok_or_else(|| bad(stringify!($name), &Field::Str(value)))?;
                Ok(())
            },
            role: Role::Plain,
        }
    };
    ($name:ident, Str) => {
        Column {
            name: stringify!($name),
            get: Get::Str(|r| r.$name.as_deref()),
            set: |r, f| {
                r.$name = opt_str(stringify!($name), f)?;
                Ok(())
            },
            role: Role::Plain,
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
            role: Role::Plain,
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
            role: Role::Plain,
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
            role: Role::Plain,
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
            role: Role::Plain,
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
            role: Role::Plain,
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
            role: Role::Plain,
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
            role: Role::Plain,
        }
    };
}

/// The file's columns, in schema order.
static COLUMNS: [Column; 41] = [
    Column {
        name: "exchange_id",
        get: Get::Str(|r| Some(r.exchange_id.as_str())),
        set: |r, f| {
            r.exchange_id = opt_str("exchange_id", f)?.unwrap_or_default();
            Ok(())
        },
        role: Role::Plain,
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
        role: Role::Plain,
    }
    .key(),
    Column {
        name: "ended_at",
        get: Get::Ts(|r| r.ended_at_us),
        set: |r, f| {
            r.ended_at_us = opt_i64("ended_at", f)?;
            Ok(())
        },
        role: Role::Plain,
    },
    column!(source, Enum(Source)),
    column!(endpoint, Str),
    column!(transport, Enum(Transport)),
    column!(provider, Str),
    column!(model_requested, Str).key(),
    column!(model, Str).key(),
    column!(response_model, Str),
    column!(account, Str).key(),
    column!(auth_index, Str),
    column!(session_id, Str).key(),
    column!(user_agent, Str),
    column!(stream, Bool),
    column!(status, OptI32),
    column!(outcome, Enum(Outcome)),
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
    column!(request_body, Str).body(),
    column!(request_body_raw, Bin).body(),
    column!(response_body, Str).body(),
    column!(response_body_raw, Bin).body(),
    column!(request_frames, List).body(),
    column!(response_frames, List).body(),
    column!(request_bytes, I64),
    column!(response_bytes, I64),
    column!(request_truncated, Bool),
    column!(response_truncated, Bool),
];

fn schema() -> parquet::errors::Result<parquet::schema::types::TypePtr> {
    schema_of(COLUMNS.iter())
}

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
    for path in COLUMNS.iter().filter(|c| c.role == Role::Body).map(Column::leaf) {
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
    /// The in-progress path, until the file is finished.
    pub(crate) fn partial(&self) -> &Path {
        &self.partial
    }

    pub(crate) fn create(dir: &Path, now: DateTime<Utc>, seq: u32) -> io::Result<Self> {
        cpa_core::private_fs::create_dir_all(dir)?;
        let partition = dir.join(format!("date={}", now.format("%Y-%m-%d")));
        cpa_core::private_fs::create_dir_all(&partition)?;
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
        let file = cpa_core::private_fs::create_new(&partial)?;
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
    let projection = schema_of(COLUMNS.iter().filter(|c| c.role == Role::Key)).map_err(io_error)?;
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

#[cfg(test)]
#[path = "file_tests.rs"]
pub(crate) mod tests;
