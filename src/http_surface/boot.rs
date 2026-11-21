use super::*;

impl BootView {
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

pub(super) struct BootTool {
    method: &'static str,
    path: &'static str,
    description: String,
    input_schema: serde_json::Value,
    annotations: serde_json::Value,
}

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

pub(super) const INDEX_TITLE_CHARS: usize = 40;

pub(super) const PROJECT_GOAL_CHARS: usize = 56;

#[derive(Default)]
pub(super) struct BootView {
    note: Option<(String, String, String)>,
    tag: String,
    self_set: Vec<(String, String)>,
    projects: Vec<(String, String, String)>,
    state: Option<String>,
    unreadable: Option<String>,
}

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

pub(super) const BOOT_FLOOR: &str = "bootstrap";

pub(super) fn boot_candidates(pen: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(pen) = pen {
        keys.push(format!("bootstrap-{pen}"));
    }
    keys.push(BOOT_FLOOR.to_string());
    keys
}

pub(super) fn boot_tag(pen: Option<&str>) -> Option<String> {
    pen.map(|pen| format!("self:{pen}"))
}

pub async fn bootstrap(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    caller: crate::identity::Caller,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let with_tools = params.flag("tools");
    let view = tokio::task::spawn_blocking({
        let paths = state.paths.clone();
        move || BootView::read(&paths, &reader)
    })
    .await
    .unwrap_or_default();
    let tools = with_tools.then(boot_tools);
    present(
        StatusCode::OK,
        json,
        &report(
            (view, tools),
            |(view, tools)| boot_json(view, tools.as_deref()).to_string(),
            |(view, tools)| boot_text(view, tools.as_deref()),
        ),
    )
}

pub(super) fn boot_text(view: &BootView, tools: Option<&[BootTool]>) -> String {
    let mut out = String::from("# ellmephant — your memory, over plain HTTP\n");
    match &view.state {
        Some(state) => out.push_str(&format!("# {state}\n")),
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

pub(super) fn indent_block(text: &str, pad: &str) -> String {
    text.lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

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
