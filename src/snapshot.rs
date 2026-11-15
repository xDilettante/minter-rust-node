use crate::iavl::{Content, Hash, MAX_HEIGHT, Node, empty_hash, node_key, root_key};
use crate::{
    Checkpoint, ExportStage, KeyRead, OperationControl, load_node, metadata_key,
    read_verified_node, write_sync,
};
use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression, bufread::ZlibDecoder, write::ZlibEncoder};
use prost::Message;
use rocksdb::{DB, WriteBatch};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAX_ITEM_BYTES: usize = 64_000_000;
const BATCH_NODES: usize = 10_000;
const BATCH_BYTES: usize = 8 * 1024 * 1024;
const APP_KEYS: &[&str] = &[
    "validators",
    "height",
    "hash",
    "versions",
    "blockDelta",
    "startHeight",
    "emission",
    "price",
];

pub(crate) fn export(
    db: &impl KeyRead,
    root: Option<Hash>,
    version: u64,
    metadata: &[(&str, Vec<u8>)],
    file: &mut File,
    control: &OperationControl,
) -> Result<u64> {
    control.export_progress(ExportStage::IavlNodes, 0)?;
    let mut encoder = ZlibEncoder::new(
        BufWriter::with_capacity(256 * 1024, file),
        Compression::new(7),
    );
    let mut frame = Vec::new();
    for (name, value) in metadata {
        ensure!(
            APP_KEYS.contains(name) && !value.is_empty(),
            "invalid snapshot metadata {name}"
        );
        encode_item(
            &mut encoder,
            &mut frame,
            Item::Store(StoreItem {
                name: (*name).into(),
                value: value.clone(),
            }),
        )?;
    }
    encode_item(
        &mut encoder,
        &mut frame,
        Item::Store(StoreItem {
            name: "state".into(),
            value: Vec::new(),
        }),
    )?;
    let nodes = if let Some(root) = root {
        let mut writer = TreeWriter {
            db,
            version,
            encoder: &mut encoder,
            frame,
            scratch: Vec::new(),
            control,
            done: 0,
        };
        writer.write_node(root, MAX_HEIGHT + 1)?
    } else {
        0
    };
    encoder.finish()?.flush()?;
    control.export_progress(ExportStage::IavlNodes, nodes)?;
    Ok(nodes)
}

struct TreeWriter<'a, 'b, R, W> {
    db: &'a R,
    version: u64,
    encoder: &'b mut W,
    frame: Vec<u8>,
    scratch: Vec<u8>,
    control: &'a OperationControl,
    done: u64,
}

impl<R: KeyRead, W: Write> TreeWriter<'_, '_, R, W> {
    fn write_node(&mut self, hash: Hash, max_height: u8) -> Result<u64> {
        self.control.check()?;
        let raw = load_node(self.db, hash)?;
        let node = read_verified_node(&raw, hash, self.version, max_height, &mut self.scratch)?;
        let (value, children) = match node.content {
            Content::Leaf(value) => (value.to_vec(), 0),
            Content::Branch { left, right } => (
                Vec::new(),
                self.write_node(left, node.height)? + self.write_node(right, node.height)?,
            ),
        };
        encode_item(
            self.encoder,
            &mut self.frame,
            Item::Iavl(IavlItem {
                key: node.key.to_vec(),
                value,
                version: node.version.try_into()?,
                height: i32::from(node.height),
            }),
        )?;
        self.done += 1;
        if self.done.is_multiple_of(1000) {
            self.control
                .export_progress(ExportStage::IavlNodes, self.done)?;
        }
        Ok(children + 1)
    }
}

fn encode_item(writer: &mut impl Write, frame: &mut Vec<u8>, item: Item) -> Result<()> {
    let item = SnapshotItem { item: Some(item) };
    ensure!(
        item.encoded_len() <= MAX_ITEM_BYTES,
        "snapshot item exceeds Go's size limit"
    );
    frame.clear();
    item.encode_length_delimited(frame)?;
    writer.write_all(frame)?;
    Ok(())
}

#[derive(Message)]
struct SnapshotItem {
    #[prost(oneof = "Item", tags = "1, 2")]
    item: Option<Item>,
}

#[derive(prost::Oneof)]
enum Item {
    #[prost(message, tag = "1")]
    Store(StoreItem),
    #[prost(message, tag = "2")]
    Iavl(IavlItem),
}

#[derive(Message)]
struct StoreItem {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(bytes, tag = "2")]
    value: Vec<u8>,
}

#[derive(Message)]
struct IavlItem {
    #[prost(bytes, tag = "1")]
    key: Vec<u8>,
    #[prost(bytes, tag = "2")]
    value: Vec<u8>,
    #[prost(int64, tag = "3")]
    version: i64,
    #[prost(int32, tag = "4")]
    height: i32,
}

pub(crate) fn open_verified(path: &Path, trusted: &Checkpoint) -> Result<File> {
    let mut file = File::open(path).with_context(|| format!("open snapshot {}", path.display()))?;
    verify_file(&mut file, trusted)?;
    Ok(file)
}

