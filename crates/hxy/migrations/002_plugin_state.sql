-- Per-plugin opaque state blobs. One row per plugin (keyed by
-- the plugin's manifest name); the host enforces a fixed quota
-- before insert so the BLOB column doesn't grow without bound.
CREATE TABLE IF NOT EXISTS plugin_state (
    plugin_name TEXT PRIMARY KEY NOT NULL,
    blob BLOB NOT NULL
);
