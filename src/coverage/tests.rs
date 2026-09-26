use std::collections::BTreeMap;
use std::path::Path;

use super::dwarf::{Function, LineMap, LineRange, PathFilter, normalize};
use super::*;
use crate::arch::GuestCpu;
use crate::arch::xtensa::tests as xt;

#[test]
fn region_marks_and_queries_byte_addresses() {
    let mut r = Region::new(0x4000_0000, 0x4000_0100);
    assert!(!r.mark(0x3fff_ffff));
    assert!(!r.mark(0x4000_0100));
    for a in [0x4000_0000, 0x4000_003f, 0x4000_0040, 0x4000_00ff] {
        assert!(r.mark(a));
    }
    assert!(r.any(0x4000_0000, 0x4000_0001));
    assert!(!r.any(0x4000_0001, 0x4000_003f));
    assert!(r.any(0x4000_0001, 0x4000_0040));
    assert!(r.any(0x4000_0040, 0x4000_0041));
    assert!(!r.any(0x4000_0041, 0x4000_00ff));
    assert!(r.any(0x4000_0041, 0x4000_0100));
    // Clipped to the region; empty ranges never hit.
    assert!(r.any(0x3fff_0000, 0x4000_0001));
    assert!(r.any(0x4000_00f0, 0x5000_0000));
    assert!(!r.any(0x4000_0000, 0x4000_0000));
    assert!(!r.any(0x5000_0000, 0x5000_0010));
}

fn two_images() -> Coverage {
    let img = |ranges: Vec<(u32, u32)>| Image {
        regions: ranges.iter().map(|&(s, e)| Region::new(s, e)).collect(),
        ranges,
        booted: false,
        hooked: Vec::new(),
    };
    Coverage {
        images: vec![img(vec![(0x100, 0x200), (0x1000, 0x1100)]), img(vec![(0x100, 0x180)])],
        active: None,
        live: Vec::new(),
    }
}

#[test]
fn hits_go_to_the_active_image_and_survive_switches() {
    let mut c = two_images();
    c.hit(0x100); // nothing active yet: ignored
    c.activate(0, [0x1000]);
    c.hit(0x104);
    c.hit(0x1010);
    c.hit(0x5000); // outside every region
    c.activate(1, []);
    c.hit(0x108);
    assert!(c.executed(0, 0x104, 0x105) && c.executed(0, 0x1010, 0x1011));
    assert!(!c.executed(0, 0x108, 0x109));
    assert!(c.executed(1, 0x108, 0x109) && !c.executed(1, 0x104, 0x105));
    // Back to image 0 (an OTA rollback, say): its earlier hits are still there.
    c.activate(0, [0x1000]);
    c.hit(0x1a0);
    assert!(c.executed(0, 0x104, 0x105) && c.executed(0, 0x1a0, 0x1a1));
    assert!(c.images[0].booted && c.images[1].booted);
    assert_eq!(c.images[0].hooked, vec![0x1000]);
    c.clear();
    assert!(!c.executed(0, 0x100, 0x200) && !c.executed(1, 0x100, 0x180));
}

fn fc(lines: &[(u32, bool)], funcs: &[(&str, u32, bool)]) -> FileCoverage {
    FileCoverage {
        lines: lines.iter().copied().collect(),
        functions: funcs.iter().map(|&(n, l, h)| (n.to_string(), (l, h))).collect(),
    }
}

#[test]
fn lcov_tracefile_format() {
    let mut files = BTreeMap::new();
    files.insert("src/b.cpp".to_string(), fc(&[(3, false)], &[]));
    files.insert(
        "src/a.cpp".to_string(),
        fc(&[(10, true), (11, false), (20, true)], &[("g(int, char)", 20, true), ("f()", 10, false)]),
    );
    assert_eq!(
        lcov(&files),
        "TN:\n\
         SF:src/a.cpp\n\
         FN:10,f()\nFN:20,g(int, char)\nFNDA:0,f()\nFNDA:1,g(int, char)\nFNF:2\nFNH:1\n\
         DA:10,1\nDA:11,0\nDA:20,1\nLF:3\nLH:2\n\
         end_of_record\n\
         SF:src/b.cpp\nFNF:0\nFNH:0\nDA:3,0\nLF:1\nLH:0\nend_of_record\n"
    );
    let s = summarize(&files);
    assert_eq!((s.files, s.lines_found, s.lines_hit, s.functions_found, s.functions_hit), (2, 4, 2, 2, 1));
}

