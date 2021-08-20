use crate::internal::{
    self, consts, Allocator, Chain, DirEntry, Entries, EntriesOrder, Sector,
    SectorInit, Version,
};
use byteorder::{LittleEndian, WriteBytesExt};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use uuid::Uuid;

//===========================================================================//

macro_rules! malformed {
    ($e:expr) => { invalid_data!("Malformed directory ({})", $e) };
    ($fmt:expr, $($arg:tt)+) => {
        invalid_data!("Malformed directory ({})", format!($fmt, $($arg)+))
    };
}

//===========================================================================//

/// A wrapper around the sector allocator that additionally provides management
/// of the CFB directory chain.
pub struct Directory<F> {
    allocator: Allocator<F>,
    dir_entries: Vec<DirEntry>,
    dir_start_sector: u32,
}

impl<F> Directory<F> {
    pub fn new(
        allocator: Allocator<F>,
        dir_entries: Vec<DirEntry>,
        dir_start_sector: u32,
    ) -> io::Result<Directory<F>> {
        let directory = Directory { allocator, dir_entries, dir_start_sector };
        directory.validate()?;
        Ok(directory)
    }

    pub fn version(&self) -> Version {
        self.allocator.version()
    }

    pub fn sector_len(&self) -> usize {
        self.allocator.sector_len()
    }

    pub fn into_inner(self) -> F {
        self.allocator.into_inner()
    }

    pub fn stream_id_for_name_chain(&self, names: &[&str]) -> Option<u32> {
        let mut stream_id = consts::ROOT_STREAM_ID;
        for name in names.iter() {
            stream_id = self.dir_entry(stream_id).child;
            loop {
                if stream_id == consts::NO_STREAM {
                    return None;
                }
                let dir_entry = self.dir_entry(stream_id);
                match internal::path::compare_names(&name, &dir_entry.name) {
                    Ordering::Equal => break,
                    Ordering::Less => stream_id = dir_entry.left_sibling,
                    Ordering::Greater => stream_id = dir_entry.right_sibling,
                }
            }
        }
        Some(stream_id)
    }

    /// Returns an iterator over the entries within a storage object.
    pub fn storage_entries(&self, path: &Path) -> io::Result<Entries> {
        let names = internal::path::name_chain_from_path(path)?;
        let path = internal::path::path_from_name_chain(&names);
        let stream_id = match self.stream_id_for_name_chain(&names) {
            Some(stream_id) => stream_id,
            None => not_found!("No such storage: {:?}", path),
        };
        let start = {
            let dir_entry = self.dir_entry(stream_id);
            if dir_entry.obj_type == consts::OBJ_TYPE_STREAM {
                invalid_input!("Not a storage: {:?}", path);
            }
            debug_assert!(
                dir_entry.obj_type == consts::OBJ_TYPE_STORAGE
                    || dir_entry.obj_type == consts::OBJ_TYPE_ROOT
            );
            dir_entry.child
        };
        Ok(Entries::new(
            EntriesOrder::Nonrecursive,
            &self.dir_entries,
            path,
            start,
        ))
    }

    /// Returns an iterator over all entries under a storage subtree, including
    /// the given path itself.  The iterator walks the storage tree in a
    /// preorder traversal.
    pub fn walk_storage(&self, path: &Path) -> io::Result<Entries> {
        let mut names = internal::path::name_chain_from_path(path)?;
        let stream_id = match self.stream_id_for_name_chain(&names) {
            Some(stream_id) => stream_id,
            None => {
                not_found!(
                    "No such object: {:?}",
                    internal::path::path_from_name_chain(&names)
                );
            }
        };
        names.pop();
        let parent_path = internal::path::path_from_name_chain(&names);
        Ok(Entries::new(
            EntriesOrder::Preorder,
            &self.dir_entries,
            parent_path,
            stream_id,
        ))
    }

