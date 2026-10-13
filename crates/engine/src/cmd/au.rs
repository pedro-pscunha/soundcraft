//! Audio Units: discovery (`engine.au_plugins`), so agents and the CLI can find the ids the mixer
//! commands take, as they can for VST3 and CLAP. Hosting itself lives in `soundcraft-au-host`
//! (macOS only; the list is empty elsewhere); id lookup for the mixer commands is shared with CLAP
//! in `cmd::clap::plugin_info`.

use super::*;
use crate::cmd;
use serde_json::json;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "engine.au_plugins",
        "List Audio Units",
        [],
        None,
        "{rescan?: bool} → [{id: \"au:<type>:<subtype>:<manufacturer>\", name, vendor, version, component_type, subtype, manufacturer, category, is_instrument}] (macOS; empty elsewhere)",
        always,
        |_, p| {
            let list = if p.get("rescan").and_then(Value::as_bool).unwrap_or(false) {
                soundcraft_au_host::rescan()
            } else {
                soundcraft_au_host::scan()
            };
            Ok(json!(list))
        }
    )]
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn au_plugins_query_returns_a_list() {
        let mut e = Engine::default();
        let v = e.execute("engine.au_plugins", &json!({})).unwrap();
        assert!(v.is_array());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn au_plugins_lists_the_system_delay_with_an_id_the_mixer_takes() {
        const DELAY: &str = "au:aufx:dely:appl";
        let mut e = Engine::default();
        let v = e.execute("engine.au_plugins", &json!({"rescan": true})).unwrap();
        let Some(d) = v.as_array().and_then(|l| l.iter().find(|d| d["id"] == DELAY)) else {
            eprintln!("{DELAY} not installed; skipping");
            return;
        };
        assert_eq!(d["is_instrument"], false);
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        e.execute("mix.insert", &json!({"track": t, "plugin": DELAY})).unwrap();
    }
}
