//! The committed Worker config must parse, since a deploy that ships a config the code
//! refuses answers `503` on every route until the next deploy

use std::path::Path;

use cloudlease_machines::{
    config::MachinesConfig,
    ids::{AppName, ClassName},
};
use serde_json::Value;

/// Drops `//` and `/* */` comments outside strings, the only JSONC the file uses besides JSON
fn strip_comments(jsonc: &str) -> String {
    let mut out = String::with_capacity(jsonc.len());
    let mut chars = jsonc.chars().peekable();
    let mut in_string = false;

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = ' ';
                for skipped in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    previous = skipped;
                }
            }
            _ => out.push(c),
        }
    }

    out
}

#[test]
fn the_committed_machines_config_parses_and_offers_its_classes() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/machines/wrangler.jsonc");
    let jsonc = std::fs::read_to_string(&path).expect("read apps/machines/wrangler.jsonc");
    let wrangler: Value = serde_json::from_str(&strip_comments(&jsonc)).expect("wrangler.jsonc");
    let raw = &wrangler["vars"]["MACHINES_CONFIG"];

    let config = MachinesConfig::parse(&raw.to_string()).expect("MACHINES_CONFIG parses");

    let apps = raw["apps"].as_object().expect("MACHINES_CONFIG.apps");
    assert!(!apps.is_empty());
    for (app, allowed) in apps {
        let app = AppName::parse(app.as_str()).expect("app name");
        let classes = allowed["classes"].as_array().expect("an app's classes");
        assert!(!classes.is_empty());

        for class in classes {
            let class = ClassName::parse(class.as_str().expect("class name")).expect("class");
            assert!(config.class_for(&app, &class).is_ok(), "{class:?}");
        }
    }
}
