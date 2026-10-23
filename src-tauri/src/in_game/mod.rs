//! The in-game data addons, `Pagle_Following`, `Pagle_Anglers` and
//! `Pagle_Content` (companion/README.md, In-game data addons;
//! `in-game-views.md`; `hunt-events.md`, The addon folder).
//!
//! [`check`] is the strict check of a downloaded body against its data file's
//! grammar (CMP-R82, HEV-R51), run before anything is written. [`folders`]
//! writes a checked body into each install's `Interface/AddOns/` (CMP-R80,
//! CMP-R83) and deletes the three folders (CMP-R85). [`timer`] says when the
//! three downloads are due (CMP-R81); the downloads themselves are the
//! queue's items, in `upload::download`.

#[cfg(test)]
pub(crate) mod backend_bodies;
pub mod check;
#[cfg(test)]
pub(crate) mod content_vector;
pub mod folders;
pub mod timer;
