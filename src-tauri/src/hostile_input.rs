//! Every parser of bytes from outside the Companion, fed thousands of
//! corruptions of valid inputs: none may panic. A panic would stop the
//! thread that reads watched files, writes the in-game folders or checks
//! for updates, so a file another addon or OS user wrote, or a reply a
//! broken server sent, could stop the Companion. Each parser must refuse
//! what it can't read with an error instead.
//!
//! The corruptions are pseudo-random from a fixed seed, so a failure names
//! the input that made it and comes back on every run.

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::in_game::check::{self, DataFile};
use crate::in_game::{backend_bodies, content_vector};
use crate::installs::{flavor_info, product_db};
use crate::{pagle_log, update_required, updater};

/// Corruptions tried per seed.
const ROUNDS: usize = 20_000;

/// Bytes a corruption inserts: the delimiters, escapes and keywords each
/// grammar turns on, and bytes outside them.
const TOKENS: &[&[u8]] = &[
    b"{",
    b"}",
    b"[",
    b"]",
    b"[[",
    b"]]",
    b"[=[",
    b"]=]",
    b"--",
    b"--[[",
    b"\"",
    b"'",
    b"\\",
    b"\\\"",
    b"\\9999",
    b"\\x",
    b"=",
    b",",
    b";",
    b"\n",
    b"\r\n",
    b"\t",
    b"\0",
    b"\x7f",
    b"\xff",
    b"\xc3",
    b"\xe2\x80",
    b"nil",
    b"true",
    b"-",
    b"0x",
    b"0",
    b"1e999",
    b"99999999999999999999",
    b".",
    b"|",
    b"=",
    b"==",
    b"{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{",
    b"PagleLog",
    b"\xEF\xBB\xBF",
];

/// xorshift64*: a fixed, dependency-free stream of pseudo-random numbers.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// `seed` with one to four corruptions: a byte changed, a run deleted,
/// a token inserted, a run doubled, or the end cut off.
fn corrupt(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(bytes.len() + 1);
        match rng.below(5) {
            0 if at < bytes.len() => bytes[at] = rng.next() as u8,
            1 if at < bytes.len() => {
                let end = (at + 1 + rng.below(16)).min(bytes.len());
                bytes.drain(at..end);
            }
            2 => {
                let token = TOKENS[rng.below(TOKENS.len())];
                bytes.splice(at..at, token.iter().copied());
            }
            3 if at < bytes.len() => {
                let end = (at + 1 + rng.below(64)).min(bytes.len());
                let run = bytes[at..end].to_vec();
                bytes.splice(at..at, run);
            }
            _ => bytes.truncate(at),
        }
    }
    bytes
}

thread_local! {
    /// Whether a panic on this thread is one [`never_panics_on`] catches.
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Installs, once, a panic hook that prints nothing for the panics
/// [`never_panics_on`] catches and passes every other one to the default
/// hook. Tests run in parallel, so the hook is never swapped back and forth,
/// which could leave another test's failure unprinted.
fn quiet_caught_panics() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !QUIET.with(Cell::get) {
                default(info);
            }
        }));
    });
}

/// Feeds `seeds` and `ROUNDS` corruptions of each to `parse`, failing with
/// the input that made it panic.
fn never_panics(what: &str, seeds: &[Vec<u8>], parse: impl Fn(&[u8])) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let inputs = seeds.iter().flat_map(|seed| {
        let corruptions: Vec<Vec<u8>> = (0..ROUNDS).map(|_| corrupt(&mut rng, seed)).collect();
        std::iter::once(seed.clone()).chain(corruptions)
    });
    never_panics_on(what, inputs, parse);
}

/// Feeds each of `inputs` to `parse`, failing with the input that made it
/// panic.
fn never_panics_on(what: &str, inputs: impl Iterator<Item = Vec<u8>>, parse: impl Fn(&[u8])) {
    quiet_caught_panics();
    QUIET.with(|quiet| quiet.set(true));
    let mut failure = None;
    for input in inputs {
        if panic::catch_unwind(AssertUnwindSafe(|| parse(&input))).is_err() {
            failure = Some(input);
            break;
        }
    }
    QUIET.with(|quiet| quiet.set(false));
    if let Some(input) = failure {
        panic!(
            "{what} panicked on {:?}",
            String::from_utf8_lossy(&input[..input.len().min(2_000)])
        );
    }
}

const PAGLE_LUA: &str = r#"PagleDB = { ["global"] = { ["decodeErrors"] = 0 }, [ [[long]] ] = 'x' }
-- a comment
PagleLog = {
  ["format"] = 1,
  ["logId"] = "3f9a2c1e7b0d4a65",
  ["characters"] = {
    { guid = "Player-4618-0A1B2C3D", name = "Ada", surname = "Brook", classID = 1, raceID = 1,
      faction = "Alliance", realmID = 4618, hardcore = false, selfFound = false,
      firstSeen = 1791731680, lastSeen = 1791735090 },
    { guid = "Player-4618-0A1B2C3E", name = "Bo", classID = 8, raceID = 4,
      faction = "Alliance", realmID = 4618, firstSeen = 1791731700, lastSeen = 1791731800 },
  },
  ["entries"] = {
    "C1|1|1791735050|0|c|494|1439|1|148|453|86|25|263|||6358x1",
    [2] = "S1|2|1791735090|0|87|150|25",
  },
}
"#;