pub(crate) fn verify_file(file: &mut File, trusted: &Checkpoint) -> Result<()> {
    verify_file_controlled(file, trusted, &OperationControl::default())
}

pub(crate) fn verify_file_controlled(
    file: &mut File,
    trusted: &Checkpoint,
    control: &OperationControl,
) -> Result<()> {
    control.check()?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "snapshot must be a regular file");
    ensure!(
        metadata.len() == trusted.bytes,
        "snapshot size differs from the trusted manifest"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut digest = DigestReader::new(control.reader(&mut *file));
    io::copy(&mut digest, &mut io::sink())?;
    digest.verify(trusted)?;
    control.check()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

pub(crate) struct Imported {
    pub nodes: WriteBatch,
    pub root: Option<Hash>,
    pub metadata: BTreeMap<String, Vec<u8>>,
}

impl Imported {
    pub fn into_batch(self, height: u64) -> WriteBatch {
        let mut batch = self.nodes;
        // Go stores an empty value at the root key for an empty tree.
        batch.put(
            root_key(height),
            self.root.as_ref().map_or(&[][..], |hash| hash.as_slice()),
        );
        for (name, value) in self.metadata {
            batch.put(metadata_key(&name), value);
        }
        batch
    }
}

pub(crate) fn load(db: &DB, file: File, trusted: &Checkpoint) -> Result<Imported> {
    load_controlled(db, file, trusted, &OperationControl::default())
}

pub(crate) fn load_controlled(
    db: &DB,
    file: File,
    trusted: &Checkpoint,
    control: &OperationControl,
) -> Result<Imported> {
    control.snapshot_progress(0, trusted.nodes)?;
    let compressed = BufReader::with_capacity(64 * 1024, DigestReader::new(control.reader(file)));
    let mut stream = BufReader::with_capacity(64 * 1024, ZlibDecoder::new(compressed));
    let mut frame = Vec::new();
    let mut metadata = BTreeMap::new();
    let mut seen_state = false;
    let mut tree = Postorder::default();
    let mut batch = WriteBatch::default();
    let mut nodes = 0;

    while let Some(len) = frame_length(&mut stream)? {
        control.check()?;
        frame.resize(len, 0);
        stream
            .read_exact(&mut frame)
            .context("truncated snapshot item")?;
        match SnapshotItem::decode(frame.as_slice())?
            .item
            .context("empty snapshot item")?
        {
            Item::Store(item) => {
                ensure!(!seen_state, "unexpected store item after the state marker");
                if item.name == "state" {
                    ensure!(item.value.is_empty(), "invalid state marker");
                    seen_state = true;
                } else {
                    ensure!(
                        APP_KEYS.contains(&item.name.as_str()),
                        "unknown AppDB key {}",
                        item.name
                    );
                    ensure!(
                        metadata.insert(item.name, item.value).is_none(),
                        "duplicate AppDB key"
                    );
                }
            }
            Item::Iavl(item) => {
                ensure!(seen_state, "IAVL node before the state marker");
                ensure!(nodes < trusted.nodes, "snapshot contains extra IAVL nodes");
                tree.add(item, trusted.height, &mut batch)
                    .with_context(|| format!("IAVL node {}", nodes + 1))?;
                nodes += 1;
                if batch.len() >= BATCH_NODES || batch.size_in_bytes() >= BATCH_BYTES {
                    write_sync(db, std::mem::take(&mut batch))?;
                    control.snapshot_progress(nodes, trusted.nodes)?;
                }
            }
        }
    }

    let decoder = stream.into_inner();
    let zlib_bytes = decoder.total_in();
    let mut compressed = decoder.into_inner();
    let mut repeated_checksum = None;
    if zlib_bytes >= 4 && trusted.bytes.checked_sub(zlib_bytes) == Some(4) {
        // The Go network exporter closes zlib through the protobuf writer and
        // then again directly. It repeats the four-byte Adler-32 trailer.
        let mut checksum = [0; 4];
        compressed.read_exact(&mut checksum)?;
        repeated_checksum = Some(checksum);
    }
    ensure!(
        compressed.fill_buf()?.is_empty(),
        "trailing bytes after the zlib stream"
    );
    let mut source = compressed.into_inner();
    if let Some(repeated) = repeated_checksum {
        let mut checksum = [0; 4];
        source.reader.seek(SeekFrom::Start(zlib_bytes - 4))?;
        source.reader.read_exact(&mut checksum)?;
        ensure!(checksum == repeated, "trailing bytes after the zlib stream");
    }
    // Check the bytes actually used by the decoder as well as the preflight read.
    source.verify(trusted)?;
    ensure!(seen_state, "snapshot is missing the state marker");
    ensure!(
        nodes == trusted.nodes,
        "snapshot node count differs from the trusted manifest"
    );
    ensure!(
        metadata.get("height").map(Vec::as_slice) == Some(&trusted.height.to_be_bytes()),
        "AppDB height differs from the trusted checkpoint"
    );
    ensure!(
        metadata.get("hash").map(Vec::as_slice) == Some(&trusted.app_hash),
        "AppDB hash differs from the trusted checkpoint"
    );
    let root = tree.root()?;
    let computed = root.unwrap_or_else(empty_hash);
    ensure!(
        computed == trusted.app_hash,
        "calculated AppHash {} differs from expected {}",
        hex::encode(computed),
        hex::encode(trusted.app_hash)
    );

    control.snapshot_progress(nodes, trusted.nodes)?;
    Ok(Imported {
        nodes: batch,
        root,
        metadata,
    })
}

fn frame_length(stream: &mut impl Read) -> Result<Option<usize>> {
    let mut len = 0;
    for shift in (0..28).step_by(7) {
        let mut byte = [0];
        let read = stream.read(&mut byte)?;
        if read == 0 && shift == 0 {
            return Ok(None);
        }
        ensure!(read == 1, "truncated snapshot frame length");
        len |= usize::from(byte[0] & 127) << shift;
        if byte[0] < 128 {
            ensure!(
                len > 0 && len <= MAX_ITEM_BYTES,
                "invalid snapshot item length {len}"
            );
            return Ok(Some(len));
        }
    }
    bail!("snapshot frame exceeds Go's item limit")
}

struct DigestReader<R> {
    reader: R,
    hash: Sha256,
    count: u64,
}

impl<R> DigestReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            hash: Sha256::new(),
            count: 0,
        }
    }

    fn verify(self, trusted: &Checkpoint) -> Result<()> {
        let actual: Hash = self.hash.finalize().into();
        ensure!(
            self.count == trusted.bytes,
            "snapshot size differs from the trusted manifest"
        );
        ensure!(
            actual == trusted.snapshot_sha256,
            "snapshot SHA-256 {} differs from expected {}",
            hex::encode(actual),
            hex::encode(trusted.snapshot_sha256)
        );
        Ok(())
    }
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.reader.read(buffer)?;
        self.hash.update(&buffer[..read]);
        self.count += read as u64;
        Ok(read)
    }
}

