//! Hook wiring driven by a manifest's `[hooks.wiring]` table.
//!
//! The eight vendors under `resources/hooks/` are wired from compiled-in
//! templates through `crate::hookset`. Every other vendor is wired from
//! here: its manifest names the shape of its hook file, where that file
//! lives, and which of the five lifecycle edges it fires under what name.
//! This module renders and merges the file from those three facts, so
//! adding a terminal agent is one TOML manifest and no Rust names it.
//!
//! The rules `hookset::wire_vendor` set for vendor homes hold here
//! unchanged: the operator's entries are merged around rather than
//! replaced, a run that would change nothing writes nothing, the original
//! is copied aside before the first edit, uninstall removes only the
//! entries Cyclops wrote, and a vendor whose directory is absent is
//! skipped. Each shape has one writer and one remover, and the tests hold
//! the three of them to that contract: wire twice is wire once, and
//! unwire after wire gives the operator's bytes back.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use cyclops_manifest::{HookWiring, Manifest, WiringEvents, WiringShape};
use serde_json::{json, Value};
use toml_edit::{ArrayOfTables, DocumentMut, Item};

use crate::consumer::{shared_agents_skill, skill_location, AssetLocation, ReceiptRequirement};
use crate::hookset::{cyclops_bin, write_atomic, UnwiredVendor, WiredVendor, WiringState};

/// Timeout the JSON shapes give a hook, in seconds. `cyclops hook` posts to
/// a local socket and returns; ten seconds is what the vendors' own
/// documentation puts on a command hook.
const JSON_TIMEOUT_SECS: u64 = 10;
/// Hermes documents `timeout: 5` on its examples.
const YAML_TIMEOUT_SECS: u64 = 5;
/// The comment lines that fence Cyclops's entries in a YAML file. YAML has
/// no merge without a parser, so the block is what the remover recognizes.
const YAML_BEGIN: &str = "# cyclops:begin";
const YAML_END: &str = "# cyclops:end";

/// The one command every shape registers: the hook receiver, told which
/// vendor event it is reporting. The daemon matches that name against the
/// pane's manifest, so it is the vendor's spelling, never Cyclops's key.
pub(crate) fn hook_command(cyclops_bin: &str, event: &str) -> String {
    format!("{cyclops_bin} hook {event}")
}

/// Is this one command hook exactly a Cyclops hook command?
///
/// Exactly the two rendered forms are ours: `<bin> hook <Event>` and
/// `<bin> hook <Event> --agent <label>`, where the event and the label are
/// plain words and `<bin>` is either one of the bin paths our own rendering
/// uses right now (`own_bins`, which covers a test binary or any install
/// prefix) or a path whose basename is exactly `cyclops` (an older install
/// at another prefix). Any other trailing token, shell operator, wrapper, or
/// suffix makes the command the operator's, and it must survive a merge:
/// `cyclops hook Stop && echo mine` is not ours.
pub(crate) fn is_cyclops_hook_command(command: &str, own_bins: &[&str]) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let (bin, event, label) = match tokens.as_slice() {
        [bin, "hook", event] => (*bin, *event, None),
        [bin, "hook", event, "--agent", label] => (*bin, *event, Some(*label)),
        _ => return false,
    };
    let plain_word = |word: &str| {
        !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    if !plain_word(event) || label.is_some_and(|label| !plain_word(label)) {
        return false;
    }
    let basename = bin.rsplit('/').next().unwrap_or(bin);
    own_bins.contains(&bin) || basename == "cyclops"
}

/// The key each JSON shape puts the shell command under. Copilot alone
/// calls it `bash`; every other documented shape says `command`.
fn command_key(shape: WiringShape) -> &'static str {
    match shape {
        WiringShape::Copilot => "bash",
        _ => "command",
    }
}

// ---------------------------------------------------------------------------
// Rendering: one function per shape, from the events a manifest declares.
// ---------------------------------------------------------------------------

/// The JSON document a JSON shape registers, or None for a text shape.
pub(crate) fn render_json(
    shape: WiringShape,
    events: &WiringEvents,
    cyclops_bin: &str,
) -> Option<Value> {
    let declared = events.declared();
    let command = |event: &str| hook_command(cyclops_bin, event);
    let mut hooks = serde_json::Map::new();
    let document = match shape {
        WiringShape::ClaudeSettings | WiringShape::ClaudeHooksFile => {
            for (_, event) in &declared {
                hooks.insert(
                    event.to_string(),
                    json!([{
                        "matcher": "",
                        "hooks": [{
                            "type": "command",
                            "command": command(event),
                            "timeout": JSON_TIMEOUT_SECS,
                        }],
                    }]),
                );
            }
            json!({ "hooks": hooks })
        }
        WiringShape::Copilot => {
            for (_, event) in &declared {
                hooks.insert(
                    event.to_string(),
                    json!([{
                        "type": "command",
                        "bash": command(event),
                        "timeoutSec": JSON_TIMEOUT_SECS,
                    }]),
                );
            }
            json!({ "version": 1, "hooks": hooks })
        }
        WiringShape::Autohand => {
            let entries: Vec<Value> = declared
                .iter()
                .map(|(_, event)| {
                    json!({
                        "event": event,
                        "command": command(event),
                        "description": "Cyclops",
                        "enabled": true,
                    })
                })
                .collect();
            json!({ "hooks": { "hooks": entries } })
        }
        WiringShape::KiroAgent => {
            for (_, event) in &declared {
                hooks.insert(event.to_string(), json!([{ "command": command(event) }]));
            }
            json!({ "hooks": hooks })
        }
        WiringShape::Tabnine => {
            for (_, event) in &declared {
                hooks.insert(
                    event.to_string(),
                    json!([{
                        "hooks": [{
                            "type": "command",
                            "command": command(event),
                            "name": "cyclops",
                        }],
                    }]),
                );
            }
            json!({ "hooks": hooks })
        }
        WiringShape::Openhands => {
            for (_, event) in &declared {
                hooks.insert(
                    event.to_string(),
                    json!([{ "command": command(event), "timeout": JSON_TIMEOUT_SECS }]),
                );
            }
            json!({ "hooks": hooks })
        }
        WiringShape::HermesYaml | WiringShape::VibeToml => return None,
    };
    Some(document)
}

/// A YAML double-quoted scalar. The command carries a binary path, and a
/// path may hold a quote or a backslash.
fn yaml_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The entries under `hooks:`, each event a sequence of one command hook,
/// at the given indentation.
fn yaml_entries(events: &WiringEvents, cyclops_bin: &str, indent: &str) -> String {
    let mut out = String::new();
    for (_, event) in events.declared() {
        out.push_str(&format!(
            "{indent}{event}:\n{indent}  - command: {}\n{indent}    timeout: {YAML_TIMEOUT_SECS}\n",
            yaml_quote(&hook_command(cyclops_bin, event))
        ));
    }
    out
}

/// One `[[hooks]]` table in Mistral Vibe's form. The name is what an
/// operator sees in the vendor's hook list; the type is the vendor event.
fn toml_hook_table(event: &str, cyclops_bin: &str) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.insert("name", toml_edit::value(format!("cyclops-{event}")));
    table.insert("type", toml_edit::value(event));
    table.insert(
        "command",
        toml_edit::value(hook_command(cyclops_bin, event)),
    );
    table
}

