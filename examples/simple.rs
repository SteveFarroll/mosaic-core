use mosaic_core::*;
use rand::rngs::OsRng;

fn main() {
    // Create a local updatable random number source
    let mut csprng = OsRng;

    // Create a new identity. This is your secret key
    let secret_key = SecretKey::generate(&mut csprng);

    // Create a new record
    let record = Record::new(
        &secret_key,
        &RecordParts {
            kind: Kind::MICROBLOG_ROOT,
            deterministic_key: None,
            timestamp: Timestamp::now().unwrap(),
            flags: RecordFlags::default(),
            app_flags: 0,
            tags_bytes: b"",
            payload: b"Hello World!",
        },
    )
    .unwrap();

    println!("{}", record);
}
