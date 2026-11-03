use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// The file operations the database uses, so tests can put a filesystem that crashes underneath it.
/// Names are file names in the database's directory.
pub trait Fs: Send + Sync {
    /// Creates an empty file, replacing any file with that name.
    fn create(&self, name: &str) -> io::Result<()>;
    fn append(&self, name: &str, data: &[u8]) -> io::Result<()>;
    /// Makes a file's data durable. Its name isn't durable until [`Fs::sync_dir`].
    fn sync(&self, name: &str) -> io::Result<()>;
    fn read(&self, name: &str) -> io::Result<Vec<u8>>;
    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// Renames a file atomically, replacing any file at `to`.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    fn remove(&self, name: &str) -> io::Result<()>;
    fn list(&self) -> io::Result<Vec<String>>;
    /// Makes creates, renames and removes in the directory durable.
    fn sync_dir(&self) -> io::Result<()>;
}

/// The real filesystem.
pub struct StdFs {
    dir: PathBuf,
}

impl StdFs {
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<StdFs> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(StdFs { dir })
    }
}

// ponytail: opens the file for every call; keep handles open if the syscalls ever show up in a profile.
impl Fs for StdFs {
    fn create(&self, name: &str) -> io::Result<()> {
        File::create(self.dir.join(name)).map(drop)
    }

    fn append(&self, name: &str, data: &[u8]) -> io::Result<()> {
        OpenOptions::new()
            .append(true)
            .open(self.dir.join(name))?
            .write_all(data)
    }

    fn sync(&self, name: &str) -> io::Result<()> {
        // Windows needs write access to flush a file.
        OpenOptions::new()
            .write(true)
            .open(self.dir.join(name))?
            .sync_all()
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        fs::read(self.dir.join(name))
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut file = File::open(self.dir.join(name))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0; len];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.dir.join(from), self.dir.join(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.dir.join(name))
    }

    fn list(&self) -> io::Result<Vec<String>> {
        fs::read_dir(&self.dir)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect()
    }

    fn sync_dir(&self) -> io::Result<()> {
        // NTFS journals directory changes itself, and Windows can't fsync a directory.
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}
