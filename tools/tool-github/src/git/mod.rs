//! Reading a local git repository's objects, byte for byte, from both loose files and packs
//! (`gix-pack`/`gix-object`/`gix-hash`, all read via their `from_data` buffer constructors —
//! see `rust-toolchain.toml` for why never `at(path)`), and writing fetched objects back as
//! loose files.

pub mod date;
pub mod object;
pub mod pack;
pub mod rebuild;
pub mod store;
pub mod walk;
pub mod write;

#[cfg(any(test, feature = "test-support"))]
pub mod test_fixture;
