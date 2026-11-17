//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

impl BootView {
    /// Read the boot from the store on a read-only connection of our own.
    fn read(paths: &Paths, reader: &crate::store::audience::Reader) -> Self {
        let conn = match db::open_read_only(paths) {
            Ok(conn) => conn,
            Err(err) => {
                return BootView {
                    unreadable: Some(err.to_string()),
                    ..BootView::default()
                };
            }
        };
        match Self::fill(&conn, reader) {
            Ok(view) => view,
            Err(err) => BootView {
                unreadable: Some(err.to_string()),
                ..BootView::default()
            },
        }
    }

    fn fill(
        conn: &rusqlite::Connection,
        reader: &crate::store::audience::Reader,
    ) -> crate::error::Result<Self> {
        let pen = reader.pen_name();
        let note = boot_candidates(pen)
            .into_iter()
            .find_map(|key| {
                conn.query_row(
                    "SELECT key, title, body FROM memory WHERE key = ?1",
                    rusqlite::params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .transpose()
            })
            .transpose()?;
        let tag = boot_tag(pen).unwrap_or_default();
        let self_set = match boot_tag(pen) {
            Some(tag) => tags::keys_with_seen(conn, &tag, reader)?,
            None => Vec::new(),
        };
        Ok(BootView {
            note,
            tag,
            self_set,
            projects: project::open(conn)?,
            state: Some(state_line(conn)?),
            unreadable: None,
        })
    }
}

/// One tool as the boot presents it: the MCP card with an HTTP verb and path where the MCP tool
/// name was. Every other field is the MCP field unchanged — same description, same schema, same
/// annotations — because the requirement is that changing transport changes the verbs and nothing
/// else.
pub(super) struct BootTool {
    /// The HTTP verb that reaches this tool.
    method: &'static str,
    /// The path template; `{key}` is the placeholder.
    path: &'static str,
    /// The law, verbatim from the tool.
    description: String,
    /// The argument schema, verbatim from the tool.
    input_schema: serde_json::Value,
    /// The annotation hints, as `tools/list` serializes them.
    annotations: serde_json::Value,
}

/// The tool surface, each MCP tool paired with the route that reaches it.
///
/// The cards come from the MCP server's own router and the verbs from [`ROUTES`], so the boot
/// cannot describe a toolkit the doors do not have: a tool the router carries but no route reaches
/// is dropped here, and `every_tool_has_a_route` fails the suite for it.
pub(super) fn boot_tools() -> Vec<BootTool> {
    tool_catalog()
        .into_iter()
        .filter_map(|card| {
            let route = ROUTES.iter().find(|route| route.tool == card.name)?;
            Some(BootTool {
                method: route.method,
                path: route.path,
                description: card.description,
                input_schema: card.input_schema,
                annotations: card.annotations,
            })
        })
        .collect()
}

/// How much of an index entry's title the boot keeps. The index exists so a cold client knows what it
/// HAS, not what it says — the body is one call away — and the whole document must survive being
/// fetched by a client that truncates on size (see [`BOOT_BUDGET_BYTES`]).
pub(super) const INDEX_TITLE_CHARS: usize = 40;

/// How much of a project's goal the boot keeps, for the same reason.
pub(super) const PROJECT_GOAL_CHARS: usize = 56;

/// What `/bootstrap` found when it looked. Every field is `None`/empty when the store could not be
/// read — the boot still says what it is and what it can be asked for, and names the missing facts
/// rather than omitting them, because silence reads as "nothing to report".
#[derive(Default)]
pub(super) struct BootView {
    /// The resident note: `(key, title, body)`.
    note: Option<(String, String, String)>,
    /// The self set's tag for THIS caller — `self:<pen>`; empty when no pen was proven.
    tag: String,
    /// The self set as `(key, title)`, key-ascending.
    self_set: Vec<(String, String)>,
    /// Open projects as `(key, status, goal)`.
    projects: Vec<(String, String, String)>,
    /// The one-line state, or `None` when there was nothing to read.
    state: Option<String>,
    /// Why there is no content, when there is none.
    unreadable: Option<String>,
}

/// The state line: the facts a caller needs before trusting anything else, all from the boot's own
/// read-only connection. A *view* of the same facts `/health` reports — no readiness predicate is
/// reimplemented here (readiness is `/health`'s job), and the encode queue is the metric it is there.
pub(super) fn state_line(conn: &rusqlite::Connection) -> crate::error::Result<String> {
    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    let projects = project::count(conn)?;
    let schema = db::migrate::version(conn)?;
    let latest = db::migrate::latest();
    let queue = encode::counts(conn)?;
    let status = if schema == latest {
        "ready"
    } else {
        "NOT READY"
    };
    let queue = match queue.oldest_s {
        Some(age) => format!(
            "{} pending (oldest {age}s), {} failed",
            queue.pending, queue.failed
        ),
        None => format!("{} pending, {} failed", queue.pending, queue.failed),
    };
    Ok(format!(
        "{status} — {memories} memories, {projects} projects, schema v{schema}, encode {queue} (detail: /health)"
    ))
}

/// The shared floor: the one document every caller may be given, whoever it is.
pub(super) const BOOT_FLOOR: &str = "bootstrap";

/// The keys a caller's resident note is looked for under, in order.
///
/// ★ A DOOR THAT CANNOT NAME ITS CALLER MUST NOT NAME A CALLER (2026-10-22). This endpoint used to
/// inline `stet-self` for everybody and index the tag `cline-self` — one agent's first-person
/// identity, served to all — which is how a cold Cline came to narrate itself as Stet. The address is
/// now built from the PROVEN pen: `bootstrap-<pen>` when it exists, else the shared floor. Anonymous
/// callers get the floor and no self set, which is honest rather than generous.
pub(super) fn boot_candidates(pen: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(pen) = pen {
        keys.push(format!("bootstrap-{pen}"));
    }
    keys.push(BOOT_FLOOR.to_string());
    keys
}

/// The self set's tag for a caller: `self:<pen>`. `None` for a caller who proved nothing.
pub(super) fn boot_tag(pen: Option<&str>) -> Option<String> {
    pen.map(|pen| format!("self:{pen}"))
}

/// `GET /bootstrap` — **the door, and what is behind it**: everything a cold client needs in one
/// read, in the order it needs it, small enough to arrive intact.
///
/// This is the view HTTP.md promised and the code never delivered. What it used to be was a verbatim
/// dump of `initialize` + all 14 tool cards — 14.8 KB, which a fetching client middle-truncated
/// (7 KB punched out of the tool list), and which contains no memory at all. A client that read it
/// learned the *shape of the API* and nothing about the store: not that there are 302 memories, not
/// that one of them is its own boot note, not that eight projects are open and waiting. So it did
/// not think of this endpoint as how you get memories — and it was right, because it was not.
///
/// So the boot leads with what is here and how to fetch it: the four calls that reach the store, the
/// resident note in full, an index of the self set, the work still open, and the style. The tool
/// catalogue is `?tools=true` — the one part that cannot be small, which the caller should choose to
/// spend tokens on rather than receive by default.
pub async fn bootstrap(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    caller: crate::identity::Caller,
) -> Response {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let with_tools = params.flag("tools");
    // The boot reads the store on its OWN read-only connection: no lock, so a boot never queues
    // behind a writer, and a boot during an outage still describes a store it cannot read.
    let view = tokio::task::spawn_blocking({
        let paths = state.paths.clone();
        move || BootView::read(&paths, &reader)
    })
    .await
    .unwrap_or_default();
    let tools = with_tools.then(boot_tools);
    render(
        StatusCode::OK,
        json,
        boot_json(&view, tools.as_deref()).to_string(),
        boot_text(&view, tools.as_deref()),
    )
}

/// The boot document in the store's text form: for a client to read, in the order it should read it.
///
/// The order is the point. A client arriving cold asks, in sequence: *what is this*, *what is in it*,
/// *how do I fetch it*, *what is mine*, *what is open*, *how does this person want me to write*,
/// *where is the rest*. The document answers in that order, and every line that names a memory or a
/// project is a call the reader can make.
pub(super) fn boot_text(view: &BootView, tools: Option<&[BootTool]>) -> String {
    let mut out = String::from("# ellmephant — your memory, over plain HTTP\n");
    match &view.state {
        Some(state) => out.push_str(&format!("# {state}\n")),
        // A boot with no store still says what it is and what it can be asked for; the missing facts
        // are NAMED, because silence would read as "there is nothing in here".
        None => out.push_str(&format!(
            "# STORE UNREADABLE, API ONLY — {}\n",
            view.unreadable.as_deref().unwrap_or("unknown error")
        )),
    }

    out.push_str(
        "\nHOW TO GET AT IT — this endpoint is the door, not a handshake. Four calls reach everything:\n\
         \x20 GET /stats?format=json                 the whole store in one object (counts, tags, queue, top)\n\
         \x20 GET /memory/{key}?format=json          one memory whole, by key\n\
         \x20 GET /search?q=…&format=json            hunt by meaning; &tags=todo for what I meant to do next\n\
         \x20 POST /memory                          write: a JSON body of `memory_upsert`'s args (see ?tools=true)\n\
         \x20 Every GET takes ?format=json. Reads are open. If a write 401s, it wants a bearer — ask me.\n",
    );

    out.push_str(&format!(
        "\n## 1. YOU — resident, in full (memory `{}`)\n",
        view.note
            .as_ref()
            .map_or(BOOT_FLOOR, |(key, _, _)| key.as_str())
    ));
    match &view.note {
        Some((_, _, body)) => out.push_str(&format!("{}\n", indent_block(body, "  "))),
        None => out.push_str("  (no resident note in this store)\n"),
    }

    if view.tag.is_empty() {
        out.push_str(
            "\n## 2. THE REST OF YOU — no self set: this caller proved no pen\n\
             \x20 (a proved pen gets `self:<pen>` indexed here; anonymous callers get the floor only)\n",
        );
    } else {
        out.push_str(&format!(
            "\n## 2. THE REST OF YOU — tag `{}`, {} keys; a body is GET /memory/{{key}}\n",
            view.tag,
            view.self_set.len()
        ));
    }
    for (key, title) in &view.self_set {
        out.push_str(&format!("  {key:<34} {}\n", clip(title, INDEX_TITLE_CHARS)));
    }

    out.push_str(&format!(
        "\n## 3. OPEN WORK — {} projects still running; state, goal and `preload`: GET /project/{{key}}\n",
        view.projects.len()
    ));
    for (key, status, goal) in &view.projects {
        out.push_str(&format!(
            "  {status:<7} {key:<20} {}\n",
            clip(goal, PROJECT_GOAL_CHARS)
        ));
    }

    out.push_str(
        "\n## 4. STYLE — how to work here (the full standing text rides ?tools=true)\n\
         \x20 smart-caveman: big words in, small words out. Upvote any memory that shaped your task.\n\
         \x20 A project is an activity over time; a memory JOINS one by carrying the `prj:<key>` tag and\n\
         \x20 POINTS at one with `[[prj:key]]`. Keys are plain names, and the tool descriptions are the spec.\n",
    );

    match tools {
        Some(tools) => {
            out.push_str(&format!(
                "\n## 5. TOOLS — {}; HTTP verbs in place of MCP verbs, arguments and descriptions unchanged\n",
                tools.len()
            ));
            out.push_str(&catalogue_text(tools));
        }
        None => {
            // The size is MEASURED, not stated: it is the one number a caller deciding whether to
            // spend the tokens needs, and a hardcoded size drifts the first time a tool description
            // grows. Rendering it here costs nothing (the catalogue is code, not a database read).
            let catalogue = catalogue_text(&boot_tools());
            out.push_str(&format!(
                "\n## 5. THE FULL CATALOGUE — all {} tools with every argument: GET /bootstrap?tools=true\n\
                 \x20 That document is {} KB of schemas — several times this one — and some fetching clients\n\
                 \x20 truncate a response that size, so it is opt-in. Everything above is the common case;\n\
                 \x20 that is the reference. Nothing in it is needed to fetch a memory.\n",
                boot_tools().len(),
                catalogue.len() / 1024
            ));
        }
    }
    out
}

/// The catalogue half of the boot: every tool, its verb, and its arguments. Split out so the default
/// view can measure it without including it.
pub(super) fn catalogue_text(tools: &[BootTool]) -> String {
    let mut out = String::new();
    for tool in tools {
        out.push_str(&format!("\n  {} {}\n", tool.method, tool.path));
        out.push_str(&format!("      {}\n", tool.description.replace('\n', " ")));
        if let Some(args) = args_text(&tool.input_schema) {
            out.push_str(&format!("      args: {args}\n"));
        }
    }
    out
}

/// A tool's arguments, one line, read from the tool's own schema — so the text form cannot drift
/// from the schema the MCP door hands a model.
pub(super) fn args_text(schema: &serde_json::Value) -> Option<String> {
    let props = schema.get("properties")?.as_object()?;
    if props.is_empty() {
        return None;
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let parts: Vec<String> = props
        .iter()
        .map(|(name, spec)| {
            let need = if required.contains(&name.as_str()) {
                "required"
            } else {
                "optional"
            };
            let kind = schema_type(spec, schema);
            match spec.get("description").and_then(|d| d.as_str()) {
                Some(description) if !description.is_empty() => {
                    format!("{name}: {kind} ({need}) — {description}")
                }
                _ => format!("{name}: {kind} ({need})"),
            }
        })
        .collect();
    Some(parts.join("; "))
}

/// A property's type as a reader wants it: scalars, arrays of them, and `enum` value sets.
///
/// A nullable property is reported by the type it actually carries — `k` is an integer you may
/// omit, not "any" — and a `$ref` is resolved against the tool's own `$defs`, so an enum shows the
/// values a caller may actually send.
pub(super) fn schema_type(spec: &serde_json::Value, root: &serde_json::Value) -> String {
    let resolved = deref(spec, root);
    if let Some(real) = resolved
        .get("anyOf")
        .and_then(|b| b.as_array())
        .and_then(|branches| {
            branches
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) != Some("null"))
        })
    {
        return schema_type(real, root);
    }
    if let Some(values) = resolved.get("enum").and_then(|e| e.as_array()) {
        return values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|");
    }
    // A unit-only enum is spelled `oneOf` of `const`s (`literal` | `regex`), one entry per variant.
    if let Some(branches) = resolved.get("oneOf").and_then(|o| o.as_array()) {
        let consts: Vec<String> = branches
            .iter()
            .filter_map(|branch| branch.get("const"))
            .map(ToString::to_string)
            .collect();
        if !consts.is_empty() && consts.len() == branches.len() {
            return consts.join("|");
        }
        if let Some(first) = branches.first() {
            return schema_type(first, root);
        }
    }
    match non_null_type(resolved).as_deref() {
        Some("array") => {
            let inner = resolved
                .get("items")
                .map_or_else(|| "any".to_string(), |item| schema_type(item, root));
            // `"a"|"b"[]` would read as if only the last value were the array.
            if inner.contains('|') {
                format!("({inner})[]")
            } else {
                format!("{inner}[]")
            }
        }
        Some(other) => other.to_string(),
        None => "any".to_string(),
    }
}

