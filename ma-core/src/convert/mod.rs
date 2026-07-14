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

// Human-readable converters
fn convert_mouse(event: &CommandEvent) -> String {
    let action = match event.action_subtype.as_str() {
        "left"       => "Left-click",
        "right"      => "Right-click",
        "double"     => "Double-click",
        "middle"     => "Middle-click",
        "triple"     => "Triple-click",
        "move"       => "Move-cursor",
        "scroll_up"  => "Scroll-up",
        "scroll_down"=> "Scroll-down",
        "drag"       => "Drag",
        "hold"       => "Hold",
        "release"    => "Release",
        other        => other,
    };

    let coords = if event.position_captured {
        format!(" at ({}, {})", event.mouse_x, event.mouse_y)
    } else {
        String::new()
    };

    // (here) is an internal CC actuation flag — not part of human-readable output.
    format!("{action}{coords}")
}

/// Convert a CC key string to a human-readable description.
///
/// CC modifier combos use AHK-style syntax:
///   ^a       → Ctrl+A
///   +{Tab}   → Shift+Tab
///   !{F4}    → Alt+F4
///   #d       → Super+D
///   {Ctrl down}a{Ctrl up} → Ctrl+A
///
/// Bare {KeyName} tokens are unwrapped to their plain name.
fn humanize_key(key: &str) -> String {
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
                if let Some(end) = remaining.find('}') {
                    let token = &remaining[start + 1..end];
                    if token.ends_with(" down") {
                        let modifier = token.trim_end_matches(" down");
                        modifiers.push(match modifier {
                            "Ctrl" | "Control" => "Ctrl",
                            "Shift"            => "Shift",
                            "Alt"              => "Alt",
                            "Super" | "Win"    => "Super",
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

    // Handle AHK-style single-char modifier prefixes: ^a !f +{Tab} #d
    let bytes = expanded.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'^' | b'+' | b'!' | b'#') {
        let modifier = match bytes[0] {
            b'^' => "Ctrl",
            b'+' => "Shift",
            b'!' => "Alt",
            b'#' => "Super",
            _    => "",
        };
        let rest = humanize_key_token(&expanded[1..]);
        return format!("{modifier}+{rest}");
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
            format!("Press: {}", humanize_key(key))
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
    match event.action_subtype.as_str() {
        // Standard click/move commands: "<x> <y> <action>"
        "left" | "right" | "double" | "middle" | "triple" | "move"
        | "scroll_up" | "scroll_down" | "hold" | "release" => {
            if event.position_captured {
                format!("{} {} {}", event.mouse_x, event.mouse_y, event.action_subtype)
            } else {
                event.raw_command.clone()
            }
        }
        // Drag: CC format is "<x1> <y1> drag <x2> <y2>"
        // position_captured gives us the destination; raw_command has the full string.
        "drag" => event.raw_command.clone(),
        _ => event.raw_command.clone(),
    }
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
}