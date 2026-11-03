//! Sorted tables on disk. A table is written once and never changed.
//!
//! ```text
//! entries  [key_len u32][kind u8][value_len u32][key][value]   sorted by key; kind 0 is a deletion
//! index    [key_len u32][key][offset u64]                      first key of every block of entries
//! footer   [index_offset u64][crc u32 over everything before the footer]
//! ```

use std::io;

use crate::{Fs, Reader, corrupt, table_name};

const BLOCK: usize = 16;
const FOOTER: usize = 12;

type Entry = (Vec<u8>, Option<Vec<u8>>);

pub struct Table {
    pub id: u64,
    index: Vec<(Vec<u8>, u64)>,
    index_offset: u64,
}

impl Table {
    /// Writes and syncs a table. Its name isn't durable until the caller syncs the directory.
    pub fn write<'a>(
        fs: &dyn Fs,
        id: u64,
        entries: impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)>,
    ) -> io::Result<Table> {
        let mut buf = Vec::new();
        let mut index = Vec::new();
        for (i, (key, value)) in entries.enumerate() {
            if i % BLOCK == 0 {
                index.push((key.to_vec(), buf.len() as u64));
            }
            let value_bytes = value.unwrap_or_default();
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.push(value.is_some() as u8);
            buf.extend_from_slice(&(value_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(key);
            buf.extend_from_slice(value_bytes);
        }
        let index_offset = buf.len() as u64;
        for (key, offset) in &index {
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key);
            buf.extend_from_slice(&offset.to_le_bytes());
        }
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&index_offset.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());

        let name = table_name(id);
        fs.create(&name)?;
        fs.append(&name, &buf)?;
        fs.sync(&name)?;
        Ok(Table {
            id,
            index,
            index_offset,
        })
    }

    /// Opens a table and checks its CRC.
    // ponytail: reads the whole file once to check it; per-block CRCs when tables get large.
    pub fn open(fs: &dyn Fs, id: u64) -> io::Result<Table> {
        let name = table_name(id);
        let buf = fs.read(&name)?;
        let body_len = buf
            .len()
            .checked_sub(FOOTER)
            .ok_or_else(|| corrupt(&name))?;
        let (body, footer) = buf.split_at(body_len);
        let mut footer = Reader::new(footer);
        let index_offset = footer.u64()?;
        if crc32fast::hash(body) != footer.u32()? {
            return Err(corrupt(&name));
        }
        let mut r = Reader::new(
            body.get(index_offset as usize..)
                .ok_or_else(|| corrupt(&name))?,
        );
        let mut index = Vec::new();
        while !r.done() {
            let len = r.u32()? as usize;
            let key = r.bytes(len)?.to_vec();
            index.push((key, r.u64()?));
        }
        Ok(Table {
            id,
            index,
            index_offset,
        })
    }

    /// Looks up a key, reading only the block that could hold it. `Some(None)` means it was deleted.
    pub fn get(&self, fs: &dyn Fs, key: &[u8]) -> io::Result<Option<Option<Vec<u8>>>> {
        let block = self
            .index
            .partition_point(|(first, _)| first.as_slice() <= key);
        if block == 0 {
            return Ok(None);
        }
        let start = self.index[block - 1].1;
        let end = self
            .index
            .get(block)
            .map_or(self.index_offset, |(_, offset)| *offset);
        let buf = fs.read_at(&table_name(self.id), start, (end - start) as usize)?;
        Ok(parse(&buf)?
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value))
    }

    /// Reads every entry, in key order.
    pub fn entries(&self, fs: &dyn Fs) -> io::Result<Vec<Entry>> {
        parse(&fs.read_at(&table_name(self.id), 0, self.index_offset as usize)?)
    }
}

fn parse(buf: &[u8]) -> io::Result<Vec<Entry>> {
    let mut r = Reader::new(buf);
    let mut out = Vec::new();
    while !r.done() {
        let key_len = r.u32()? as usize;
        let kind = r.u8()?;
        let value_len = r.u32()? as usize;
        let key = r.bytes(key_len)?.to_vec();
        let value = r.bytes(value_len)?.to_vec();
        out.push((key, (kind == 1).then_some(value)));
    }
    Ok(out)
}
