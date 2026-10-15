//! The in-game data addons, `Pagle_Following` and `Pagle_Anglers`
//! (companion/README.md, In-game data addons; `in-game-views.md`).
//!
//! [`check`] is the strict check of a downloaded body against its data file's
//! grammar (CMP-R82), run before anything is written. [`folders`] writes a
//! checked body into each install's `Interface/AddOns/` (CMP-R80, CMP-R83)
//! and deletes both folders (CMP-R85).

pub mod check;
pub mod folders;