    pub fn open_chain(
        &mut self,
        start_sector_id: u32,
        init: SectorInit,
    ) -> Chain<F> {
        Chain::new(&mut self.allocator, start_sector_id, init)
    }

    pub fn root_dir_entry(&self) -> &DirEntry {
        self.dir_entry(consts::ROOT_STREAM_ID)
    }

    pub fn dir_entry(&self, stream_id: u32) -> &DirEntry {
        &self.dir_entries[stream_id as usize]
    }

    fn dir_entry_mut(&mut self, stream_id: u32) -> &mut DirEntry {
        &mut self.dir_entries[stream_id as usize]
    }

    fn validate(&self) -> io::Result<()> {
        // Note: The MS-CFB spec says that root entries MUST be colored black,
        // but apparently some implementations don't actually do this (see
        // https://social.msdn.microsoft.com/Forums/sqlserver/en-US/
        // 9290d877-d91f-4509-ace9-cb4575c48514/red-black-tree-in-mscfb).  So
        // we don't complain if the root is red.
        let root_entry = self.root_dir_entry();
        if root_entry.name != consts::ROOT_DIR_NAME {
            malformed!(
                "root entry name is {:?}, but should be {:?}",
                root_entry.name,
                consts::ROOT_DIR_NAME
            );
        }
        if root_entry.stream_len % consts::MINI_SECTOR_LEN as u64 != 0 {
            malformed!(
                "root stream len is {}, but should be multiple of {}",
                root_entry.stream_len,
                consts::MINI_SECTOR_LEN
            );
        }
        let mut visited = HashSet::new();
        let mut stack = vec![(consts::ROOT_STREAM_ID, false)];
        while let Some((stream_id, parent_is_red)) = stack.pop() {
            if visited.contains(&stream_id) {
                malformed!("loop in tree");
            }
            visited.insert(stream_id);
            let dir_entry = self.dir_entry(stream_id);
            if stream_id == consts::ROOT_STREAM_ID {
                if dir_entry.obj_type != consts::OBJ_TYPE_ROOT {
                    malformed!(
                        "wrong object type for root entry: {}",
                        dir_entry.obj_type
                    );
                }
            } else if dir_entry.obj_type != consts::OBJ_TYPE_STORAGE
                && dir_entry.obj_type != consts::OBJ_TYPE_STREAM
            {
                malformed!(
                    "wrong object type for non-root entry: {}",
                    dir_entry.obj_type
                );
            }
            let node_is_red = dir_entry.color == consts::COLOR_RED;
            if parent_is_red && node_is_red {
                malformed!("two red nodes in a row");
            }
            let left_sibling = dir_entry.left_sibling;
            if left_sibling != consts::NO_STREAM {
                if left_sibling as usize >= self.dir_entries.len() {
                    malformed!(
                        "left sibling index is {}, but directory entry count \
                         is {}",
                        left_sibling,
                        self.dir_entries.len()
                    );
                }
                let entry = &self.dir_entry(left_sibling);
                if internal::path::compare_names(&entry.name, &dir_entry.name)
                    != Ordering::Less
                {
                    malformed!(
                        "name ordering, {:?} vs {:?}",
                        dir_entry.name,
                        entry.name
                    );
                }
                stack.push((left_sibling, node_is_red));
            }
            let right_sibling = dir_entry.right_sibling;
            if right_sibling != consts::NO_STREAM {
                if right_sibling as usize >= self.dir_entries.len() {
                    malformed!(
                        "right sibling index is {}, but directory entry count \
                         is {}",
                        right_sibling, self.dir_entries.len());
                }
                let entry = &self.dir_entry(right_sibling);
                if internal::path::compare_names(&dir_entry.name, &entry.name)
                    != Ordering::Less
                {
                    malformed!(
                        "name ordering, {:?} vs {:?}",
                        dir_entry.name,
                        entry.name
                    );
                }
                stack.push((right_sibling, node_is_red));
            }
            let child = dir_entry.child;
            if child != consts::NO_STREAM {
                if child as usize >= self.dir_entries.len() {
                    malformed!(
                        "child index is {}, but directory entry count is {}",
                        child,
                        self.dir_entries.len()
                    );
                }
                stack.push((child, false));
            }
        }
        Ok(())
    }
}