/// The file a shape would write from nothing: what `render_json` gives,
/// pretty-printed, or the YAML or TOML text for the two text shapes. At
/// runtime [`wire_text`] on an empty file produces the same bytes; this is
/// the form the tests hold each shape to.
#[cfg(test)]
pub(crate) fn render(wiring: &HookWiring, cyclops_bin: &str) -> String {
    match wiring.shape {
        WiringShape::HermesYaml => {
            format!(
                "hooks:\n{}",
                yaml_entries(&wiring.events, cyclops_bin, "  ")
            )
        }
        WiringShape::VibeToml => {
            let mut doc = DocumentMut::new();
            let mut hooks = ArrayOfTables::new();
            for (_, event) in wiring.events.declared() {
                hooks.push(toml_hook_table(event, cyclops_bin));
            }
            doc.insert("hooks", Item::ArrayOfTables(hooks));
            doc.to_string()
        }
        shape => pretty(&render_json(shape, &wiring.events, cyclops_bin).expect("a JSON shape")),
    }
}

fn pretty(document: &Value) -> String {
    let mut text = serde_json::to_string_pretty(document).expect("a JSON value serializes");
    text.push('\n');
    text
}

// ---------------------------------------------------------------------------
// JSON merge: replace only this project's own entries, keep everything else.
// ---------------------------------------------------------------------------

/// The bin paths our own rendering (`src`) uses for its hook commands.
fn own_hook_bins<'a>(src: &'a [Value], key: &str) -> Vec<&'a str> {
    let mut bins = Vec::new();
    for entry in src {
        let direct = std::iter::once(entry);
        let nested = entry
            .get("hooks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for bin in direct
            .chain(nested)
            .filter_map(|hook| hook.get(key).and_then(Value::as_str))
            .filter_map(|command| command.split_whitespace().next())
        {
            if !bins.contains(&bin) {
                bins.push(bin);
            }
        }
    }
    bins
}

/// The outer object of a hook group with its `hooks` removed: the matcher
/// and whatever metadata the author put around the commands.
fn group_shell(entry: &Value) -> Value {
    let mut shell = entry.clone();
    if let Some(object) = shell.as_object_mut() {
        object.remove("hooks");
    }
    shell
}

/// The group shells our own rendering (`src`) uses right now.
fn own_group_shells(src: &[Value]) -> Vec<Value> {
    src.iter()
        .filter(|entry| entry.get("hooks").and_then(Value::as_array).is_some())
        .map(group_shell)
        .collect()
}

/// Strip only this project's own command hooks out of one event entry.
///
/// Lifecycle and direct entries are command objects. Tool hooks use a
/// group with a nested `hooks` array. An unrelated direct object is cloned
/// whole; a mixed group keeps its matcher and every operator-owned sibling.
/// Only exact Cyclops command objects are ever removed: an object none of
/// whose hooks are ours (an empty array included) is untouched, and a group
/// whose hooks were all ours goes whole only when its outer object is
/// exactly a shell we render; an operator's outer object (its matcher, its
/// metadata) survives with the Cyclops commands removed, even emptied.
fn without_cyclops_hooks(
    entry: &Value,
    key: &str,
    own_bins: &[&str],
    own_shells: &[Value],
) -> Option<Value> {
    let ours = |hook: &Value| {
        hook.get(key)
            .and_then(Value::as_str)
            .is_some_and(|command| is_cyclops_hook_command(command, own_bins))
    };
    if let Some(hooks) = entry.get("hooks").and_then(Value::as_array) {
        let kept: Vec<Value> = hooks.iter().filter(|hook| !ours(hook)).cloned().collect();
        if kept.len() == hooks.len() {
            return Some(entry.clone());
        }
        if kept.is_empty() && own_shells.contains(&group_shell(entry)) {
            return None;
        }
        let mut stripped = entry.clone();
        stripped["hooks"] = Value::Array(kept);
        return Some(stripped);
    }
    (!ours(entry)).then(|| entry.clone())
}

/// Merge `src` into `dst`, replacing only this project's own entries.
///
/// Objects recurse so an unrelated sibling key is never visited. Arrays are
/// the case that matters: a vendor's event list holds the operator's
/// handlers next to ours, so ours are filtered out and re-appended while
/// theirs keep their order. That is what makes a second run a no-op instead
/// of a file that grows a duplicate handler every update. `key` is the
/// field the shape keeps the shell command under.
pub(crate) fn merge_json(dst: &mut Value, src: &Value, key: &str) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, sv) in s {
                merge_json(d.entry(k.clone()).or_insert(Value::Null), sv, key);
            }
        }
        (d @ Value::Array(_), Value::Array(s)) => {
            let own_bins = own_hook_bins(s, key);
            let own_shells = own_group_shells(s);
            let kept: Vec<Value> = d
                .as_array()
                .map(|groups| {
                    groups
                        .iter()
                        .filter_map(|entry| {
                            without_cyclops_hooks(entry, key, &own_bins, &own_shells)
                        })
                        .collect()
                })
                .unwrap_or_default();
            *d = Value::Array(kept.into_iter().chain(s.iter().cloned()).collect());
        }
        (d, s) => *d = s.clone(),
    }
}

/// What one removal did to one node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Removal {
    Unchanged,
    Changed,
    /// Changed, and nothing of the node is left.
    Emptied,
}

fn remove_json_at(dst: &mut Value, src: &Value, key: &str, prune: bool) -> Removal {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            let mut changed = false;
            let mut emptied = Vec::new();
            for (k, sv) in s {
                if let Some(dv) = d.get_mut(k) {
                    match remove_json_at(dv, sv, key, prune) {
                        Removal::Unchanged => {}
                        Removal::Changed => changed = true,
                        Removal::Emptied => {
                            changed = true;
                            emptied.push(k.clone());
                        }
                    }
                }
            }
            if prune {
                for k in emptied {
                    d.remove(&k);
                }
            }
            if !changed {
                Removal::Unchanged
            } else if prune && d.is_empty() {
                Removal::Emptied
            } else {
                Removal::Changed
            }
        }
        (destination @ Value::Array(_), Value::Array(source)) => {
            let own_bins = own_hook_bins(source, key);
            let own_shells = own_group_shells(source);
            let original = destination.as_array().expect("array matched above");
            let kept: Vec<Value> = original
                .iter()
                .filter_map(|entry| without_cyclops_hooks(entry, key, &own_bins, &own_shells))
                .collect();
            if kept.len() == original.len() && kept == *original {
                Removal::Unchanged
            } else {
                let emptied = kept.is_empty();
                *destination = Value::Array(kept);
                if emptied {
                    Removal::Emptied
                } else {
                    Removal::Changed
                }
            }
        }
        _ => Removal::Unchanged,
    }
}

/// Remove this project's entries from a vendor document without changing
/// any sibling configuration. The inverse of [`merge_json`] for the arrays
/// Cyclops owns; scalar configuration is never reset, and an event list
/// emptied by the removal stays as an empty list.
pub(crate) fn remove_json(dst: &mut Value, src: &Value, key: &str) -> bool {
    remove_json_at(dst, src, key, false) != Removal::Unchanged
}

/// [`remove_json`], also dropping a key whose list or object the removal
/// emptied. A list that held only Cyclops's entries was Cyclops's list, and
/// leaving `"Stop": []` behind is not giving the operator's file back.
fn remove_json_pruned(dst: &mut Value, src: &Value, key: &str) -> bool {
    remove_json_at(dst, src, key, true) != Removal::Unchanged
}

// ---------------------------------------------------------------------------
// Text-level wire and unwire, one pair per shape family. Pure: given the
// file's current text, they say what the file should become.
// ---------------------------------------------------------------------------

/// What a wire or unwire wants done with the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rewrite {
    /// The file already says this; write nothing.
    Unchanged,
    /// Replace the file with this text.
    Write(String),
    /// Nothing but Cyclops's entries was in it; the file goes.
    Remove,
}

