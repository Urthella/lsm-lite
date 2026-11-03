//! Crashes the database on [`SimFs`] and checks what it recovers. Every acknowledged write must
//! survive; the one in progress when the process died may or may not.

use std::collections::BTreeMap;
use std::io;

use lsm_lite::sim::SimFs;
use lsm_lite::{Db, Options};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

const KEYS: u64 = 24;

/// Small enough that a few writes flush the memtable and a few flushes compact.
fn small() -> Options {
    Options {
        memtable_bytes: 200,
        compact_at: 3,
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        (z ^ (z >> 31)) % n
    }
}

#[derive(Debug)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

fn key(i: u64) -> Vec<u8> {
    format!("k{i:02}").into_bytes()
}

fn random_op(rng: &mut Rng) -> Op {
    let key = key(rng.below(KEYS));
    if rng.below(4) == 0 {
        return Op::Delete(key);
    }
    let len = rng.below(40);
    Op::Put(key, (0..len).map(|_| rng.below(256) as u8).collect())
}

fn apply(db: &mut Db, op: &Op) -> io::Result<()> {
    match op {
        Op::Put(k, v) => db.put(k, v),
        Op::Delete(k) => db.delete(k),
    }
}

fn apply_model(model: &mut Model, op: &Op) {
    match op {
        Op::Put(k, v) => model.insert(k.clone(), v.clone()),
        Op::Delete(k) => model.remove(k),
    };
}

fn simulated(e: &io::Error) -> bool {
    e.to_string() == "simulated crash"
}

/// Checks the database holds `model`, or `model` plus the write that was in progress. Returns
/// whichever it holds.
fn check(db: &Db, model: &Model, in_flight: Option<&Op>, context: &str) -> Model {
    let found: Model = db.scan(..).unwrap().into_iter().collect();
    let mut with = model.clone();
    if let Some(op) = in_flight {
        apply_model(&mut with, op);
    }
    assert!(
        found == *model || found == with,
        "{context}: recovered the wrong state\nfound: {found:?}\nwant:  {model:?}\nor with {in_flight:?}"
    );
    for i in 0..KEYS {
        assert_eq!(
            db.get(&key(i)).unwrap(),
            found.get(&key(i)).cloned(),
            "{context}: get and scan disagree"
        );
    }
    found
}

/// Opens after a crash, which can crash too.
fn recover(fs: &SimFs, rng: &mut Rng, context: &str) -> Db {
    loop {
        if rng.below(3) == 0 {
            fs.fail_after(Some(rng.below(30)));
        }
        match Db::open(fs.clone(), small()) {
            Ok(db) => {
                fs.fail_after(None);
                return db;
            }
            Err(e) if simulated(&e) => fs.crash(),
            Err(e) => panic!("{context}: recovery failed: {e}"),
        }
    }
}

#[test]
fn random_crashes_lose_nothing_acknowledged() {
    for seed in 0..300 {
        let fs = SimFs::new(seed);
        let mut rng = Rng(seed);
        let mut model = Model::new();
        let mut db = Db::open(fs.clone(), small()).unwrap();
        for step in 0..300 {
            let context = format!("seed {seed}, step {step}");
            if rng.below(15) == 0 {
                fs.fail_after(Some(rng.below(40)));
            }
            if rng.below(40) == 0 {
                // A clean restart, with no crash pending.
                fs.fail_after(None);
                drop(db);
                db = Db::open(fs.clone(), small())
                    .unwrap_or_else(|e| panic!("{context}: reopen failed: {e}"));
                model = check(&db, &model, None, &context);
            }
            let op = random_op(&mut rng);
            match apply(&mut db, &op) {
                Ok(()) => apply_model(&mut model, &op),
                Err(e) if simulated(&e) => {
                    drop(db);
                    fs.crash();
                    db = recover(&fs, &mut rng, &context);
                    model = check(&db, &model, Some(&op), &context);
                }
                Err(e) => panic!("{context}: {e}"),
            }
        }
    }
}

#[test]
fn a_crash_at_every_operation_loses_nothing_acknowledged() {
    let mut rng = Rng(7);
    let ops: Vec<Op> = (0..100).map(|_| random_op(&mut rng)).collect();
    let total = {
        let fs = SimFs::new(0);
        let mut db = Db::open(fs.clone(), small()).unwrap();
        ops.iter().for_each(|op| apply(&mut db, op).unwrap());
        fs.ops()
    };
    assert!(
        total > 400,
        "the workload should flush and compact many times, ran {total} operations"
    );

    for point in 0..total {
        for seed in 0..8 {
            let context = format!("crash at operation {point}, seed {seed}");
            let fs = SimFs::new(seed);
            fs.fail_after(Some(point));
            let mut model = Model::new();
            let mut in_flight = None;
            let opened = Db::open(fs.clone(), small());
            if let Err(e) = &opened {
                assert!(simulated(e), "{context}: {e}");
            }
            if let Ok(mut db) = opened {
                for op in &ops {
                    match apply(&mut db, op) {
                        Ok(()) => apply_model(&mut model, op),
                        Err(e) if simulated(&e) => {
                            in_flight = Some(op);
                            break;
                        }
                        Err(e) => panic!("{context}: {e}"),
                    }
                }
            }
            fs.crash();
            let mut db = Db::open(fs.clone(), small())
                .unwrap_or_else(|e| panic!("{context}: recovery failed: {e}"));
            let mut model = check(&db, &model, in_flight, &context);

            // It keeps working after recovering.
            for op in &ops[..10] {
                apply(&mut db, op).unwrap();
                apply_model(&mut model, op);
            }
            check(&db, &model, None, &context);
        }
    }
}

#[test]
fn a_failed_write_stops_the_database_until_it_reopens() {
    let fs = SimFs::new(1);
    let mut db = Db::open(fs.clone(), small()).unwrap();
    db.put(b"a", b"1").unwrap();
    // The log append works, then its sync fails.
    fs.fail_after(Some(1));
    assert!(simulated(&db.put(b"b", b"2").unwrap_err()));
    fs.fail_after(None);
    let err = db.put(b"c", b"3").unwrap_err();
    assert!(!simulated(&err), "{err}");
    assert!(db.get(b"a").is_err());
    drop(db);
    let db = Db::open(fs.clone(), small()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"c").unwrap(), None);
}

#[test]
fn works_on_a_real_directory() {
    let dir = std::env::temp_dir().join(format!("lsm-lite-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rng = Rng(1);
    let mut model = Model::new();
    let mut db = Db::open_dir(&dir, small()).unwrap();
    for i in 0..400 {
        if i % 100 == 99 {
            drop(db);
            db = Db::open_dir(&dir, small()).unwrap();
        }
        let op = random_op(&mut rng);
        apply(&mut db, &op).unwrap();
        apply_model(&mut model, &op);
    }
    drop(db);
    let db = Db::open_dir(&dir, small()).unwrap();
    check(&db, &model, None, "real directory");
    assert_eq!(
        db.scan(key(5)..key(9)).unwrap(),
        model
            .range(key(5)..key(9))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>()
    );

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert!(files.contains(&"MANIFEST".to_string()), "{files:?}");
    assert_eq!(
        files.iter().filter(|f| f.ends_with(".log")).count(),
        1,
        "{files:?}"
    );
    assert!(
        files.iter().filter(|f| f.ends_with(".sst")).count() < small().compact_at,
        "{files:?}"
    );
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
