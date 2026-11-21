use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

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

/// What this checks: no second `const`/`static` item named `*TOOLS` under `src/` outside the
/// catalog. What it does not check: a list under another name, a function-local slice, a
/// runtime `Vec`, a `match` arm, or anything under `tests/`. The name says exactly the former.
#[test]
fn no_second_const_named_tools() {
    for (path, text) in sources(&root().join("src")) {
        if path.ends_with("mcp/catalog.rs") {
            continue;
        }
        for line in code_only(&text).lines() {
            let line = line.trim();
            let Some(item) = item_after_visibility(line) else {
                continue;
            };
            let declares = item.starts_with("const ") || item.starts_with("static ");
            assert!(
                !(declares && item.contains("TOOLS")),
                "{}: `{line}` is a second list of tool names — put the row in \
                 `mcp::catalog::TOOLS` (membership is one fact, stated once)",
                path.display()
            );
        }
    }
}

/// Strip a leading `pub`/`pub(..)` and return the rest of the item, if the line declares one.
fn item_after_visibility(line: &str) -> Option<&str> {
    let rest = match line.strip_prefix("pub") {
        Some(rest) if rest.starts_with('(') => {
            let close = rest.find(')')?;
            rest[close + 1..].trim_start()
        }
        Some(rest) => rest.trim_start(),
        None => line,
    };
    Some(rest)
}

#[test]
fn identity_is_built_in_one_place_per_door() {
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

    let mcp = std::fs::read_to_string(root().join("src/mcp/context.rs")).expect("context module");
    assert_eq!(
        code_only(&mcp).matches("from_headers(").count(),
        1,
        "the MCP door builds a `Caller` somewhere other than `caller_of`"
    );
}

#[test]
fn no_layer_but_the_wire_renders_the_wire() {
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
