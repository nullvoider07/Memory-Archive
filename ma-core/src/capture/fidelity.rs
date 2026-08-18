// /Memory-Archive/ma-core/src/capture/fidelity.rs
//
// Content-level record fidelity check.
//
// The version gate in `compat` closes a known defect in a known release. This
// closes nothing — it watches. Its job is the case the version gate cannot
// have: a truncation bug that has not been found yet, in a release that is
// nominally supported.
//
// What it looks for is the signature the one known instance left. Control-Center
// below 1.2.2 rebuilt its report of a typed command by scanning for a closing
// quote, so a command containing one was stored cut at that quote — leaving a
// string with an odd number of quote characters where the real command had a
// matched pair. That signature is cheap to test and does not depend on knowing
// which version or which bug produced it.
//
// It is a heuristic, so it never alters the record and never fails a step or a
// session. It attaches a flag beside the step saying "this looks cut, check it
// against the frames" and leaves every decision to a person. A false positive
// costs one glance at a frame; a false negative is a corpus entry that is
// quietly wrong, which is the failure actually worth spending on.
//
// Deliberately *not* here: any attempt to repair the text. The true command is
// not recoverable from a truncated copy — the one repair in the corpus was read
// off a frame by a person and stamped with its provenance. A guess would be
// indistinguishable from a record.

use ma_proto::control_center::CommandEvent;
use serde::{Deserialize, Serialize};

/// A suspicion attached to a recorded step. Advisory only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSuspect {
    /// Which check fired, as a stable slug.
    pub check: String,
    /// What it means and what to do about it, for whoever reads the record.
    pub note: String,
}

/// Count quote characters that are not backslash-escaped.
///
/// Counts the run of backslashes before each quote: an even run leaves the quote
/// live, an odd run escapes it. `\\"` is an escaped backslash followed by a live
/// quote, and reading it the other way is how a naive scanner gets this wrong in
/// the first place.
fn unescaped_quote_count(text: &str, quote: char) -> usize {
    let mut count = 0;
    let mut backslashes = 0;

    for ch in text.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == quote && backslashes % 2 == 0 {
            count += 1;
        }
        backslashes = 0;
    }
    count
}

/// True when `text` ends in a *lone* escape — an odd run of backslashes that is
/// its own token rather than part of a path.
///
/// The distinction is what makes this check usable, and it comes from the corpus
/// rather than from taste. Of 164 typed payloads, four end in a backslash and all
/// four are Windows paths whose last argument is a destination directory
/// (`...\corpus-seed\dest\`, `...\found\`, `...\Q3\`) — in every one, the run
/// follows a word character. The two known truncations end the same way and
/// differ in exactly this: `printf \` and `osascript -e \`, where the run
/// follows a space. Requiring whitespace before it separates the two classes
/// cleanly on the only evidence there is.
fn ends_with_lone_escape(text: &str) -> bool {
    let run = text.chars().rev().take_while(|c| *c == '\\').count();
    if run % 2 == 0 || run == 0 {
        return false;
    }
    match text.chars().rev().nth(run) {
        // The payload is nothing but backslashes.
        None => true,
        Some(prev) => prev.is_whitespace(),
    }
}

