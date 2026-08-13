let cargo_home = $env.CARGO_HOME? | default ""
if ($cargo_home | is-empty) {
  error make {msg: "CARGO_HOME must be set"}
}

let result = ^cargo metadata --frozen --offline --locked --format-version 1 | complete

if $result.exit_code != 0 {
  error make {msg: "Cargo could not resolve the locked source graph offline"}
}