fn parse_json(existing: &str) -> Result<Value, String> {
    if existing.trim().is_empty() {
        return Ok(json!({}));
    }
    let document: Value =
        serde_json::from_str(existing).map_err(|error| format!("not valid JSON ({error})"))?;
    if !document.is_object() {
        return Err("top level is not a JSON object".into());
    }
    Ok(document)
}

fn wire_json(
    shape: WiringShape,
    events: &WiringEvents,
    existing: &str,
    cyclops_bin: &str,
) -> Result<Rewrite, String> {
    let ours = render_json(shape, events, cyclops_bin).expect("a JSON shape");
    let mut document = parse_json(existing)?;
    let before = document.clone();
    merge_json(&mut document, &ours, command_key(shape));
    Ok(if document == before {
        Rewrite::Unchanged
    } else {
        Rewrite::Write(pretty(&document))
    })
}

fn unwire_json(
    shape: WiringShape,
    events: &WiringEvents,
    existing: &str,
    cyclops_bin: &str,
) -> Result<Rewrite, String> {
    if existing.trim().is_empty() {
        return Ok(Rewrite::Unchanged);
    }
    let ours = render_json(shape, events, cyclops_bin).expect("a JSON shape");
    let mut document = parse_json(existing)?;
    if !remove_json_pruned(&mut document, &ours, command_key(shape)) {
        return Ok(Rewrite::Unchanged);
    }
    // What our rendering carries besides hooks (Copilot's `version`). A
    // file left holding only that, or nothing, was created by the wire and
    // goes with it.
    let mut shell = ours.clone();
    if let Some(object) = shell.as_object_mut() {
        object.remove("hooks");
    }
    Ok(if document == shell || document == json!({}) {
        Rewrite::Remove
    } else {
        Rewrite::Write(pretty(&document))
    })
}

/// `text` with Cyclops's marker block cut out, and whether one was there.
fn yaml_without_block(text: &str) -> Result<(String, bool), String> {
    let mut out = String::new();
    let mut inside = false;
    let mut found = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if !inside && trimmed == YAML_BEGIN {
            inside = true;
            found = true;
            continue;
        }
        if inside {
            if trimmed == YAML_END {
                inside = false;
            }
            continue;
        }
        out.push_str(line);
    }
    if inside {
        return Err(format!(
            "a `{YAML_BEGIN}` line with no `{YAML_END}` after it; merge by hand"
        ));
    }
    Ok((out, found))
}

/// The line index of a bare column-0 `hooks:` key, if the file has one. A
/// `hooks:` that holds an inline value cannot take entries under it, and
/// an event already configured there would become a duplicate key, so both
/// refuse rather than guess.
fn yaml_hooks_line(text: &str, events: &WiringEvents) -> Result<Option<usize>, String> {
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("hooks:") else {
            continue;
        };
        let rest = rest.trim();
        if !rest.is_empty() && !rest.starts_with('#') {
            return Err("`hooks:` holds an inline value; merge by hand".into());
        }
        for below in lines[index + 1..]
            .iter()
            .take_while(|below| below.trim().is_empty() || below.starts_with([' ', '\t']))
        {
            let below = below.trim();
            for (_, event) in events.declared() {
                if below == format!("{event}:") || below.starts_with(&format!("{event}: ")) {
                    return Err(format!(
                        "hooks.{event} is already configured; merge by hand"
                    ));
                }
            }
        }
        return Ok(Some(index));
    }
    Ok(None)
}

/// `base` (a file with no Cyclops block) with the block put in: under an
/// existing `hooks:` key, or appended with a `hooks:` key of its own.
fn yaml_with_block(base: &str, events: &WiringEvents, cyclops_bin: &str) -> Result<String, String> {
    let mut out = String::new();
    match yaml_hooks_line(base, events)? {
        Some(index) => {
            for (i, line) in base.split_inclusive('\n').enumerate() {
                out.push_str(line);
                if i == index {
                    if !line.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&format!("  {YAML_BEGIN}\n"));
                    out.push_str(&yaml_entries(events, cyclops_bin, "  "));
                    out.push_str(&format!("  {YAML_END}\n"));
                }
            }
        }
        None => {
            out.push_str(base);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("{YAML_BEGIN}\nhooks:\n"));
            out.push_str(&yaml_entries(events, cyclops_bin, "  "));
            out.push_str(&format!("{YAML_END}\n"));
        }
    }
    Ok(out)
}

fn wire_yaml(events: &WiringEvents, existing: &str, cyclops_bin: &str) -> Result<Rewrite, String> {
    let (base, _) = yaml_without_block(existing)?;
    let wired = yaml_with_block(&base, events, cyclops_bin)?;
    Ok(if wired == existing {
        Rewrite::Unchanged
    } else {
        Rewrite::Write(wired)
    })
}

fn unwire_yaml(existing: &str) -> Result<Rewrite, String> {
    let (base, found) = yaml_without_block(existing)?;
    Ok(if !found {
        Rewrite::Unchanged
    } else if base.trim().is_empty() {
        Rewrite::Remove
    } else {
        Rewrite::Write(base)
    })
}

fn is_cyclops_toml_table(table: &toml_edit::Table, own_bins: &[&str]) -> bool {
    table
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(|command| is_cyclops_hook_command(command, own_bins))
}

fn parse_toml(existing: &str) -> Result<DocumentMut, String> {
    existing
        .parse::<DocumentMut>()
        .map_err(|error| format!("not valid TOML ({error})"))
}

