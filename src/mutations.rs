use crate::iavl::{Content, Hash, MAX_HEIGHT, Node, node_key};
use crate::{GenesisStage, OperationControl, load_node};
use anyhow::{Context, Result, bail, ensure};
use rocksdb::{DB, WriteBatch};
use std::cmp::Ordering;

pub enum Mutation {
    Set { key: Vec<u8>, value: Vec<u8> },
    Remove(Vec<u8>),
}

pub(crate) fn apply(
    db: &DB,
    version: u64,
    root: Option<Hash>,
    operations: impl IntoIterator<Item = Mutation>,
    batch: &mut WriteBatch,
    reused: impl FnMut(Hash),
) -> Result<Option<Hash>> {
    apply_inner(db, version, root, operations, batch, reused, |_, _| Ok(()))
}

pub(crate) fn apply_genesis(
    db: &DB,
    operations: impl IntoIterator<Item = Mutation>,
    batch: &mut WriteBatch,
    control: &OperationControl,
) -> Result<Option<Hash>> {
    apply_inner(
        db,
        1,
        None,
        operations,
        batch,
        |_| {},
        |phase, done| control.genesis_progress(phase, done),
    )
}

fn apply_inner(
    db: &DB,
    version: u64,
    root: Option<Hash>,
    operations: impl IntoIterator<Item = Mutation>,
    batch: &mut WriteBatch,
    mut reused: impl FnMut(Hash),
    mut progress: impl FnMut(GenesisStage, u64) -> Result<()>,
) -> Result<Option<Hash>> {
    let mut edit = Edit {
        db,
        version,
        scratch: Vec::new(),
    };
    let mut root = root.map(Link::Stored);
    progress(GenesisStage::IavlMutations, 0)?;
    for (index, operation) in operations.into_iter().enumerate() {
        root = match operation {
            Mutation::Set { key, value } => Some(match root {
                Some(link) => edit.set(link, key, value)?.0,
                None => edit.leaf(key, value),
            }),
            Mutation::Remove(key) => match root {
                Some(link) => match edit.remove(link, &key)? {
                    Removal::Unchanged(link) | Removal::Replaced(link, _) => Some(link),
                    Removal::Deleted => None,
                },
                None => None,
            },
        };
        progress(GenesisStage::IavlMutations, index as u64 + 1)?;
    }
    progress(GenesisStage::IavlNodes, 0)?;
    root.map(|root| root.append(batch, &mut edit.scratch, &mut reused, &mut progress, &mut 0))
        .transpose()
}

// Load nodes lazily and reuse the original hash of every untouched subtree.
enum Link {
    Stored(Hash),
    Loaded(Box<Loaded>),
}

struct Loaded {
    hash: Option<Hash>,
    key: Vec<u8>,
    version: u64,
    height: u8,
    size: u64,
    body: Body,
}

enum Body {
    Leaf(Vec<u8>),
    Branch { left: Link, right: Link },
}

impl Loaded {
    fn link(self) -> Link {
        Link::Loaded(Box::new(self))
    }
}

impl Link {
    fn append(
        &self,
        batch: &mut WriteBatch,
        scratch: &mut Vec<u8>,
        reused: &mut impl FnMut(Hash),
        progress: &mut impl FnMut(GenesisStage, u64) -> Result<()>,
        done: &mut u64,
    ) -> Result<Hash> {
        let node = match self {
            Self::Stored(hash) => {
                reused(*hash);
                return Ok(*hash);
            }
            Self::Loaded(node) => node,
        };
        if let Some(hash) = node.hash {
            reused(hash);
            return Ok(hash);
        }
        let content = match &node.body {
            Body::Leaf(value) => Content::Leaf(value),
            Body::Branch { left, right } => Content::Branch {
                left: left.append(batch, scratch, reused, progress, done)?,
                right: right.append(batch, scratch, reused, progress, done)?,
            },
        };
        let node = Node {
            key: &node.key,
            height: node.height,
            size: node.size,
            version: node.version,
            content,
        };
        let hash = node.hash(scratch);
        node.encode(scratch);
        batch.put(node_key(hash), &*scratch);
        *done += 1;
        progress(GenesisStage::IavlNodes, *done)?;
        Ok(hash)
    }
}

enum Removal {
    Unchanged(Link),
    Deleted,
    // A changed leftmost key propagates until the first right-child edge.
    Replaced(Link, Option<Vec<u8>>),
}

struct Edit<'a> {
    db: &'a DB,
    version: u64,
    scratch: Vec<u8>,
}

type Dimensions = (u8, u64);