impl<F: Seek> Directory<F> {
    pub fn seek_within_header(
        &mut self,
        offset_within_header: u64,
    ) -> io::Result<Sector<F>> {
        self.allocator.seek_within_header(offset_within_header)
    }

    fn seek_to_dir_entry(&mut self, stream_id: u32) -> io::Result<Sector<F>> {
        self.seek_within_dir_entry(stream_id, 0)
    }

    fn seek_within_dir_entry(
        &mut self,
        stream_id: u32,
        offset_within_dir_entry: usize,
    ) -> io::Result<Sector<F>> {
        let dir_entries_per_sector =
            self.version().dir_entries_per_sector() as u32;
        let index_within_sector = stream_id % dir_entries_per_sector;
        let mut directory_sector = self.dir_start_sector;
        for _ in 0..(stream_id / dir_entries_per_sector) {
            debug_assert_ne!(directory_sector, consts::END_OF_CHAIN);
            directory_sector = self.allocator.next(directory_sector);
        }
        self.allocator.seek_within_subsector(
            directory_sector,
            index_within_sector,
            consts::DIR_ENTRY_LEN,
            offset_within_dir_entry as u64,
        )
    }
}

impl<F: Write + Seek> Directory<F> {
    /// Allocates a new chain with one sector, and returns the starting sector
    /// number.
    pub fn begin_chain(&mut self, init: SectorInit) -> io::Result<u32> {
        self.allocator.begin_chain(init)
    }

    /// Given the starting sector (or any internal sector) of a chain, extends
    /// the end of that chain by one sector and returns the new sector number,
    /// updating the FAT as necessary.
    pub fn extend_chain(
        &mut self,
        start_sector_id: u32,
        init: SectorInit,
    ) -> io::Result<u32> {
        self.allocator.extend_chain(start_sector_id, init)
    }

    /// Given the start sector of a chain, deallocates the entire chain.
    pub fn free_chain(&mut self, start_sector_id: u32) -> io::Result<()> {
        self.allocator.free_chain(start_sector_id)
    }

    /// Inserts a new directory entry into the tree under the specified parent
    /// entry, then returns the new stream ID.
    pub fn insert_dir_entry(
        &mut self,
        parent_id: u32,
        name: &str,
        obj_type: u8,
    ) -> io::Result<u32> {
        debug_assert_ne!(obj_type, consts::OBJ_TYPE_UNALLOCATED);
        // Create a new directory entry.
        let stream_id = self.allocate_dir_entry()?;
        let now = internal::time::current_timestamp();
        *self.dir_entry_mut(stream_id) = DirEntry {
            name: name.to_string(),
            obj_type,
            color: consts::COLOR_BLACK,
            left_sibling: consts::NO_STREAM,
            right_sibling: consts::NO_STREAM,
            child: consts::NO_STREAM,
            clsid: Uuid::nil(),
            state_bits: 0,
            creation_time: now,
            modified_time: now,
            start_sector: if obj_type == consts::OBJ_TYPE_STREAM {
                consts::END_OF_CHAIN
            } else {
                0
            },
            stream_len: 0,
        };

        // Insert the new entry into the tree.
        let mut sibling_id = self.dir_entry(parent_id).child;
        let mut prev_sibling_id = parent_id;
        let mut ordering = Ordering::Equal;
        while sibling_id != consts::NO_STREAM {
            let sibling = self.dir_entry(sibling_id);
            prev_sibling_id = sibling_id;
            ordering = internal::path::compare_names(name, &sibling.name);
            sibling_id = match ordering {
                Ordering::Less => sibling.left_sibling,
                Ordering::Greater => sibling.right_sibling,
                Ordering::Equal => panic!("internal error: insert duplicate"),
            };
        }
        match ordering {
            Ordering::Less => {
                self.dir_entry_mut(prev_sibling_id).left_sibling = stream_id;
                let mut sector =
                    self.seek_within_dir_entry(prev_sibling_id, 68)?;
                sector.write_u32::<LittleEndian>(stream_id)?;
            }
            Ordering::Greater => {
                self.dir_entry_mut(prev_sibling_id).right_sibling = stream_id;
                let mut sector =
                    self.seek_within_dir_entry(prev_sibling_id, 72)?;
                sector.write_u32::<LittleEndian>(stream_id)?;
            }
            Ordering::Equal => {
                debug_assert_eq!(prev_sibling_id, parent_id);
                self.dir_entry_mut(parent_id).child = stream_id;
                let mut sector = self.seek_within_dir_entry(parent_id, 76)?;
                sector.write_u32::<LittleEndian>(stream_id)?;
            }
        }
        // TODO: rebalance tree

        // Write new entry to underyling file.
        self.write_dir_entry(stream_id)?;
        Ok(stream_id)
    }

