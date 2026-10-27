use std::{fs::read_to_string, path::PathBuf};

use toml::{Value, from_str};

use super::{Files, isolated, options};
use crate::config::{RegenerateOptions, Sources, regenerate_config};

#[test]
fn stripping_unknowns_preserves_terminal_maps_and_dynamic_names() {
	if !isolated("config::tests::hierarchy::residue::stripping_unknowns_preserves_terminal_maps_and_dynamic_names", &[]) { return; }

	let files = Files::new();
	let fixture = concat!(
		"[global]\nserver_name = 'residue.example'\nfuture_root = [1, 2]\n",
		"[global.network.http]\nfuture_timeout = 17\n",
		"[global.rooms]\ndefault_power_level_content_override = {users_default = 7, events = \
		 {'m.room.name' = 42}}\n",
		"[global.storage_provider.sample.local]\nbase_path = 'baseline/media'\n",
	);

	let input = files.write("input.toml", fixture);
	let original: Value = from_str(fixture).expect("residue fixture parses");
	let sources = Sources { paths: vec![input], overrides: None };

	for strip_unknown in [false, true] {
		let (output, rendered) = render(&files, &sources, "output", strip_unknown);
		let parsed: Value = from_str(&rendered).expect("output TOML parses");
		let global = &parsed["global"];

		assert_eq!(global.get("future_root").is_none(), strip_unknown);
		let timeout_absent = global["network"]["http"]
			.get("future_timeout")
			.is_none();

		assert_eq!(timeout_absent, strip_unknown);

		assert_eq!(global["rooms"], original["global"]["rooms"]);
		assert_eq!(global["storage_provider"], original["global"]["storage_provider"]);

		let sources = Sources { paths: vec![output], overrides: None };
		let (again, second) = render(&files, &sources, "again", strip_unknown);

		if strip_unknown {
			assert_eq!(from_str::<Value>(&second).expect("second TOML parses"), parsed);
			assert!(rendered.contains("#future_root = [1, 2]"));
			assert!(rendered.contains("#future_timeout = 17"));
		} else {
			assert_eq!(second, rendered);
		}

		let sources = Sources { paths: vec![again], overrides: None };
		let (_, third) = render(&files, &sources, "third", strip_unknown);

		assert_eq!(third, second);
	}
}

fn render(
	files: &Files,
	sources: &Sources,
	name: &str,
	strip_unknown: bool,
) -> (PathBuf, String) {
	let output = files
		.0
		.join(format!("{name}-{strip_unknown}.toml"));

	let settings = RegenerateOptions { strip_unknown, ..options(&output) };

	regenerate_config(sources, settings).expect("residue regeneration");
	let rendered = read_to_string(&output).expect("output read");

	(output, rendered)
}
