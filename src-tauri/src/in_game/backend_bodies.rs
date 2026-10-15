//! Bodies the backend sends for the two in-game downloads, copied from its
//! own tests (`timemir/pagle-backend`, `src/test/java/app/pagle/ingame/`), so
//! the Companion's check (CMP-R82) and downloads are tested against what the
//! server writes, not only against the grammar as read here.
//!
//! - `FollowingFileTest`: the Contracts example, a file with no Anglers, a
//!   card with no title, Lua escapes and no GUIDs, and a lifetime count past
//!   2³¹.
//! - `AnglersFileTest`: the Contracts example, two untrimmed blocks, and the
//!   trimmed files of IGV-R33, down to blocks that trimming emptied
//!   (`cardCount = 0`, which the backend keeps by design).
//! - `RegionBlockWriterTest`: blocks as the snapshot build writes them (a
//!   card with no key, titles in byte order, escaped titles, capped fields),
//!   wrapped in a file the way `AnglersFile` wraps stored blocks. Where the
//!   test asserts only part of a block, the rest is filled in by the same
//!   writer's rules and says so.

/// `FollowingFileTest.EXAMPLE`: IGV Contracts' example, byte for byte.
pub const FOLLOWING_EXAMPLE: &str = "PagleFollowingData = {\n\
v = 1,\n\
generatedAt = 1791735100,\n\
count = 1,\n\
anglers = {\n\
{ name = \"Ada\", more = 1, hardcore = false, title = \"Warden of Feralas Waters\", level = 23, \
prestige = 1, ledger = 37, legends = 3, legendsTotal = 12, waters = 4, catches = 5120, asOf = 20261012, \
guids = { \"Player-4618-0A1B2C3D\", \"Player-4618-0A1B2C3E\" } },\n\
},\n\
ok = true,\n\
}\n";

/// `FollowingFileTest`: the follower of no one (IGV-R34).
pub const FOLLOWING_NONE: &str = "PagleFollowingData = {\n\
v = 1,\n\
generatedAt = 1791735100,\n\
count = 0,\n\
anglers = {\n\
},\n\
ok = true,\n\
}\n";

/// `FollowingFileTest`: Ada and Bo (no title, Hardcore, one GUID), in a file
/// of `FollowingFile.write(1, …)`.
pub const FOLLOWING_TWO: &str = "PagleFollowingData = {\n\
v = 1,\n\
generatedAt = 1,\n\
count = 2,\n\
anglers = {\n\
{ name = \"Ada\", more = 1, hardcore = false, title = \"Warden of Feralas Waters\", level = 23, \
prestige = 1, ledger = 37, legends = 3, legendsTotal = 12, waters = 4, catches = 5120, asOf = 20261012, \
guids = { \"Player-4618-0A1B2C3D\", \"Player-4618-0A1B2C3E\" } },\n\
{ name = \"Bo\", more = 0, hardcore = true, title = \"\", level = 1, prestige = 0, ledger = 0, \
legends = 0, legendsTotal = 0, waters = 0, catches = 0, asOf = 20270105, \
guids = { \"Player-1-1\" } },\n\
},\n\
ok = true,\n\
}\n";

/// `FollowingFileTest`: a name and title through the Lua string writer
/// (`\"`, `\\`, `\010`, `\127`, UTF-8), no GUIDs, and a lifetime catch count
/// past 2³¹.
pub const FOLLOWING_ESCAPES: &str = "PagleFollowingData = {\n\
v = 1,\n\
generatedAt = 1,\n\
count = 2,\n\
anglers = {\n\
{ name = \"Æ\\\"g\\\\ir\\010\", more = 0, hardcore = false, title = \"Ti\\127tle\", level = 1, \
prestige = 0, ledger = 0, legends = 0, legendsTotal = 0, waters = 0, catches = 0, asOf = 20261012, \
guids = {  } },\n\
{ name = \"Ada\", more = 0, hardcore = false, title = \"\", level = 1, prestige = 0, ledger = 0, \
legends = 0, legendsTotal = 0, waters = 0, catches = 4000000000, asOf = 20261012, guids = {  } },\n\
},\n\
ok = true,\n\
}\n";

pub const FOLLOWING: [&str; 4] = [
    FOLLOWING_EXAMPLE,
    FOLLOWING_NONE,
    FOLLOWING_TWO,
    FOLLOWING_ESCAPES,
];

