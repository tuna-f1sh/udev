use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{env, fs, mem};
use std::collections::linked_list::Iter;

use crate::{Error, Result, Udev, UdevEntry, UdevList};

mod line;
mod trie;

pub use line::*;
pub use trie::*;

static NODE_SIZE: AtomicUsize = AtomicUsize::new(24);
static CHILD_ENTRY_SIZE: AtomicUsize = AtomicUsize::new(16);
static VALUE_ENTRY_SIZE: AtomicUsize = AtomicUsize::new(32);

/// Gets the [Node](TrieNode) size loaded from the [TrieHeader].
pub fn node_size() -> usize {
    NODE_SIZE.load(Ordering::Relaxed)
}

pub(crate) fn set_node_size(val: usize) {
    NODE_SIZE.store(val, Ordering::SeqCst);
}

/// Gets the [ChildEntry](TrieChildEntry) size loaded from the [TrieHeader].
pub fn child_entry_size() -> usize {
    CHILD_ENTRY_SIZE.load(Ordering::Relaxed)
}

pub(crate) fn set_child_entry_size(val: usize) {
    CHILD_ENTRY_SIZE.store(val, Ordering::SeqCst);
}

/// Gets the [ValueEntry](TrieValueEntry) size loaded from the [TrieHeader].
pub fn value_entry_size() -> usize {
    VALUE_ENTRY_SIZE.load(Ordering::Relaxed)
}

pub(crate) fn set_value_entry_size(val: usize) {
    VALUE_ENTRY_SIZE.store(val, Ordering::SeqCst);
}

#[cfg(target_os = "linux")]
const UDEV_LIBEXEC_DIR: &str = "/usr/lib/udev";
// FIXME: add udev libexec dirs for other OSes

fn get_hwdb_bin_paths() -> String {
    const DEFAULT_LOCATIONS: [&str; 2] = ["/etc/udev", UDEV_LIBEXEC_DIR];

    if let Ok(by_env) = env::var("UDEV_HWDB_BIN") {
        DEFAULT_LOCATIONS
            .iter()
            .fold(by_env, |path, loc| format!("{path}\0{loc}/hwdb.bin"))
    } else {
        DEFAULT_LOCATIONS
            .iter()
            .fold(String::new(), |path, loc| format!("{path}\0{loc}/hwdb.bin"))
    }
}

/// Represents the on-disk hardware database.
///
/// Retrieves properties from the hardware database.
#[repr(C)]
pub struct UdevHwdb {
    udev: Arc<Udev>,
    bin_paths: String,
    hwdb_path: String,
    head: TrieHeader,
    properties_list: UdevList,
}

impl UdevHwdb {
    /// Creates a new [UdevHwdb].
    pub fn new(udev: Arc<Udev>) -> Result<Self> {
        let mut hwdb_path = String::new();
        let bin_paths = get_hwdb_bin_paths();

        let (head, metadata) = {
            // In the original `libudev`, they `mmap` the entire on-disk database into a `const char *`
            // union, which leads to inherently unsafe access in Rust.
            //
            // Instead, we'll just parse the header for now, which advances the `File` struct's internal
            // cursor, and delay further parsing for subsequent calls to the various node entry, and value calls.
            //
            // Alternatively, we could parse the properties list now, and avoid keeping the file
            // struct, file metadata, and `TrieHeader` in the `UdevHwdb` struct. Instead, we would just
            // keep the parsed `properties_list`.
            //
            // TBD.

            let mut bin_file: Option<fs::File> = None;

            for path in bin_paths.split('\0') {
                if let Ok(f) = fs::OpenOptions::new().read(true).open(path) {
                    bin_file = Some(f);
                    path.clone_into(&mut hwdb_path);
                    break;
                }
                let errno = io::Error::last_os_error();
                if errno.raw_os_error() == Some(libc::ENOENT) {
                    Ok(())
                } else {
                    Err(Error::UdevHwdb(format!(
                        "error reading {path}, errno: {errno}"
                    )))
                }?;
            }

            let mut file = bin_file.ok_or(Error::UdevHwdb(
                "unable to find hwdb.bin database file".into(),
            ))?;

            let metadata = file.metadata()?;
            let mut hwdb_head_buf = [0u8; mem::size_of::<TrieHeader>()];

            file.read_exact(&mut hwdb_head_buf)?;

            (TrieHeader::try_from(hwdb_head_buf.as_ref())?, metadata)
        };

        let properties_list = UdevList::new(Arc::clone(&udev));

        set_node_size(head.node_size() as usize);
        set_child_entry_size(head.child_entry_size() as usize);
        set_value_entry_size(head.value_entry_size() as usize);

        log::debug!("=== trie on-disk ===");
        log::debug!("tool version:           {}", head.tool_version());
        log::debug!("file size:         {:8} bytes", metadata.len());
        log::debug!("header size:       {:8} bytes", head.header_size());
        log::debug!("node size:         {:8} bytes", head.node_size());
        log::debug!("child size:        {:8} bytes", head.child_entry_size());
        log::debug!("value size:        {:8} bytes", head.value_entry_size());
        log::debug!("strings:           {:8} bytes", head.strings_len());
        log::debug!("nodes:             {:8} bytes", head.nodes_len());

        Ok(Self {
            udev,
            bin_paths,
            hwdb_path,
            head,
            properties_list,
        })
    }

