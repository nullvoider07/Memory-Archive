// /Memory-Archive/ma-core/src/capture/compat.rs
//
// Control-Center version gate.
//
// Protobuf already absorbs additive wire changes — an unknown field is ignored,
// so a new field in CommandEvent needs no code here. What it cannot absorb is a
// change of *meaning* in a field that already exists: `position_captured` was a
// best-effort cursor readback before Control-Center 1.2.0 and a verified value
// (or false) from 1.2.0 onward, with no difference on the wire at all. Nothing
// observable at runtime distinguishes the two, so nothing at runtime can adapt
// to it; the knowledge lives in a changelog a person read.
//
// That makes an unrecognised version a refusal rather than a guess. The cost of
// guessing wrong is a corpus session that records confidently and wrongly, which
// is the failure this whole capture path is built to avoid — and it is discovered
// long after the environment that produced it is gone.

use std::fmt;

/// Oldest Control-Center this build can *connect to and drive*. 1.0.0 has no TLS
/// and no scope enforcement; the transport negotiation in `stream` handles it.
///
/// This is deliberately not a statement about record fidelity — see
/// [`RECORD_FIDELITY_MIN`]. The two floors are separate because they failed
/// separately: a 1.0.0 agent connects, actuates and reports success while
/// storing a truncated copy of what it typed.
pub const SUPPORTED_MIN: Version = Version::new(1, 0, 0);

/// Oldest Control-Center **agent** that reports a typed command faithfully.
///
/// Below this, the agent reverse-engineered its own `human_command` by scanning
/// for a closing quote with a naive `find('"')`, so anything typed with an
/// embedded quote was stored truncated at that quote. The keystrokes actuated
/// correctly and the artifact on disk was byte-exact — only the record was
/// damaged, which is what makes it dangerous: every self-reporting signal says
/// success while the trace is quietly lossy. Control-Center 1.2.2 (`122e406`)
/// removed the mechanism, making `human_command` a controller-supplied field
/// instead of an agent-derived guess.
///
/// This is checked against the **agent** version, not the server's. The defect
/// lived in `crates/agent`, and a modern server can front an old agent.
pub const RECORD_FIDELITY_MIN: Version = Version::new(1, 2, 2);

/// Newest Control-Center this build has been verified against. Raise it only
/// alongside a run of the compatibility matrix in `integration-tests/`.
pub const SUPPORTED_MAX: Version = Version::new(1, 3, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }

    /// Parse `1.2.1`, tolerating a `v` prefix and a `-rc1` / `+build` suffix.
    /// Returns None for anything else — an unparseable version is treated as
    /// unknown rather than coerced into a number that would gate wrongly.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let core = trimmed
            .split(['-', '+'])
            .next()
            .unwrap_or(trimmed);

        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The outcome of checking a reported server version against this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compat {
    Supported(Version),
    /// Newer than this build was verified against — refuse unless overridden.
    TooNew { found: Version, max: Version },
    /// Older than the oldest supported release.
    TooOld { found: Version, min: Version },
    /// The server did not report a version, or reported one we cannot parse.
    /// Not a refusal: Control-Center 1.0.0 predates the checks this gate relies
    /// on, and turning a silent-but-working setup into a hard failure would be a
    /// regression rather than a safeguard.
    Unknown(String),
}

/// Evaluate a reported version. `max_override` blesses a newer release without a
/// rebuild, for when its release notes confirm the command stream is unchanged.
pub fn evaluate(reported: &str, max_override: Option<Version>) -> Compat {
    let max = max_override.unwrap_or(SUPPORTED_MAX).max(SUPPORTED_MAX);

    match Version::parse(reported) {
        None => Compat::Unknown(reported.to_string()),
        Some(found) if found > max => Compat::TooNew { found, max },
        Some(found) if found < SUPPORTED_MIN => Compat::TooOld {
            found,
            min: SUPPORTED_MIN,
        },
        Some(found) => Compat::Supported(found),
    }
}

impl Compat {
    /// The operator-facing refusal, or None when the version is acceptable.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Compat::Supported(_) | Compat::Unknown(_) => None,
            Compat::TooNew { found, max } => Some(format!(
                "Control-Center {found} is newer than this build of Memory Archive has been \
                 verified against (up to {max}). Refusing to record rather than risk a silent \
                 mismatch: a field can change meaning without changing on the wire, as \
                 `position_captured` did in 1.2.0, and the result is a session that records \
                 confidently and wrongly. Either run Control-Center {max}, update Memory \
                 Archive (`memory-archive update`) to a build that lists {found} as supported, \
                 or — once the release notes confirm the command stream is unchanged — set \
                 `control_center_max_version = \"{found}\"`. To bypass the gate entirely, set \
                 `control_center_allow_unsupported = true`."
            )),
            Compat::TooOld { found, min } => Some(format!(
                "Control-Center {found} is older than the oldest release Memory Archive \
                 supports ({min}). Upgrade Control-Center, or set \
                 `control_center_allow_unsupported = true` to record anyway."
            )),
        }
    }

    /// The version to record as session provenance. Empty when unknown.
    pub fn recorded(&self) -> String {
        match self {
            Compat::Supported(v) => v.to_string(),
            Compat::TooNew { found, .. } | Compat::TooOld { found, .. } => found.to_string(),
            Compat::Unknown(raw) => raw.trim().to_string(),
        }
    }
}

