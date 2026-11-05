//! cloud-init's NoCloud seed for a Hyper-V place: a CD image labelled cidata
//! holding files, in the root folder only. Windows has no program to make
//! one, so runner-up writes it: ISO 9660 with Joliet names, since
//! cloud-init's names (user-data, meta-data) are not plain ISO 9660 ones, and
//! Linux reads the Joliet names when both are there.

// Unix builds keep this so its tests run there; the Windows build checks it for dead code.
#![cfg_attr(not(windows), allow(dead_code))]
use std::collections::{BTreeMap, HashMap, HashSet};

const SECTOR: usize = 2048;

pub fn seed_iso(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, String> {
    let mut names = Vec::new();
    for name in files.keys() {
        if name.is_empty() || name.len() > 60 || name.contains(['/', '\\', ';', '*', ':', '?', '"', '<', '>', '|']) {
            return Err(format!("seed file name {} is not allowed", crate::go::quote(name)));
        }
        names.push(name.as_str());
    }

    // Sectors 0-15 are the system area; then the primary and Joliet
    // descriptors, the end of descriptors, the four path tables, the two root
    // folders and the files.
    const PVD_AT: u32 = 16;
    const SVD_AT: u32 = 17;
    const END_AT: u32 = 18;
    const L_PATH_AT: u32 = 19;
    const M_PATH_AT: u32 = 20;
    const L_JOLIET_AT: u32 = 21;
    const M_JOLIET_AT: u32 = 22;
    const ROOT_AT: u32 = 23;
    const JOLIET_ROOT_AT: u32 = 24;
    const FILE_AT: u32 = 25;
    let mut at = HashMap::new();
    let mut next = FILE_AT;
    for name in &names {
        at.insert(*name, next);
        next += files[*name].len().div_ceil(SECTOR) as u32;
    }
    let total = next;

    // Plain ISO 9660 names: upper-case letters, digits and _, at most eight
    // and three, unique among themselves.
    let clean = |s: &str, n: usize| {
        let mut b = String::new();
        for r in s.chars() {
            if b.len() == n {
                break;
            }
            if r.is_ascii_uppercase() || r.is_ascii_digit() {
                b.push(r);
            } else if r != '-' {
                b.push('_');
            }
        }
        b
    };
    let mut plain = HashMap::new();
    let mut used = HashSet::new();
    for (i, name) in names.iter().enumerate() {
        let upper = name.to_uppercase();
        let (base, ext) = upper.split_once('.').unwrap_or((&upper, ""));
        let mut p = format!("{}.{};1", clean(base, 8), clean(ext, 3));
        if used.contains(&p) || p.starts_with('.') {
            p = format!("FILE{i:04}.;1");
        }
        used.insert(p.clone());
        plain.insert(*name, p);
    }

    let mut img = vec![0u8; total as usize * SECTOR];
    let mut put = |lba: u32, b: &[u8]| {
        let start = lba as usize * SECTOR;
        img[start..start + b.len()].copy_from_slice(b);
    };

    let folder = |own: u32, name: &dyn Fn(&str) -> Vec<u8>| {
        let mut b = record(&[0], own, SECTOR as u32, true);
        b.extend(record(&[1], own, SECTOR as u32, true));
        for n in &names {
            b.extend(record(&name(n), at[n], files[*n].len() as u32, false));
        }
        b
    };
    let root = folder(ROOT_AT, &|n| plain[n].as_bytes().to_vec());
    let jroot = folder(JOLIET_ROOT_AT, &|n| ucs2(&format!("{n};1")));
    if root.len() > SECTOR || jroot.len() > SECTOR {
        return Err("too many seed files".into());
    }
    put(ROOT_AT, &root);
    put(JOLIET_ROOT_AT, &jroot);
    for n in &names {
        put(at[n], &files[*n]);
    }

    // One path table entry each: the root folder.
    let path_table = |extent: u32, big: bool| {
        let mut t = vec![0u8; 10];
        t[0] = 1;
        if big {
            t[2..6].copy_from_slice(&extent.to_be_bytes());
            t[6..8].copy_from_slice(&1u16.to_be_bytes());
        } else {
            t[2..6].copy_from_slice(&extent.to_le_bytes());
            t[6..8].copy_from_slice(&1u16.to_le_bytes());
        }
        t
    };
    put(L_PATH_AT, &path_table(ROOT_AT, false));
    put(M_PATH_AT, &path_table(ROOT_AT, true));
    put(L_JOLIET_AT, &path_table(JOLIET_ROOT_AT, false));
    put(M_JOLIET_AT, &path_table(JOLIET_ROOT_AT, true));

    let descriptor = |kind: u8, text: &dyn Fn(&str, usize) -> Vec<u8>, root_rec: &[u8], l_path: u32, m_path: u32| {
        let mut d = vec![0u8; SECTOR];
        d[0] = kind;
        d[1..6].copy_from_slice(b"CD001");
        d[6] = 1;
        d[8..40].copy_from_slice(&text("", 32));
        d[40..72].copy_from_slice(&text("cidata", 32));
        both32(&mut d[80..], total);
        both16(&mut d[120..], 1);
        both16(&mut d[124..], 1);
        both16(&mut d[128..], SECTOR as u16);
        both32(&mut d[132..], 10);
        d[140..144].copy_from_slice(&l_path.to_le_bytes());
        d[148..152].copy_from_slice(&m_path.to_be_bytes());
        d[156..156 + root_rec.len()].copy_from_slice(root_rec);
        for (off, n) in [(190, 128), (318, 128), (446, 128), (574, 128), (702, 37), (739, 37), (776, 37)] {
            let t = text("", n);
            d[off..off + t.len()].copy_from_slice(&t);
        }
        // No dates.
        for off in [813, 830, 847, 864] {
            d[off..off + 16].copy_from_slice(b"0000000000000000");
        }
        d[881] = 1;
        d
    };
    let ascii = |s: &str, n: usize| format!("{s:<n$}").into_bytes();
    let joliet = |s: &str, n: usize| {
        let mut b = ucs2(s);
        while b.len() + 2 <= n {
            b.extend([0, b' ']);
        }
        b.truncate(n - n % 2);
        b
    };
    let pvd = descriptor(1, &ascii, &record(&[0], ROOT_AT, SECTOR as u32, true), L_PATH_AT, M_PATH_AT);
    let mut svd = descriptor(2, &joliet, &record(&[0], JOLIET_ROOT_AT, SECTOR as u32, true), L_JOLIET_AT, M_JOLIET_AT);
    // UCS-2 level 3.
    svd[88..91].copy_from_slice(b"%/E");
    put(PVD_AT, &pvd);
    put(SVD_AT, &svd);
    let mut end = vec![0u8; 7];
    end[0] = 255;
    end[1..6].copy_from_slice(b"CD001");
    end[6] = 1;
    put(END_AT, &end);
    Ok(img)
}

/// One folder entry.
fn record(id: &[u8], extent: u32, size: u32, dir: bool) -> Vec<u8> {
    let mut n = 33 + id.len();
    if n % 2 == 1 {
        n += 1;
    }
    let mut r = vec![0u8; n];
    r[0] = n as u8;
    both32(&mut r[2..], extent);
    both32(&mut r[10..], size);
    // The time is 1 January 2000, 00:00 UTC: an image's bytes depend only on
    // its files.
    r[18..25].copy_from_slice(&[100, 1, 1, 0, 0, 0, 0]);
    if dir {
        r[25] = 2;
    }
    both16(&mut r[28..], 1);
    r[32] = id.len() as u8;
    r[33..33 + id.len()].copy_from_slice(id);
    r
}

fn ucs2(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

/// v little-endian then big-endian, as ISO 9660 keeps its numbers.
fn both16(b: &mut [u8], v: u16) {
    b[..2].copy_from_slice(&v.to_le_bytes());
    b[2..4].copy_from_slice(&v.to_be_bytes());
}

fn both32(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
    b[4..8].copy_from_slice(&v.to_be_bytes());
}
