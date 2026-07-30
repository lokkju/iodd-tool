//! CLI surface tests. Phase 1 asserts the shape of the interface and the
//! exit-code discipline; behaviour lands with each later phase.
//!
//! The crate denies unwrap/expect/panic so the write path can never abort on a
//! device holding real data. clippy's `allow-*-in-tests` covers `#[cfg(test)]`
//! modules but not helper functions in an integration-test crate, so the
//! exemption is spelled out here.
#![allow(clippy::expect_used, clippy::panic)]

use assert_cmd::Command;
use predicates::str::contains;

fn iodd() -> Command {
    Command::cargo_bin("iodd").expect("binary builds")
}

#[test]
fn help_exits_zero() {
    iodd().arg("--help").assert().success();
}

#[test]
fn version_exits_zero() {
    iodd().arg("--version").assert().success();
}

#[test]
fn every_subcommand_has_help() {
    for sub in ["create", "convert", "verify", "doctor", "retype"] {
        iodd()
            .args([sub, "--help"])
            .assert()
            .success()
            .stdout(contains(sub));
    }
}

/// SPEC.md reserves exit 2 for "target exists". clap defaults usage errors to
/// 2, which would collide, so main.rs remaps them to 1.
#[test]
fn usage_errors_exit_one_not_two() {
    iodd().arg("--definitely-not-a-flag").assert().code(1);
    iodd().arg("nonesuch-subcommand").assert().code(1);
    // create requires --size and --out
    iodd().arg("create").assert().code(1);
}

#[test]
fn no_arguments_is_a_usage_error() {
    iodd().assert().code(1);
}

#[test]
fn documented_create_flags_are_accepted() {
    iodd()
        .args(["create", "--help"])
        .assert()
        .success()
        .stdout(contains("--size"))
        .stdout(contains("--out"))
        .stdout(contains("--removable"))
        .stdout(contains("--force"))
        .stdout(contains("--creator"))
        .stdout(contains("--keep-on-fail"));
}

#[test]
fn documented_doctor_flags_are_accepted() {
    iodd()
        .args(["doctor", "--help"])
        .assert()
        .success()
        .stdout(contains("--type"))
        .stdout(contains("--format"))
        .stdout(contains("--strict"))
        .stdout(contains("--iso-max-fragments"));
}

/// Subcommands not yet built must fail cleanly rather than silently
/// succeeding. `verify` left this list in phase 2.
#[test]
fn unbuilt_subcommands_say_so() {
    for sub in [
        vec!["create", "--size", "1M", "--out", "/tmp/x.vhd"],
        vec!["convert", "--source", "/tmp/a", "--out", "/tmp/b.vhd"],
        vec!["doctor", "/tmp"],
        vec!["retype", "/tmp/x.vhd", "--to", "removable"],
    ] {
        iodd()
            .args(&sub)
            .assert()
            .failure()
            .stderr(contains("not implemented"));
    }
}

/// A missing file is a usage error (exit 1), not a structural failure.
#[test]
fn verify_reports_a_missing_file_as_usage() {
    iodd().args(["verify", "/nonexistent-zzz"]).assert().code(1);
}

#[test]
fn verify_rejects_a_directory() {
    iodd().args(["verify", "/tmp"]).assert().code(1);
}