/// The outcome of checking the reported Control-Center **agent** version against
/// the record-fidelity floor.
///
/// Separate from [`Compat`] because it answers a different question and arrives
/// at a different time. [`Compat`] asks "can this build drive that server?" and
/// is answered at connect, from `GetServerIdentity`. This asks "will what that
/// agent reports match what it did?" and cannot be answered until the first
/// event arrives, because no connect-time RPC carries the agent version —
/// `ConnectionMetadata` does not include it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFidelity {
    /// At or above the fidelity floor.
    Faithful(Version),
    /// Known to store a damaged copy of what it typed.
    Lossy { found: Version, min: Version },
    /// The agent did not report a version, or reported one we cannot parse.
    ///
    /// Not a refusal, matching [`Compat::Unknown`]: sessions recorded before
    /// Control-Center reported a version at all are silent-but-working, and
    /// turning them into a hard failure would be a regression rather than a
    /// safeguard. It is still worth saying out loud, because it means the
    /// session carries no fidelity evidence either way.
    Unknown(String),
}

/// Evaluate the agent version carried by a `CommandEvent`.
pub fn evaluate_agent(reported: &str) -> AgentFidelity {
    match Version::parse(reported) {
        None => AgentFidelity::Unknown(reported.to_string()),
        Some(found) if found < RECORD_FIDELITY_MIN => AgentFidelity::Lossy {
            found,
            min: RECORD_FIDELITY_MIN,
        },
        Some(found) => AgentFidelity::Faithful(found),
    }
}

