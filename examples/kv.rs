//! A command line for trying the store out.
//!
//! ```text
//! cargo run --example kv -- DIR put KEY VALUE
//! cargo run --example kv -- DIR get KEY
//! cargo run --example kv -- DIR delete KEY
//! cargo run --example kv -- DIR scan [FROM [TO]]
//! cargo run --example kv -- DIR fill N    writes key0..keyN, printing each number once it's durable
//! ```
//!
//! `MEMTABLE_BYTES` sets how big the memtable gets before it's flushed.

use std::io::{self, Write};
use std::ops::Bound;

use lsm_lite::{Db, Options};

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, command, rest @ ..] = &args[..] else {
        return usage();
    };
    let mut opts = Options::default();
    if let Some(bytes) = std::env::var("MEMTABLE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        opts.memtable_bytes = bytes;
    }
    let mut db = Db::open_dir(dir, opts)?;
    match (command.as_str(), rest) {
        ("put", [key, value]) => db.put(key.as_bytes(), value.as_bytes())?,
        ("delete", [key]) => db.delete(key.as_bytes())?,
        ("get", [key]) => match db.get(key.as_bytes())? {
            Some(value) => println!("{}", String::from_utf8_lossy(&value)),
            None => {
                eprintln!("not found");
                std::process::exit(1);
            }
        },
        ("scan", bounds) if bounds.len() <= 2 => {
            let from = bounds
                .first()
                .map_or(Bound::Unbounded, |k| Bound::Included(k.as_bytes().to_vec()));
            let to = bounds
                .get(1)
                .map_or(Bound::Unbounded, |k| Bound::Excluded(k.as_bytes().to_vec()));
            for (key, value) in db.scan((from, to))? {
                println!(
                    "{}\t{}",
                    String::from_utf8_lossy(&key),
                    String::from_utf8_lossy(&value)
                );
            }
        }
        ("fill", [n]) => {
            let n: u64 = n.parse().map_err(io::Error::other)?;
            let mut out = io::stdout().lock();
            for i in 0..n {
                db.put(
                    format!("key{i:08}").as_bytes(),
                    format!("value{i}").as_bytes(),
                )?;
                writeln!(out, "{i}")?;
                out.flush()?;
            }
        }
        _ => return usage(),
    }
    Ok(())
}

fn usage() -> io::Result<()> {
    eprintln!("usage: kv DIR put KEY VALUE | get KEY | delete KEY | scan [FROM [TO]] | fill N");
    std::process::exit(2);
}
