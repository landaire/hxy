if not ("reindeer.toml" | path exists) {
  error make {msg: "missing Reindeer configuration"}
}

let result = (^reindeer -c reindeer.toml buckify --stdout | complete)

if $result.exit_code != 0 {
  error make {msg: $"Reindeer could not generate the dependency graph: ($result.stderr)"}
}

if not ($result.stdout | str contains 'crate_root = "third-party/overrides/egui-phosphor/src/lib.rs"') {
  error make {msg: "Reindeer must resolve egui-phosphor from the local overlay"}
}

if not ($result.stdout | str contains 'name = "hxy-0.5-hxy"') {
  error make {msg: "Reindeer must generate the hxy binary target"}
}

if not ($result.stdout | str contains 'name = "hxy"') {
  error make {msg: "Reindeer must generate the hermetic Nix-backed hxy target"}
}

if $result.stdout != (open --raw BUCK) {
  error make {msg: "BUCK is stale; regenerate it with reindeer -c reindeer.toml buckify"}
}