/// Follow a `$ref` one hop against the schema's own `$defs`. A ref that does not resolve is left
/// alone rather than guessed at.
pub(super) fn deref<'a>(
    spec: &'a serde_json::Value,
    root: &'a serde_json::Value,
) -> &'a serde_json::Value {
    let name = spec
        .get("$ref")
        .and_then(|r| r.as_str())
        .and_then(|r| r.strip_prefix("#/$defs/"));
    name.and_then(|name| root.get("$defs").and_then(|defs| defs.get(name)))
        .unwrap_or(spec)
}

/// The type a property actually carries: a `type` may be a name, or an array of them when the
/// property is nullable (`["integer", "null"]` is an optional integer).
pub(super) fn non_null_type(spec: &serde_json::Value) -> Option<String> {
    match spec.get("type")? {
        serde_json::Value::String(name) => (name != "null").then(|| name.clone()),
        serde_json::Value::Array(names) => names
            .iter()
            .filter_map(|n| n.as_str())
            .find(|n| *n != "null")
            .map(ToString::to_string),
        _ => None,
    }
}

/// Prefix every line of `text` with `pad`, so a block reads as one section.
pub(super) fn indent_block(text: &str, pad: &str) -> String {
    text.lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cut `text` to at most `max` characters on a word boundary, marking the cut — an index entry says
/// what a memory is ABOUT, and a hard cut mid-word reads as corruption where a marked cut reads as a
/// limit. Every index line in the boot goes through this.
pub(super) fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head
        .rfind(' ')
        .map(|i| head[..i].to_string())
        .unwrap_or(head);
    format!("{cut} …")
}

