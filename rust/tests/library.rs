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

#[test]
fn clones_as_references() {
    use packdir::container::{ClonePut, relative};
    let d = std::env::temp_dir().join(format!("packdir-lib-clones-{}", std::process::id()));
    let plain = d.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let bytes = vec![7u8; 70000];
    for name in ["x", "y", "z"] {
        fs::write(plain.join(name), &bytes).unwrap();
    }
    let file = |name: &str| plain.join(name).to_string_lossy().into_owned();
    let (leaf, other) = (d.join("leaf.sqlar"), d.join("other.sqlar"));
    let o = OpenOptions::default();
    let c = Container::open(&leaf, &o).unwrap();
    c.apply(Apply::default(), &[Put { member: "x".into(), source: file("x") }], &[]).unwrap();
    let y = ClonePut { clone: "y".into(), file: file("y"), original: "x".into() };
    assert_eq!(c.apply_clones(Apply::default(), &[y]).unwrap(), 70000);
    let k = Container::open(&other, &o).unwrap();
    let original = relative(d.join("other").to_str().unwrap(), d.join("leaf/x").to_str().unwrap());
    assert_eq!(original, "../leaf/x");
    k.apply_clones(Apply::default(), &[ClonePut { clone: "z".into(), file: file("z"), original }]).unwrap();
    assert_eq!(c.members().unwrap().iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["x", "y"]);
    assert_eq!((c.read("y").unwrap(), k.read("z").unwrap()), (bytes.clone(), bytes.clone()));
    assert_eq!((c.verify().unwrap(), k.verify().unwrap()), (Vec::new(), Vec::new()));

    // A renamed original moves its references; a removed one materializes them.
    c.rename("x", "w").unwrap();
    assert_eq!(c.clone_of("y").unwrap().map(|(o, _)| o).as_deref(), Some("w"));
    assert_eq!(k.clone_of("z").unwrap().map(|(o, _)| o).as_deref(), Some("../leaf/w"));
    c.remove(&["w".to_string()]).unwrap();
    assert_eq!((c.clone_of("y").unwrap(), k.clone_of("z").unwrap()), (None, None));
    assert_eq!((c.read("y").unwrap(), k.read("z").unwrap()), (bytes.clone(), bytes));
    assert_eq!((c.verify().unwrap(), k.verify().unwrap()), (Vec::new(), Vec::new()));
    c.close().unwrap();
    k.close().unwrap();
    fs::remove_dir_all(&d).unwrap();
}
