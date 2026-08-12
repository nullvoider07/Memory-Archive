// /Memory-Archive/ma-core/src/convert/mod.rs

use ma_proto::control_center::CommandEvent;

/// Convert a CommandEvent to a human-readable action string.
/// Never fails — unknown or malformed events fall back to the raw command.
pub fn to_human_readable(event: &CommandEvent) -> String {
    let converted = match event.action_type.as_str() {
        "mouse"    => convert_mouse(event),
        "keyboard" => convert_keyboard(event),
        "position" => convert_position(event),
        _          => event.raw_command.clone(),
    };

    // Prefix failed commands so they stand out in the converted file.
    if !event.success {
        format!("[FAILED] {converted}")
    } else {
        converted
    }
}

/// Modifier names the Control-Center agent prefixes onto a mouse action name,
/// paired with the symbol the CC mouse grammar takes as a prefix.
///
/// Two of the four symbols are one physical key reported under several platform
/// names. The agent names `!` Option on macOS and Alt elsewhere, and `#` Cmd on
/// macOS, Win on Windows and Super on Linux. Every name it can emit must map back
/// to its symbol: a name missing from this table is not recognised by
/// [`split_mouse_modifiers`], and the modifier is then dropped from the record,
/// turning a modified click into a plausible record of a plain one.
const MOUSE_MODIFIERS: [(&str, &str); 7] = [
    ("Cmd", "#"),
    ("Win", "#"),
    ("Super", "#"),
    ("Ctrl", "^"),
    ("Shift", "+"),
    ("Option", "!"),
    ("Alt", "!"),
];

/// Name a modifier symbol for the OS that reported the event.
///
/// `^` and `+` are the same key everywhere. `!` and `#` are not, and naming them
/// from a fixed table labels a Windows Super-click "Cmd". The names returned here
/// match what the agent itself emits for each platform.
fn modifier_name(symbol: char, os_type: &str) -> Option<&'static str> {
    Some(match symbol {
        '^' => "Ctrl",
        '+' => "Shift",
        '!' if os_type == "MACOS" => "Option",
        '!' => "Alt",
        '#' if os_type == "MACOS" => "Cmd",
        '#' if os_type == "WINDOWS" => "Win",
        '#' => "Super",
        _ => return None,
    })
}

/// Split a raw mouse command into its modifier names and the rest.
///
/// The agent composes "Cmd+Shift+Left-clicked at X=…" by joining modifier names
/// onto the action name. Splitting on '+' is only safe because every leading
/// token is matched against the known modifier names — an action name is never
/// consumed, so a raw command with no modifiers comes back untouched.
fn split_mouse_modifiers(raw: &str) -> (Vec<&str>, &str) {
    let mut modifiers = Vec::new();
    let mut rest = raw;

    while let Some(boundary) = rest.find('+') {
        let head = &rest[..boundary];
        if !MOUSE_MODIFIERS.iter().any(|(name, _)| *name == head) {
            break;
        }
        modifiers.push(head);
        rest = &rest[boundary + 1..];
    }

    (modifiers, rest)
}

/// Modifier names carried by a raw command that is the CC command itself.
///
/// When the pointer readback does not settle, the agent records the command
/// verbatim — "Executed: 770 310 #left" — instead of a sentence. The modifiers are
/// then symbols attached to the action token, where [`split_mouse_modifiers`] looks
/// for names and finds nothing. Without this the modifier is silently dropped from
/// the record, which is the failure this module exists to prevent.
///
/// `!` and `#` are each one key under several platform names, so the label follows
/// the reporting OS — see [`modifier_name`].
fn modifiers_from_command_form(rest: &str, subtype: &str, os_type: &str) -> Vec<&'static str> {
    if subtype.is_empty() {
        return Vec::new();
    }

    for token in rest.split_whitespace() {
        let Some(symbols) = token.strip_suffix(subtype) else { continue };
        if symbols.is_empty() || !symbols.chars().all(|c| matches!(c, '#' | '^' | '+' | '!')) {
            continue;
        }

        let mut names = Vec::new();
        for symbol in symbols.chars() {
            let Some(name) = modifier_name(symbol, os_type) else { continue };
            if !names.contains(&name) {
                names.push(name);
            }
        }
        return names;
    }

    Vec::new()
}

/// Strip a modifier-symbol prefix from an action subtype, e.g. "#left" -> "left".
///
/// The agent sends the bare verb. A server-side regression once let the prefix leak
/// into `action_subtype`, which turned every modified click into an unmatched verb
/// and wrote "#left at (770, 310)" into the corpus — a plausible-looking record of
/// an action nobody can replay. Tolerating the prefix here means a repeat degrades
/// to a correct record rather than a corrupt one.
fn split_subtype_symbols(subtype: &str) -> (&str, &str) {
    let bare = subtype.trim_start_matches(['#', '^', '+', '!']);
    (&subtype[..subtype.len() - bare.len()], bare)
}

/// The repeat count a scroll reports, from "Scrolled down 5 notches at X=…".
///
/// Agents before the count fix omitted it entirely, so its absence is normal and
/// means "unspecified", not "one".
fn scan_notches(s: &str) -> Option<i32> {
    let at = s.find(" notch")?;
    let head = s[..at].trim_end();

    // Walk back over the digits by character, never by byte: a multi-byte character
    // before the number would otherwise put the slice index inside it.
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(i, _)| i)
        .last()?;

    head[start..].parse().ok()
}

/// Leading integer and the remainder, for reading "X=1393" style fields.
fn take_int(s: &str) -> (Option<i32>, &str) {
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(s.len());
    (s[..end].parse().ok(), &s[end..])
}

/// Points a drag path may carry before it stops being convertible: an origin, a
/// destination and up to six waypoints.
///
/// `via` repeats freely in the agent's grammar, and `raw_command` arrives over the
/// wire, so a megabyte of coordinates must allocate a handful of entries rather than
/// a hundred thousand. Every other mouse action reports one pair and is read
/// directly; only a drag has a path to walk.
const MAX_DRAG_POINTS: usize = 8;

/// The longest `action_subtype` rendered verbatim into a record label.
///
/// The field is not a closed set of verbs. The server builds it from a client-supplied
/// free-text command: lowercased, modifier-stripped, first whitespace-delimited token,
/// checked against no list. An unrecognised value therefore reaches the label
/// unvalidated, and the token is bounded to one word but not to a length. It is capped
/// and character-restricted here for the same reason coordinate scanning is capped.
const MAX_SUBTYPE_LEN: usize = 32;

/// A drag's path as the agent reports it: `Dragged [from …] [via …]* to …`.
struct DragPath {
    from: Option<(i32, i32)>,
    via: Vec<(i32, i32)>,
    to: (i32, i32),
}

/// The result of reading a drag path.
///
/// `TooLong` is kept separate from `Absent` deliberately: a path that overruns the
/// bound must not be rendered from the points that fit, because a truncated path reads
/// as a complete record of a gesture nobody performed.
enum DragScan {
    Path(DragPath),
    TooLong,
    Absent,
}

/// The clause a coordinate pair belongs to, taken from the nearest keyword before it.
///
/// The keywords carry a trailing space so they cannot match inside a longer word, and
/// the text searched is only what lies between the previous pair and this one.
fn drag_clause(head: &str) -> &'static str {
    let mut best: Option<(usize, &'static str)> = None;
    for keyword in ["from ", "via ", "to "] {
        if let Some(at) = head.rfind(keyword) {
            if best.is_none_or(|(seen, _)| at >= seen) {
                best = Some((at, keyword.trim_end()));
            }
        }
    }
    best.map(|(_, keyword)| keyword).unwrap_or("")
}

/// The path in a drag's reported sentence.
///
/// Waypoints are read because a path-dependent gesture cannot be replayed without
/// them: a selection drag that traces a shape is not the straight line between its
/// endpoints. A pair with no preceding keyword is the destination, which is how the
/// pre-1.2.2 wording ("Dragged at X=…, Y=…") still converts.
fn scan_drag_path(s: &str) -> DragScan {
    let mut from = None;
    let mut via = Vec::new();
    let mut to = None;
    let mut seen = 0usize;
    let mut rest = s;

    while let Some(x_at) = rest.find("X=") {
        let head = &rest[..x_at];
        let (x, after_x) = take_int(&rest[x_at + 2..]);

        let Some(y_at) = after_x.find("Y=") else { break };
        let (y, after_y) = take_int(&after_x[y_at + 2..]);

        let (Some(x), Some(y)) = (x, y) else {
            rest = after_y;
            continue;
        };

        seen += 1;
        if seen > MAX_DRAG_POINTS {
            return DragScan::TooLong;
        }
        match drag_clause(head) {
            "from" => from = Some((x, y)),
            "via" => via.push((x, y)),
            _ => to = Some((x, y)),
        }
        rest = after_y;
    }

    match to {
        Some(to) => DragScan::Path(DragPath { from, via, to }),
        None => DragScan::Absent,
    }
}