/// The boot document as `?format=json` — the MCP handshake's own shape.
///
/// The fields are `initialize`'s (`serverInfo`, `capabilities`, `instructions`) plus `tools/list`'s
/// (`tools`), and each tool carries `method`/`path` **where MCP carried `name`**. Everything else on
/// a card — `title`, `description`, `inputSchema`, `annotations` — is the MCP field untouched.
///
/// `protocolVersion` is the one field an MCP boot has that this does not: it names a *transport*
/// version, and over HTTP there is no MCP protocol to negotiate. Left out rather than faked.
/// The boot document as `?format=json` — the same facts, shaped for a machine.
///
/// `serverInfo`/`capabilities`/`instructions` stay in MCP's own names so a client moving over from
/// MCP can read what it already knows how to read; `tools` appears only with `?tools=true`, and the
/// content sections carry the same cuts as the text form so the two cannot diverge in size or shape.
pub(super) fn boot_json(view: &BootView, tools: Option<&[BootTool]>) -> serde_json::Value {
    let mut doc = serde_json::json!({
        "serverInfo": { "name": SERVER_NAME, "version": crate::VERSION },
        "capabilities": { "tools": {} },
        "instructions": instructions(),
        "howToUse": {
            "what": "This is a memory store reachable over plain HTTP. It is the door, not a handshake.",
            "calls": [
                { "call": "GET /stats?format=json", "for": "the whole store in one object (counts, tags, encode queue, top tags/useful)" },
                { "call": "GET /memory/{key}?format=json", "for": "one memory whole, by key" },
                { "call": "GET /search?q=…&format=json", "for": "hunt by meaning; &tags=todo for what I meant to do next" },
                { "call": "GET /search?format=json&tags=cline-self", "for": "every key of the resident self set" },
                { "call": "GET /projects?format=json", "for": "all projects; ?status=active to narrow" },
                { "call": "POST /memory", "for": "write: a JSON body of `memory_upsert`'s args" },
                { "call": "GET /bootstrap?tools=true", "for": "the full tool catalogue with every argument (≈15KB)" }
            ],
            "notes": [
                "Every GET takes ?format=json; reads are open.",
                "A write may 401: that means it wants a bearer token.",
                "A write returns before its vector exists (the store embeds it a moment later)."
            ]
        },
        "state": view.state,
        "unreadable": view.unreadable,
        "youResident": view.note.as_ref().map(|(key, title, body)| serde_json::json!({
            "key": key, "title": title, "body": body,
        })),
        "selfSet": {
            "tag": view.tag,
            "keys": view.self_set.iter().map(|(key, title)| serde_json::json!({
                "key": key, "title": clip(title, INDEX_TITLE_CHARS),
            })).collect::<Vec<_>>(),
        },
        "openProjects": view.projects.iter().map(|(key, status, goal)| serde_json::json!({
            "key": key, "status": status, "goal": clip(goal, PROJECT_GOAL_CHARS),
            "call": format!("GET /project/{key}?hydrate=full"),
        })).collect::<Vec<_>>(),
    });
    if let Some(tools) = tools {
        let tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "method": tool.method,
                    "path": tool.path,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                    "annotations": tool.annotations,
                })
            })
            .collect();
        doc["tools"] = serde_json::Value::Array(tools);
    }
    doc
}