    /// Removes a directory entry from the tree and deallocates it.
    pub fn remove_dir_entry(
        &mut self,
        parent_id: u32,
        name: &str,
    ) -> io::Result<()> {
        // Find the directory entry with the given name below the parent.
        let mut stream_ids = Vec::new();
        let mut stream_id = self.dir_entry(parent_id).child;
        loop {
            debug_assert_ne!(stream_id, consts::NO_STREAM);
            debug_assert!(!stream_ids.contains(&stream_id));
            stream_ids.push(stream_id);
            let dir_entry = self.dir_entry(stream_id);
            match internal::path::compare_names(name, &dir_entry.name) {
                Ordering::Equal => break,
                Ordering::Less => stream_id = dir_entry.left_sibling,
                Ordering::Greater => stream_id = dir_entry.right_sibling,
            }
        }
        debug_assert_eq!(self.dir_entry(stream_id).child, consts::NO_STREAM);

        // Restructure the tree.
        let mut replacement_id = consts::NO_STREAM;
        loop {
            let left_sibling = self.dir_entry(stream_id).left_sibling;
            let right_sibling = self.dir_entry(stream_id).right_sibling;
            if left_sibling == consts::NO_STREAM
                && right_sibling == consts::NO_STREAM
            {
                break;
            } else if left_sibling == consts::NO_STREAM {
                replacement_id = right_sibling;
                break;
            } else if right_sibling == consts::NO_STREAM {
                replacement_id = left_sibling;
                break;
            }
            let mut predecessor_id = left_sibling;
            loop {
                stream_ids.push(predecessor_id);
                let next_id = self.dir_entry(predecessor_id).right_sibling;
                if next_id == consts::NO_STREAM {
                    break;
                }
                predecessor_id = next_id;
            }
            let mut pred_entry = self.dir_entry(predecessor_id).clone();
            debug_assert_eq!(pred_entry.right_sibling, consts::NO_STREAM);
            pred_entry.left_sibling = left_sibling;
            pred_entry.right_sibling = right_sibling;
            pred_entry.write_to(&mut self.seek_to_dir_entry(stream_id)?)?;
            *self.dir_entry_mut(stream_id) = pred_entry;
            stream_id = predecessor_id;
        }
        // TODO: recolor nodes

        // Remove the entry.
        debug_assert_eq!(stream_ids.last(), Some(&stream_id));
        stream_ids.pop();
        if let Some(&sibling_id) = stream_ids.last() {
            if self.dir_entry(sibling_id).left_sibling == stream_id {
                self.dir_entry_mut(sibling_id).left_sibling = replacement_id;
                let mut sector = self.seek_within_dir_entry(sibling_id, 68)?;
                sector.write_u32::<LittleEndian>(replacement_id)?;
            } else {
                debug_assert_eq!(
                    self.dir_entry(sibling_id).right_sibling,
                    stream_id
                );
                self.dir_entry_mut(sibling_id).right_sibling = replacement_id;
                let mut sector = self.seek_within_dir_entry(sibling_id, 72)?;
                sector.write_u32::<LittleEndian>(replacement_id)?;
            }
        } else {
            self.dir_entry_mut(parent_id).child = replacement_id;
            let mut sector = self.seek_within_dir_entry(parent_id, 76)?;
            sector.write_u32::<LittleEndian>(replacement_id)?;
        }
        self.free_dir_entry(stream_id)?;
        Ok(())
    }