/// Inspect a command event for signs its recorded text was cut short.
///
/// Returns `None` for everything it has no opinion about, which is most events:
/// only a typed payload is checked, because only a typed payload carries text
/// whose shape can be tested. Coordinates and key names are already structured.
///
/// The three signals below are the three shapes the known truncations actually
/// left, in the order of how cleanly they discriminate. This is a signature
/// check, not a proof of correctness: it cannot certify a payload is intact, and
/// a cut landing somewhere none of these describe will pass silently. That limit
/// is why the version gate in `compat` exists as well — the two defend different
/// halves, and neither replaces the other.
pub fn inspect(event: &CommandEvent) -> Option<RecordSuspect> {
    if event.action_type != "keyboard" || event.action_subtype != "type" {
        return None;
    }

    // The agent sends "Typed: {text}". A payload without the prefix is already
    // odd, but not this check's business — convert handles that fallback.
    let text = event.raw_command.strip_prefix("Typed: ")?;
    if text.is_empty() {
        return None;
    }

    let doubles = unescaped_quote_count(text, '"');
    let singles = unescaped_quote_count(text, '\'');

    let (check, detail) = if ends_with_lone_escape(text) {
        (
            "dangling-escape",
            "The recorded text ends in a lone backslash. That is what an escape sitting \
             immediately before the cut point looks like, and it is the shape both known \
             truncations left. A trailing backslash that is part of a path is not flagged."
                .to_string(),
        )
    } else if doubles % 2 == 1 {
        (
            "unbalanced-double-quote",
            "The recorded text contains an odd number of unescaped double-quote characters, \
             which is the signature of a command cut short at a quote."
                .to_string(),
        )
    } else if singles % 2 == 1 {
        (
            "unbalanced-single-quote",
            "The recorded text contains an odd number of single-quote characters, which can \
             mean a command cut short at a quote. Note that a single quote is also an \
             ordinary apostrophe, so typed prose can trip this legitimately."
                .to_string(),
        )
    } else {
        return None;
    };

    Some(RecordSuspect {
        check: check.to_string(),
        note: format!(
            "{detail} Actuation is unaffected — if this step ran, the keystrokes were \
             delivered and any artifact on disk is real; it is the stored copy of the command \
             that may be incomplete. Verify it against this step's frames before annotating, \
             and do not edit the record without recording why."
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(raw: &str) -> CommandEvent {
        CommandEvent {
            action_type: "keyboard".to_string(),
            action_subtype: "type".to_string(),
            raw_command: raw.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn flags_the_one_truncation_the_corpus_actually_holds() {
        // textedit-new-saveas step 2, recorded 2026-07-22 on agent 1.0.0. The
        // agent typed `printf "Title\nDate\nstatus: open" > ~/Documents/readme.txt`
        // in full and stored this. Note it carries no quote at all — the cut
        // landed before the first one, which is why an unbalanced-quote check
        // alone would miss the very case this exists for.
        let s = inspect(&typed("Typed: printf \\")).expect("must flag");
        assert_eq!(s.check, "dangling-escape");
    }

    #[test]
    fn flags_the_second_known_truncation() {
        // S026, 2026-07-25, same agent defect:
        // `osascript -e "tell application \"System Events\" to ..."`.
        let s = inspect(&typed("Typed: osascript -e \\")).expect("must flag");
        assert_eq!(s.check, "dangling-escape");
    }

    #[test]
    fn flags_the_single_quote_truncation() {
        // S011: `sed -i '' 's/colour/color/g' doc-colour.txt` stored as this.
        let s = inspect(&typed("Typed: sed -i '")).expect("must flag");
        assert_eq!(s.check, "unbalanced-single-quote");
    }

    #[test]
    fn passes_the_intact_form_of_that_same_command() {
        // The repaired value. The check must be quiet on the thing it is meant
        // to protect, or it is noise rather than a signal.
        assert_eq!(
            inspect(&typed(
                "Typed: printf \"Title\\nDate\\nstatus: open\" > ~/Documents/readme.txt"
            )),
            None
        );
    }

    #[test]
    fn passes_the_quoted_command_that_proved_the_fix() {
        // S063, agent 1.2.2, two single-quoted operands, recorded in full.
        assert_eq!(
            inspect(&typed(
                "Typed: (Get-Content $env:USERPROFILE\\corpus-seed\\doc-colour.txt) \
                 -replace 'colour','color' | Set-Content \
                 $env:USERPROFILE\\corpus-seed\\doc-colour.txt"
            )),
            None
        );
    }

    #[test]
    fn the_four_legitimate_trailing_backslashes_are_not_flagged() {
        // Every payload in the corpus that ends in a backslash. All four are a
        // destination directory. Flagging these would train the reader to
        // ignore the check, which is worse than not having it.
        for raw in [
            "Typed: Copy-Item $env:USERPROFILE\\corpus-seed\\report.pdf \
             $env:USERPROFILE\\corpus-seed\\dest\\",
            "Typed: Move-Item $env:USERPROFILE\\corpus-seed\\report.pdf \
             $env:USERPROFILE\\corpus-seed\\dest\\",
            "Typed: New-Item -ItemType Directory \
             $env:USERPROFILE\\corpus-seed\\Projects\\2026\\Q3 -Force; Move-Item \
             $env:USERPROFILE\\corpus-seed\\report.pdf \
             $env:USERPROFILE\\corpus-seed\\Projects\\2026\\Q3\\",
            "Typed: Get-ChildItem $env:USERPROFILE\\corpus-seed -Recurse -Filter *invoice* \
             | Copy-Item -Destination $env:USERPROFILE\\corpus-seed\\found\\",
        ] {
            assert_eq!(inspect(&typed(raw)), None, "false positive on: {raw}");
        }
    }

    #[test]
    fn a_path_separator_and_a_cut_are_told_apart_by_what_precedes_them() {
        // The whole discriminator, isolated.
        assert!(ends_with_lone_escape("printf \\"));
        assert!(!ends_with_lone_escape("C:\\dest\\"));
        // An even run is an escaped backslash, not a dangling one.
        assert!(!ends_with_lone_escape("printf \\\\"));
        // A payload that is nothing but one backslash is still a cut.
        assert!(ends_with_lone_escape("\\"));
    }

    #[test]
    fn an_escaped_quote_does_not_close_a_pair() {
        // `\"` is escaped and must not count; the string below has one live
        // quote and is genuinely unbalanced.
        let s = inspect(&typed("Typed: echo \\\" and \"")).expect("must flag");
        assert_eq!(s.check, "unbalanced-double-quote");
        // An escaped backslash leaves the quote after it live, so this is balanced.
        assert_eq!(inspect(&typed("Typed: echo \"a\\\\\" b")), None);
    }

    #[test]
    fn signals_are_reported_strongest_first() {
        // All three could fire here. The one that discriminates best must win,
        // or the note sends the reader after an apostrophe.
        let s = inspect(&typed("Typed: echo \" don't \\")).expect("must flag");
        assert_eq!(s.check, "dangling-escape");

        let s = inspect(&typed("Typed: echo \" don't")).expect("must flag");
        assert_eq!(s.check, "unbalanced-double-quote");
    }

    #[test]
    fn an_apostrophe_is_flagged_but_says_so() {
        // A real false-positive class. Still worth reporting — S011 was cut at a
        // single quote — but the note has to admit what else it means.
        let s = inspect(&typed("Typed: don't stop")).expect("must flag");
        assert_eq!(s.check, "unbalanced-single-quote");
        assert!(s.note.contains("apostrophe"));
    }

    #[test]
    fn every_note_says_actuation_is_unaffected() {
        // The one thing a reader must not conclude from this flag is that the
        // step did not happen. Both known truncations actuated perfectly.
        for raw in ["Typed: printf \\", "Typed: echo \"", "Typed: don't"] {
            let s = inspect(&typed(raw)).expect("must flag");
            assert!(s.note.contains("Actuation is unaffected"), "on: {raw}");
            assert!(s.note.contains("frames"), "must point at the evidence: {raw}");
        }
    }

    #[test]
    fn ignores_everything_that_is_not_typed_text() {
        let mut click = typed("Left-clicked at X=250, Y=386");
        click.action_type = "mouse".to_string();
        click.action_subtype = "left".to_string();
        assert_eq!(inspect(&click), None);

        let mut press = typed("Pressed: Return");
        press.action_subtype = "press".to_string();
        assert_eq!(inspect(&press), None);
    }

    #[test]
    fn a_payload_without_the_prefix_is_not_this_checks_business() {
        assert_eq!(inspect(&typed("printf \"unterminated")), None);
        assert_eq!(inspect(&typed("Typed: ")), None);
    }

    #[test]
    fn never_panics_on_hostile_input() {
        // raw_command arrives over the wire. A check that panics on a malformed
        // payload would take the capture down, which is worse than the record it
        // was added to protect.
        for raw in [
            "Typed: ",
            "Typed: \\",
            "Typed: \\\\\\",
            "Typed: \"\"\"\"\"",
            "Typed: '''",
            "Typed: \u{1F600}\"",
            "Typed: \u{1F600}\\",
            "Typed: \0\"",
        ] {
            let _ = inspect(&typed(raw));
        }
    }
}
