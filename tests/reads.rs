use lsm_lite::sim::SimFs;
use lsm_lite::{Db, Fs, Options};

const N: u64 = 2000;

/// Keys in a scrambled order, so every table covers nearly the whole key range.
fn key(i: u64) -> Vec<u8> {
    format!("key{:05}", i * 7919 % N).into_bytes()
}

#[test]
fn lookups_skip_tables_that_cannot_hold_the_key() {
    let fs = SimFs::new(0);
    let opts = Options {
        memtable_bytes: 4096,
        compact_at: 1000,
    };
    let mut db = Db::open(fs.clone(), opts).unwrap();
    for i in 0..N {
        db.put(&key(i), b"v").unwrap();
    }
    let tables = fs
        .list()
        .unwrap()
        .iter()
        .filter(|n| n.ends_with(".sst"))
        .count() as u64;
    assert!(tables >= 10, "only {tables} tables");

    let before = fs.ops();
    for i in 0..N {
        let mut missing = key(i);
        missing.push(b'x');
        assert_eq!(db.get(&missing).unwrap(), None);
    }
    let missing_reads = fs.ops() - before;

    let before = fs.ops();
    for i in 0..N {
        assert_eq!(db.get(&key(i)).unwrap(), Some(b"v".to_vec()));
    }
    let present_reads = fs.ops() - before;

    eprintln!(
        "{tables} tables: {missing_reads} reads for {N} missing keys, {present_reads} for {N} present ones"
    );
    // Without filters, a missing key costs a read in nearly every table.
    assert!(
        missing_reads < N * tables / 20,
        "{missing_reads} reads for {N} missing keys in {tables} tables"
    );
    assert!(
        present_reads < N * 3 / 2,
        "{present_reads} reads for {N} present keys"
    );
}