/// `AnglersFileTest.EXAMPLE_FILE`: IGV Contracts' example, byte for byte.
pub const ANGLERS_EXAMPLE: &str = "PagleAnglersData = {\n\
v = 1,\n\
generatedAt = 1791734400,\n\
count = 1,\n\
blocks = {\n\
{ region = \"eu\", builtAt = 1791734400, cardCount = 2, keyCount = 3, \
titles = { \"Seeker of the Deep\", \"Warden of Feralas Waters\" }, \
cards = \"00223001037031200016001210007121\", \
keys = \"0439501234567000002046180A1B2C3D000001046180A1B2C3E000001\" },\n\
},\n\
ok = true,\n\
}\n";

/// `AnglersFileTest.US`.
const US: &str = "{ region = \"us\", builtAt = 1791734400, cardCount = 3, keyCount = 4, \
titles = { \"An\\\"gler\", \"Seeker\" }, \
cards = \"001600121000712100223001037031200003000301000120\", \
keys = \"012340000000A000001046180A1B2C3D000002046180A1B2C3E0000020999900000001000003\" },\n";

/// `AnglersFileTest.EU`.
const EU: &str = "{ region = \"eu\", builtAt = 1791730000, cardCount = 2, keyCount = 2, \
titles = { \"Wärden\" }, \
cards = \"00150020090051200001000000500120\", \
keys = \"01403000000020000010461800000003000002\" },\n";

/// The file `AnglersFile` writes around `blocks` (`AnglersFileTest.file`).
pub fn anglers_file(generated_at: u64, blocks: &[&str]) -> String {
    format!(
        "PagleAnglersData = {{\nv = 1,\ngeneratedAt = {generated_at},\ncount = {},\nblocks = {{\n{}}},\nok = true,\n}}\n",
        blocks.len(),
        blocks.concat()
    )
}