fn wire_toml(events: &WiringEvents, existing: &str, cyclops_bin: &str) -> Result<Rewrite, String> {
    let mut doc = parse_toml(existing)?;
    let own_bins = [cyclops_bin];
    let ours: Vec<String> = events
        .declared()
        .iter()
        .map(|(_, event)| hook_command(cyclops_bin, event))
        .collect();
    let present: Vec<String> = doc
        .get("hooks")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(ArrayOfTables::iter)
        .filter(|table| is_cyclops_toml_table(table, &own_bins))
        .filter_map(|table| table.get("command").and_then(Item::as_str))
        .map(String::from)
        .collect();
    if present == ours {
        return Ok(Rewrite::Unchanged);
    }
    if !doc.contains_key("hooks") {
        doc.insert("hooks", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let hooks = doc
        .get_mut("hooks")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| "`hooks` is not an array of tables; merge by hand".to_string())?;
    hooks.retain(|table| !is_cyclops_toml_table(table, &own_bins));
    for (_, event) in events.declared() {
        hooks.push(toml_hook_table(event, cyclops_bin));
    }
    Ok(Rewrite::Write(doc.to_string()))
}

fn unwire_toml(existing: &str, cyclops_bin: &str) -> Result<Rewrite, String> {
    let mut doc = parse_toml(existing)?;
    let Some(hooks) = doc.get_mut("hooks").and_then(Item::as_array_of_tables_mut) else {
        return Ok(Rewrite::Unchanged);
    };
    let before = hooks.len();
    hooks.retain(|table| !is_cyclops_toml_table(table, &[cyclops_bin]));
    if hooks.len() == before {
        return Ok(Rewrite::Unchanged);
    }
    if hooks.is_empty() {
        doc.remove("hooks");
    }
    let text = doc.to_string();
    Ok(if text.trim().is_empty() {
        Rewrite::Remove
    } else {
        Rewrite::Write(text)
    })
}

/// What the file at `wiring.file` should become to carry Cyclops's hooks,
/// given what it says now (`""` for a file that does not exist).
pub(crate) fn wire_text(
    wiring: &HookWiring,
    existing: &str,
    cyclops_bin: &str,
) -> Result<Rewrite, String> {
    match wiring.shape {
        WiringShape::HermesYaml => wire_yaml(&wiring.events, existing, cyclops_bin),
        WiringShape::VibeToml => wire_toml(&wiring.events, existing, cyclops_bin),
        shape => wire_json(shape, &wiring.events, existing, cyclops_bin),
    }
}

/// What the file should become with Cyclops's hooks taken back out.
pub(crate) fn unwire_text(
    wiring: &HookWiring,
    existing: &str,
    cyclops_bin: &str,
) -> Result<Rewrite, String> {
    match wiring.shape {
        WiringShape::HermesYaml => unwire_yaml(existing),
        WiringShape::VibeToml => unwire_toml(existing, cyclops_bin),
        shape => unwire_json(shape, &wiring.events, existing, cyclops_bin),
    }
}

// ---------------------------------------------------------------------------
// The catalog: every shipped manifest with a wiring table, as a consumer.
// ---------------------------------------------------------------------------

/// What the manifest cannot say about a vendor: where its skills go, and
/// the directory that proves it is installed when that is not the first
/// directory under the hook file. Rows come from the skills.sh catalog and
/// the vendor's own documentation. A manifest with no row still becomes a
/// consumer: its install directory is derived from `file`, and its skill
/// goes to the shared `~/.agents/skills` copy.
struct VendorFacts {
    id: &'static str,
    /// Home-relative; None derives it from the wiring file.
    install_dir: Option<&'static str>,
    /// Home-relative skills directory; None is the shared copy.
    skills_dir: Option<&'static str>,
}

const VENDOR_FACTS: &[VendorFacts] = &[
    VendorFacts {
        id: "adal",
        install_dir: None,
        skills_dir: Some(".adal/skills"),
    },
    VendorFacts {
        id: "auggie",
        install_dir: None,
        skills_dir: Some(".augment/skills"),
    },
    VendorFacts {
        id: "autohand",
        install_dir: None,
        skills_dir: Some(".autohand/skills"),
    },
    VendorFacts {
        id: "bob",
        install_dir: None,
        skills_dir: Some(".bob/skills"),
    },
    VendorFacts {
        id: "codebuddy",
        install_dir: None,
        skills_dir: Some(".codebuddy/skills"),
    },
    VendorFacts {
        id: "commandcode",
        install_dir: None,
        skills_dir: Some(".commandcode/skills"),
    },
    VendorFacts {
        id: "continue",
        install_dir: None,
        skills_dir: Some(".continue/skills"),
    },
    VendorFacts {
        id: "copilot",
        install_dir: None,
        skills_dir: Some(".copilot/skills"),
    },
    VendorFacts {
        id: "cortex",
        install_dir: Some(".snowflake/cortex"),
        skills_dir: Some(".snowflake/cortex/skills"),
    },
    VendorFacts {
        id: "dcode",
        install_dir: None,
        skills_dir: Some(".deepagents/agent/skills"),
    },
    VendorFacts {
        id: "devin",
        install_dir: None,
        skills_dir: Some(".config/devin/skills"),
    },
    VendorFacts {
        id: "droid",
        install_dir: None,
        skills_dir: Some(".factory/skills"),
    },
    VendorFacts {
        id: "grok",
        install_dir: None,
        skills_dir: Some(".grok/skills"),
    },
    VendorFacts {
        id: "hermes",
        install_dir: None,
        skills_dir: Some(".hermes/skills"),
    },
    VendorFacts {
        id: "iflow",
        install_dir: None,
        skills_dir: Some(".iflow/skills"),
    },
    VendorFacts {
        id: "junie",
        install_dir: None,
        skills_dir: Some(".junie/skills"),
    },
    VendorFacts {
        id: "kiro",
        install_dir: None,
        skills_dir: Some(".kiro/skills"),
    },
    VendorFacts {
        id: "pa",
        install_dir: Some(".posit/assistant"),
        skills_dir: Some(".posit/assistant/skills"),
    },
    VendorFacts {
        id: "qoder",
        install_dir: None,
        skills_dir: Some(".qoder/skills"),
    },
    VendorFacts {
        id: "qodercn",
        install_dir: None,
        skills_dir: Some(".qoder-cn/skills"),
    },
    VendorFacts {
        id: "tabnine",
        install_dir: Some(".tabnine/agent"),
        skills_dir: Some(".tabnine/agent/skills"),
    },
    VendorFacts {
        id: "traecli",
        install_dir: None,
        skills_dir: Some(".trae-cn/skills"),
    },
    VendorFacts {
        id: "vibe",
        install_dir: None,
        skills_dir: Some(".vibe/skills"),
    },
];

/// The directory that proves the vendor is installed, read off a home hook
/// file: its first directory under the home, or the vendor's directory
/// under `.config` (which every XDG vendor shares and proves nothing by
/// itself). None for a file directly under the home or a project file.
fn derived_install_dir(wiring: &HookWiring) -> Option<String> {
    let rest = wiring.home_relative()?;
    let mut parts = Path::new(rest).components().filter_map(|c| match c {
        Component::Normal(name) => name.to_str(),
        _ => None,
    });
    let first = parts.next()?;
    let second = parts.next()?;
    Some(if first == ".config" {
        parts.next()?;
        format!("{first}/{second}")
    } else {
        first.to_string()
    })
}

/// One vendor wired from its manifest. Names are `'static` because the
/// setup report holds them that way; the catalog is built once per process.
#[derive(Debug, Clone)]
pub(crate) struct Consumer {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) wiring: &'static HookWiring,
    install_dir: &'static str,
    skills_dir: Option<&'static str>,
}

/// Canonical paths for one catalog consumer on this machine. The hook is
/// None for a project-scoped wiring: there is no one file to inspect.
pub(crate) struct CatalogLocations {
    pub(crate) install_root: PathBuf,
    pub(crate) hook: Option<AssetLocation>,
    pub(crate) skill: AssetLocation,
}

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_string().into_boxed_str())
}

impl Consumer {
    /// The consumer a manifest describes, or None for a manifest with no
    /// wiring table or one of the template-wired vendors, which keep their
    /// `CliKind` path.
    pub(crate) fn from_manifest(manifest: &Manifest) -> Option<Consumer> {
        if crate::hookset::CliKind::from_name(&manifest.agent.id).is_some() {
            return None;
        }
        let wiring = manifest.hooks.wiring.as_ref()?;
        let facts = VENDOR_FACTS
            .iter()
            .find(|facts| facts.id == manifest.agent.id);
        let install_dir = facts
            .and_then(|facts| facts.install_dir)
            .map(str::to_string)
            .or_else(|| derived_install_dir(wiring))?;
        Some(Consumer {
            id: leak(&manifest.agent.id),
            name: leak(&manifest.agent.display_name),
            wiring: Box::leak(Box::new(wiring.clone())),
            install_dir: leak(&install_dir),
            skills_dir: facts.and_then(|facts| facts.skills_dir),
        })
    }

    /// The receipt this vendor can earn: exact when its prompt edge is
    /// wired and its payload names the prompt field, screen otherwise.
    pub(crate) fn receipt(&self) -> ReceiptRequirement {
        if self.wiring.events.prompt_submit.is_some() && self.wiring.payload_prompt_field.is_some()
        {
            ReceiptRequirement::ExactHook
        } else {
            ReceiptRequirement::Screen
        }
    }