    /// Adds a new (uninitialized) entry to the directory and returns the new
    /// stream ID.
    fn allocate_dir_entry(&mut self) -> io::Result<u32> {
        // If there's an existing unalloated directory entry, use that.
        for (stream_id, entry) in self.dir_entries.iter().enumerate() {
            if entry.obj_type == consts::OBJ_TYPE_UNALLOCATED {
                return Ok(stream_id as u32);
            }
        }
        // Otherwise, we need a new entry; if there's not room in the directory
        // chain to add it, then first we need to add a new directory sector.
        let dir_entries_per_sector = self.version().dir_entries_per_sector();
        let unallocated_dir_entry = DirEntry::unallocated();
        if self.dir_entries.len() % dir_entries_per_sector == 0 {
            let start_sector = self.dir_start_sector;
            self.allocator.extend_chain(start_sector, SectorInit::Dir)?;
        }
        // Add a new entry to the end of the directory and return it.
        let stream_id = self.dir_entries.len() as u32;
        self.dir_entries.push(unallocated_dir_entry);
        Ok(stream_id)
    }

    /// Deallocates the specified directory entry.
    fn free_dir_entry(&mut self, stream_id: u32) -> io::Result<()> {
        debug_assert_ne!(stream_id, consts::ROOT_STREAM_ID);
        let dir_entry = DirEntry::unallocated();
        dir_entry.write_to(&mut self.seek_to_dir_entry(stream_id)?)?;
        *self.dir_entry_mut(stream_id) = dir_entry;
        // TODO: Truncate directory chain if last directory sector is now all
        //       unallocated.
        Ok(())
    }

    /// Calls the given function with a mutable reference to the specified
    /// directory entry, then writes the updated directory entry to the
    /// underlying file once the function returns.
    pub fn with_dir_entry_mut<W>(
        &mut self,
        stream_id: u32,
        func: W,
    ) -> io::Result<()>
    where
        W: FnOnce(&mut DirEntry),
    {
        func(&mut self.dir_entries[stream_id as usize]);
        self.write_dir_entry(stream_id)
    }

    /// Calls the given function with a mutable reference to the root directory
    /// entry, then writes the updated directory entry to the underlying file
    /// once the function returns.
    pub fn with_root_dir_entry_mut<W>(&mut self, func: W) -> io::Result<()>
    where
        W: FnOnce(&mut DirEntry),
    {
        self.with_dir_entry_mut(consts::ROOT_STREAM_ID, func)
    }

    fn write_dir_entry(&mut self, stream_id: u32) -> io::Result<()> {
        let mut chain = Chain::new(
            &mut self.allocator,
            self.dir_start_sector,
            SectorInit::Dir,
        );
        let offset = (consts::DIR_ENTRY_LEN as u64) * (stream_id as u64);
        chain.seek(SeekFrom::Start(offset))?;
        self.dir_entries[stream_id as usize].write_to(&mut chain)
    }

    /// Flushes all changes to the underlying file.
    pub fn flush(&mut self) -> io::Result<()> {
        self.allocator.flush()
    }
}

//===========================================================================//