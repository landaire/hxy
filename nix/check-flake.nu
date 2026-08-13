let required = ["buck2" "cargo" "reindeer" "rustc" "nu" "pkg-config"]

for command in $required {
  if (which $command | is-empty) {
    error make {msg: $"missing required command: ($command)"}
  }
}

if $env.CARGO_NET_OFFLINE? != "true" {
  error make {msg: "CARGO_NET_OFFLINE must be true"}
}

let cargo_home = $env.CARGO_HOME? | default ""
if ($cargo_home | is-empty) {
  error make {msg: "CARGO_HOME must be set"}
}

if not (($cargo_home | path join "config.toml") | path exists) {
  error make {msg: "CARGO_HOME must contain config.toml"}
}

let egui_phosphor = "vendor/egui-phosphor"
if not ($egui_phosphor | path exists) {
  error make {msg: "Nix must materialize the egui-phosphor overlay"}
}
