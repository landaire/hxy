# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//android/tools:jdk_system_image.bzl", "jdk_system_image")
load("@prelude//tests:test_toolchain.bzl", "noop_test_toolchain")
load("@prelude//toolchains:android.bzl", "android_sdk_tools", "system_android_toolchain")
load("@prelude//toolchains:cxx.bzl", "CxxToolsInfo", "cxx_tools_info_toolchain", "system_cxx_toolchain")
load("@prelude//toolchains:dex.bzl", "system_dex_toolchain", "system_noop_dex_toolchain")
load("@prelude//toolchains:erlang.bzl", "system_erlang_toolchain")
load("@prelude//toolchains:genrule.bzl", "system_genrule_toolchain")
load("@prelude//toolchains:haskell.bzl", "system_haskell_toolchain")
load(
    "@prelude//toolchains:java.bzl",
    "java_test_toolchain",
    "javacd_toolchain",
    "system_java_bootstrap_toolchain",
    "system_java_lib",
    "system_java_tool",
    "system_prebuilt_jar_bootstrap_toolchain",
)
load("@prelude//toolchains:kotlin.bzl", "kotlincd_toolchain", "system_kotlin_bootstrap_toolchain")
load("@prelude//toolchains:ocaml.bzl", "system_ocaml_toolchain")
load("@prelude//toolchains:python.bzl", "remote_python_toolchain", "system_python_wheel_toolchain")
load("@prelude//toolchains:remote_test_execution.bzl", "remote_test_execution_toolchain")
load("@prelude//cxx:cxx_toolchain_types.bzl", "LinkerType")
load("@prelude//toolchains:rust.bzl", "system_rust_toolchain")
load("@prelude//toolchains:zip_file.bzl", "zip_file_toolchain")
load("@prelude//toolchains/go:system_go_bootstrap_toolchain.bzl", "system_go_bootstrap_toolchain")
load("@prelude//toolchains/go:system_go_toolchain.bzl", "system_go_toolchain")

def _hack_impl(ctx):
    return ctx.attrs.actual.providers

android_hack_alias = rule(
    impl = _hack_impl,
    attrs = {
        "actual": attrs.toolchain_dep(),
    },
)

# Forwards another toolchain's providers, but is itself a toolchain rule so
# it can be used as a `toolchain_dep`. Lets `:cxx` select its backing
# toolchain on the target OS.
toolchain_alias = rule(
    impl = _hack_impl,
    attrs = {
        "actual": attrs.toolchain_dep(),
    },
    is_toolchain_rule = True,
)

def _wasm_cxx_tools_impl(ctx):
    # rustc's wasm32-wasip2 target links a cdylib into a component with
    # wasm-component-ld; the "wasm" linker type makes the prelude emit
    # wasm-ld flags instead of the host's mach-o/ELF ones. Absolute paths
    # for the no-PATH-search build shim (see the shellHook that writes
    # these config values).
    return [
        DefaultInfo(),
        CxxToolsInfo(
            compiler = read_config("hxy_cxx", "cc", None),
            compiler_type = "clang",
            cxx_compiler = read_config("hxy_cxx", "cxx", None),
            asm_compiler = read_config("hxy_cxx", "cc", None),
            asm_compiler_type = "clang",
            rc_compiler = None,
            cvtres_compiler = None,
            archiver = read_config("hxy_cxx", "ar", None),
            archiver_type = "gnu",
            linker = read_config("hxy_wasm", "linker", None),
            linker_type = LinkerType("wasm"),
        ),
    ]

wasm_cxx_tools = rule(
    impl = _wasm_cxx_tools_impl,
    attrs = {},
)

