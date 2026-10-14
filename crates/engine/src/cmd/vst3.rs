//! Third-party VST3 plugins: discovery (`engine.vst3_plugins`). Hosting itself lives in
//! `soundcraft-vst3-host`; id lookup for the mixer commands is shared with CLAP in
//! `cmd::clap::plugin_info`.

use super::*;
use crate::cmd;
use serde_json::json;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "engine.vst3_plugins",
        "List VST3 Plugins",
        [],
        None,
        "{rescan?: bool} → {plugins: [{id: \"vst3:<class id hex>\", class_id, name, vendor, version, sdk_version, sub_categories, category, is_instrument, path}], skipped: [{path, reason, from_cache}]}. The apps scan in helper processes and cache the result per bundle; skipped lists the bundles that hung, crashed or failed (from_cache: by an earlier scan, not tried again). rescan retries every bundle, the skipped ones too, and rewrites the cache",
        always,
        |_, p| {
            let rescan = p.get("rescan").and_then(Value::as_bool).unwrap_or(false);
            let (plugins, skipped) = soundcraft_vst3_host::scan_with_skipped(rescan);
            Ok(json!({"plugins": plugins, "skipped": skipped}))
        }
    )]
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn vst3_plugins_query_returns_the_plugins_and_the_skipped_bundles() {
        crate::no_installed_vst3();
        let mut e = Engine::default();
        let v = e.execute("engine.vst3_plugins", &json!({})).unwrap();
        assert!(v["plugins"].is_array() && v["skipped"].is_array(), "{v}");
        assert_eq!(v["plugins"], json!([]), "no installed plugin is scanned by the tests");
    }

    #[test]
    fn unknown_vst3_insert_is_an_error() {
        crate::no_installed_vst3();
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        let id = "vst3:00000000000000000000000000000000";
        assert!(e.execute("mix.insert", &json!({"track": t, "plugin": id})).is_err());
        assert!(e.execute("mix.insert", &json!({"track": t, "plugin": "vst3:not-hex"})).is_err());
        assert!(crate::cmd::clap::plugin_info(id).is_none());
    }
}