/// An `action_subtype` safe to render as a label.
///
/// Anything outside the shape a verb can take is replaced rather than echoed: the
/// value arrives over the wire and no layer between the client and here constrains it.
fn safe_subtype(subtype: &str) -> &str {
    let plausible = subtype.len() <= MAX_SUBTYPE_LEN
        && !subtype.is_empty()
        && subtype
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

    if plausible {
        subtype
    } else {
        "unknown-action"
    }
}

// Human-readable converters
fn convert_mouse(event: &CommandEvent) -> String {
    let (symbols, subtype) = split_subtype_symbols(&event.action_subtype);
    let (mut modifiers, rest) = split_mouse_modifiers(&event.raw_command);

    // Fall back to the command form ("Executed: 770 310 #left"), which the agent
    // records whenever the pointer readback does not settle.
    if modifiers.is_empty() {
        modifiers = modifiers_from_command_form(rest, subtype, &event.os_type);
    }
    // Last resort: a prefix that leaked into the subtype itself.
    if modifiers.is_empty() && !symbols.is_empty() {
        modifiers = modifiers_from_command_form(event.action_subtype.as_str(), subtype, &event.os_type);
    }

    // A modifier held during a pointer action is part of the gesture: a Cmd-click
    // adds to a selection where a plain click replaces it. Dropping it would
    // describe an action that was never performed.
    let prefix = if modifiers.is_empty() {
        String::new()
    } else {
        format!("{}+", modifiers.join("+"))
    };

    // A drag has two endpoints and is meaningless with one — the step cannot be
    // replayed from a single coordinate. Both are read from the raw command,
    // which is the only place the agent reports the second one.
    if subtype == "drag" {
        match scan_drag_path(rest) {
            DragScan::Path(path) => {
                let mut line = format!("{prefix}Drag");
                // Absent for a `here` drag, which starts wherever the cursor already
                // was. The origin is not recoverable from the record, so it is left
                // out rather than guessed — naming the destination twice would be a
                // record of a gesture that did not happen.
                //
                // `mouse_x`/`mouse_y` is not a second source for it: the agent reads
                // the pointer back *after* the gesture, and for a coordinate drag that
                // readback is verified against the destination. Falling back to it
                // would render exactly the zero-length drag this avoids.
                if let Some((x, y)) = path.from {
                    line.push_str(&format!(" from ({x}, {y})"));
                }
                for (x, y) in &path.via {
                    line.push_str(&format!(" via ({x}, {y})"));
                }
                line.push_str(&format!(" to ({}, {})", path.to.0, path.to.1));
                return line;
            }
            // Rendering the points that fit would read as a complete record of a
            // shorter gesture, and echoing the agent's sentence would put an
            // unbounded string in the record — `raw_command` arrives over the wire.
            // So the record names the failure and carries no wire data at all.
            DragScan::TooLong => {
                return format!(
                    "{prefix}Drag path of more than {MAX_DRAG_POINTS} points, not recorded"
                );
            }
            DragScan::Absent => {}
        }
    }

    let action = match subtype {
        "left"        => "Left-click",
        // `click` is the macOS-only alias for `left`. It builds the same argv, so it
        // records as the same gesture; without this arm it fell through and named the
        // step with the bare verb.
        "click"       => "Left-click",
        "right"       => "Right-click",
        "double"      => "Double-click",
        "middle"      => "Middle-click",
        "triple"      => "Triple-click",
        "move"        => "Move-cursor",
        "scroll_up"   => "Scroll-up",
        "scroll_down" => "Scroll-down",
        "scroll_left" => "Scroll-left",
        "scroll_right"=> "Scroll-right",
        "drag"        => "Drag",
        "hold"        => "Hold",
        "release"     => "Release",
        other         => safe_subtype(other),
    };

    // An agent reporting no subtype would otherwise yield an empty label, which in
    // the record cannot be told apart from a step that was never converted. The
    // module contract is to fall back to the raw command.
    if action.is_empty() {
        return event.raw_command.clone();
    }

    // A scroll without its repeat count cannot be replayed. Agents before the count
    // fix omit it, so an absent count means unspecified rather than one.
    let count = match scan_notches(rest) {
        Some(1) => " 1 notch".to_string(),
        Some(n) => format!(" {n} notches"),
        None => String::new(),
    };

    let coords = if event.position_captured {
        format!(" at ({}, {})", event.mouse_x, event.mouse_y)
    } else {
        String::new()
    };

    // (here) is an internal CC actuation flag — not part of human-readable output.
    format!("{prefix}{action}{count}{coords}")
}

/// Convert a CC key string to a human-readable description.
///
/// CC modifier combos use AHK-style syntax:
///   ^a       → Ctrl+A
///   +{Tab}   → Shift+Tab
///   !{F4}    → Alt+F4      (Option+F4 on macOS)
///   #d       → Super+D     (Cmd+D on macOS, Win+D on Windows)
///   #+s      → Super+Shift+S
///   {Ctrl down}a{Ctrl up} → Ctrl+A
///
/// `!` and `#` name a different key per platform, so the label follows the OS that
/// reported the event — see [`modifier_name`].
///
/// Bare {KeyName} tokens are unwrapped to their plain name.
fn humanize_key(key: &str, os_type: &str) -> String {
    // Handle {Modifier down}key{Modifier up} style from some CC backends
    let expanded = if key.contains(" down}") || key.contains(" up}") {
        let mut modifiers: Vec<&str> = Vec::new();
        let mut base = String::new();
        let mut remaining = key;

        loop {
            if let Some(start) = remaining.find('{') {
                let before = &remaining[..start];
                if !before.is_empty() && !before.chars().all(|c| c == ' ') {
                    base.push_str(before);
                }
                // Search for the closing brace from the opening one. Searching from 0
                // finds a '}' that precedes the '{' when the key string is malformed
                // (an outer brace pair stripped upstream leaves "Ctrl down}a{Ctrl up"),
                // which made the slice below panic and take the watch loop — and the
                // whole capture session — down with it. This function documents itself
                // as infallible; a malformed key must degrade to a poor label, not an
                // aborted session.
                if let Some(end) = remaining[start..].find('}').map(|offset| start + offset) {
                    let token = &remaining[start + 1..end];
                    if token.ends_with(" down") {
                        let modifier = token.trim_end_matches(" down");
                        // "Win" and "Super" are the agent's own platform names for
                        // one key and pass through unchanged; collapsing them to a
                        // single label would rename the key the operator pressed.
                        modifiers.push(match modifier {
                            "Ctrl" | "Control" => "Ctrl",
                            "Shift"            => "Shift",
                            "Cmd" | "Command"  => "Cmd",
                            other              => other,
                        });
                    }
                    // Skip {Modifier up} tokens entirely
                    remaining = &remaining[end + 1..];
                } else {
                    break;
                }
            } else {
                base.push_str(remaining);
                break;
            }
        }

        let base = humanize_bare_key(base.trim());
        if modifiers.is_empty() {
            base
        } else {
            format!("{}+{}", modifiers.join("+"), base.to_uppercase())
        }
    } else {
        key.to_string()
    };

    // AHK-style modifier prefixes: ^a  !{F4}  +{Tab}  #d  #+s
    //
    // Every leading symbol is consumed, not just the first. Stripping one and
    // handing the rest to `humanize_key_token` — which does not itself understand
    // prefixes — rendered a two-modifier chord as "Super++s".
    let mut rest = expanded.as_str();
    let mut names: Vec<&'static str> = Vec::new();
    while rest.len() >= 2 {
        // A symbol standing alone is the key itself, not a prefix, so the length
        // check above stops before consuming it.
        let Some(symbol) = rest.chars().next() else { break };
        let Some(name) = modifier_name(symbol, os_type) else { break };
        if !names.contains(&name) {
            names.push(name);
        }
        rest = &rest[symbol.len_utf8()..];
    }

    if !names.is_empty() {
        return format!("{}+{}", names.join("+"), humanize_key_token(rest));
    }

    humanize_key_token(&expanded)
}

fn humanize_key_token(key: &str) -> String {
    let unwrapped = if key.starts_with('{') && key.ends_with('}') {
        &key[1..key.len() - 1]
    } else {
        key
    };
    humanize_bare_key(unwrapped)
}