    pub(crate) fn locations(&self, home: &Path) -> CatalogLocations {
        let install_root = home.join(self.install_dir);
        let hook = self.wiring.home_relative().map(|rest| {
            match Path::new(rest).strip_prefix(self.install_dir) {
                Ok(relative) => AssetLocation {
                    root: install_root.clone(),
                    relative: relative.to_path_buf(),
                },
                Err(_) => AssetLocation {
                    root: home.to_path_buf(),
                    relative: PathBuf::from(rest),
                },
            }
        });
        let skill = match self.skills_dir {
            None => shared_agents_skill(home),
            Some(dir) => skill_location(home, self.install_dir, dir),
        };
        CatalogLocations {
            install_root,
            hook,
            skill,
        }
    }
}

/// Every shipped manifest with a wiring table, by id. Built once; the
/// manifests are compiled in, so this is a parse and not a read.
pub(crate) fn catalog() -> &'static [Consumer] {
    static CATALOG: OnceLock<Vec<Consumer>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut out: Vec<Consumer> = crate::manifests::shipped()
            .filter_map(|(name, body)| Manifest::parse(body, Path::new(name)).ok())
            .filter_map(|manifest| Consumer::from_manifest(&manifest))
            .collect();
        out.sort_by(|a, b| a.id.cmp(b.id));
        out
    })
}

// ---------------------------------------------------------------------------
// The filesystem: the same acts hookset performs for the template vendors.
// ---------------------------------------------------------------------------

fn read_existing(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("can't read {}: {error}", path.display())),
    }
}

/// Copy aside before the first edit, and only then. A backup rewritten on
/// every run would eventually hold this project's own output and stop
/// being the thing the operator wanted back.
fn back_up(path: &Path, existing: &str) -> Result<Option<PathBuf>, String> {
    if existing.is_empty() {
        return Ok(None);
    }
    let backup = PathBuf::from(format!("{}.before-cyclops", path.display()));
    if !backup.exists() {
        std::fs::copy(path, &backup).map_err(|error| {
            format!(
                "can't back up {} to {}: {error}",
                path.display(),
                backup.display()
            )
        })?;
    }
    Ok(Some(backup))
}

/// Put Cyclops's entries in the file this consumer reads on its own.
///
/// Ok(None) means the vendor is not installed on this machine, or wires a
/// project file setup has no project for. Neither is a failure.
pub(crate) fn wire(consumer: &Consumer) -> Result<Option<WiredVendor>, String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(None);
    };
    wire_in(consumer, &home, &cyclops_bin())
}

pub(crate) fn wire_in(
    consumer: &Consumer,
    home: &Path,
    cyclops_bin: &str,
) -> Result<Option<WiredVendor>, String> {
    let locations = consumer.locations(home);
    let Some(hook) = locations.hook else {
        return Ok(None);
    };
    if !locations.install_root.is_dir() {
        return Ok(None);
    }
    let path = hook.path();
    let existing = read_existing(&path)?.unwrap_or_default();
    let text = match wire_text(consumer.wiring, &existing, cyclops_bin)
        .map_err(|why| format!("{}: {why}; left alone", path.display()))?
    {
        Rewrite::Write(text) => text,
        Rewrite::Unchanged | Rewrite::Remove => {
            return Ok(Some(WiredVendor {
                vendor: consumer.id,
                path,
                unchanged: true,
                backup: None,
            }));
        }
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("can't create {}: {error}", dir.display()))?;
    }
    let backup = back_up(&path, &existing)?;
    write_atomic(&path, &text)
        .map_err(|error| format!("can't write {}: {error}", path.display()))?;
    Ok(Some(WiredVendor {
        vendor: consumer.id,
        path,
        unchanged: false,
        backup,
    }))
}

/// Remove only Cyclops's entries from this consumer's file during explicit
/// uninstall. A file that held nothing else goes with them; every other
/// byte stays.
pub(crate) fn unwire(consumer: &Consumer) -> Result<Option<UnwiredVendor>, String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(None);
    };
    unwire_in(consumer, &home, &cyclops_bin())
}

pub(crate) fn unwire_in(
    consumer: &Consumer,
    home: &Path,
    cyclops_bin: &str,
) -> Result<Option<UnwiredVendor>, String> {
    let Some(hook) = consumer.locations(home).hook else {
        return Ok(None);
    };
    let path = hook.path();
    let Some(existing) = read_existing(&path)? else {
        return Ok(None);
    };
    let removed = match unwire_text(consumer.wiring, &existing, cyclops_bin)
        .map_err(|why| format!("{}: {why}; left alone", path.display()))?
    {
        Rewrite::Unchanged => false,
        Rewrite::Write(text) => {
            write_atomic(&path, &text)
                .map_err(|error| format!("can't write {}: {error}", path.display()))?;
            true
        }
        Rewrite::Remove => {
            std::fs::remove_file(&path)
                .map_err(|error| format!("can't remove {}: {error}", path.display()))?;
            true
        }
    };
    Ok(Some(UnwiredVendor {
        vendor: consumer.id,
        path,
        removed,
    }))
}

/// Evaluate hook wiring from bytes obtained by a caller-owned safe reader.
pub(crate) fn inspect_bytes(consumer: &Consumer, bytes: &[u8]) -> WiringState {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return WiringState::Unreadable;
    };
    match wire_text(consumer.wiring, text, &cyclops_bin()) {
        Ok(Rewrite::Unchanged) => WiringState::Current,
        Ok(_) => WiringState::NeedsUpdate,
        Err(_) => WiringState::Invalid,
    }
}

/// The repository's manifests, for tests that must see a vendor before
/// the coordinator registers it in `manifests.rs`.
#[cfg(test)]
pub(crate) fn repo_manifests() -> Vec<Manifest> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
    let mut all: Vec<Manifest> = cyclops_manifest::load_dir(&dir)
        .expect("every shipped manifest parses")
        .into_values()
        .collect();
    all.sort_by(|a, b| a.agent.id.cmp(&b.agent.id));
    all
}