    /// Gets a reference to the [TrieHeader].
    pub const fn header(&self) -> &TrieHeader {
        &self.head
    }

    /// Looks up a matching device in the hardware database and populates property list.
    ///
    /// Parameters:
    ///
    /// - `modalias`: modalias string
    /// - `flags`: (unused), preserved for easier mapping to `libudev` C API
    ///
    /// From the `libudev` documentation:
    ///
    /// ```no_build,no_run
    /// The lookup key is a `modalias` string, whose formats are defined for the Linux kernel modules.
    /// Examples are: pci:v00008086d00001C2D*, usb:v04F2pB221*. The first entry
    /// of a list of retrieved properties is returned.
    /// ```
    ///
    /// Returns: an iterator of [UdevEntry]s for device.
    pub fn get_properties_list_entry(&mut self, modalias: &str, _flags: u32) -> Result<Iter<UdevEntry>> {
        // For now, do the naive thing, and read the entire HWDB into memory (12M+!!!)
        //
        // Using the BufReader to jump around to all the various offsets will probably be
        // more efficient, but harder to follow. BufReader only supports relative `Seek`ing.
        //
        // Nodes are also not sequential in the on-disk format, which would make parsing
        // easier, but lose some of the structure of the HWDB. According to the man page
        // (`man 7 hwdb`), entries later in the HWDB have higher priority, which some tools
        // may rely on.
        //
        // `libudev` does not appear to track priority.
        //
        // Loading everything into memory at one time also avoids some other tool updating the
        // HWDB while we are parsing it.
        let file = fs::OpenOptions::new()
            .read(true)
            .open(&self.hwdb_path).map_err(|err| {
                log::warn!("unable to open HWDB file: {err}");
                Error::UdevHwdb("unable to open HWDB file".into())
            })?;

        let metadata = file
            .metadata()
            .map_err(|err| {
                log::error!("unable to get HWDB metadata: {err}");
                Error::UdevHwdb("unable to get HWDB metadata".into())
            })?;

        let file_len = metadata.len() as usize;

        let mut reader = io::BufReader::new(file);
        let mut hwdb_buf = Vec::with_capacity(file_len);

        reader
            .read_to_end(&mut hwdb_buf)
            .map_err(|err| {
                log::warn!("error reading HWDB into memory: {err}");
                Error::UdevHwdb("error reading HWDB into memory".into())
            })?;

        self.properties_list.clear();

        Self::trie_search(&mut self.properties_list, &self.head, &hwdb_buf, modalias)
            .map_err(|err| {
                log::warn!("error looking up property list UdevEntry: {err}");
                Error::UdevHwdb("error looking up property list UdevEntry".into())
            })?;

        Ok(self.properties_list.iter())
    }

    /// Gets a reference to the [properties list](UdevList).
    pub const fn properties_list(&self) -> &UdevList {
        &self.properties_list
    }

    /// Adds a key-value pair to the property list.
    pub fn add_property(&mut self, key: &str, value: &str) -> Result<()> {
        Self::_add_property(&mut self.properties_list, key, value)
    }