/// Map OS-native bare key names to consistent plain English labels.
///
/// macOS (osascript): Return, Delete, ForwardDelete, Escape
/// Linux (xdotool):   Return, BackSpace, Delete, Escape
/// Windows (AHK v2):  Enter, Backspace, Delete, Escape
fn humanize_bare_key(key: &str) -> String {
    match key {
        "Return" | "Enter"                      => "Enter".into(),
        "Delete" | "BackSpace" | "Backspace"    => "Backspace".into(),
        "ForwardDelete" | "Del"                 => "Delete".into(),
        "Escape" | "Esc"                        => "Escape".into(),
        "Tab"                                   => "Tab".into(),
        "Space" | "space"                       => "Space".into(),
        "Up"                                    => "Up".into(),
        "Down"                                  => "Down".into(),
        "Left"                                  => "Left".into(),
        "Right"                                 => "Right".into(),
        "Home"                                  => "Home".into(),
        "End"                                   => "End".into(),
        "Page_Up"   | "Prior" | "PgUp"          => "Page Up".into(),
        "Page_Down" | "Next"  | "PgDn"          => "Page Down".into(),
        "Insert"                                => "Insert".into(),
        "CapsLock"                              => "Caps Lock".into(),
        "NumLock"                               => "Num Lock".into(),
        "ScrollLock"                            => "Scroll Lock".into(),
        "PrintScreen"                           => "Print Screen".into(),
        "Pause"                                 => "Pause".into(),
        "F1"  => "F1".into(),   "F2"  => "F2".into(),
        "F3"  => "F3".into(),   "F4"  => "F4".into(),
        "F5"  => "F5".into(),   "F6"  => "F6".into(),
        "F7"  => "F7".into(),   "F8"  => "F8".into(),
        "F9"  => "F9".into(),   "F10" => "F10".into(),
        "F11" => "F11".into(),  "F12" => "F12".into(),
        "VolumeUp"      => "Volume Up".into(),
        "VolumeDown"    => "Volume Down".into(),
        "Mute"          => "Mute".into(),
        "BrightnessUp"  => "Brightness Up".into(),
        "PlayPause"     => "Play/Pause".into(),
        "LWin" | "RWin" => "Win".into(),
        other           => other.into(),
    }
}

fn convert_keyboard(event: &CommandEvent) -> String {
    match event.action_subtype.as_str() {
        "type" => {
            // CC agent sends raw_command as "Typed: {text}" — strip the prefix.
            let text = event.raw_command
                .strip_prefix("Typed: ")
                .unwrap_or(&event.raw_command);
            format!("Type: {text}")
        }
        "press" => {
            // CC agent sends raw_command as "Pressed: {key}" — strip the prefix.
            let key = event.raw_command
                .strip_prefix("Pressed: ")
                .unwrap_or(&event.raw_command);
            format!("Press: {}", humanize_key(key, &event.os_type))
        }
        other => format!("Keyboard: {other} {}", event.raw_command),
    }
}

fn convert_position(event: &CommandEvent) -> String {
    if event.position_captured {
        format!("Position query at ({}, {})", event.mouse_x, event.mouse_y)
    } else {
        format!("Position query: {}", event.raw_command)
    }
}

// CC command generation

/// Convert a CommandEvent to the exact command string Control-Center accepts.
///
/// Used to populate cc_commands.json — the machine-executable counterpart
/// to the human-readable converted_input.md.
///
/// The CC command language is platform-agnostic. Each OS agent reports key
/// names using its native backend naming (osascript / xdotool / AHK), which
/// this function normalizes to the canonical CC {KeyName} format.
pub fn to_cc_command(event: &CommandEvent) -> String {
    match event.action_type.as_str() {
        "mouse"    => cc_mouse(event),
        "keyboard" => cc_keyboard(event),
        "position" => cc_position(event),
        _          => event.raw_command.clone(),
    }
}

fn cc_mouse(event: &CommandEvent) -> String {
    let (leaked, subtype) = split_subtype_symbols(&event.action_subtype);
    let (modifiers, rest) = split_mouse_modifiers(&event.raw_command);

    // CC takes modifiers as a symbol prefix on the action: "770 310 #left".
    let symbols: String = modifiers
        .iter()
        .filter_map(|name| {
            MOUSE_MODIFIERS
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, symbol)| *symbol)
        })
        .collect();

    // A prefix that leaked into the subtype is still a modifier the command needs.
    let symbols = if symbols.is_empty() { leaked.to_string() } else { symbols };

    // A scroll replays wrong without its repeat count.
    let count = match scan_notches(rest) {
        Some(n) => format!(" {n}"),
        None => String::new(),
    };

    match subtype {
        // Standard click/move commands: "<x> <y> <action>"
        "left" | "click" | "right" | "double" | "middle" | "triple" | "move"
        | "scroll_up" | "scroll_down" | "scroll_left" | "scroll_right"
        | "hold" | "release" => {
            if event.position_captured {
                format!(
                    "{} {} {}{}{}",
                    event.mouse_x, event.mouse_y, symbols, subtype, count
                )
            } else {
                // Without a position the agent records the command verbatim behind
                // an "Executed: " label. The label is not part of the command, and
                // leaving it in put a string CC cannot run into cc_commands.json.
                fallback_command(&event.raw_command)
            }
        }
        // Drag: CC format is "<x1> <y1> drag <x2> <y2>". Both endpoints come from the
        // raw command, which is the only place both appear.
        //
        // `mouse_x`/`mouse_y` is not a substitute for either, and must not be used
        // here: it is a single post-gesture readback whose meaning has differed by
        // platform. macOS and Linux have always reported the destination; Windows
        // reported the *origin* through 1.2.2 — a race between the async AHK watcher
        // and a readback verified against the first coordinate in the command — and
        // reports the destination from 1.3.0. Building the replay command from it
        // would yield `<dest> <dest> drag <dest>` on the platforms that report the
        // destination, which CC cannot execute as the intended gesture.
        "drag" => match scan_drag_path(rest) {
            DragScan::Path(DragPath { from: Some(from), via, to }) => {
                let mut command = format!("{} {} {}drag", from.0, from.1, symbols);
                // Waypoints switch the command to the "via … to …" form; without
                // them the two-coordinate form is what CC has always taken, and it
                // must stay byte-identical for the drags already in the corpus.
                if via.is_empty() {
                    command.push_str(&format!(" {} {}", to.0, to.1));
                } else {
                    for (x, y) in &via {
                        command.push_str(&format!(" via {x} {y}"));
                    }
                    command.push_str(&format!(" to {} {}", to.0, to.1));
                }
                command
            }
            // A path past the bound has no runnable form: the straight line between
            // its endpoints is a different gesture, and the agent's sentence is
            // unbounded. Neither belongs in a commands file.
            DragScan::TooLong => {
                format!("drag path of more than {MAX_DRAG_POINTS} points, not recorded")
            }
            // No origin is not a runnable drag either: a `here` drag starts wherever
            // the cursor was, and inventing a start point would replay a different
            // gesture. Emitting what the agent reported keeps the record honest.
            _ => fallback_command(&event.raw_command),
        },
        _ => fallback_command(&event.raw_command),
    }
}

/// The best CC command available when one cannot be rebuilt from the event.
///
/// A raw command the agent could not describe is stored verbatim behind an
/// "Executed: " label. The label is a report, not part of the command, so it is
/// stripped — what remains is the command exactly as it was issued.
fn fallback_command(raw: &str) -> String {
    raw.strip_prefix("Executed: ").unwrap_or(raw).to_string()
}

fn cc_keyboard(event: &CommandEvent) -> String {
    match event.action_subtype.as_str() {
        "type" => {
            let text = event.raw_command
                .strip_prefix("Typed: ")
                .unwrap_or(&event.raw_command);
            format!("type {text}")
        }
        "press" => {
            let key = event.raw_command
                .strip_prefix("Pressed: ")
                .unwrap_or(&event.raw_command);
            // Modifier combos (^c, +{Tab}, ⌘c etc.) are already in CC format.
            // Bare OS-native key names need normalizing to CC {KeyName} format.
            if is_modifier_combo(key) {
                format!("press {key}")
            } else {
                format!("press {}", normalize_key(key))
            }
        }
        _ => event.raw_command.clone(),
    }
}

fn cc_position(_event: &CommandEvent) -> String {
    // "position" is a standalone CC command — no arguments needed.
    // The coordinates in the event are the result of the query, not input.
    "position".to_string()
}

/// Returns true if the key string is already a modifier combo in CC format.
/// These pass through unchanged — only bare key names need normalizing.
fn is_modifier_combo(key: &str) -> bool {
    key.starts_with('^')    // Ctrl
        || key.starts_with('+') // Shift
        || key.starts_with('!') // Alt / Option
        || key.starts_with('#') // Super / Win / Cmd
        || key.starts_with('⌘') // macOS Unicode Cmd
        || key.starts_with('⌃') // macOS Unicode Ctrl
        || key.starts_with('⇧') // macOS Unicode Shift
        || key.starts_with('⌥') // macOS Unicode Option
        || key.starts_with('{') // Already in {KeyName} format
}

