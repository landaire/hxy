"""An execution platform that consults a remote action cache.

Actions still run locally. When [buck2_re_client] names a cache, Buck2 first
looks up each action's digest there and uploads the result after running it.
CI selects this platform when the job reaches the shared cache of
landaire/build-cache; otherwise builds use prelude//platforms:default.

Action keys cover each command and its declared inputs, not tools that build
scripts find through the Nix shell (pkg-config, for example). `host_tools`
names the runner image and the flake and goes into every action key as a
platform property, so a flake or image update does not reuse stale results.
"""

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")

def _cached_execution_platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    configuration = ConfigurationInfo(constraints = constraints, values = {})

    if ctx.attrs.remote_cache_enabled and not ctx.attrs.host_tools:
        fail("[buck2_re_client] names a cache, but [ci] host_tools is not set")
    properties = {"host-tools": ctx.attrs.host_tools} if ctx.attrs.host_tools else None

    platform = ExecutionPlatformInfo(
        label = ctx.label.raw_target(),
        configuration = configuration,
        executor_config = CommandExecutorConfig(
            local_enabled = True,
            remote_enabled = False,
            remote_cache_enabled = ctx.attrs.remote_cache_enabled,
            allow_cache_uploads = ctx.attrs.remote_cache_enabled,
            remote_execution_properties = properties,
            use_windows_path_separators = ctx.attrs.use_windows_path_separators,
        ),
    )
    return [
        DefaultInfo(),
        platform,
        PlatformInfo(label = str(ctx.label.raw_target()), configuration = configuration),
        ExecutionPlatformRegistrationInfo(
            platforms = [platform],
            exec_marker_constraint = get_exec_platform_marker(),
        ),
    ]

cached_execution_platform = rule(
    impl = _cached_execution_platform_impl,
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "host_tools": attrs.option(attrs.string(), default = None),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "remote_cache_enabled": attrs.bool(default = False),
        "use_windows_path_separators": attrs.bool(default = False),
    },
)

def remote_cache_configured() -> bool:
    # Enabling the cache without an engine is fatal to every action, not a
    # fallback to local.
    return read_root_config("buck2_re_client", "engine_address") != None