#[test]
fn pagle_lua_never_panics_the_reader() {
    // CMP-R30, CMP-R31: a Pagle.lua that doesn't parse or isn't valid is
    // an outcome, never a crash
    let parsed = pagle_log::parse(PAGLE_LUA.as_bytes());
    assert!(
        matches!(parsed, Ok(pagle_log::Found::Valid(_))),
        "{parsed:?}"
    );
    never_panics("pagle_log::parse", &[PAGLE_LUA.into()], |bytes| {
        let _ = pagle_log::parse(bytes);
    });
}

#[test]
fn in_game_bodies_never_panic_the_check() {
    // CMP-R82: a body that fails the check is refused, never a crash
    let mut seeds: Vec<(DataFile, Vec<u8>)> = backend_bodies::FOLLOWING
        .iter()
        .map(|body| (DataFile::Following, body.as_bytes().to_vec()))
        .collect();
    seeds.extend(
        backend_bodies::anglers()
            .into_iter()
            .take(4)
            .map(|(_, body)| (DataFile::Anglers, body.into_bytes())),
    );
    seeds.push((
        DataFile::Content,
        content_vector::content_file("good").into_bytes(),
    ));
    for (file, seed) in seeds {
        assert_eq!(check::check(file, &seed), Ok(()), "{}", file.name());
        never_panics(file.name(), &[seed], |bytes| {
            let _ = check::check(file, bytes);
        });
    }
}

#[test]
fn content_payloads_never_panic_the_check() {
    // HEV-R51: the decoded payload is checked against Core's grammar
    let good = content_vector::signed("good");
    let file = content_vector::content_file("good");
    let payload = STANDARD.decode(&good.payload).unwrap();
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let bodies = (0..ROUNDS).map(|_| {
        let corrupted = STANDARD.encode(corrupt(&mut rng, &payload));
        file.replace(&good.payload, &corrupted).into_bytes()
    });
    never_panics_on("check_content's payload", bodies, |bytes| {
        let _ = check::check_content(bytes);
    });
}

#[test]
fn battle_net_files_never_panic_the_scan() {
    // CMP-R11, CMP Contracts "product.db" and ".flavor.info"
    let mut db = Vec::new();
    // ProductDb { products: [Product { uid, productCode, client: Client { location, name } }] }
    let client = [
        &[0x0a, 9][..],
        b"C:/WoW/x/",
        &[0x6a, 16],
        b"_classic_beta_xx",
    ]
    .concat();
    let product = [
        &[0x0a, 4][..],
        b"wowc",
        &[0x12, 4],
        b"wowc",
        &[0x1a, client.len() as u8],
        &client,
    ]
    .concat();
    db.extend_from_slice(&[0x0a, product.len() as u8]);
    db.extend_from_slice(&product);
    assert_eq!(product_db::decode(&db).map(|c| c.len()), Ok(1));
    never_panics("product_db::decode", &[db], |bytes| {
        let _ = product_db::decode(bytes);
    });

    let info = b"\xEF\xBB\xBFBranch!STRING:0|Product Flavor!STRING:0\r\n# comment\n\nus|wow_classic_beta\n";
    assert_eq!(
        flavor_info::product_code(info).as_deref(),
        Some("wow_classic_beta")
    );
    never_panics("flavor_info::product_code", &[info.to_vec()], |bytes| {
        let _ = flavor_info::product_code(bytes);
    });
}

#[test]
fn server_documents_never_panic_their_readers() {
    // DST-R51, DST-R63: a bad latest.json or config is a failed check
    let latest = br#"{ "version": "1.4.2", "notes": "x", "pub_date": "2026-11-20T10:00:00Z",
  "platforms": { "windows-x86_64": { "signature": "c2ln", "url": "https://github.com/timemir/pagle-companion/releases/download/v1.4.2/a.exe" } } }"#;
    assert!(updater::release::read_latest(latest, "windows-x86_64").is_some());
    never_panics("read_latest", &[latest.to_vec()], |bytes| {
        let _ = updater::release::read_latest(bytes, "windows-x86_64");
    });

    let config = br#"{ "minCompanionVersion": "1.2.0", "foreverProductUid": "wow_classic_beta" }"#;
    assert!(update_required::config::parse(config).is_some());
    never_panics("config::parse", &[config.to_vec()], |bytes| {
        let _ = update_required::config::parse(bytes);
    });
}