/// Every `Pagle_Anglers` body of the backend's tests.
pub fn anglers() -> Vec<(&'static str, String)> {
    vec![
        ("the Contracts example", ANGLERS_EXAMPLE.to_owned()),
        // AnglersFileTest: untrimmed blocks, the older builtAt.
        (
            "us and eu untrimmed",
            anglers_file(1_791_730_000, &[US, EU]),
        ),
        // AnglersFileTest: no stored block (IGV-R4, IGV-R32).
        ("no block", anglers_file(1_791_735_100, &[])),
        // AnglersFileTest.KEEP_THREE (IGV-R33).
        (
            "trimmed to three cards",
            anglers_file(
                1_791_730_000,
                &[
                    "{ region = \"us\", builtAt = 1791734400, cardCount = 1, keyCount = 2, \
                     titles = { \"Seeker\" }, cards = \"0012300103703120\", \
                     keys = \"046180A1B2C3D000001046180A1B2C3E000001\" },\n",
                    EU,
                ],
            ),
        ),
        // AnglersFileTest.KEEP_FOUR (IGV-R33).
        (
            "trimmed to four cards",
            anglers_file(
                1_791_730_000,
                &[
                    "{ region = \"us\", builtAt = 1791734400, cardCount = 2, keyCount = 3, \
                     titles = { \"An\\\"gler\", \"Seeker\" }, cards = \"00160012100071210022300103703120\", \
                     keys = \"012340000000A000001046180A1B2C3D000002046180A1B2C3E000002\" },\n",
                    EU,
                ],
            ),
        ),
        // AnglersFileTest: keepTwo, one card left in each block.
        (
            "trimmed to one card a block",
            anglers_file(
                1_791_730_000,
                &[
                    "{ region = \"us\", builtAt = 1791734400, cardCount = 1, keyCount = 1, \
                     titles = { \"An\\\"gler\" }, cards = \"0016001210007121\", \
                     keys = \"012340000000A000001\" },\n",
                    "{ region = \"eu\", builtAt = 1791730000, cardCount = 1, keyCount = 1, \
                     titles = { \"Wärden\" }, cards = \"0015002009005120\", \
                     keys = \"0140300000002000001\" },\n",
                ],
            ),
        ),
        // AnglersFileTest: keepNone, every block emptied by trimming.
        (
            "trimming emptied every block",
            anglers_file(
                1_791_730_000,
                &[
                    "{ region = \"us\", builtAt = 1791734400, cardCount = 0, keyCount = 0, \
                     titles = {  }, cards = \"\", keys = \"\" },\n",
                    "{ region = \"eu\", builtAt = 1791730000, cardCount = 0, keyCount = 0, \
                     titles = {  }, cards = \"\", keys = \"\" },\n",
                ],
            ),
        ),
        // AnglersDownloadIT.emptyBlock beside AnglersFileTest.EU: one region
        // emptied, the other kept.
        (
            "one block emptied",
            anglers_file(
                1_791_730_000,
                &[
                    "{ region = \"tw\", builtAt = 1791734400, cardCount = 0, keyCount = 0, \
                     titles = {  }, cards = \"\", keys = \"\" },\n",
                    EU,
                ],
            ),
        ),
        // RegionBlockWriterTest: a card with no title and no key.
        (
            "a card with no key",
            anglers_file(
                1_791_734_400,
                &[
                    "{ region = \"eu\", builtAt = 1791734400, cardCount = 1, keyCount = 0, \
                   titles = {  }, cards = \"0000500000000000\", keys = \"\" },\n",
                ],
            ),
        ),
        // RegionBlockWriterTest: cards numbered by Angler, one key; titles
        // empty as for untitled cards.
        (
            "cards numbered from 1",
            anglers_file(
                1_791_734_400,
                &[
                    "{ region = \"us\", builtAt = 1791734400, cardCount = 2, keyCount = 1, \
                   titles = {  }, cards = \"00001000000000000000200000000000\", \
                   keys = \"0000100000001000002\" },\n",
                ],
            ),
        ),
        // RegionBlockWriterTest: titles distinct and in byte order (the test
        // asserts the titles and each card's index; level 1 otherwise zero).
        (
            "titles in byte order",
            anglers_file(
                1_791_734_400,
                &[
                    "{ region = \"eu\", builtAt = 1791734400, cardCount = 4, keyCount = 0, \
                   titles = { \"B\", \"b\", \"Ä\" }, \
                   cards = \"0020100000000000001010000000000000201000000000000030100000000000\", \
                   keys = \"\" },\n",
                ],
            ),
        ),
        // RegionBlockWriterTest: a title through the Lua string writer
        // (\", \\, \009, \127); its card filled in as above.
        (
            "an escaped title",
            anglers_file(
                1_791_734_400,
                &[
                    "{ region = \"eu\", builtAt = 1791734400, cardCount = 1, keyCount = 0, \
                   titles = { \"say \\\"hi\\\"\\\\\\009\\127\" }, \
                   cards = \"0010100000000000\", keys = \"\" },\n",
                ],
            ),
        ),
        // RegionBlockWriterTest: numbers too large written as their field's
        // largest value.
        (
            "capped fields",
            anglers_file(
                1_791_734_400,
                &[
                    "{ region = \"eu\", builtAt = 1791734400, cardCount = 1, keyCount = 0, \
                   titles = {  }, cards = \"0009999999999991\", keys = \"\" },\n",
                ],
            ),
        ),
        ("999 titles and a card past them", thousand_titles()),
    ]
}

/// `RegionBlockWriterTest`: 1,000 titled cards keep the first 999 titles by
/// bytes, and the card past them gets index 0 (IGV-R16).
fn thousand_titles() -> String {
    let titles: Vec<String> = (0..999).map(|i| format!("\"T{i:04}\"")).collect();
    let cards: String = (0..1000)
        .map(|i| format!("{:03}0100000000000", if i < 999 { i + 1 } else { 0 }))
        .collect();
    let block = format!(
        "{{ region = \"eu\", builtAt = 1791734400, cardCount = 1000, keyCount = 0, \
         titles = {{ {} }}, cards = \"{cards}\", keys = \"\" }},\n",
        titles.join(", ")
    );
    anglers_file(1_791_734_400, &[&block])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_game::check::{check_anglers, check_following};

    #[test]
    fn every_following_body_the_backend_writes_passes_the_check() {
        // CMP-R82, IGV-R41: a body the server legitimately sends is written
        for body in FOLLOWING {
            assert_eq!(check_following(body.as_bytes()), Ok(()), "{body}");
        }
    }

    #[test]
    fn every_anglers_body_the_backend_writes_passes_the_check() {
        // CMP-R82, IGV-R41: a body the server legitimately sends is written,
        // blocks emptied by trimming (IGV-R33) included
        for (what, body) in anglers() {
            assert_eq!(check_anglers(body.as_bytes()), Ok(()), "{what}");
        }
    }
}
