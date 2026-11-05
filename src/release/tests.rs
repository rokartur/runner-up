use super::*;

fn read(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/testdata/release/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// A release-shaped set made with openssl, as the release workflow signs
/// (openssl pkeyutl -sign -rawin), with a throwaway key whose public half is
/// test-key.pub: checksums.txt as sha256sum prints it, its signature, and the
/// two archives it lists.
fn signed_fixture() -> (Signed, Vec<PublicKey>) {
    use base64::Engine;
    let pub_b64 = String::from_utf8(read("test-key.pub")).unwrap();
    let pub_bytes = base64::engine::general_purpose::STANDARD.decode(pub_b64.trim()).unwrap();
    let key = PublicKey::from_bytes(&pub_bytes.try_into().unwrap()).unwrap();
    let name = "runner-up-v9.0.2-windows-amd64.zip";
    let s = Signed {
        name: name.into(),
        archive: read(name),
        checksums: read("checksums.txt"),
        signature: read("checksums.txt.sig"),
    };
    (s, vec![key])
}

fn clone(s: &Signed) -> Signed {
    Signed {
        name: s.name.clone(),
        archive: s.archive.clone(),
        checksums: s.checksums.clone(),
        signature: s.signature.clone(),
    }
}

// Intent (README, "Updating runner-up"): the Windows service accepts a release
// whose checksums the release signing key signed with openssl, as the release
// workflow does, and only for its own platform; it learns the version from the
// archive.
#[test]
fn a_signed_release_is_accepted_for_its_own_platform() {
    let (s, keys) = signed_fixture();
    assert_eq!(s.verify("windows-amd64", &keys), Ok("v9.0.2".to_string()));
    assert!(s.verify("windows-arm64", &keys).is_err(), "an amd64 archive was accepted for arm64");
}

// Intent (README, "Updating runner-up": only the key's holder controls the PC):
// anything the key did not vouch for is refused: another key, changed
// checksums, a changed archive, an archive the checksums do not list, and every
// release when the program was built with no key at all.
#[test]
fn anything_the_signing_key_did_not_vouch_for_is_refused() {
    let (s, keys) = signed_fixture();
    let other = ed25519_dalek::SigningKey::from_bytes(&[7; 32]).verifying_key();
    let mut cases: Vec<(&str, Signed, Vec<PublicKey>)> = vec![
        ("another key", clone(&s), vec![other]),
        ("no key", clone(&s), vec![]),
    ];
    let mut c = clone(&s);
    let text = String::from_utf8(c.checksums.clone()).unwrap().replacen("1a2d", "1a2e", 1);
    c.checksums = text.into_bytes();
    cases.push(("changed checksums", c, keys.clone()));
    let mut c = clone(&s);
    c.archive.push(0);
    cases.push(("changed archive", c, keys.clone()));
    let mut c = clone(&s);
    c.name = "runner-up-v9.0.3-windows-amd64.zip".into();
    cases.push(("unlisted archive", c, keys.clone()));
    let mut c = clone(&s);
    c.name = "checksums.txt".into();
    cases.push(("not an archive name", c, keys.clone()));
    for (name, c, k) in cases {
        if let Ok(v) = c.verify("windows-amd64", &k) {
            panic!("{name}: accepted as {v}");
        }
    }
    assert_eq!(s.verify("windows-amd64", &[]), Err(ERR_NO_SIGNING_KEY.to_string()));
}

// Intent (README, "Updating runner-up"): the program comes out of each
// platform's archive as the release workflow packs it: runner-up in a tar.gz,
// runner-up.exe in a zip.
#[test]
fn the_program_comes_out_of_each_platforms_archive() {
    let (s, _) = signed_fixture();
    let exe = program(&s.archive, "windows-amd64").unwrap();
    assert!(exe.starts_with(b"MZ"));
    let tgz = read("runner-up-v9.0.2-linux-amd64.tar.gz");
    let sums = parse_checksums(&s.checksums).unwrap();
    assert_eq!(sums["runner-up-v9.0.2-linux-amd64.tar.gz"], sha256(&tgz));
    let prog = program(&tgz, "linux-amd64").unwrap();
    assert!(String::from_utf8_lossy(&prog).contains("v9.0.2"));
    assert!(program(&tgz, "windows-amd64").is_err(), "a tar.gz was read as a Windows archive");
}

// Intent (README, "Updating runner-up": the Windows service never installs an
// older release, so an old signed release with a known flaw cannot be pushed to
// it): versions order as semantic versions, a pre-release before its release.
#[test]
fn release_versions_order_as_semantic_versions() {
    let ordered = ["v0.1.9", "v0.1.10", "v0.2.0-alpha.2", "v0.2.0-alpha.10", "v0.2.0-beta", "v0.2.0", "v1.0.0"];
    for i in 0..ordered.len() {
        for j in 0..ordered.len() {
            assert_eq!(compare(ordered[i], ordered[j]), i.cmp(&j), "{} vs {}", ordered[i], ordered[j]);
        }
    }
    assert!(compare("dev", "v0.0.1").is_lt(), "a build with no release version was not older than every release");
}
