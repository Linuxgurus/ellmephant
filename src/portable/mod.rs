//! Portability: JSONL export/import (P8).
//!
//! Canonical fields `{key,scope,type,title,body,source,tags,created,updated}` — no
//! ids, no vectors (both local/derived). Import is upsert-by-key and re-embeds only
//! when the row hash changes, so a second import is a no-op.

pub mod export;
pub mod import;
