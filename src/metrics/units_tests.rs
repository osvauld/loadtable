use super::units::*;

fn u(done: u64, total: u64) -> Units {
    Units { done, total }
}

#[test]
fn parser_reads_the_n_over_m_pair_of_a_cargo_progress_line() {
    assert_eq!(
        parse_progress("Building [=====>        ] 12/57: foo, bar"),
        Some(u(12, 57))
    );
    assert_eq!(
        parse_progress("Building [ ] 0/1: ptest(bin)"),
        Some(u(0, 1))
    );
    assert_eq!(parse_progress("  Building [==>] 3/9  "), Some(u(3, 9)));
}

#[test]
fn parser_accepts_a_finished_build_where_n_equals_m() {
    assert_eq!(
        parse_progress("Building [=========>] 57/57"),
        Some(u(57, 57))
    );
}

#[test]
fn parser_rejects_garbage_and_impossible_pairs() {
    for bad in [
        "",
        "Building",
        "Building [==>",
        "Building [==>] x/57",
        "Building [==>] 12/",
        "Building [==>] 12/57x",
        "Building [==>] 58/57",
        "Building [==>] 0/0",
        "Building 42% of the thing",
        "   Compiling foo v0.1.0",
        "Building [==>] 99999999999999999999999/5",
    ] {
        assert_eq!(parse_progress(bad), None, "{bad:?}");
    }
}

fn run(chunks: &[&str]) -> (String, Option<Units>) {
    let mut f = ProgressFilter::default();
    let mut log = vec![];
    let mut latest = None;
    for c in chunks {
        let out = f.feed(c.as_bytes());
        log.extend(out.log);
        latest = out.latest.or(latest);
    }
    log.extend(f.finish());
    (String::from_utf8(log).unwrap(), latest)
}

#[test]
fn redrawn_cr_lines_collapse_to_the_last_progress_line() {
    let (log, latest) = run(&[
        "Building [>   ] 1/9: a\rBuilding [=>  ] 2/9: a, b\rBuilding [==> ] 3/9: c\r",
        "    Finished dev\n",
    ]);
    assert_eq!(latest, Some(u(3, 9)));
    assert_eq!(log, "    Finished dev\nBuilding [==> ] 3/9: c\n");
}

#[test]
fn interleaved_compiler_output_survives_and_the_erase_redraws_are_dropped() {
    let (log, latest) = run(&[
        "Building [>   ] 1/9: a\r",
        "                    \r   Compiling a v0.1.0\n",
        "Building [=>  ] 2/9: b\r                    \rwarning: unused\n",
        "Building [==> ] 9/9\r",
    ]);
    assert_eq!(latest, Some(u(9, 9)));
    assert_eq!(
        log,
        "   Compiling a v0.1.0\nwarning: unused\nBuilding [==> ] 9/9\n"
    );
}

#[test]
fn chunk_boundaries_inside_a_progress_line_do_not_change_the_result() {
    let text = "x\nBuilding [>   ] 1/9: a\r  \r   Compiling a\nBuilding [=>] 2/9: b\r";
    let whole = run(&[text]);
    for cut in 0..text.len() {
        assert_eq!(run(&[&text[..cut], &text[cut..]]), whole, "cut at {cut}");
    }
    assert_eq!(whole.1, Some(u(2, 9)));
}

#[test]
fn total_changing_mid_build_follows_the_latest_line() {
    let (_, latest) = run(&["Building [>] 5/10: a\r", "Building [>] 5/14: a\r"]);
    assert_eq!(latest, Some(u(5, 14)));
}

#[test]
fn ordinary_output_passes_through_byte_for_byte() {
    let text = "plain\r\nline two\n   \nBuilding site\n\rtail without newline";
    let (log, latest) = run(&[text]);
    assert_eq!(log, text);
    assert_eq!(latest, None);
}

#[test]
fn a_long_unterminated_line_is_not_held_back() {
    let mut f = ProgressFilter::default();
    let long = "y".repeat(5000);
    assert_eq!(f.feed(long.as_bytes()).log, long.as_bytes());
}

#[test]
fn a_thousand_redraws_leave_one_line_in_the_log() {
    let mut text = String::new();
    for i in 0..1000 {
        text.push_str(&format!("Building [>   ] {i}/1000: crate{i}\r"));
    }
    let (log, latest) = run(&[&text]);
    assert_eq!(latest, Some(u(999, 1000)));
    assert_eq!(log, "Building [>   ] 999/1000: crate999\n");
}

#[test]
fn units_file_round_trips() {
    let dir = std::env::temp_dir().join(format!("units-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(read(&dir), None);
    write(&dir, u(4, 8));
    write(&dir, u(5, 8));
    assert_eq!(read(&dir), Some(u(5, 8)));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn crates_397_of_a_376_median_shows_usually_text_not_a_fraction() {
    let (fraction, text) = describe(397, Some(376), None);
    assert_eq!(text, "397 crates (usually ~376)");
    assert_eq!(
        fraction, None,
        "past the median there is no known remainder"
    );
}

#[test]
fn below_the_median_the_estimate_gives_a_fraction_and_still_says_usually() {
    let (fraction, text) = describe(188, Some(376), None);
    assert_eq!(text, "188 crates (usually ~376)");
    assert_eq!(fraction, Some(0.5));
}

#[test]
fn without_history_or_units_only_the_count_is_shown() {
    assert_eq!(describe(7, None, None), (None, "7 crates".into()));
    assert_eq!(describe(7, Some(0), None), (None, "7 crates".into()));
}

#[test]
fn cargo_units_win_over_history_and_never_exceed_one() {
    let (fraction, text) = describe(397, Some(376), Some((30, 60)));
    assert_eq!((fraction, text.as_str()), (Some(0.5), "30/60 units"));
    // An impossible pair falls back to the crate text.
    assert_eq!(
        describe(397, Some(376), Some((61, 60))).1,
        "397 crates (usually ~376)"
    );
}

fn env_for(
    present: &[&str],
    inherited: &[(&str, &str)],
) -> std::collections::BTreeMap<String, String> {
    let mut set = std::collections::BTreeMap::new();
    progress_env(
        |n| present.contains(&n),
        |n| {
            inherited
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        },
        |n, v| {
            set.insert(n.to_string(), v);
        },
    );
    set
}

#[test]
fn progress_env_sets_both_variables_by_default() {
    let set = env_for(&[], &[]);
    assert_eq!(set["CARGO_TERM_PROGRESS_WHEN"], "always");
    assert_eq!(set["CARGO_TERM_PROGRESS_WIDTH"], "80");
}

#[test]
fn progress_env_does_not_override_values_already_chosen() {
    let set = env_for(&["CARGO_TERM_PROGRESS_WHEN"], &[]);
    assert!(!set.contains_key("CARGO_TERM_PROGRESS_WHEN"));
    assert_eq!(set["CARGO_TERM_PROGRESS_WIDTH"], "80");
    let set = env_for(&[], &[("CARGO_TERM_PROGRESS_WIDTH", "120")]);
    assert_eq!(set["CARGO_TERM_PROGRESS_WIDTH"], "120");
}