    pub(crate) fn _add_property(list: &mut UdevList, key: &str, value: &str) -> Result<()> {
        if let Some(nkey) = key.strip_prefix(' ') {
            list.add_entry(nkey, value)
                .map(|_| ())
                .ok_or(Error::UdevHwdb("unable to add property".into()))
        } else {
            // Silently ignore all properties which do not start with a
            // space; future extensions might use additional prefixes.
            Ok(())
        }
    }

    /// Parses all [TrieEntry] nodes from an in-memory HWDB buffer.
    pub fn parse_nodes<'a>(
        head: &'a TrieHeader,
        hwdb_buf: &'a [u8],
    ) -> impl Iterator<Item = TrieEntry> + 'a {
        let nodes_len = head.nodes_len() as usize;
        let node_start = mem::size_of::<TrieHeader>();
        let node_end = node_start.saturating_add(nodes_len);

        let buf_len = hwdb_buf.len();

        let mut idx = node_start;

        std::iter::from_fn(move || {
            if (0..buf_len).contains(&node_start)
                && (0..buf_len).contains(&node_end)
                && idx < nodes_len
            {
                TrieEntry::try_from(&hwdb_buf[idx..])
                    .map(|entry| {
                        idx = idx.saturating_add(entry.len());
                        entry
                    })
                    .map_err(|err| {
                        log::error!("Error parsing TrieEntry: {err}");
                    })
                    .ok()
            } else {
                None
            }
        })
    }

    fn trie_search(
        list: &mut UdevList,
        head: &TrieHeader,
        hwdb_buf: &[u8],
        search: &str,
    ) -> Result<()> {
        let mut line_buf = LineBuf::new();
        let mut i = 0usize;
        let nodes_root_off = head.nodes_root_off() as usize;

        let mut node = if nodes_root_off < hwdb_buf.len() {
            TrieEntry::try_from(&hwdb_buf[nodes_root_off..]).ok()
        } else {
            None
        };

        log::trace!("Search term: {search}");
        let search_count = search.chars().count();

        while let Some(n) = node {
            let prefix_off = n.node().prefix_off() as usize;
            if prefix_off > 0 {
                let ts = trie_string(hwdb_buf, prefix_off);
                for (p, c) in ts.chars().enumerate() {
                    if c == '*' || c == '?' || c == '[' {
                        return line_buf.trie_fnmatch(list, hwdb_buf, &n, p, &search[i + p..]);
                    }
                    if search_count > i && Some(c) != search.chars().nth(i + p) {
                        return Ok(());
                    }
                }

                i = i.saturating_add(ts.chars().count());
            }

            if let Some(child) = n.lookup_child(hwdb_buf, b'*') {
                log::trace!("found matching child entry (glob): {child:?}");
                line_buf.add_char(b'*')?;
                line_buf.trie_fnmatch(list, hwdb_buf, &child, 0, &search[i..])?;
                line_buf.remove_char();
            }

            if let Some(child) = n.lookup_child(hwdb_buf, b'?') {
                log::trace!("found matching child entry (optional): {child:?}");
                line_buf.add_char(b'?')?;
                line_buf.trie_fnmatch(list, hwdb_buf, &child, 0, &search[i..])?;
                line_buf.remove_char();
            }

            if let Some(child) = n.lookup_child(hwdb_buf, b'[') {
                log::trace!("found matching child entry (range): {child:?}");
                line_buf.add_char(b'[')?;
                line_buf.trie_fnmatch(list, hwdb_buf, &child, 0, &search[i..])?;
                line_buf.remove_char();
            }

            if search.chars().nth(i) == Some('\0') || i >= search_count || i >= search.len() {
                for value in n.values().iter() {
                    let key_str = trie_string(hwdb_buf, value.key_off() as usize);
                    let val_str = trie_string(hwdb_buf, value.value_off() as usize);

                    log::trace!("Matching property, key: {key_str}, value: {val_str}");

                    Self::_add_property(list, key_str, val_str)?;
                }
            }

            node = n.lookup_child(hwdb_buf, *search.as_bytes().get(i).unwrap_or(&0));
            i = i.saturating_add(1);
            log::trace!("No match found, searching next child[{i}]: {node:?}");
        }

        Ok(())
    }
}