impl AgentFidelity {
    /// The operator-facing refusal, or None when the agent is acceptable.
    pub fn refusal(&self) -> Option<String> {
        match self {
            AgentFidelity::Faithful(_) | AgentFidelity::Unknown(_) => None,
            AgentFidelity::Lossy { found, min } => Some(format!(
                "Control-Center agent {found} does not record typed commands \
                 faithfully. Below {min} the agent rebuilt its own report of what it \
                 typed by scanning for a closing quote, so any command containing one \
                 is stored truncated at that quote. The keystrokes actuate correctly \
                 and the artifact on disk is byte-exact — only the record is damaged, \
                 so nothing at capture time looks wrong and the loss is found long \
                 after the environment that produced it is gone. Refusing to record \
                 rather than write a trace that cannot be trusted. Upgrade the \
                 Control-Center agent to {min} or later (the server version is checked \
                 separately and does not cover this — the defect is in the agent). To \
                 record anyway, set `control_center_allow_unsupported = true`."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_decorated_versions() {
        assert_eq!(Version::parse("1.2.1"), Some(Version::new(1, 2, 1)));
        assert_eq!(Version::parse("v1.2.1"), Some(Version::new(1, 2, 1)));
        assert_eq!(Version::parse(" 1.2.1 "), Some(Version::new(1, 2, 1)));
        assert_eq!(Version::parse("1.3.0-rc1"), Some(Version::new(1, 3, 0)));
        assert_eq!(Version::parse("1.3.0+build7"), Some(Version::new(1, 3, 0)));
    }

    #[test]
    fn rejects_versions_it_cannot_read() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("latest"), None);
    }

    #[test]
    fn orders_by_component_not_lexically() {
        // "1.10.0" < "1.9.0" as strings; the gate must not think 1.10 is older.
        assert!(Version::parse("1.10.0").unwrap() > Version::parse("1.9.0").unwrap());
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.99.99").unwrap());
    }

    #[test]
    fn every_version_in_the_matrix_is_supported() {
        for v in ["1.0.0", "1.1.0", "1.2.0", "1.2.1", "1.2.2", "1.3.0"] {
            assert!(
                matches!(evaluate(v, None), Compat::Supported(_)),
                "{v} is exercised by the compatibility matrix and must pass the gate"
            );
        }
    }

    #[test]
    fn a_newer_release_is_refused_with_an_actionable_message() {
        let verdict = evaluate("1.4.0", None);
        assert!(matches!(verdict, Compat::TooNew { .. }));
        let msg = verdict.refusal().expect("TooNew must refuse");
        assert!(msg.contains("1.4.0"));
        assert!(msg.contains("control_center_max_version"));
        assert!(msg.contains("memory-archive update"));
    }

    #[test]
    fn an_explicit_max_blesses_a_newer_release() {
        let verdict = evaluate("1.4.0", Version::parse("1.4.0"));
        assert!(matches!(verdict, Compat::Supported(_)));
        assert!(verdict.refusal().is_none());
    }

    #[test]
    fn an_override_below_the_compiled_max_cannot_narrow_the_range() {
        // Setting a lower ceiling must not start refusing versions this build
        // genuinely supports — the field blesses newer releases, it is not a cap.
        let verdict = evaluate("1.2.1", Version::parse("1.1.0"));
        assert!(matches!(verdict, Compat::Supported(_)));
    }

    #[test]
    fn an_unreadable_version_does_not_refuse() {
        let verdict = evaluate("", None);
        assert!(matches!(verdict, Compat::Unknown(_)));
        assert!(verdict.refusal().is_none(), "unknown must not become a hard failure");
    }

    #[test]
    fn the_fidelity_floor_is_not_the_connect_floor() {
        // These are separate facts and must not be collapsed. 1.0.0 connects
        // fine and records typed commands wrongly; conflating the two is what
        // let the truncation through in the first place.
        assert!(RECORD_FIDELITY_MIN > SUPPORTED_MIN);
        assert!(matches!(evaluate("1.0.0", None), Compat::Supported(_)));
        assert!(matches!(
            evaluate_agent("1.0.0"),
            AgentFidelity::Lossy { .. }
        ));
    }

    #[test]
    fn agents_that_truncate_are_refused_with_an_actionable_message() {
        // 1.0.0 is the version that produced the one damaged record in the
        // corpus (textedit-new-saveas step 2, 2026-07-22): the agent typed
        // `printf "Title\n..."` in full and stored `Typed: printf \`.
        for v in ["1.0.0", "1.1.0", "1.2.0", "1.2.1"] {
            let verdict = evaluate_agent(v);
            assert!(
                matches!(verdict, AgentFidelity::Lossy { .. }),
                "agent {v} predates the human_command fix and must be refused"
            );
            let msg = verdict.refusal().expect("a lossy agent must refuse");
            assert!(msg.contains(v), "the refusal must name the version found");
            assert!(msg.contains("1.2.2"), "the refusal must name the fix version");
            assert!(
                msg.contains("control_center_allow_unsupported"),
                "the refusal must name the override"
            );
        }
    }

    #[test]
    fn the_refusal_reads_as_prose_not_as_a_collapsed_literal() {
        // This message is printed to an operator's console verbatim: it is
        // carried to the CLI as the IPC disconnect reason, not just logged. A
        // wrapped literal that loses its line continuations still contains every
        // word a `contains` assertion looks for, so nothing above would catch it
        // — the damage is only visible in the whitespace.
        let msg = evaluate_agent("1.0.0")
            .refusal()
            .expect("a lossy agent must refuse");
        assert!(
            !msg.contains("  "),
            "the refusal must not carry a run of spaces: {msg}"
        );
        assert!(
            !msg.contains('\n'),
            "the refusal is one paragraph — the caller decides how to wrap it"
        );
    }

    #[test]
    fn agents_at_or_above_the_fix_are_accepted() {
        for v in ["1.2.2", "1.3.0", "2.0.0"] {
            let verdict = evaluate_agent(v);
            assert!(
                matches!(verdict, AgentFidelity::Faithful(_)),
                "agent {v} carries the fix and must record"
            );
            assert!(verdict.refusal().is_none());
        }
    }

    #[test]
    fn an_agent_that_reports_no_version_does_not_refuse() {
        // 21 corpus sessions predate Control-Center reporting a version at all.
        // Refusing them would turn a working setup into a hard failure.
        for v in ["", "   ", "unknown"] {
            let verdict = evaluate_agent(v);
            assert!(matches!(verdict, AgentFidelity::Unknown(_)));
            assert!(
                verdict.refusal().is_none(),
                "an unreadable agent version must not become a hard failure"
            );
        }
    }

    #[test]
    fn the_agent_gate_is_not_capped_by_the_server_ceiling() {
        // evaluate() refuses anything above SUPPORTED_MAX; the agent gate has no
        // upper bound, because a newer agent cannot reintroduce an older defect.
        // Getting this wrong would refuse every future release.
        assert!(matches!(
            evaluate_agent("9.9.9"),
            AgentFidelity::Faithful(_)
        ));
    }

    #[test]
    fn provenance_records_what_the_server_reported() {
        assert_eq!(evaluate("1.2.1", None).recorded(), "1.2.1");
        assert_eq!(evaluate("1.4.0", None).recorded(), "1.4.0");
        assert_eq!(evaluate("weird", None).recorded(), "weird");
    }
}
