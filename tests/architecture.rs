//! THE BOUNDARIES, AS FAILING TESTS (soma/ellmephant#231).
//!
//! The epic's done-when is mostly prose: "a door may parse, call one verb and render, but it may not
//! decide who may write, which gate applies, or how a row is shaped". Prose does not fail a build.
//! These four checks do, and each one names the defect it caught the day it was written:
//!
//! 1. the channel gate was hand-placed in seven write verbs per door — `memory_adopt` and
//!    `channel_prune` still carried their own copy, and agreed only by luck;
//! 2. identity was built in twenty places per door, so a seventh handler could copy the ladder;
//! 3. `store::` knew how a row is *presented* (`*_to_wire_json` lived beside the rows);
//! 4. the doors were 5,000-line files, and nothing stopped them growing back.
//!
//! A source scan is unusual, and it is the right tool here: these are properties of the SHAPE of the
//! crate, not of any runtime behaviour, and the compiler cannot express "only `ops::` may call this".

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Every `.rs` file under a directory, recursively.
fn sources(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                out.push((path, text));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Drop whole-line comments, so a sentence ABOUT a rule does not count as the rule.
fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("//") || trimmed.starts_with("///"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn only_ops_places_the_channel_gate() {
    for dir in ["src/http_surface", "src/mcp/tools"] {
        for (path, text) in sources(&root().join(dir)) {
            let code = code_only(&text);
            assert!(
                !code.contains("channel_gate("),
                "{} calls `channel_gate` directly — the gate belongs in `ops::`, once, or the next \
                 verb copies whichever door its author read (the divergence #231 was filed over)",
                path.display()
            );
        }
    }
}

#[test]
fn identity_is_built_in_one_place_per_door() {
    // The REST door: exactly one mention in code, the `FromRequestParts` impl.
    let mut http = 0;
    for (path, text) in sources(&root().join("src/http_surface")) {
        http += code_only(&text).matches("from_headers(").count();
        assert!(
            path.exists(),
            "unreachable: keeps the loop honest about what it scanned"
        );
    }
    assert_eq!(
        http, 1,
        "the HTTP door builds a `Caller` in {http} places; it must be one (the `Server`-style \
         extractor), or a new handler can invent a different ladder"
    );

    // The MCP door: one helper (`caller_of`), which every tool goes through.
    let mcp = std::fs::read_to_string(root().join("src/mcp/context.rs")).expect("context module");
    assert_eq!(
        code_only(&mcp).matches("from_headers(").count(),
        1,
        "the MCP door builds a `Caller` somewhere other than `caller_of`"
    );
}

#[test]
fn no_layer_but_the_wire_renders_the_wire() {
    // `store::` and `search::` answer questions; `wire::` lays the answer out (soma/ellmephant#236).
    // `search::to_wire_json` was the hold-out: it leaned on `Fields::row`, which was an argument for
    // moving `Fields`, not for leaving a `cols` table inside the search layer.
    for dir in ["src/store", "src/search"] {
        for (path, text) in sources(&root().join(dir)) {
            let code = code_only(&text);
            assert!(
                !code.contains("fn to_wire_json")
                    && !code.contains("fn dangling_to_wire_json")
                    && !code.contains("fn backlinks_to_wire_json")
                    && !code.contains("fn plan_to_wire_json")
                    && !code.contains("fn list_to_wire_json")
                    && !code.contains("fn view_to_wire_json"),
                "{} defines a wire renderer — `store::`/`search::` know rows, `wire::` knows the \
                 wire (#236)",
                path.display()
            );
        }
    }
}

/// One outcome, one body, two renderings (soma/ellmephant#236). A handler that calls `render`
/// directly has quietly gone back to deciding its JSON and text separately, which is the drift the
/// Presenter exists to make impossible. `render` is therefore called in exactly one place.
#[test]
fn handlers_present_rather_than_render() {
    let mut calls = Vec::new();
    for (path, text) in sources(&root().join("src/http_surface")) {
        for line in code_only(&text).lines() {
            if line.contains("render(") && !line.contains("fn render(") {
                calls.push(path.display().to_string());
            }
        }
    }
    assert_eq!(
        calls,
        vec![
            root()
                .join("src/http_surface/present.rs")
                .display()
                .to_string()
        ],
        "`render` is called outside the Presenter; route the verb through `present(...)` instead"
    );
}

/// The epic's line goal, as a ceiling rather than a wish: 5,000-line doors grew back once already,
/// and the next feature will reach for the nearest file.
#[test]
fn the_doors_stay_small() {
    for (name, cap) in [("src/mcp/mod.rs", 1000), ("src/http_surface/mod.rs", 1000)] {
        let path = root().join(name);
        let lines = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .lines()
            .count();
        assert!(
            lines <= cap,
            "{name} is {lines} lines (cap {cap}) — the split is not a licence to regrow the file",
        );
    }
}