/// Normalize an OS-native key name to the CC {KeyName} format.
///
/// Each platform's agent backend reports key names differently:
///   macOS  (osascript) — Return, Delete, ForwardDelete, Escape, ...
///   Linux  (xdotool)   — Return, BackSpace, Delete, Escape, ...
///   Windows (AHK v2)   — Enter, Backspace, Delete, Escape, ...
///
/// All map to CC's canonical {KeyName} format, which is platform-agnostic.
fn normalize_key(key: &str) -> String {
    match key {
        // Enter
        // macOS: Return | Linux: Return | Windows: Enter
        "Return" | "Enter" => "{Enter}".into(),

        // Backspace
        // macOS: Delete | Linux: BackSpace | Windows: Backspace
        "Delete" | "BackSpace" | "Backspace" => "{Backspace}".into(),

        // Forward Delete
        // macOS: ForwardDelete | Linux: Delete | Windows: Delete
        "ForwardDelete" => "{Del}".into(),

        // Escape
        // All platforms: Escape
        "Escape" => "{Esc}".into(),

        // Tab
        // All platforms: Tab
        "Tab" => "{Tab}".into(),

        // Space
        // macOS: Space | Linux: space | Windows: Space
        "Space" | "space" => "{Space}".into(),

        // Arrow keys
        // macOS: Up/Down/Left/Right | Linux: Up/Down/Left/Right | Windows: Up/Down/Left/Right
        "Up"    => "{Up}".into(),
        "Down"  => "{Down}".into(),
        "Left"  => "{Left}".into(),
        "Right" => "{Right}".into(),

        // Home / End
        "Home" => "{Home}".into(),
        "End"  => "{End}".into(),

        // Page Up / Page Down
        // macOS: Page_Up/Page_Down | Linux: Page_Up/Page_Down | Windows: PgUp/PgDn
        "Page_Up"  | "Prior" | "PgUp" => "{PgUp}".into(),
        "Page_Down"| "Next"  | "PgDn" => "{PgDn}".into(),

        // Function keys
        // All platforms report as F1–F12
        "F1"  => "{F1}".into(),
        "F2"  => "{F2}".into(),
        "F3"  => "{F3}".into(),
        "F4"  => "{F4}".into(),
        "F5"  => "{F5}".into(),
        "F6"  => "{F6}".into(),
        "F7"  => "{F7}".into(),
        "F8"  => "{F8}".into(),
        "F9"  => "{F9}".into(),
        "F10" => "{F10}".into(),
        "F11" => "{F11}".into(),
        "F12" => "{F12}".into(),

        // Media keys
        // Supported on macOS and Windows only (per CC docs — not Linux)
        "VolumeUp"      => "{VolumeUp}".into(),
        "VolumeDown"    => "{VolumeDown}".into(),
        "Mute"          => "{Mute}".into(),
        "BrightnessUp"  => "{BrightnessUp}".into(),
        "PlayPause"     => "{PlayPause}".into(),

        // Windows-only
        "LWin" => "{LWin}".into(),
        "RWin" => "{RWin}".into(),
        other => format!("{{{other}}}"),
    }
}

// Tests
#[cfg(test)]
mod tests {
    use super::*;
    use ma_proto::control_center::CommandEvent;

    // A test fixture builder, called 27 times below with positional literals.
    // A params struct here would make every call site longer without making any
    // of them safer — the arguments are visible at each call, in one file.
    #[allow(clippy::too_many_arguments)]
    fn event(
        action_type: &str,
        action_subtype: &str,
        raw_command: &str,
        success: bool,
        is_here: bool,
        x: i32,
        y: i32,
        pos_captured: bool,
    ) -> CommandEvent {
        CommandEvent {
            action_type: action_type.to_string(),
            action_subtype: action_subtype.to_string(),
            raw_command: raw_command.to_string(),
            success,
            is_here_command: is_here,
            mouse_x: x,
            mouse_y: y,
            position_captured: pos_captured,
            ..Default::default()
        }
    }

    #[test]
    fn test_left_click_with_coords() {
        let e = event("mouse", "left", "here left", true, false, 320, 240, true);
        assert_eq!(to_human_readable(&e), "Left-click at (320, 240)");
    }

    #[test]
    fn test_left_click_here() {
        // (here) is stripped — it's a CC internal flag, not human-readable info.
        let e = event("mouse", "left", "here left", true, true, 320, 240, true);
        assert_eq!(to_human_readable(&e), "Left-click at (320, 240)");
    }

    #[test]
    fn test_right_click_no_coords() {
        let e = event("mouse", "right", "right", true, false, 0, 0, false);
        assert_eq!(to_human_readable(&e), "Right-click");
    }

    #[test]
    fn test_double_click() {
        let e = event("mouse", "double", "double", true, false, 100, 200, true);
        assert_eq!(to_human_readable(&e), "Double-click at (100, 200)");
    }

    #[test]
    fn test_keyboard_type() {
        let e = event("keyboard", "type", "Typed: hello world", true, false, 0, 0, false);
        assert_eq!(to_human_readable(&e), "Type: hello world");
    }

    // AHK-style modifier prefixes are expanded to plain English (^c → Ctrl+c).
    #[test]
    fn test_keyboard_press() {
        let e = event("keyboard", "press", "Pressed: ^c", true, false, 0, 0, false);
        assert_eq!(to_human_readable(&e), "Press: Ctrl+c");
    }

    // OS-native bare key names are normalized to one label across platforms:
    // macOS/Linux "Return" and Windows "Enter" both convert to "Enter" (see
    // humanize_bare_key), so corpus output does not fork per OS.
    #[test]
    fn test_keyboard_press_return_normalized() {
        let e = event("keyboard", "press", "Pressed: Return", true, false, 0, 0, false);
        assert_eq!(to_human_readable(&e), "Press: Enter");
    }

    #[test]
    fn test_failed_command() {
        let e = event("mouse", "left", "here left", false, false, 0, 0, false);
        assert_eq!(to_human_readable(&e), "[FAILED] Left-click");
    }

    // Waypoints, added by CC 1.3.0. A path-dependent gesture is not the straight line
    // between its endpoints, so dropping the waypoints records a different action.

    #[test]
    fn test_waypoint_drag_keeps_every_point_in_order() {
        let e = event(
            "mouse", "drag",
            "Dragged from X=500, Y=400 via X=700, Y=500 via X=800, Y=600 to X=900, Y=700",
            true, false, 500, 400, true,
        );
        assert_eq!(
            to_human_readable(&e),
            "Drag from (500, 400) via (700, 500) via (800, 600) to (900, 700)"
        );
        assert_eq!(to_cc_command(&e), "500 400 drag via 700 500 via 800 600 to 900 700");
    }

    #[test]
    fn test_modified_waypoint_drag_keeps_its_modifier() {
        let e = event(
            "mouse", "drag",
            "Cmd+Dragged from X=500, Y=400 via X=700, Y=500 to X=900, Y=700",
            true, false, 500, 400, true,
        );
        assert_eq!(
            to_human_readable(&e),
            "Cmd+Drag from (500, 400) via (700, 500) to (900, 700)"
        );
        assert_eq!(to_cc_command(&e), "500 400 #drag via 700 500 to 900 700");
    }

    #[test]
    fn test_here_drag_records_its_destination() {
        // Before CC 1.3.0 a `here` drag recorded one point and read as stationary.
        // The origin stays absent: it is wherever the cursor was, and naming the
        // destination twice would record a gesture that did not happen.
        let e = event("mouse", "drag", "Dragged to X=900, Y=700", true, false, 900, 700, true);
        assert_eq!(to_human_readable(&e), "Drag to (900, 700)");

        let via = event(
            "mouse", "drag", "Dragged via X=700, Y=500 to X=900, Y=700",
            true, false, 900, 700, true,
        );
        assert_eq!(to_human_readable(&via), "Drag via (700, 500) to (900, 700)");
    }

    #[test]
    fn test_plain_drag_is_byte_identical_after_waypoint_support() {
        // The common case must not move: every drag already in the corpus converts
        // to exactly the string it did before waypoints existed.
        let e = event(
            "mouse", "drag", "Dragged from X=305, Y=358 to X=705, Y=658",
            true, false, 305, 358, true,
        );
        assert_eq!(to_human_readable(&e), "Drag from (305, 358) to (705, 658)");
        assert_eq!(to_cc_command(&e), "305 358 drag 705 658");
    }

