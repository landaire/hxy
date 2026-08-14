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

let expected = open --raw BUCK

if $result.stdout != $expected {
  let expected_lines = $expected | lines
  let generated_lines = $result.stdout | lines
  let line_count = [($expected_lines | length) ($generated_lines | length)] | math max
  let difference = 0..<$line_count
    | each {|index|
      {
        index: $index
        expected: ($expected_lines | get -o $index)
        generated: ($generated_lines | get -o $index)
      }
    }
    | where {|line| $line.expected != $line.generated}
    | first

  if $difference == null {
    error make {
      msg: $"BUCK differs at end of file; expected ($expected | str length) characters, generated ($result.stdout | str length)"
    }
  } else {
    error make {
      msg: $"BUCK is stale at line ($difference.index + 1); expected: ($difference.expected); generated: ($difference.generated)"
    }
  }
}