struct Subtree {
    height: u8,
    size: u64,
    version: u64,
    hash: Hash,
    min: Vec<u8>,
    max: Vec<u8>,
}

#[derive(Default)]
struct Postorder {
    stack: Vec<Subtree>,
    encoded: Vec<u8>,
    hash_input: Vec<u8>,
}

impl Postorder {
    fn add(
        &mut self,
        item: IavlItem,
        checkpoint_height: u64,
        batch: &mut WriteBatch,
    ) -> Result<()> {
        let height = u8::try_from(item.height).context("invalid IAVL height")?;
        let version = u64::try_from(item.version).context("invalid IAVL version")?;
        ensure!(height <= MAX_HEIGHT, "IAVL height exceeds Go int8");
        ensure!(
            (1..=checkpoint_height).contains(&version),
            "IAVL version exceeds checkpoint range"
        );
        let (content, size, min, max) = if height == 0 {
            (
                Content::Leaf(&item.value),
                1,
                item.key.clone(),
                item.key.clone(),
            )
        } else {
            ensure!(item.value.is_empty(), "IAVL branch contains a value");
            let right = self.stack.pop().context("IAVL branch has no right child")?;
            let left = self.stack.pop().context("IAVL branch has no left child")?;
            ensure!(
                height == left.height.max(right.height) + 1
                    && left.height.abs_diff(right.height) <= 1,
                "invalid IAVL branch height or balance"
            );
            ensure!(
                version >= left.version.max(right.version),
                "IAVL child is newer than its parent"
            );
            // The separator controls lookups even though IAVL excludes it from AppHash.
            ensure!(
                left.max < item.key && item.key == right.min,
                "invalid IAVL branch separator"
            );
            let size = left
                .size
                .checked_add(right.size)
                .context("IAVL size overflow")?;
            ensure!(i64::try_from(size).is_ok(), "IAVL size exceeds Go int64");
            (
                Content::Branch {
                    left: left.hash,
                    right: right.hash,
                },
                size,
                left.min,
                right.max,
            )
        };

        let node = Node {
            height,
            size,
            version,
            key: &item.key,
            content,
        };
        let hash = node.hash(&mut self.hash_input);
        node.encode(&mut self.encoded);
        batch.put(node_key(hash), &self.encoded);
        self.stack.push(Subtree {
            height,
            size,
            version,
            hash,
            min,
            max,
        });
        ensure!(
            self.stack.len() <= usize::from(MAX_HEIGHT) + 1,
            "invalid IAVL postorder stack"
        );
        Ok(())
    }

    fn root(&self) -> Result<Option<Hash>> {
        match self.stack.as_slice() {
            [] => Ok(None),
            [root] => Ok(Some(root.hash)),
            _ => bail!("snapshot contains multiple IAVL roots"),
        }
    }
}
