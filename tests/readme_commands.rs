//! Keeps the README from drifting: every subcommand must appear in it as
//! `aorus-lcd <name>`.
//!
//! The list comes from `--help` because the clap definition lives in the binary,
//! where tests cannot reach it. The child gets `--dry-run` and a lock path that
//! cannot exist, as in every test that starts the binary.

use std::process::Command;

use aorus_lcd::bus::lock::LOCK_PATH_ENV;

const README: &str = include_str!("../README.md");

/// The names in the `Commands:` section of `--help`, without clap's own `help`.
fn subcommands() -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_aorus-lcd"))
        .args(["--dry-run", "--help"])
        .env(LOCK_PATH_ENV, "/nonexistent/aorus-lcd-test/lock")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect()
}

/// Whether `text` has `aorus-lcd name` as whole words, so that `on` is not
/// satisfied by a mention of something longer.
fn mentions(text: &str, name: &str) -> bool {
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|word| !word.is_empty())
        .collect();
    words.windows(2).any(|pair| pair == ["aorus-lcd", name])
}

#[test]
fn the_list_of_subcommands_is_not_empty() {
    // Otherwise a change to the shape of `--help` would pass the test below.
    assert!(subcommands().iter().any(|name| name == "status"));
}

#[test]
fn the_readme_mentions_every_subcommand() {
    let missing: Vec<String> = subcommands()
        .into_iter()
        .filter(|name| !mentions(README, name))
        .collect();

    assert!(missing.is_empty(), "missing from README.md: {missing:?}");
}

#[test]
fn a_longer_word_is_no_mention() {
    assert!(mentions("run `aorus-lcd on` first", "on"));
    assert!(!mentions("the aorus-lcd online note", "on"));
}
