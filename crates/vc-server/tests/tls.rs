use vc_server::tls;

#[test]
fn cert_generated_once_then_reused() {
    let dir = tempfile::tempdir().unwrap();
    let (c1, k1) = tls::load_or_create_cert(dir.path()).unwrap();
    assert!(dir.path().join("cert.der").exists());
    assert!(dir.path().join("key.der").exists());
    let mtime = std::fs::metadata(dir.path().join("cert.der"))
        .unwrap()
        .modified()
        .unwrap();
    let (c2, k2) = tls::load_or_create_cert(dir.path()).unwrap();
    assert_eq!(c1.as_ref(), c2.as_ref());
    assert_eq!(k1.secret_der(), k2.secret_der());
    assert_eq!(
        std::fs::metadata(dir.path().join("cert.der"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    // A different dir gets a different cert.
    let dir2 = tempfile::tempdir().unwrap();
    let (c3, _) = tls::load_or_create_cert(dir2.path()).unwrap();
    assert_ne!(c1.as_ref(), c3.as_ref());
    // The loaded pair builds a valid TLS config.
    tls::rustls_server_config(c2, k2).unwrap();
}

#[test]
fn fingerprint_stable_for_same_cert() {
    let dir = tempfile::tempdir().unwrap();
    let (c1, _) = tls::load_or_create_cert(dir.path()).unwrap();
    let (c2, _) = tls::load_or_create_cert(dir.path()).unwrap();
    let f1 = tls::fingerprint(c1.as_ref());
    assert_eq!(f1, tls::fingerprint(c2.as_ref()));
    assert_ne!(f1, tls::fingerprint(b"something else"));
    let hex = tls::fingerprint_hex(&f1);
    assert_eq!(hex.len(), 64);
    assert!(
        hex.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    // Known SHA-256 vector.
    assert_eq!(
        tls::fingerprint_hex(&tls::fingerprint(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