impl Edit<'_> {
    fn read(&mut self, hash: Hash) -> Result<Loaded> {
        let raw = load_node(self.db, hash)?;
        let node = Node::decode(&raw)?;
        ensure!(
            node.version < self.version && node.hash(&mut self.scratch) == hash,
            "invalid stored IAVL node {}",
            hex::encode(hash)
        );
        Ok(Loaded {
            hash: Some(hash),
            key: node.key.to_vec(),
            height: node.height,
            size: node.size,
            version: node.version,
            body: match node.content {
                Content::Leaf(value) => Body::Leaf(value.to_vec()),
                Content::Branch { left, right } => Body::Branch {
                    left: Link::Stored(left),
                    right: Link::Stored(right),
                },
            },
        })
    }

    fn load(&mut self, link: Link) -> Result<Loaded> {
        match link {
            Link::Stored(hash) => self.read(hash),
            Link::Loaded(node) => Ok(*node),
        }
    }

    fn measure(&mut self, link: &mut Link) -> Result<Dimensions> {
        if let Link::Stored(hash) = *link {
            *link = self.read(hash)?.link();
        }
        let Link::Loaded(node) = link else {
            unreachable!();
        };
        Ok((node.height, node.size))
    }

    fn leaf(&self, key: Vec<u8>, value: Vec<u8>) -> Link {
        Loaded {
            hash: None,
            key,
            version: self.version,
            height: 0,
            size: 1,
            body: Body::Leaf(value),
        }
        .link()
    }

    fn make_branch(
        &self,
        key: Vec<u8>,
        left: Link,
        right: Link,
        (left_height, left_size): Dimensions,
        (right_height, right_size): Dimensions,
    ) -> Result<Link> {
        let height = left_height.max(right_height) + 1;
        let size = left_size + right_size;
        ensure!(
            height <= MAX_HEIGHT && size <= i64::MAX as u64,
            "IAVL branch metadata overflow"
        );
        Ok(Loaded {
            hash: None,
            key,
            version: self.version,
            height,
            size,
            body: Body::Branch { left, right },
        }
        .link())
    }

    fn branch(&mut self, key: Vec<u8>, mut left: Link, mut right: Link) -> Result<Link> {
        let left_dims = self.measure(&mut left)?;
        let right_dims = self.measure(&mut right)?;
        self.make_branch(key, left, right, left_dims, right_dims)
    }

    fn split_branch(&mut self, link: Link) -> Result<(Vec<u8>, Link, Link)> {
        let node = self.load(link)?;
        let Body::Branch { left, right } = node.body else {
            bail!("IAVL rotation requires an inner node");
        };
        Ok((node.key, left, right))
    }

    fn balance(&mut self, key: Vec<u8>, mut left: Link, mut right: Link) -> Result<Link> {
        let left_dims = self.measure(&mut left)?;
        let right_dims = self.measure(&mut right)?;
        let delta = i16::from(left_dims.0) - i16::from(right_dims.0);
        // Each rotation stamps the promoted and demoted nodes with this version.
        if delta > 1 {
            let (left_key, mut outer, mut inner) = self.split_branch(left)?;
            if self.measure(&mut outer)?.0 >= self.measure(&mut inner)?.0 {
                let right = self.branch(key, inner, right)?;
                return self.branch(left_key, outer, right);
            }
            let (inner_key, inner_left, inner_right) = self.split_branch(inner)?;
            let left = self.branch(left_key, outer, inner_left)?;
            let right = self.branch(key, inner_right, right)?;
            return self.branch(inner_key, left, right);
        }
        if delta < -1 {
            let (right_key, mut inner, mut outer) = self.split_branch(right)?;
            if self.measure(&mut outer)?.0 >= self.measure(&mut inner)?.0 {
                let left = self.branch(key, left, inner)?;
                return self.branch(right_key, left, outer);
            }
            let (inner_key, inner_left, inner_right) = self.split_branch(inner)?;
            let left = self.branch(key, left, inner_left)?;
            let right = self.branch(right_key, inner_right, outer)?;
            return self.branch(inner_key, left, right);
        }
        self.make_branch(key, left, right, left_dims, right_dims)
    }

    fn set(&mut self, link: Link, key: Vec<u8>, value: Vec<u8>) -> Result<(Link, bool)> {
        let node = self.load(link)?;
        let ordering = key.cmp(&node.key);
        let Body::Branch { left, right } = node.body else {
            if ordering == Ordering::Equal {
                return Ok((self.leaf(key, value), true));
            }
            let separator = if ordering == Ordering::Less {
                node.key.clone()
            } else {
                key.clone()
            };
            let leaf = self.leaf(key, value);
            let (left, right) = if ordering == Ordering::Less {
                (leaf, node.link())
            } else {
                (node.link(), leaf)
            };
            return Ok((
                self.make_branch(separator, left, right, (0, 1), (0, 1))?,
                false,
            ));
        };

        let (left, right, updated) = if ordering == Ordering::Less {
            let (left, updated) = self.set(left, key, value)?;
            (left, right, updated)
        } else {
            let (right, updated) = self.set(right, key, value)?;
            (left, right, updated)
        };
        if updated {
            return Ok((
                Loaded {
                    hash: None,
                    version: self.version,
                    body: Body::Branch { left, right },
                    ..node
                }
                .link(),
                true,
            ));
        }
        Ok((self.balance(node.key, left, right)?, false))
    }

    fn remove(&mut self, link: Link, key: &[u8]) -> Result<Removal> {
        let node = self.load(link)?;
        let Body::Branch { left, right } = node.body else {
            return Ok(if key == node.key {
                Removal::Deleted
            } else {
                Removal::Unchanged(node.link())
            });
        };

        let edit_left = key < node.key.as_slice();
        let (edited, sibling) = if edit_left {
            (left, right)
        } else {
            (right, left)
        };
        let (edited, new_minimum, changed) = match self.remove(edited, key)? {
            Removal::Deleted => {
                return Ok(Removal::Replaced(sibling, edit_left.then_some(node.key)));
            }
            Removal::Unchanged(link) => (link, None, false),
            Removal::Replaced(link, new_minimum) => (link, new_minimum, true),
        };
        let (left, right) = if edit_left {
            (edited, sibling)
        } else {
            (sibling, edited)
        };
        if !changed {
            return Ok(Removal::Unchanged(
                Loaded {
                    body: Body::Branch { left, right },
                    ..node
                }
                .link(),
            ));
        }
        let (separator, new_minimum) = if edit_left {
            (node.key, new_minimum)
        } else {
            (new_minimum.unwrap_or(node.key), None)
        };
        let replacement = self
            .balance(separator, left, right)
            .context("rebalance after removal")?;
        Ok(Removal::Replaced(replacement, new_minimum))
    }
}