/// [`catalog`] built from the repository rather than the binary.
#[cfg(test)]
pub(crate) fn repo_catalog() -> Vec<Consumer> {
    repo_manifests()
        .iter()
        .filter_map(Consumer::from_manifest)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_manifest::WiringEvent;

    const BIN: &str = "/opt/cyclops/bin/cyclops";

    fn two_events() -> WiringEvents {
        WiringEvents {
            prompt_submit: Some("UserPromptSubmit".into()),
            stop: Some("Stop".into()),
            ..WiringEvents::default()
        }
    }

    fn wiring(shape: WiringShape, file: &str) -> HookWiring {
        HookWiring {
            shape,
            file: file.into(),
            events: two_events(),
            payload_prompt_field: None,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn each_json_shape_renders_its_documented_form() {
        let events = two_events();
        let cmd = |event: &str| format!("{BIN} hook {event}");
        let cases = [
            (
                WiringShape::ClaudeSettings,
                json!({ "hooks": {
                    "UserPromptSubmit": [{ "matcher": "", "hooks": [{ "type": "command", "command": cmd("UserPromptSubmit"), "timeout": 10 }] }],
                    "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": cmd("Stop"), "timeout": 10 }] }],
                }}),
            ),
            (
                WiringShape::Copilot,
                json!({ "version": 1, "hooks": {
                    "UserPromptSubmit": [{ "type": "command", "bash": cmd("UserPromptSubmit"), "timeoutSec": 10 }],
                    "Stop": [{ "type": "command", "bash": cmd("Stop"), "timeoutSec": 10 }],
                }}),
            ),
            (
                WiringShape::Autohand,
                json!({ "hooks": { "hooks": [
                    { "event": "UserPromptSubmit", "command": cmd("UserPromptSubmit"), "description": "Cyclops", "enabled": true },
                    { "event": "Stop", "command": cmd("Stop"), "description": "Cyclops", "enabled": true },
                ]}}),
            ),
            (
                WiringShape::KiroAgent,
                json!({ "hooks": {
                    "UserPromptSubmit": [{ "command": cmd("UserPromptSubmit") }],
                    "Stop": [{ "command": cmd("Stop") }],
                }}),
            ),
            (
                WiringShape::Tabnine,
                json!({ "hooks": {
                    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": cmd("UserPromptSubmit"), "name": "cyclops" }] }],
                    "Stop": [{ "hooks": [{ "type": "command", "command": cmd("Stop"), "name": "cyclops" }] }],
                }}),
            ),
            (
                WiringShape::Openhands,
                json!({ "hooks": {
                    "UserPromptSubmit": [{ "command": cmd("UserPromptSubmit"), "timeout": 10 }],
                    "Stop": [{ "command": cmd("Stop"), "timeout": 10 }],
                }}),
            ),
        ];
        for (shape, expected) in cases {
            assert_eq!(
                render_json(shape, &events, BIN),
                Some(expected.clone()),
                "{}",
                shape.name()
            );
            assert_eq!(
                render_json(WiringShape::ClaudeHooksFile, &events, BIN),
                render_json(WiringShape::ClaudeSettings, &events, BIN)
            );
            let text = render(&wiring(shape, "~/.vendor/hooks.json"), BIN);
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), expected);
            assert!(text.ends_with('\n'));
        }
        assert_eq!(render_json(WiringShape::HermesYaml, &events, BIN), None);
        assert_eq!(render_json(WiringShape::VibeToml, &events, BIN), None);
    }

    #[test]
    fn the_text_shapes_render_their_documented_forms() {
        let yaml = render(
            &wiring(WiringShape::HermesYaml, "~/.hermes/config.yaml"),
            BIN,
        );
        assert_eq!(
            yaml,
            format!(
                "hooks:\n  UserPromptSubmit:\n    - command: \"{BIN} hook UserPromptSubmit\"\n      timeout: 5\n  Stop:\n    - command: \"{BIN} hook Stop\"\n      timeout: 5\n"
            )
        );
        let toml = render(&wiring(WiringShape::VibeToml, "~/.vibe/hooks.toml"), BIN);
        assert_eq!(
            toml,
            format!(
                "[[hooks]]\nname = \"cyclops-UserPromptSubmit\"\ntype = \"UserPromptSubmit\"\ncommand = \"{BIN} hook UserPromptSubmit\"\n\n[[hooks]]\nname = \"cyclops-Stop\"\ntype = \"Stop\"\ncommand = \"{BIN} hook Stop\"\n"
            )
        );
        // The rendered TOML is what the vendor reads back.
        let parsed: toml::Table = toml.parse().unwrap();
        assert_eq!(parsed["hooks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn yaml_quoting_survives_a_path_with_quotes() {
        assert_eq!(yaml_quote(r#"/p "q"\x"#), r#""/p \"q\"\\x""#);
    }

    /// One operator file per JSON shape, holding their own handler under
    /// one of our events, a handler under an event we never touch, and a
    /// scalar we never touch. Wire keeps all three; wire again is a no-op;
    /// unwire hands the original bytes back.
    #[test]
    fn json_wire_is_idempotent_and_unwire_restores_the_operators_file() {
        let shapes = [
            WiringShape::ClaudeSettings,
            WiringShape::ClaudeHooksFile,
            WiringShape::Copilot,
            WiringShape::Autohand,
            WiringShape::KiroAgent,
            WiringShape::Tabnine,
            WiringShape::Openhands,
        ];
        for shape in shapes {
            let key = command_key(shape);
            // Copilot's format requires `version`; a scalar our rendering
            // sets is never reset on uninstall, the rule hookset follows.
            let theirs = match shape {
                WiringShape::Copilot => json!({
                    "version": 1,
                    "model": "gpt",
                    "hooks": {
                        "Stop": [{ "matcher": "Bash", "hooks": [{ "type": "command", key: "/bin/their-notifier" }] }],
                        "PreToolUse": [{ "hooks": [{ "type": "command", key: "echo mine" }] }],
                    },
                }),
                WiringShape::Autohand => json!({
                    "model": "gpt",
                    "hooks": { "enabled": true, "hooks": [
                        { "event": "Stop", "command": "/bin/their-notifier", "enabled": true }
                    ]},
                }),
                _ => json!({
                    "model": "gpt",
                    "hooks": {
                        "Stop": [{ "matcher": "Bash", "hooks": [{ "type": "command", key: "/bin/their-notifier" }] }],
                        "PreToolUse": [{ "hooks": [{ "type": "command", key: "echo mine" }] }],
                    },
                }),
            };
            let original = pretty(&theirs);
            let wiring = wiring(shape, "~/.vendor/hooks.json");

            let Rewrite::Write(wired) = wire_text(&wiring, &original, BIN).unwrap() else {
                panic!("{}: first wire wrote nothing", shape.name());
            };
            let doc: Value = serde_json::from_str(&wired).unwrap();
            assert_eq!(doc["model"], "gpt", "{}", shape.name());
            assert!(
                wired.contains("/bin/their-notifier")
                    && wired.contains(&format!("{BIN} hook Stop")),
                "{}: {wired}",
                shape.name()
            );
            if shape != WiringShape::Autohand {
                assert_eq!(doc["hooks"]["PreToolUse"], theirs["hooks"]["PreToolUse"]);
                assert_eq!(doc["hooks"]["Stop"][0], theirs["hooks"]["Stop"][0]);
            }
            if shape == WiringShape::Copilot {
                assert_eq!(doc["version"], 1);
            }

            assert_eq!(
                wire_text(&wiring, &wired, BIN).unwrap(),
                Rewrite::Unchanged,
                "{}: a second wire changed the file",
                shape.name()
            );
            assert_eq!(
                unwire_text(&wiring, &wired, BIN).unwrap(),
                Rewrite::Write(original.clone()),
                "{}",
                shape.name()
            );
            assert_eq!(
                unwire_text(&wiring, &original, BIN).unwrap(),
                Rewrite::Unchanged,
                "{}: unwire of a clean file changed it",
                shape.name()
            );

            // From nothing, and back to nothing.
            let Rewrite::Write(fresh) = wire_text(&wiring, "", BIN).unwrap() else {
                panic!("{}", shape.name());
            };
            assert_eq!(
                serde_json::from_str::<Value>(&fresh).unwrap(),
                render_json(shape, &wiring.events, BIN).unwrap()
            );
            assert_eq!(unwire_text(&wiring, &fresh, BIN).unwrap(), Rewrite::Remove);
        }
    }

    #[test]
    fn a_stale_entry_from_an_older_binary_is_replaced_not_accumulated() {
        let wiring = wiring(WiringShape::Copilot, "~/.copilot/hooks/cyclops.json");
        let stale = pretty(&json!({ "version": 1, "hooks": {
            "Stop": [{ "type": "command", "bash": "/old/prefix/cyclops hook Stop", "timeoutSec": 10 }]
        }}));
        let Rewrite::Write(wired) = wire_text(&wiring, &stale, BIN).unwrap() else {
            panic!()
        };
        assert!(!wired.contains("/old/prefix"), "{wired}");
        let doc: Value = serde_json::from_str(&wired).unwrap();
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_file_that_is_not_json_is_left_alone_with_a_reason() {
        let wiring = wiring(WiringShape::ClaudeSettings, "~/.vendor/settings.json");
        let error = wire_text(&wiring, "not json", BIN).unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
        let error = wire_text(&wiring, "[1, 2]", BIN).unwrap_err();
        assert!(error.contains("not a JSON object"), "{error}");
    }

    #[test]
    fn yaml_wire_appends_a_fenced_block_and_unwire_removes_it() {
        let wiring = wiring(WiringShape::HermesYaml, "~/.hermes/config.yaml");
        let original = "model: sonnet\ntoolsets:\n  - terminal\n";
        let Rewrite::Write(wired) = wire_text(&wiring, original, BIN).unwrap() else {
            panic!()
        };
        assert_eq!(
            wired,
            format!(
                "{original}{YAML_BEGIN}\nhooks:\n  UserPromptSubmit:\n    - command: \"{BIN} hook UserPromptSubmit\"\n      timeout: 5\n  Stop:\n    - command: \"{BIN} hook Stop\"\n      timeout: 5\n{YAML_END}\n"
            )
        );
        assert_eq!(wire_text(&wiring, &wired, BIN).unwrap(), Rewrite::Unchanged);
        assert_eq!(
            unwire_text(&wiring, &wired, BIN).unwrap(),
            Rewrite::Write(original.to_string())
        );
        assert_eq!(
            unwire_text(&wiring, original, BIN).unwrap(),
            Rewrite::Unchanged
        );

        // A file with no final newline gets one before the block.
        let Rewrite::Write(wired) = wire_text(&wiring, "model: sonnet", BIN).unwrap() else {
            panic!()
        };
        assert!(wired.starts_with(&format!("model: sonnet\n{YAML_BEGIN}\n")));

        // A block from an older binary is replaced, not stacked.
        let stale = wired.replace(BIN, "/old/cyclops");
        let Rewrite::Write(rewired) = wire_text(&wiring, &stale, BIN).unwrap() else {
            panic!()
        };
        assert_eq!(rewired, wired);
        assert_eq!(rewired.matches(YAML_BEGIN).count(), 1);

        // From nothing, and back to nothing.
        let Rewrite::Write(fresh) = wire_text(&wiring, "", BIN).unwrap() else {
            panic!()
        };
        assert!(fresh.starts_with(&format!("{YAML_BEGIN}\nhooks:\n")));
        assert_eq!(unwire_text(&wiring, &fresh, BIN).unwrap(), Rewrite::Remove);
    }

    #[test]
    fn yaml_wire_goes_under_an_existing_hooks_key() {
        let wiring = wiring(WiringShape::HermesYaml, "~/.hermes/config.yaml");
        let original =
            "model: sonnet\nhooks:\n  pre_tool_call:\n    - command: \"theirs\"\nother: 1\n";
        let Rewrite::Write(wired) = wire_text(&wiring, original, BIN).unwrap() else {
            panic!()
        };
        assert_eq!(
            wired,
            format!(
                "model: sonnet\nhooks:\n  {YAML_BEGIN}\n  UserPromptSubmit:\n    - command: \"{BIN} hook UserPromptSubmit\"\n      timeout: 5\n  Stop:\n    - command: \"{BIN} hook Stop\"\n      timeout: 5\n  {YAML_END}\n  pre_tool_call:\n    - command: \"theirs\"\nother: 1\n"
            )
        );
        assert_eq!(wire_text(&wiring, &wired, BIN).unwrap(), Rewrite::Unchanged);
        assert_eq!(
            unwire_text(&wiring, &wired, BIN).unwrap(),
            Rewrite::Write(original.to_string())
        );

        let conflict = "hooks:\n  Stop:\n    - command: theirs\n";
        let error = wire_text(&wiring, conflict, BIN).unwrap_err();
        assert!(
            error.contains("hooks.Stop is already configured"),
            "{error}"
        );
        let flow = "hooks: {}\n";
        let error = wire_text(&wiring, flow, BIN).unwrap_err();
        assert!(error.contains("inline value"), "{error}");
        let unterminated = format!("{YAML_BEGIN}\nhooks:\n");
        assert!(wire_text(&wiring, &unterminated, BIN).is_err());
    }

    #[test]
    fn toml_wire_keeps_the_operators_tables_and_unwire_restores_their_bytes() {
        let wiring = wiring(WiringShape::VibeToml, "~/.vibe/hooks.toml");
        let original = "# my hooks\n\n[[hooks]]\nname = \"lint\" # keep\ntype = \"pre_tool\"\nmatch = \"bash\"\ncommand = \"lint.sh\"\n";
        let Rewrite::Write(wired) = wire_text(&wiring, original, BIN).unwrap() else {
            panic!()
        };
        assert!(wired.starts_with(original), "{wired}");
        let parsed: toml::Table = wired.parse().unwrap();
        let hooks = parsed["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 3);
        assert_eq!(hooks[0]["name"].as_str(), Some("lint"));
        assert_eq!(hooks[1]["name"].as_str(), Some("cyclops-UserPromptSubmit"));
        assert_eq!(hooks[2]["type"].as_str(), Some("Stop"));
        assert_eq!(
            hooks[2]["command"].as_str(),
            Some(format!("{BIN} hook Stop").as_str())
        );

        assert_eq!(wire_text(&wiring, &wired, BIN).unwrap(), Rewrite::Unchanged);
        assert_eq!(
            unwire_text(&wiring, &wired, BIN).unwrap(),
            Rewrite::Write(original.to_string())
        );
        assert_eq!(
            unwire_text(&wiring, original, BIN).unwrap(),
            Rewrite::Unchanged
        );

        // A stale table from an older binary is replaced, not stacked.
        let stale = wired.replace(BIN, "/old/cyclops");
        let Rewrite::Write(rewired) = wire_text(&wiring, &stale, BIN).unwrap() else {
            panic!()
        };
        assert_eq!(rewired.matches("[[hooks]]").count(), 3);
        assert!(!rewired.contains("/old/cyclops"));

        // From nothing, and back to nothing.
        let Rewrite::Write(fresh) = wire_text(&wiring, "", BIN).unwrap() else {
            panic!()
        };
        assert_eq!(fresh, render(&wiring, BIN));
        assert_eq!(unwire_text(&wiring, &fresh, BIN).unwrap(), Rewrite::Remove);

        let inline = "hooks = [{ name = \"x\" }]\n";
        assert!(wire_text(&wiring, inline, BIN).is_err());
    }

    #[test]
    fn the_install_dir_is_the_vendors_directory_under_the_home() {
        let derive = |file: &str| {
            derived_install_dir(&HookWiring {
                shape: WiringShape::ClaudeSettings,
                file: file.into(),
                events: two_events(),
                payload_prompt_field: None,
            })
        };
        assert_eq!(
            derive("~/.copilot/hooks/cyclops.json").as_deref(),
            Some(".copilot")
        );
        assert_eq!(derive("~/.adal/settings.json").as_deref(), Some(".adal"));
        assert_eq!(
            derive("~/.config/devin/config.json").as_deref(),
            Some(".config/devin")
        );
        assert_eq!(derive("~/.config/hooks.json"), None);
        assert_eq!(derive("~/hooks.json"), None);
        assert_eq!(derive(".openhands/hooks.json"), None);
    }

    /// The contract the coordinator's list edit relies on: a manifest with
    /// a wiring table needs nothing in Rust to become a consumer, and the
    /// eight template-wired vendors never do.
    #[test]
    fn every_repo_manifest_with_wiring_is_a_consumer_and_no_template_vendor_is() {
        let home = Path::new("/home/op");
        let manifests = repo_manifests();
        assert!(manifests.len() >= 12);
        let mut seen = 0;
        for manifest in &manifests {
            let consumer = Consumer::from_manifest(manifest);
            let id = manifest.agent.id.as_str();
            if crate::hookset::CliKind::from_name(id).is_some() {
                assert!(consumer.is_none(), "{id} has a CliKind and a catalog entry");
                continue;
            }
            let Some(wiring) = &manifest.hooks.wiring else {
                assert!(consumer.is_none(), "{id}");
                continue;
            };
            let consumer = consumer.unwrap_or_else(|| {
                panic!("{id} declares [hooks.wiring] but no consumer; add a VENDOR_FACTS row")
            });
            seen += 1;
            assert_eq!(consumer.id, id);
            assert_eq!(consumer.name, manifest.agent.display_name);
            let locations = consumer.locations(home);
            assert!(locations.install_root.starts_with(home), "{id}");
            assert_ne!(
                locations.install_root, home,
                "{id}: the home proves nothing"
            );
            assert!(locations.skill.path().starts_with(home), "{id}");
            assert!(locations.skill.path().ends_with("cyclops/SKILL.md"), "{id}");
            match wiring.home_relative() {
                Some(rest) => {
                    let hook = locations.hook.expect("a home file has a hook location");
                    assert_eq!(hook.path(), home.join(rest), "{id}");
                }
                None => assert!(locations.hook.is_none(), "{id}"),
            }
            // Every shape renders and round-trips from nothing.
            let Rewrite::Write(fresh) = wire_text(consumer.wiring, "", BIN).unwrap() else {
                panic!("{id}")
            };
            assert_eq!(
                wire_text(consumer.wiring, &fresh, BIN).unwrap(),
                Rewrite::Unchanged
            );
            assert_eq!(
                unwire_text(consumer.wiring, &fresh, BIN).unwrap(),
                Rewrite::Remove
            );
        }
        assert!(seen > 0, "no repo manifest declares [hooks.wiring]");
        for facts in VENDOR_FACTS {
            assert!(
                manifests.iter().any(|m| m.agent.id == facts.id),
                "VENDOR_FACTS names {}, which ships no manifest",
                facts.id
            );
        }
    }

    #[test]
    fn the_receipt_floor_reads_the_prompt_edge_and_its_payload_field() {
        let by_id = |id: &str| {
            repo_catalog()
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        assert_eq!(by_id("copilot").receipt(), ReceiptRequirement::ExactHook);
        assert_eq!(by_id("grok").receipt(), ReceiptRequirement::Screen);
        assert_eq!(by_id("vibe").receipt(), ReceiptRequirement::Screen);
        assert_eq!(
            by_id("copilot")
                .wiring
                .events
                .get(WiringEvent::PromptSubmit),
            Some("userPromptSubmitted")
        );
    }

    /// The whole filesystem act against a fake home, on the Copilot
    /// manifest from the repository: not installed writes nothing; installed
    /// writes the file and reports it; a rerun is unchanged; uninstall
    /// removes the file it created; an operator's file is backed up once
    /// and comes back on uninstall.
    #[test]
    fn wiring_a_fake_home_follows_the_vendor_home_rules() {
        let home = scratch("cyc-wiring-home");
        let copilot = repo_catalog()
            .into_iter()
            .find(|c| c.id == "copilot")
            .expect("copilot ships a wiring table");

        assert!(wire_in(&copilot, &home, BIN).unwrap().is_none());
        assert!(
            !home.join(".copilot").exists(),
            "wiring invented the vendor dir"
        );
        assert!(unwire_in(&copilot, &home, BIN).unwrap().is_none());

        std::fs::create_dir_all(home.join(".copilot")).unwrap();
        let wired = wire_in(&copilot, &home, BIN).unwrap().expect("installed");
        assert_eq!(wired.vendor, "copilot");
        assert_eq!(wired.path, home.join(".copilot/hooks/cyclops.json"));
        assert!(!wired.unchanged);
        assert_eq!(wired.backup, None);
        let text = std::fs::read_to_string(&wired.path).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            render_json(WiringShape::Copilot, &copilot.wiring.events, BIN).unwrap()
        );
        assert_eq!(
            inspect_bytes(&copilot, text.as_bytes()).word(),
            "needs_update"
        );

        let again = wire_in(&copilot, &home, BIN).unwrap().expect("installed");
        assert!(again.unchanged);
        assert_eq!(std::fs::read_to_string(&wired.path).unwrap(), text);

        let unwired = unwire_in(&copilot, &home, BIN).unwrap().expect("installed");
        assert!(unwired.removed);
        assert!(!wired.path.exists(), "a file that was only ours stays");
        assert!(home.join(".copilot/hooks").is_dir());

        let theirs = pretty(&json!({ "version": 1, "hooks": {
            "sessionEnd": [{ "type": "command", "bash": "say bye", "timeoutSec": 3 }]
        }}));
        std::fs::write(&wired.path, &theirs).unwrap();
        let wired = wire_in(&copilot, &home, BIN).unwrap().expect("installed");
        let backup = wired.backup.expect("an operator file is copied aside");
        assert_eq!(
            backup,
            home.join(".copilot/hooks/cyclops.json.before-cyclops")
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), theirs);
        let merged = std::fs::read_to_string(&wired.path).unwrap();
        assert!(merged.contains("say bye") && merged.contains("hook agentStop"));
        let unwired = unwire_in(&copilot, &home, BIN).unwrap().expect("installed");
        assert!(unwired.removed);
        assert_eq!(std::fs::read_to_string(&wired.path).unwrap(), theirs);
        assert!(!unwire_in(&copilot, &home, BIN).unwrap().unwrap().removed);

        std::fs::write(&wired.path, "{ not json").unwrap();
        let error = wire_in(&copilot, &home, BIN).unwrap_err();
        assert!(error.contains("left alone"), "{error}");
        assert_eq!(std::fs::read_to_string(&wired.path).unwrap(), "{ not json");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_text_shape_wires_its_vendor_file_in_a_fake_home() {
        let home = scratch("cyc-wiring-text-home");
        let catalog = repo_catalog();
        let hermes = catalog.iter().find(|c| c.id == "hermes").expect("hermes");
        let vibe = catalog.iter().find(|c| c.id == "vibe").expect("vibe");
        std::fs::create_dir_all(home.join(".hermes")).unwrap();
        std::fs::create_dir_all(home.join(".vibe")).unwrap();
        std::fs::write(home.join(".hermes/config.yaml"), "model: sonnet\n").unwrap();

        let wired = wire_in(hermes, &home, BIN).unwrap().expect("installed");
        let text = std::fs::read_to_string(&wired.path).unwrap();
        assert!(
            text.starts_with("model: sonnet\n# cyclops:begin\nhooks:\n"),
            "{text}"
        );
        assert!(text.contains("hook on_session_start"), "{text}");
        assert!(wire_in(hermes, &home, BIN).unwrap().unwrap().unchanged);
        assert!(unwire_in(hermes, &home, BIN).unwrap().unwrap().removed);
        assert_eq!(
            std::fs::read_to_string(&wired.path).unwrap(),
            "model: sonnet\n"
        );

        let wired = wire_in(vibe, &home, BIN).unwrap().expect("installed");
        assert_eq!(wired.path, home.join(".vibe/hooks.toml"));
        let text = std::fs::read_to_string(&wired.path).unwrap();
        assert!(text.contains("name = \"cyclops-post_agent\""), "{text}");
        assert!(wire_in(vibe, &home, BIN).unwrap().unwrap().unchanged);
        assert!(unwire_in(vibe, &home, BIN).unwrap().unwrap().removed);
        assert!(!wired.path.exists());

        let _ = std::fs::remove_dir_all(&home);
    }
}
