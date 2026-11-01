//! packdir as a library, the way a Rust project uses it.
use packdir::container::{Apply, Container, OpenOptions, Put, plan, repack};
use packdir::tursolite::{Engine, Journal};
use std::fs;

fn engines() -> Vec<(Engine, Journal)> {
    vec![(Engine::Sqlite, Journal::Delete), (Engine::Turso, Journal::Delete), (Engine::Turso, Journal::Mvcc)]
}

#[test]
fn a_directory_in_a_container() {
    for (engine, journal) in engines() {
        let d = std::env::temp_dir().join(format!("packdir-lib-{}-{engine:?}-{journal:?}", std::process::id()));
        let plain = d.join("plain");
        fs::create_dir_all(&plain).unwrap();
        for i in 0..6 {
            fs::write(plain.join(format!("f{i}")), vec![i as u8; 90000]).unwrap();
        }
        let path = d.join("leaf.sqlar");
        let o = OpenOptions { journal, engine: Some(engine), ..OpenOptions::default() };
        let c = Container::open(&path, &o).unwrap();
        let (puts, retain) = plan(&[], &c.members().unwrap(), plain.to_str().unwrap()).unwrap();
        assert_eq!((puts.len(), retain.len()), (6, 0));
        let a = Apply { writers: 3, ..Apply::default() };
        assert_eq!(c.apply(a, &puts, &retain).unwrap(), 540000);
        assert_eq!(c.read("f2").unwrap(), vec![2u8; 90000]);
        assert!(c.epoch().unwrap() >= 1);

        c.rename("f0", "renamed").unwrap();
        c.remove(&["f1".to_string()]).unwrap();
        assert!(c.rename("f2", "a/b").is_err());
        let extra = Put { member: "f0".into(), source: plain.join("f3").to_string_lossy().into_owned() };
        c.apply(Apply::default(), &[extra], &[]).unwrap();
        assert_eq!(c.verify().unwrap(), Vec::<String>::new());
        let names: Vec<_> = c.members().unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["f0", "f2", "f3", "f4", "f5", "renamed"]);
        c.close().unwrap();

        let (kept, _, _) = repack(path.to_str().unwrap(), false, Journal::Delete, Some(engine)).unwrap();
        assert_eq!(kept, 6);
        let reopened = Container::open(&path, &OpenOptions { create: false, read_only: true, engine: Some(Engine::Sqlite), ..OpenOptions::default() }).unwrap();
        let out = d.join("out");
        reopened.restore(false, out.to_str().unwrap()).unwrap();
        assert_eq!(fs::read(out.join("renamed")).unwrap(), vec![0u8; 90000]);
        reopened.close().unwrap();
        fs::remove_dir_all(&d).unwrap();
    }
}