    // The drag wordings that actually occur in the recorded corpus, taken verbatim
    // from session metadata along with the event fields stored beside them. Two of
    // the three carry no origin in the sentence, and the record cannot recover one:
    // `mouse_x`/`mouse_y` is the post-gesture readback, which for a coordinate drag
    // is verified against the destination — the values below are the real ones from
    // `finder-move-to-folder`, where the field and the destination are equal.
    //
    // These record the destination alone rather than inventing an origin.
    //
    // The stored `converted_command` for these steps carries an origin that this
    // module never produced — "Drag from (433, 360) to (543, 248)" for the first. That
    // is not a converter artefact: the corpus's six drag steps were corrected by hand
    // in 2026-08, recovering each origin from the frames, and the reasoning per step is
    // recorded in `sft-ablation/DRAG-STEP-ORIGINS.md`. Re-running the converter over
    // those sessions would overwrite that work with the destination-only form below.
    #[test]
    fn test_corpus_drag_wordings_record_only_what_is_recoverable() {
        // 'Dragged to X=…' — mouse_x/mouse_y equals the destination.
        let e = event("mouse", "drag", "Dragged to X=543, Y=248", true, false, 543, 248, true);
        assert_eq!(to_human_readable(&e), "Drag to (543, 248)");

        // 'Held button at at X=…' — a drag the agent classified by its first event.
        let e = event(
            "mouse", "drag", "Held button at at X=877, Y=261",
            true, false, 877, 261, true,
        );
        assert_eq!(to_human_readable(&e), "Drag to (877, 261)");

        // The new wording carries its origin, so it round-trips in full. This is the
        // form 1.3.0 emits, and it is byte-identical to what the corpus already holds.
        let e = event(
            "mouse", "drag", "Dragged from X=305, Y=358 to X=705, Y=658",
            true, false, 705, 658, true,
        );
        assert_eq!(to_human_readable(&e), "Drag from (305, 358) to (705, 658)");
    }

    // Control-Center 1.3.0 changes what a Windows agent reports in `mouse_x`/`mouse_y`
    // for a drag: the destination, where 1.0.0–1.2.2 reported the origin (and, for a
    // `here` drag, the pre-drag position) while still setting `position_captured`.
    // That is a wire-observable change, so both outputs are pinned against it here.
    //
    // Both must be unaffected, because both are built from `raw_command` alone. If a
    // future change makes either consult `mouse_x`/`mouse_y`, this test fails — which
    // is the point: the same field means different things across supported agents.
    #[test]
    fn test_windows_drag_is_unaffected_by_the_reported_position() {
        let raw = "Dragged from X=500, Y=400 to X=900, Y=700";

        // 1.3.0: the field holds the destination.
        let after = event("mouse", "drag", raw, true, false, 900, 700, true);
        // 1.2.2 and earlier on Windows: the same gesture, the field holds the origin.
        let before = event("mouse", "drag", raw, true, false, 500, 400, true);

        for e in [&after, &before] {
            assert_eq!(to_human_readable(e), "Drag from (500, 400) to (900, 700)");
            assert_eq!(to_cc_command(e), "500 400 drag 900 700");
        }
    }

    // `click` is the macOS-only alias for `left`. It builds the same argv, so it must
    // record as the same gesture.

    #[test]
    fn test_click_alias_records_as_a_left_click() {
        let e = event("mouse", "click", "Left-clicked at X=770, Y=310", true, false, 770, 310, true);
        assert_eq!(to_human_readable(&e), "Left-click at (770, 310)");
        assert_eq!(to_cc_command(&e), "770 310 click");
    }

    #[test]
    fn test_modified_click_alias_keeps_its_modifier() {
        let e = event("mouse", "click", "Cmd+Left-clicked at X=770, Y=310", true, false, 770, 310, true);
        assert_eq!(to_human_readable(&e), "Cmd+Left-click at (770, 310)");
        assert_eq!(to_cc_command(&e), "770 310 #click");
    }

    // `middle` was in both maps but never exercised: it failed at execution on macOS
    // and so never reached a record. CC 1.3.0 implements it, so records will start
    // carrying it — pinned here before the first one arrives.

    #[test]
    fn test_middle_click_records_and_replays() {
        let e = event("mouse", "middle", "Middle-clicked at X=960, Y=540", true, false, 960, 540, true);
        assert_eq!(to_human_readable(&e), "Middle-click at (960, 540)");
        assert_eq!(to_cc_command(&e), "960 540 middle");

        let modified = event(
            "mouse", "middle", "Ctrl+Shift+Middle-clicked at X=960, Y=540",
            true, false, 960, 540, true,
        );
        assert_eq!(to_human_readable(&modified), "Ctrl+Shift+Middle-click at (960, 540)");
        assert_eq!(to_cc_command(&modified), "960 540 ^+middle");
    }

    // action_subtype is not a closed set of verbs: the server derives it from a
    // client-supplied free-text command and validates it against no list, so it
    // reaches the label as untrusted input.

    #[test]
    fn test_unrecognised_subtype_is_named_but_not_echoed_unbounded() {
        let long = "a".repeat(5_000);
        let e = event("mouse", &long, "Executed: 1 2 whatever", true, false, 1, 2, true);
        let human = to_human_readable(&e);
        assert_eq!(human, "unknown-action at (1, 2)");
        assert!(human.len() < 128);
    }

    #[test]
    fn test_subtype_outside_a_verbs_shape_is_replaced() {
        for subtype in ["rm -rf /", "left\nright", "<script>", ""] {
            let e = event("mouse", subtype, "Executed: 1 2 x", true, false, 1, 2, true);
            let human = to_human_readable(&e);
            assert!(
                !human.contains(subtype) || subtype.is_empty(),
                "subtype {subtype:?} reached the record verbatim: {human}"
            );
        }
    }

    #[test]
    fn test_a_plausible_unknown_subtype_still_names_itself() {
        // Bounding must not erase a verb CC adds later — only shapes a verb cannot
        // take are replaced.
        let e = event("mouse", "quadruple", "Executed: 1 2 quadruple", true, false, 1, 2, true);
        assert_eq!(to_human_readable(&e), "quadruple at (1, 2)");
    }

    #[test]
    fn test_position_query() {
        let e = event("position", "", "position", true, false, 512, 300, true);
        assert_eq!(to_human_readable(&e), "Position query at (512, 300)");
    }

    // Unknown action types pass the raw command through unchanged rather than
    // synthesizing a "{type}-{subtype}" label.
    #[test]
    fn test_unknown_action_type() {
        let e = event("scroll", "down", "scroll down", true, false, 0, 0, false);
        assert_eq!(to_human_readable(&e), "scroll down");
    }

    #[test]
    fn test_cc_mouse_right_click() {
        let e = event("mouse", "right", "Right-clicked at X=747, Y=1024", true, true, 747, 1024, true);
        assert_eq!(to_cc_command(&e), "747 1024 right");
    }

    #[test]
    fn test_cc_mouse_left_click() {
        let e = event("mouse", "left", "Left-clicked at X=960, Y=540", true, false, 960, 540, true);
        assert_eq!(to_cc_command(&e), "960 540 left");
    }

    #[test]
    fn test_cc_keyboard_type() {
        let e = event("keyboard", "type", "Typed: youtube.com", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "type youtube.com");
    }