#[test]
fn images_merge_by_line_and_hooked_functions_are_left_out() {
    let lr = |start, end, file, line| LineRange { start, end, file, line };
    let func = |addr, size, name: &str, line| Function { addr, size, name: name.into(), file: 0, line };
    let map = LineMap {
        files: vec!["src/main.c".into()],
        ranges: vec![
            lr(0x10, 0x14, 0, 1),
            lr(0x14, 0x18, 0, 2),
            lr(0x18, 0x1c, 0, 1),
            lr(0x20, 0x28, 0, 5),
            lr(0x30, 0x34, 0, 7),
        ],
        functions: vec![func(0x10, 0x10, "main", 1), func(0x20, 8, "hooked", 5), func(0x30, 4, "continued", 7)],
    };
    let ran = [0x18u32, 0x30];
    let executed = |s: u32, e: u32| ran.iter().any(|a| (s..e).contains(a));
    let mut files = BTreeMap::new();
    merge_image(&mut files, &map, executed, &[0x20, 0x30]);
    // Line 1 has two ranges, one of which ran; `hooked` never ran and is dropped;
    // `continued` has a hook but its body ran, so it stays.
    assert_eq!(
        files["src/main.c"],
        fc(&[(1, true), (2, false), (7, true)], &[("main", 1, false), ("continued", 7, true)])
    );
    // A second image of the same source: union.
    merge_image(&mut files, &map, |s, e| (s..e).contains(&0x14), &[]);
    assert_eq!(
        files["src/main.c"],
        fc(
            &[(1, true), (2, true), (5, false), (7, true)],
            &[("main", 1, false), ("hooked", 5, false), ("continued", 7, true)]
        )
    );
}

#[test]
fn path_filter_relativizes_and_restricts() {
    assert_eq!(normalize(Path::new("/a/b/../c/./d.c")), Path::new("/a/c/d.c"));
    let f = PathFilter { root: Some("/fw".into()), include: vec!["src/".into(), "lib/".into()] };
    assert_eq!(f.display(Path::new("/fw/src/../src/bl.cpp")).as_deref(), Some("src/bl.cpp"));
    assert_eq!(f.display(Path::new("/fw/lib/trmnl/x.cpp")).as_deref(), Some("lib/trmnl/x.cpp"));
    assert_eq!(f.display(Path::new("/fw/.pio/libdeps/x.cpp")), None);
    assert_eq!(f.display(Path::new("/idf/components/x.c")), None);
    let all = PathFilter { root: Some("/fw".into()), include: vec![] };
    assert_eq!(all.display(Path::new("/idf/x.c")).as_deref(), Some("/idf/x.c"));
}

/// Run a self-checking Xtensa test program with coverage, then map it with its DWARF.
#[test]
fn dwarf_mapping_of_a_real_program() {
    let data = std::fs::read(xt::elf_path("branches")).unwrap();
    let mut cov = Coverage::new(&[&data]).unwrap();
    cov.activate(0, []);
    let mut run = xt::load("branches");
    assert_eq!(
        run.run_with(10_000_000, |cpu, _| {
            cov.hit(cpu.pc());
            false
        }),
        0
    );

    let map = LineMap::from_elf(&data, &cov.images[0].ranges, &PathFilter::default()).unwrap();
    let mut rep = Reporter::new(vec![], PathFilter::default(), "unused".into());
    rep.maps = vec![Some(map)];
    let files = rep.collect(&cov).unwrap();
    let file = |suffix: &str| files.iter().find(|(p, _)| p.ends_with(suffix)).map(|e| e.1).unwrap();

    let main = file("tests/xtensa/src/branches.c");
    assert_eq!(main.lines.get(&16), Some(&true), "first check in main()");
    assert_eq!(main.functions.get("main"), Some(&(15, true)));
    let rt = file("tests/xtensa/rt/rt.c");
    // Only a failing check gets there.
    assert_eq!(rt.lines.get(&17), Some(&false));
    assert_eq!(rt.lines.get(&18), Some(&false));
    assert!(matches!(rt.functions.get("rt_fail_eq"), Some((16 | 17, false))));

    // HLE-replaced functions that never ran are left out.
    let addr = rep.maps[0].as_ref().unwrap().functions.iter().find(|f| f.name == "rt_fail_eq").unwrap().addr;
    cov.activate(0, [addr]);
    let files = rep.collect(&cov).unwrap();
    let rt = files.iter().find(|(p, _)| p.ends_with("rt/rt.c")).unwrap().1;
    assert!(!rt.functions.contains_key("rt_fail_eq") && !rt.lines.contains_key(&17));
    assert!(lcov(&files).contains("FNDA:1,main\n"));
}