def hxy_system_toolchains():
    """
    All the default toolchains, suitable for a quick demo or early prototyping.
    Most real projects should copy/paste the implementation to configure them.
    """
    android_sdk_tools(
        name = "android_sdk_tools",
        visibility = ["PUBLIC"],
    )

    jdk_system_image(
        name = "jdk_system_image",
        core_for_system_modules_jar = ":android_sdk_tools[core-for-system-modules.jar]",
    )

    system_android_toolchain(
        name = "android",
        android_sdk_tools_target = ":android_sdk_tools",
        jdk_system_image = ":jdk_system_image",
        visibility = ["PUBLIC"],
    )

    android_hack_alias(
        name = "android-hack",
        actual = ":cxx",
        visibility = ["PUBLIC"],
    )

    # The build-script cc shim runs the compiler through the prelude's
    # from_any_dir.py, which uses os.execl and does NOT search PATH. A bare
    # "clang" therefore fails to exec; feed absolute tool paths from the Nix
    # dev shell (see the shellHook that writes these config values).
    # The host cxx toolchain. `:cxx` below selects between this and the wasm
    # variant on the target OS, so a build under //platforms:wasm32-wasip2
    # links plugin components with wasm-component-ld while native builds are
    # unchanged.
    system_cxx_toolchain(
        name = "cxx-host",
        compiler = read_config("hxy_cxx", "cc", None),
        cxx_compiler = read_config("hxy_cxx", "cxx", None),
        archiver = read_config("hxy_cxx", "ar", None),
        linker = read_config("hxy_cxx", "cc", None),
        visibility = ["PUBLIC"],
    )

    wasm_cxx_tools(
        name = "wasm_cxx_tools",
        visibility = ["PUBLIC"],
    )

    cxx_tools_info_toolchain(
        name = "cxx-wasm",
        cxx_tools_info = ":wasm_cxx_tools",
        # wasm has no shared libraries; a cdylib links statically into one
        # self-contained component.
        link_style = "static",
        visibility = ["PUBLIC"],
    )

    toolchain_alias(
        name = "cxx",
        actual = select({
            "DEFAULT": ":cxx-host",
            "prelude//os:wasi": ":cxx-wasm",
        }),
        visibility = ["PUBLIC"],
    )

    # The prelude's cxx toolchain attr defaults to a select with a
    # config//:none branch naming toolchains//:cxx_no_default_deps. That
    # branch is never chosen in a configured build, but unconfigured
    # queries (buck2 uquery deps(...)) traverse every branch and fail if
    # the target does not exist.
    android_hack_alias(
        name = "cxx_no_default_deps",
        actual = ":cxx",
        visibility = ["PUBLIC"],
    )

    system_dex_toolchain(
        name = "dex",
        android_sdk_tools_target = ":android_sdk_tools",
        visibility = ["PUBLIC"],
    )

    system_noop_dex_toolchain(
        name = "empty_dex",
        visibility = ["PUBLIC"],
    )

    system_genrule_toolchain(
        name = "genrule",
        visibility = ["PUBLIC"],
    )

    system_go_toolchain(
        name = "go",
        visibility = ["PUBLIC"],
    )

    system_go_bootstrap_toolchain(
        name = "go_bootstrap",
        visibility = ["PUBLIC"],
    )

    system_haskell_toolchain(
        name = "haskell",
        visibility = ["PUBLIC"],
    )

    javacd_toolchain(
        name = "java",
        java = ":java_tool",
        javac = ":javac_tool",
        jar = ":jar_tool",
        jlink = ":jlink_tool",
        jmod = ":jmod_tool",
        jrt_fs_jar = ":jrt_fs_jar",
        visibility = ["PUBLIC"],
    )

    system_java_bootstrap_toolchain(
        name = "java_bootstrap",
        java = ":java_tool",
        javac = ":javac_tool",
        jlink = ":jlink_tool",
        jmod = ":jmod_tool",
        jrt_fs_jar = ":jrt_fs_jar",
        visibility = ["PUBLIC"],
    )

    javacd_toolchain(
        name = "java_for_android",
        java = ":java_tool",
        javac = ":javac_tool",
        jar = ":jar_tool",
        jlink = ":jlink_tool",
        jmod = ":jmod_tool",
        jrt_fs_jar = ":jrt_fs_jar",
        visibility = ["PUBLIC"],
    )

    javacd_toolchain(
        name = "java_for_host_test",
        java = ":java_tool",
        javac = ":javac_tool",
        java_for_tests = ":java_tool",
        jar = ":jar_tool",
        jlink = ":jlink_tool",
        jmod = ":jmod_tool",
        jrt_fs_jar = ":jrt_fs_jar",
        visibility = ["PUBLIC"],
    )

    java_test_toolchain(
        name = "java_test",
        visibility = [
            "PUBLIC",
        ],
    )

    java_home = read_root_config("java", "java_home", "/usr/local/java-runtime/impl/17")
    system_java_tool(
        name = "java_tool",
        tool_name = java_home + "/bin/java",
        visibility = ["PUBLIC"],
    )

    system_java_tool(
        name = "javac_tool",
        tool_name = java_home + "/bin/javac",
        visibility = ["PUBLIC"],
    )

    system_java_tool(
        name = "jar_tool",
        tool_name = java_home + "/bin/jar",
        visibility = ["PUBLIC"],
    )

    system_java_tool(
        name = "jlink_tool",
        tool_name = java_home + "/bin/jlink",
        visibility = ["PUBLIC"],
    )

    system_java_tool(
        name = "jmod_tool",
        tool_name = java_home + "/bin/jmod",
        visibility = ["PUBLIC"],
    )

    system_java_lib(
        name = "jrt_fs_jar",
        jar = java_home + "/lib/jrt-fs.jar",
    )

    kotlincd_toolchain(
        name = "kotlin",
        visibility = ["PUBLIC"],
    )

    system_kotlin_bootstrap_toolchain(
        name = "kotlin_bootstrap",
        visibility = ["PUBLIC"],
    )

    kotlincd_toolchain(
        name = "kotlin_for_android",
        visibility = ["PUBLIC"],
    )

    system_ocaml_toolchain(
        name = "ocaml",
        visibility = ["PUBLIC"],
    )

    # TODO(ianc) Make this not a bootstrap toolchain
    system_prebuilt_jar_bootstrap_toolchain(
        name = "prebuilt_jar",
        java = ":java_tool",
        visibility = ["PUBLIC"],
    )

    system_prebuilt_jar_bootstrap_toolchain(
        name = "prebuilt_jar_bootstrap",
        java = ":java_tool",
        visibility = ["PUBLIC"],
    )

    system_prebuilt_jar_bootstrap_toolchain(
        name = "prebuilt_jar_bootstrap_no_snapshot",
        java = ":java_tool",
        visibility = ["PUBLIC"],
    )

    remote_python_toolchain(
        name = "python",
        visibility = ["PUBLIC"],
    )

    system_python_wheel_toolchain(
        name = "python_wheel",
        visibility = ["PUBLIC"],
    )

    system_rust_toolchain(
        name = "rust",
        default_edition = "2021",
        # The prelude's default triple select (toolchains/rust.bzl) covers
        # only linux/macos/windows. Add a wasi branch so a build under
        # //platforms:wasm32-wasip2 cross-compiles the plugin components; the
        # host branches replicate the prelude defaults for the two platforms
        # this repo builds on, leaving native builds unchanged.
        rustc_target_triple = select({
            "prelude//os:wasi": "wasm32-wasip2",
            "prelude//os:linux": select({
                "prelude//cpu:arm64": "aarch64-unknown-linux-gnu",
                "prelude//cpu:x86_64": "x86_64-unknown-linux-gnu",
            }),
            "prelude//os:macos": select({
                "prelude//cpu:arm64": "aarch64-apple-darwin",
                "prelude//cpu:x86_64": "x86_64-apple-darwin",
            }),
        }),
        # -Copt-level is required: cc-rs build scripts read OPT_LEVEL, which the
        # prelude derives from this flag (Buck has no cargo profile to supply
        # it). 0 matches cargo's dev profile; rustc's opt-level-tied defaults
        # (debug-assertions, overflow-checks) then follow automatically.
        # Debug info defaults to line-tables-only -- enough for file:line in
        # backtraces without the symbol-table bloat of full debuginfo. Override
        # either via config, e.g. `@modes/debug-full`.
        rustc_flags = [
            "-Copt-level=" + read_config("hxy_rust", "opt-level", "0"),
            "-Cdebuginfo=" + read_config("hxy_rust", "debuginfo", "line-tables-only"),
        ] + select({
            # A shipped plugin component wants size, not the host default; the
            # later flags win over the ones above for the wasm build only.
            "prelude//os:wasi": ["-Copt-level=s", "-Cstrip=symbols"],
            "DEFAULT": [],
        }),
        visibility = ["PUBLIC"],
    )

    remote_test_execution_toolchain(
        name = "remote_test_execution",
        visibility = ["PUBLIC"],
    )

    noop_test_toolchain(
        name = "test",
        visibility = ["PUBLIC"],
    )

    zip_file_toolchain(
        name = "zip_file",
        visibility = ["PUBLIC"],
    )

    system_erlang_toolchain(
        name = "erlang-default",
        visibility = ["PUBLIC"],
    )
