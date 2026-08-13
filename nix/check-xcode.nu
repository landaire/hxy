let expected_developer_dir = "/Applications/Xcode.app/Contents/Developer"
let expected_version = "Xcode 26.6"
let expected_build = "Build version 17F113"
let expected_sdk_version = "26.5"
let expected_arch = "arm64"

if (^/usr/bin/uname -m | str trim) != $expected_arch {
  error make {msg: $"expected Darwin architecture ($expected_arch)"}
}

let developer_dir = (^/usr/bin/env -u DEVELOPER_DIR /usr/bin/xcode-select -p | str trim)
if $developer_dir != $expected_developer_dir {
  error make {msg: $"expected DEVELOPER_DIR ($expected_developer_dir), got ($developer_dir)"}
}

let version = (^/usr/bin/env -u DEVELOPER_DIR /usr/bin/xcodebuild -version | lines)
if (($version | length) < 2) or (($version | get 0) != $expected_version) or (($version | get 1) != $expected_build) {
  error make {msg: $"expected ($expected_version) / ($expected_build), got ($version | str join '; ')"}
}

let sdk = (^/usr/bin/env -u DEVELOPER_DIR /usr/bin/xcrun --sdk macosx --show-sdk-path | str trim)
if not ($sdk | str starts-with $expected_developer_dir) {
  error make {msg: $"macOS SDK is outside the selected Xcode: ($sdk)"}
}

if (^/usr/bin/env -u DEVELOPER_DIR /usr/bin/xcrun --sdk macosx --show-sdk-version | str trim) != $expected_sdk_version {
  error make {msg: $"expected macOS SDK ($expected_sdk_version)"}
}

print $expected_developer_dir