    #[test]
    fn test_cc_press_macos_return() {
        let e = event("keyboard", "press", "Pressed: Return", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press {Enter}");
    }

    #[test]
    fn test_cc_press_linux_return() {
        let e = event("keyboard", "press", "Pressed: Return", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press {Enter}");
    }

    #[test]
    fn test_cc_press_windows_enter() {
        let e = event("keyboard", "press", "Pressed: Enter", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press {Enter}");
    }

    #[test]
    fn test_cc_press_macos_delete() {
        let e = event("keyboard", "press", "Pressed: Delete", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press {Backspace}");
    }

    #[test]
    fn test_cc_press_linux_backspace() {
        let e = event("keyboard", "press", "Pressed: BackSpace", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press {Backspace}");
    }

    #[test]
    fn test_cc_mouse_hold() {
        let e = event("mouse", "hold", "Hold at X=500, Y=300", true, false, 500, 300, true);
        assert_eq!(to_cc_command(&e), "500 300 hold");
    }

    #[test]
    fn test_cc_mouse_release() {
        let e = event("mouse", "release", "Release at X=500, Y=300", true, false, 500, 300, true);
        assert_eq!(to_cc_command(&e), "500 300 release");
    }

    // Drag — both endpoints
    //
    // mouse_x/mouse_y carry the origin only, so a drag is reconstructed from the
    // raw command. A drag with one endpoint cannot be replayed.

    #[test]
    fn test_drag_reports_both_endpoints() {
        let e = event(
            "mouse", "drag", "Dragged from X=305, Y=358 to X=705, Y=658",
            true, false, 305, 358, true,
        );
        assert_eq!(to_human_readable(&e), "Drag from (305, 358) to (705, 658)");
    }

    #[test]
    fn test_cc_drag_is_executable() {
        let e = event(
            "mouse", "drag", "Dragged from X=300, Y=200 to X=800, Y=550",
            true, false, 300, 200, true,
        );
        assert_eq!(to_cc_command(&e), "300 200 drag 800 550");
    }

    #[test]
    fn test_drag_destination_only_keeps_what_was_reported() {
        // Agent 1.2.1 reported the destination alone; the origin is unrecoverable
        // and must not be invented.
        let e = event("mouse", "drag", "Dragged to X=543, Y=248", true, false, 543, 248, true);
        assert_eq!(to_human_readable(&e), "Drag to (543, 248)");
        assert_eq!(to_cc_command(&e), "Dragged to X=543, Y=248");
    }

    #[test]
    fn test_drag_unparseable_raw_falls_back_to_position() {
        let e = event("mouse", "drag", "Executed: drag", true, false, 120, 240, true);
        assert_eq!(to_human_readable(&e), "Drag at (120, 240)");
    }

    // Modifier-held pointer actions
    //
    // A Cmd-click adds to a selection where a plain click replaces it, so a record
    // that drops the modifier describes an action that did not happen.

    #[test]
    fn test_modified_click_keeps_its_modifier() {
        let e = event("mouse", "left", "Cmd+Left-clicked at X=770, Y=310", true, false, 770, 310, true);
        assert_eq!(to_human_readable(&e), "Cmd+Left-click at (770, 310)");
        assert_eq!(to_cc_command(&e), "770 310 #left");
    }

    #[test]
    fn test_multiple_modifiers_keep_order() {
        let e = event("mouse", "left", "Cmd+Shift+Left-clicked at X=10, Y=20", true, false, 10, 20, true);
        assert_eq!(to_human_readable(&e), "Cmd+Shift+Left-click at (10, 20)");
        assert_eq!(to_cc_command(&e), "10 20 #+left");
    }

    #[test]
    fn test_modified_drag_keeps_both_endpoints_and_modifier() {
        let e = event(
            "mouse", "drag", "Option+Dragged from X=200, Y=300 to X=900, Y=640",
            true, false, 200, 300, true,
        );
        assert_eq!(to_human_readable(&e), "Option+Drag from (200, 300) to (900, 640)");
        assert_eq!(to_cc_command(&e), "200 300 !drag 900 640");
    }

    // `#` is one key under three platform names: the agent names it Cmd on macOS,
    // Win on Windows and Super on Linux (agent/src/main.rs). Before this was fixed,
    // MOUSE_MODIFIERS knew only "Cmd", so a Windows or Linux modified click failed
    // the name match and recorded as an unmodified one — a plausible record of a
    // gesture that did not happen, and the exact failure split_mouse_modifiers
    // exists to prevent. This is the regression test for that silent drop.

    #[test]
    fn test_super_click_is_not_silently_downgraded_to_a_plain_click() {
        for (os, name) in [("WINDOWS", "Win"), ("LINUX", "Super"), ("MACOS", "Cmd")] {
            let e = event_on(
                os, "left", &format!("{name}+Left-clicked at X=770, Y=310"), 770, 310, true,
            );
            assert_eq!(
                to_human_readable(&e),
                format!("{name}+Left-click at (770, 310)"),
                "{os}: modifier must survive into the record",
            );
            // The symbol round-trips regardless of which name carried it.
            assert_eq!(to_cc_command(&e), "770 310 #left", "{os}: replay must keep '#'");
        }
    }

    // `!` is the same shape of coupling as `#`, one column over: the agent names it
    // Option on macOS and Alt on Windows and Linux. Both names were already in
    // MOUSE_MODIFIERS, so this was never broken — but it was pinned only on the
    // symbol path, which is not the path that failed for `#`. Dropping either name
    // from the table would have gone unnoticed by the suite.
    #[test]
    fn test_the_option_alt_key_survives_under_either_platform_name() {
        for (os, name) in [("MACOS", "Option"), ("WINDOWS", "Alt"), ("LINUX", "Alt")] {
            let e = event_on(
                os, "left", &format!("{name}+Left-clicked at X=770, Y=310"), 770, 310, true,
            );
            assert_eq!(
                to_human_readable(&e),
                format!("{name}+Left-click at (770, 310)"),
                "{os}: modifier must survive into the record",
            );
            assert_eq!(to_cc_command(&e), "770 310 !left", "{os}: replay must keep '!'");
        }
    }

    #[test]
    fn test_command_form_names_the_super_key_per_platform() {
        for (os, name) in [("MACOS", "Cmd"), ("WINDOWS", "Win"), ("LINUX", "Super")] {
            let e = event_on(os, "left", "Executed: 1 2 #left", 0, 0, false);
            assert_eq!(to_human_readable(&e), format!("{name}+Left-click"), "os: {os}");
        }
    }

    #[test]
    fn test_keyboard_super_key_is_named_per_platform() {
        for (os, name) in [("MACOS", "Cmd"), ("WINDOWS", "Win"), ("LINUX", "Super")] {
            let e = event_on_kb(os, "press", "Pressed: #d");
            assert_eq!(to_human_readable(&e), format!("Press: {name}+d"), "os: {os}");
        }
    }

    #[test]
    fn test_keyboard_multi_symbol_chord_names_every_modifier() {
        // Only the first symbol used to be consumed, leaving the rest to a function
        // that does not parse prefixes — "#+s" rendered as "Super++s".
        let e = event_on_kb("MACOS", "press", "Pressed: #+s");
        assert_eq!(to_human_readable(&e), "Press: Cmd+Shift+s");

        let win = event_on_kb("WINDOWS", "press", "Pressed: ^+{Tab}");
        assert_eq!(to_human_readable(&win), "Press: Ctrl+Shift+Tab");
    }

    #[test]
    fn test_a_lone_modifier_symbol_is_a_key_not_a_prefix() {
        // "#" alone has nothing to modify; consuming it would leave an empty key.
        let e = event_on_kb("MACOS", "press", "Pressed: #");
        assert_eq!(to_human_readable(&e), "Press: #");
    }

    #[test]
    fn test_braced_modifier_keeps_the_platform_name_it_arrived_with() {
        let win = event_on_kb("WINDOWS", "press", "Pressed: {Win down}d{Win up}");
        assert_eq!(to_human_readable(&win), "Press: Win+D");

        let linux = event_on_kb("LINUX", "press", "Pressed: {Super down}d{Super up}");
        assert_eq!(to_human_readable(&linux), "Press: Super+D");
    }

    // The chord wordings actually present in the recorded corpus, with the OS that
    // recorded each. The agent spells these with modifier *names*, which take a
    // different code path from the symbol forms above — so the per-OS symbol naming
    // must leave every one of them byte-identical. Re-running the converter over the
    // corpus is not an option (it would overwrite the hand-recovered drag origins),
    // which makes this the only guard against silent drift in already-captured data.
    #[test]
    fn test_recorded_corpus_chords_are_unchanged() {
        for (os, raw, expected) in [
            ("MACOS",   "Pressed: Cmd+Q",       "Press: Cmd+Q"),
            ("MACOS",   "Pressed: Cmd+A",       "Press: Cmd+A"),
            ("MACOS",   "Pressed: Cmd+Shift+5", "Press: Cmd+Shift+5"),
            ("WINDOWS", "Pressed: Ctrl+C",      "Press: Ctrl+C"),
            ("WINDOWS", "Pressed: Ctrl+V",      "Press: Ctrl+V"),
        ] {
            let e = event_on_kb(os, "press", raw);
            assert_eq!(to_human_readable(&e), expected, "{os}: {raw}");
        }
    }

    #[test]
    fn test_unmodified_action_is_untouched() {
        // The '+' split must never consume part of an action name.
        let e = event("mouse", "left", "Left-clicked at X=960, Y=540", true, false, 960, 540, true);
        assert_eq!(to_human_readable(&e), "Left-click at (960, 540)");
        assert_eq!(to_cc_command(&e), "960 540 left");
    }

    #[test]
    fn test_hold_and_release_stay_themselves() {
        // A standalone hold is a hold. Only a drag is a drag.
        let h = event("mouse", "hold", "Held button at X=500, Y=300", true, false, 500, 300, true);
        let r = event("mouse", "release", "Released button at X=500, Y=300", true, false, 500, 300, true);
        assert_eq!(to_human_readable(&h), "Hold at (500, 300)");
        assert_eq!(to_human_readable(&r), "Release at (500, 300)");
    }

    #[test]
    fn test_horizontal_scroll_is_named() {
        let e = event("mouse", "scroll_left", "Scrolled left at X=40, Y=50", true, false, 40, 50, true);
        assert_eq!(to_human_readable(&e), "Scroll-left at (40, 50)");
        assert_eq!(to_cc_command(&e), "40 50 scroll_left");
    }

    // Command-form fallback
    //
    // When the pointer readback does not settle the agent records the command
    // verbatim behind "Executed: ", so the modifiers arrive as symbols on the action
    // token rather than as names. Dropping them there would lose the gesture in
    // exactly the case the rest of this module protects.

    fn event_on(
        os: &str,
        subtype: &str,
        raw: &str,
        x: i32,
        y: i32,
        pos_captured: bool,
    ) -> CommandEvent {
        CommandEvent {
            os_type: os.to_string(),
            ..event("mouse", subtype, raw, true, false, x, y, pos_captured)
        }
    }

    /// A keyboard event attributed to a specific reporting OS. `!` and `#` name a
    /// different physical key per platform, so the label depends on this field.
    fn event_on_kb(os: &str, subtype: &str, raw: &str) -> CommandEvent {
        CommandEvent {
            os_type: os.to_string(),
            ..event("keyboard", subtype, raw, true, false, 0, 0, false)
        }
    }

    #[test]
    fn test_command_form_keeps_its_modifier() {
        let e = event_on("MACOS", "left", "Executed: 770 310 #left", 0, 0, false);
        assert_eq!(to_human_readable(&e), "Cmd+Left-click");
        // The "Executed: " label is a report, not part of the command.
        assert_eq!(to_cc_command(&e), "770 310 #left");
    }

    #[test]
    fn test_command_form_drag_keeps_modifier_and_endpoints() {
        let e = event_on("MACOS", "drag", "Executed: 200 300 !drag 900 640", 0, 0, false);
        assert_eq!(to_human_readable(&e), "Option+Drag");
        assert_eq!(to_cc_command(&e), "200 300 !drag 900 640");
    }

    #[test]
    fn test_command_form_alt_is_named_per_platform() {
        let mac = event_on("MACOS", "left", "Executed: 1 2 !left", 0, 0, false);
        let win = event_on("WINDOWS", "left", "Executed: 1 2 !left", 0, 0, false);
        assert_eq!(to_human_readable(&mac), "Option+Left-click");
        assert_eq!(to_human_readable(&win), "Alt+Left-click");
    }

    #[test]
    fn test_command_form_without_modifiers_is_unchanged() {
        let e = event_on("MACOS", "left", "Executed: 770 310 left", 0, 0, false);
        assert_eq!(to_human_readable(&e), "Left-click");
        assert_eq!(to_cc_command(&e), "770 310 left");
    }

    // Scroll repeat count
    //
    // A scroll replays wrong without its count. Agents before the count fix omit it,
    // so its absence means unspecified — never one.

    #[test]
    fn test_scroll_count_is_recorded() {
        let e = event("mouse", "scroll_down", "Scrolled down 5 notches at X=770, Y=310", true, false, 770, 310, true);
        assert_eq!(to_human_readable(&e), "Scroll-down 5 notches at (770, 310)");
        assert_eq!(to_cc_command(&e), "770 310 scroll_down 5");
    }

    #[test]
    fn test_scroll_count_of_one_is_singular() {
        let e = event("mouse", "scroll_up", "Scrolled up 1 notch at X=1, Y=2", true, false, 1, 2, true);
        assert_eq!(to_human_readable(&e), "Scroll-up 1 notch at (1, 2)");
        assert_eq!(to_cc_command(&e), "1 2 scroll_up 1");
    }

    #[test]
    fn test_modified_scroll_keeps_count_and_modifier() {
        let e = event("mouse", "scroll_down", "Cmd+Scrolled down 3 notches at X=770, Y=310", true, false, 770, 310, true);
        assert_eq!(to_human_readable(&e), "Cmd+Scroll-down 3 notches at (770, 310)");
        assert_eq!(to_cc_command(&e), "770 310 #scroll_down 3");
    }

    #[test]
    fn test_scroll_without_count_names_none() {
        // Two wordings must both convert, and identically. The doubled "at" is the
        // pre-fix form and is what the existing corpus contains; the single "at" is
        // what agents record from the count fix onward. The label comes from the
        // subtype, so neither wording reaches the output.
        for raw in [
            "Scrolled down at at X=504, Y=948",
            "Scrolled down at X=504, Y=948",
        ] {
            let e = event("mouse", "scroll_down", raw, true, false, 504, 948, true);
            assert_eq!(to_human_readable(&e), "Scroll-down at (504, 948)", "raw: {raw}");
            assert_eq!(to_cc_command(&e), "504 948 scroll_down", "raw: {raw}");
        }
    }

    #[test]
    fn test_scroll_with_all_four_verbs_and_both_wordings() {
        for (subtype, label, raw) in [
            ("scroll_up",    "Scroll-up",    "Scrolled up 3 notches at X=9, Y=9"),
            ("scroll_down",  "Scroll-down",  "Scrolled down 3 notches at X=9, Y=9"),
            ("scroll_left",  "Scroll-left",  "Scrolled left 3 notches at X=9, Y=9"),
            ("scroll_right", "Scroll-right", "Scrolled right 3 notches at X=9, Y=9"),
        ] {
            let e = event("mouse", subtype, raw, true, false, 9, 9, true);
            assert_eq!(to_human_readable(&e), format!("{label} 3 notches at (9, 9)"));
            assert_eq!(to_cc_command(&e), format!("9 9 {subtype} 3"));
        }
    }

    #[test]
    fn test_non_numeric_count_is_no_count_never_one() {
        // "here scroll_down fast" records without a count. Inventing one would
        // replay a scroll the operator never performed.
        let e = event("mouse", "scroll_down", "Scrolled down at X=770, Y=310", true, false, 770, 310, true);
        assert_eq!(to_human_readable(&e), "Scroll-down at (770, 310)");
        assert_eq!(scan_notches("Scrolled down fast notches at X=1, Y=2"), None);
    }

    // A modifier prefix that leaks into action_subtype must not corrupt the record.

    #[test]
    fn test_leaked_subtype_prefix_degrades_to_a_correct_record() {
        let e = event_on("MACOS", "#left", "Executed: 770 310 #left", 0, 0, false);
        assert_eq!(to_human_readable(&e), "Cmd+Left-click");
        assert_eq!(to_cc_command(&e), "770 310 #left");
    }

    #[test]
    fn test_leaked_subtype_prefix_with_position() {
        let e = event("mouse", "#scroll_down", "Cmd+Scrolled down 3 notches at X=770, Y=310", true, false, 770, 310, true);
        assert_eq!(to_human_readable(&e), "Cmd+Scroll-down 3 notches at (770, 310)");
        assert_eq!(to_cc_command(&e), "770 310 #scroll_down 3");
    }

    #[test]
    fn test_doubled_at_wording_still_parses() {
        // hold/release/scroll carry a doubled "at" because the agent's action-name
        // table mixes two conventions. The subtype drives the label, so the wording
        // does not reach the output — but the coordinates must still be read.
        let hold = event("mouse", "hold", "Cmd+Held button at at X=770, Y=810", true, false, 770, 810, true);
        assert_eq!(to_human_readable(&hold), "Cmd+Hold at (770, 810)");

        let scroll = event("mouse", "scroll_down", "Cmd+Scrolled down at at X=770, Y=810", true, false, 770, 810, true);
        assert_eq!(to_human_readable(&scroll), "Cmd+Scroll-down at (770, 810)");
        assert_eq!(to_cc_command(&scroll), "770 810 #scroll_down");
    }

    // Stress
    //
    // to_human_readable and to_cc_command document themselves as infallible, and
    // they run inside the capture watch loop: a panic here takes the session down
    // and loses the take. raw_command arrives over the wire, so these exercise it
    // as untrusted input rather than as well-formed agent output.

    /// xorshift64*, so the corpus of inputs is random but identical on every run.
    fn rng(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    #[test]
    fn stress_malformed_raw_commands_never_panic() {
        // Fragments chosen to land on the parser's edges: truncated fields, missing
        // separators, multi-byte characters adjacent to the byte offsets the parser
        // slices at, and integers that overflow i32.
        let fragments = [
            "X=", "Y=", "X=1", "Y=2", "X=-", "-", "=", "+", ",", " ", "",
            "X=99999999999999999999", "Y=-99999999999999999999",
            "Cmd+", "Shift+", "Option+", "Alt+", "Ctrl+", "Cmd+Cmd+", "++", "+X=1",
            "Dragged from", " to ", "Dragged to", "Held button at at",
            "é", "→", "🖱", "Ｘ=1", "X＝1", "\u{0}", "\n", "\t",
            "X=1, Y=2 to X=3, Y=4", "X=1,Y=2", "X = 1, Y = 2",
        ];
        let subtypes = [
            "drag", "left", "right", "hold", "release", "move",
            "scroll_left", "scroll_up", "unknown_subtype", "",
        ];

        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..20_000 {
            let mut raw = String::new();
            let parts = (rng(&mut state) % 8) as usize;
            for _ in 0..parts {
                raw.push_str(fragments[(rng(&mut state) as usize) % fragments.len()]);
            }
            let subtype = subtypes[(rng(&mut state) as usize) % subtypes.len()];
            let x = rng(&mut state) as i32;
            let y = rng(&mut state) as i32;
            let captured = rng(&mut state).is_multiple_of(2);
            let success = rng(&mut state).is_multiple_of(2);

            let e = event("mouse", subtype, &raw, success, false, x, y, captured);
            let human = to_human_readable(&e);
            let cc = to_cc_command(&e);

            // Infallible means non-empty too: an empty label in the record would be
            // indistinguishable from a step that was never converted.
            assert!(!human.is_empty() || raw.is_empty());
            assert!(!cc.is_empty() || raw.is_empty());
            assert_eq!(success, !human.starts_with("[FAILED]"));
        }
    }

    #[test]
    fn stress_multibyte_boundaries_never_panic() {
        // Slicing by byte offset panics if an index lands inside a character. Every
        // offset used comes from find(), so this pins that invariant against
        // characters placed either side of each marker.
        for filler in ["é", "→", "🖱", "\u{200B}", "ｱ"] {
            for shape in [
                format!("{filler}X={filler}1{filler}, Y={filler}2{filler}"),
                format!("Cmd{filler}+Left-clicked at X=1, Y=2"),
                format!("Cmd+{filler}Dragged from X=1, Y=2 to X=3, Y=4"),
                format!("Dragged from X=1{filler}, Y=2 to X=3, Y=4{filler}"),
                format!("{filler}+{filler}"),
            ] {
                for subtype in ["drag", "left", "hold"] {
                    let e = event("mouse", subtype, &shape, true, false, 5, 6, true);
                    let _ = to_human_readable(&e);
                    let _ = to_cc_command(&e);
                }
            }
        }
    }

    #[test]
    fn stress_oversized_input_is_bounded() {
        // A megabyte of coordinate pairs must not build a megabyte of parsed points,
        // and must not reach the record either. Since drags carry waypoints the
        // record can no longer claim this is a two-point drag, so it names the
        // failure instead — bounded, and carrying none of the wire data.
        let raw = format!("Dragged from {}", "X=1, Y=2 to ".repeat(100_000));
        let e = event("mouse", "drag", &raw, true, false, 1, 2, true);

        let human = to_human_readable(&e);
        let cc = to_cc_command(&e);
        assert_eq!(human, "Drag path of more than 8 points, not recorded");
        assert_eq!(cc, "drag path of more than 8 points, not recorded");
        // The bound is the point: neither output may grow with the input.
        assert!(human.len() < 128 && cc.len() < 128);
    }

    #[test]
    fn stress_keyboard_modifier_symbols_cannot_run_away() {
        // The symbol loop consumes one byte per iteration and dedupes into a set of
        // at most four names, so a long run must terminate in linear time and cannot
        // grow the output with it. Asserted on the output, not just on completion:
        // a run that echoed each symbol would pass a bare "did not hang" check.
        for run in [2, 1_000, 200_000] {
            let raw = format!("Pressed: {}k", "#".repeat(run));
            let e = event_on_kb("MACOS", "press", &raw);
            let out = to_human_readable(&e);
            assert_eq!(out, "Press: Cmd+k", "run of {run}");
        }

        // Mixed symbols dedupe to one name each, in first-seen order.
        let raw = format!("Pressed: {}k", "#^+!".repeat(50_000));
        let e = event_on_kb("MACOS", "press", &raw);
        assert_eq!(to_human_readable(&e), "Press: Cmd+Ctrl+Shift+Option+k");
    }

    #[test]
    fn stress_keyboard_symbols_never_split_a_character() {
        // The loop slices by `symbol.len_utf8()`. Only ASCII symbols are modifiers,
        // so the offset is always 1 — but a multi-byte character sitting where the
        // next symbol would be must end the scan, not be sliced into.
        for filler in ["é", "→", "🖱", "\u{200B}", "ｱ", "⌘", "⌥"] {
            for shape in [
                filler.to_string(),
                format!("#{filler}"),
                format!("{filler}#k"),
                format!("#{filler}#k"),
                format!("#^+!{filler}"),
                format!("{{Ctrl down}}{filler}{{Ctrl up}}"),
                format!("#{{{filler} down}}k"),
            ] {
                for os in ["MACOS", "WINDOWS", "LINUX", ""] {
                    let e = event_on_kb(os, "press", &format!("Pressed: {shape}"));
                    let out = to_human_readable(&e);
                    assert!(out.starts_with("Press: "), "{os} {shape:?} -> {out:?}");
                }
            }
        }
    }

    #[test]
    fn stress_modifier_prefix_cannot_run_away() {
        // A long run of valid modifier names must terminate and must not be mistaken
        // for an action name.
        let raw = format!("{}Left-clicked at X=1, Y=2", "Cmd+".repeat(10_000));
        let e = event("mouse", "left", &raw, true, false, 1, 2, true);
        let cc = to_cc_command(&e);
        assert!(cc.starts_with("1 2 "));
        assert!(cc.ends_with("left"));
    }

    #[test]
    fn stress_coordinate_overflow_is_rejected_not_wrapped() {
        // An out-of-range integer must fail to parse rather than wrap into a
        // plausible-looking coordinate.
        let e = event(
            "mouse", "drag", "Dragged from X=4294967296, Y=2 to X=3, Y=4",
            true, false, 7, 8, true,
        );
        // The first pair is unreadable, so only the second is found and the record
        // degrades to the destination-only form rather than reporting a wrong origin.
        assert_eq!(to_human_readable(&e), "Drag to (3, 4)");
    }

    #[test]
    fn test_failed_drag_keeps_failed_prefix() {
        let e = event(
            "mouse", "drag", "Dragged from X=1, Y=2 to X=3, Y=4",
            false, false, 1, 2, true,
        );
        assert_eq!(to_human_readable(&e), "[FAILED] Drag from (1, 2) to (3, 4)");
    }

    #[test]
    fn test_cc_press_windows_backspace() {
        let e = event("keyboard", "press", "Pressed: Backspace", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press {Backspace}");
    }

    #[test]
    fn test_cc_press_ctrl_c_passthrough() {
        let e = event("keyboard", "press", "Pressed: ^c", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press ^c");
    }

    #[test]
    fn test_cc_press_shift_tab_passthrough() {
        let e = event("keyboard", "press", "Pressed: +{Tab}", true, false, 0, 0, false);
        assert_eq!(to_cc_command(&e), "press +{Tab}");
    }

    #[test]
    fn test_cc_position() {
        let e = event("position", "", "position", true, false, 512, 300, true);
        assert_eq!(to_cc_command(&e), "position");
    }

    // A well-formed explicit sequence still reads as the chord it is.
    #[test]
    fn test_keyboard_press_explicit_down_up() {
        let e = event(
            "keyboard", "press", "Pressed: {Ctrl down}a{Ctrl up}", true, false, 0, 0, false,
        );
        assert_eq!(to_human_readable(&e), "Press: Ctrl+A");
    }

    // Typed text is copied out from behind the "Typed: " prefix and nothing else is
    // parsed, so quotes and backslashes are carried through untouched. Control-Center
    // pins the other end of this (tests/unit/test_actuation_argv.py, QUOTED_PAYLOADS);
    // between them the corpus rule against quotes in a `type` command has no basis
    // left in either repo.
    #[test]
    fn typed_text_keeps_its_quotes_and_backslashes() {
        for payload in [
            r#"printf "Title\nBody" > note.txt"#,
            r#"osascript -e "tell app \"X\" to y""#,
            r"ends with a backslash \",
            r#"both "quoted" and trailing \"#,
        ] {
            let e = event(
                "keyboard", "type", &format!("Typed: {payload}"), true, false, 0, 0, false,
            );
            assert_eq!(to_human_readable(&e), format!("Type: {payload}"));
            assert_eq!(to_cc_command(&e), format!("type {payload}"));
        }
    }

    // The regression: a Windows agent that stripped the outer braces sent
    // "Ctrl down}a{Ctrl up", where '}' precedes '{'. Searching for the closing brace
    // from index 0 produced end < start, and the slice panicked inside the watch
    // loop, so the session recorded zero steps and the operator saw no error until
    // `done`. A malformed key is allowed to convert badly; it is not allowed to abort.
    #[test]
    fn malformed_brace_order_does_not_panic() {
        for raw in [
            "Pressed: Ctrl down}a{Ctrl up",
            "Pressed: Ctrl down}{Tab}{Ctrl up",
            "Pressed: Ctrl down}{Shift down}{Esc}{Shift up}{Ctrl up",
            "Pressed: }{ down}",
            "Pressed: } up}",
        ] {
            let e = event("keyboard", "press", raw, true, false, 0, 0, false);
            let converted = to_human_readable(&e);
            assert!(converted.starts_with("Press: "), "{raw:?} → {converted:?}");
        }
    }
}